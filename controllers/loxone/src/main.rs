//! `mpc-controller-loxone` — the unified Loxone controller. Every Loxone-bound
//! MPC decision (heating relays, EV power, future HVAC/boiler/shading) becomes one UDP virtual-input
//! datagram, exactly as `mpc-controller-growatt` owns the inverter.
//!
//! Subscribes the single `mpc/control/loxone` topic, prepends the `MPCActive` heartbeat gate,
//! translates the generic key→value writes into one `key=value;…` datagram ([`translate`]), and —
//! only with *both* the config `armed` flag and the `MPC_CONTROLLER_ARM` env token — sends it to the
//! Miniserver. Otherwise it logs the would-send datagram. On the `valid_until` deadman it releases the
//! gate (`MPCActive=0`, so Loxone reverts to its native logic) or holds. An independent ~10 s heartbeat
//! re-sends the live datagram so `MPCActive` stays fresh between commands and self-heals dropped UDP.

mod config;
mod translate;

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use controller_common::{PendingSlot, UdpClient};
use controller_protocol::{
    topics, ControlCommand, ControllerStatus, LoxoneWrite, Mode, Payload, PlannedAction,
    SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};

use crate::config::LoxoneControllerConfig;
use crate::translate::translate;

/// The exact env token required (alongside `armed: true`) before any datagram is sent.
const ARM_TOKEN: &str = "i-understand-this-actuates";

/// How often the live datagram is re-sent to keep `MPCActive` fresh on the Loxone side, independent
/// of the command stream (also self-heals a dropped UDP packet). Loxone's own staleness window on
/// `MPCActive` should comfortably exceed this.
const HEARTBEAT_REFRESH: Duration = Duration::from_secs(10);

/// item G: how often the pending NEXT command is checked against the clock. Well under the "≤1 s"
/// the spec asks for, so a command is promoted within a fraction of a second of its `apply_at` mark
/// rather than riding the (much coarser) 10 s heartbeat or 5 s deadman ticks.
const PENDING_CHECK_INTERVAL: Duration = Duration::from_millis(500);

fn resolve_armed(cfg: &LoxoneControllerConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// Prepend the heartbeat gate (when its key is configured) to a command's writes. `active` is the
/// `MPCActive` value: `true` (=1) on a live command, `false` (=0) on the deadman release.
fn with_heartbeat(heartbeat_key: &str, writes: &[LoxoneWrite], active: bool) -> Vec<LoxoneWrite> {
    let mut out = Vec::with_capacity(writes.len() + 1);
    if !heartbeat_key.is_empty() {
        out.push(LoxoneWrite {
            key: heartbeat_key.to_string(),
            value: f64::from(active),
        });
    }
    out.extend_from_slice(writes);
    out
}

struct State {
    cfg: LoxoneControllerConfig,
    target: String,
    client: AsyncClient,
    armed: bool,
    sender: Option<UdpClient>,
    last_seq: Option<u64>,
    last_command_at: Option<DateTime<Utc>>,
    reverted: bool,
    valid_until: Option<DateTime<Utc>>,
    /// Monotonic copy of `valid_until` for the deadman, so a backward wall-clock step can't delay the
    /// failsafe (Instant is unaffected by clock changes).
    deadman_at: Option<Instant>,
    /// The last armed datagram, re-sent by the heartbeat to keep the Loxone side fresh.
    last_message: Option<String>,
    /// item G: the pending NEXT command, held until its `apply_at` mark (or dropped if it ages out
    /// first) — see [`Self::on_next_command`] / [`Self::check_pending`].
    pending_next: PendingSlot<ControlCommand>,
    /// Ordering high-water for the `/next` topic — tracked SEPARATELY from `last_seq` (the
    /// current-command channel), since a pending command hasn't been applied yet and must not let a
    /// stale/duplicate redelivery on this topic reject a genuinely newer one on the other.
    pending_last_seq: Option<u64>,
}

impl State {
    /// Adopt `cmd` as the controller's current decision — the deadman/seq bookkeeping and datagram
    /// send, identical whichever of three paths got here: an ordinary current-topic command, a next
    /// command that was already due on receipt, or one promoted by [`Self::check_pending`] at its
    /// mark. `reason` is a short label for the log line. `now` is the caller's clock reading — taking
    /// it as a parameter (rather than reading `Utc::now()` here) is what makes this method, and the
    /// next-command path above it, testable with a synthetic clock (see the crate's tests).
    async fn adopt(&mut self, cmd: &ControlCommand, reason: &str, now: DateTime<Utc>) {
        let Payload::Loxone { writes } = &cmd.payload else {
            println!(
                "[loxone] ignoring non-loxone payload ({reason}, seq {})",
                cmd.command_seq
            );
            return;
        };

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(now);
        self.valid_until = Some(cmd.valid_until);
        self.deadman_at = Some(controller_common::monotonic_deadline(cmd.valid_until));
        self.reverted = false;

        // Send immediately on every command (no change-only skip) so new setpoints land at once; the
        // heartbeat timer re-sends between commands to keep `MPCActive` fresh and self-heal dropped UDP.
        let full = with_heartbeat(&self.cfg.heartbeat_key, writes, true);
        let actions: Vec<PlannedAction> = translate(&full, &self.target).into_iter().collect();
        // Remember the live datagram so the heartbeat can re-send it between commands.
        self.last_message = actions.first().map(|a| a.message.clone());
        let ctx = format!("{reason} seq {}", cmd.command_seq);
        self.apply(actions, &ctx).await;
    }

    async fn on_command(&mut self, bytes: &[u8]) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[loxone] ignoring malformed command JSON: {e}");
                return;
            }
        };
        let now = Utc::now();
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, now) {
            println!("[loxone] ignoring command: {why}");
            return;
        }
        self.adopt(&cmd, "command", now).await;
    }

    /// item G: a NEXT command arrived on the `/next` topic. Gated exactly like the current-command
    /// path (`accept`, against the `/next` channel's OWN ordering high-water), then handed to the
    /// pending slot: applied right away if it's already due (a late plan, or an old producer that
    /// never set `apply_at`), otherwise held until [`Self::check_pending`] promotes it at the mark.
    async fn on_next_command(&mut self, bytes: &[u8], now: DateTime<Utc>) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[loxone] ignoring malformed next-command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.pending_last_seq, now) {
            println!("[loxone] ignoring next command: {why}");
            return;
        }
        self.pending_last_seq = Some(cmd.command_seq);
        let apply_at = cmd.apply_at;
        let valid_until = cmd.valid_until;
        match self.pending_next.receive(cmd, apply_at, valid_until, now) {
            Some(due) => self.adopt(&due, "next command (due on receipt)", now).await,
            None => println!("[loxone] next command pending, apply_at={apply_at:?}"),
        }
    }

    /// item G: called on every [`PENDING_CHECK_INTERVAL`] tick — promotes the pending next command
    /// once the clock reaches its `apply_at` mark, never before. A no-op when nothing is pending or
    /// the mark hasn't arrived yet.
    async fn check_pending(&mut self, now: DateTime<Utc>) {
        if let Some(due) = self.pending_next.poll(now) {
            self.adopt(&due, "next command (due at mark)", now).await;
        }
    }

    async fn apply(&mut self, mut actions: Vec<PlannedAction>, ctx: &str) {
        println!(
            "[loxone] {ctx} — {} datagram(s) [{}]:",
            actions.len(),
            if self.armed { "ARMED" } else { "dry-run" }
        );
        for act in actions.iter_mut() {
            if self.armed {
                if let Some(sender) = &self.sender {
                    match sender.send(&act.message) {
                        Ok(()) => act.published = true,
                        Err(e) => eprintln!("[loxone] UDP send to {} failed: {e}", self.target),
                    }
                }
            }
            println!(
                "    {} {} {}  ({})",
                if act.published { "SENT" } else { "would-send" },
                act.target,
                act.message,
                act.reason
            );
        }
        self.publish_status(actions).await;
    }

    /// Re-send the live datagram so `MPCActive` (and every setpoint) stays fresh on the Loxone side
    /// between commands — independent of the command stream, and self-healing against UDP loss. Goes
    /// quiet once the deadman has released the gate (or in dry-run / before the first command).
    async fn heartbeat_refresh(&self) {
        if !self.armed || self.reverted {
            return;
        }
        // The deadman and heartbeat are independent select! timers, so a heartbeat tick can land
        // BETWEEN a command's expiry and the deadman tick that latches `reverted` — re-asserting
        // `MPCActive=1` plus the stale relays at the exact moment the gate should be aging out.
        // Never refresh past the deadline, whichever timer wins the race.
        if self.deadman_at.is_none_or(|d| Instant::now() >= d) {
            return;
        }
        let (Some(sender), Some(msg)) = (&self.sender, &self.last_message) else {
            return;
        };
        if let Err(e) = sender.send(msg) {
            eprintln!("[loxone] heartbeat UDP send to {} failed: {e}", self.target);
        }
    }

    async fn check_deadman(&mut self) {
        let Some(deadman) = self.deadman_at else {
            return;
        };
        if Instant::now() < deadman {
            return;
        }
        let first = !self.reverted;
        if first {
            println!(
                "[loxone] DEADMAN expired (valid_until {:?}) → failsafe '{}'",
                self.valid_until, self.cfg.failsafe
            );
        }
        let first_release = !self.reverted;
        self.reverted = true;
        // item G: a pending next command can be scheduled well beyond the current command's deadman
        // window (its `apply_at` is routinely minutes out). Left in place, it would later spring the
        // controller back out of this very failsafe at its own mark — discard it now, same as any
        // other stale decision the deadman exists to invalidate.
        self.pending_next.clear();
        if self.cfg.failsafe == "release" {
            // Drop the gate: `MPCActive=0` → loxone reverts to its native logic across every
            // output. Re-sent on EVERY deadman tick while released (idempotent): `release` exists
            // for analog-gate wiring with no Off-Delay timeout, where a single lost UDP datagram
            // would leave `MPCActive=1` latched — exactly the stale state the failsafe must
            // clear. A newly accepted command resets `reverted`/`deadman_at` and re-arms.
            let release = with_heartbeat(&self.cfg.heartbeat_key, &[], false);
            let actions: Vec<PlannedAction> =
                translate(&release, &self.target).into_iter().collect();
            if first_release {
                self.apply(actions, "failsafe release (MPCActive=0)").await;
            } else {
                // Subsequent ticks re-send the datagram SILENTLY. Going through `apply()` every
                // 5 s printed a header plus a line per action and published a QoS1 status — ~720
                // MQTT messages and ~1400 log lines per hour, for as long as the brain was down,
                // onto a Synology whose containers run without log rotation. The datagram itself is
                // what heals a dropped packet; the announcement only needs to happen once.
                if self.armed {
                    if let Some(sender) = &self.sender {
                        for act in &actions {
                            if let Err(e) = sender.send(&act.message) {
                                eprintln!(
                                    "[loxone] failsafe release re-send to {} failed: {e}",
                                    self.target
                                );
                            }
                        }
                    }
                }
            }
        }
        // "hold" → send nothing on the wire; the last datagram persists until loxone's own staleness
        // handles it. But DO publish the status once, so `deadman_expired: true` is observable in
        // the mode the house actually runs: `apply()` (the only other caller of publish_status) is
        // reached solely on the `release` path, so under the shipped `failsafe: "hold"` the one
        // signal that the safety net fired never appeared on `mpc/status/loxone` at all.
        else if first_release {
            self.publish_status(Vec::new()).await;
        }
    }

    async fn publish_status(&self, actions: Vec<PlannedAction>) {
        let status = ControllerStatus {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: self.cfg.controller_id.clone(),
            at: Utc::now(),
            mode: if self.armed {
                Mode::Armed
            } else {
                Mode::DryRun
            },
            last_command_at: self.last_command_at,
            deadman_expired: self.reverted,
            telemetry: serde_json::Value::Null,
            actions,
        };
        if let Ok(json) = serde_json::to_string(&status) {
            // `try_publish`, not the blocking `publish`. rumqttc only drains its bounded request
            // channel while a connection exists, so during the very outage that trips the deadman
            // the queue fills and `publish().await` blocks FOREVER — stalling this controller's
            // whole event loop, deadman tick included. A dropped status message costs nothing:
            // status is re-published on the next command or tick.
            let _ = self.client.try_publish(
                topics::status(&self.cfg.controller_id),
                QoS::AtLeastOnce,
                false,
                json.into_bytes(),
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "loxone.json5".to_string());
    let cfg = LoxoneControllerConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    let target = cfg.loxone_target();
    if armed {
        println!("*** mpc-controller-loxone ARMED — WILL SEND UDP to {target} ***");
    } else if cfg.armed {
        println!("--- mpc-controller-loxone: config armed but MPC_CONTROLLER_ARM token absent → DRY-RUN ---");
    } else {
        println!("--- mpc-controller-loxone DRY-RUN — logging only, loxone is untouched ---");
    }

    let sender = if armed {
        Some(UdpClient::bind(target.clone())?)
    } else {
        None
    };

    let mut opts = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    let health = topics::health(&cfg.controller_id);
    opts.set_last_will(LastWill::new(
        health.clone(),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    client
        .subscribe(&cfg.control_topic, QoS::AtLeastOnce)
        .await?;
    // item G: the sibling NEXT-command topic — a separate retained topic (see
    // `topics::command_next`'s doc) so the current-command subscription/handling above is untouched.
    let next_topic = topics::command_next(&cfg.controller_id);
    client.subscribe(&next_topic, QoS::AtLeastOnce).await?;
    client
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;

    let control_topic = cfg.control_topic.clone();
    println!("[loxone] listening on {control_topic} (+ {next_topic}) → UDP {target}");
    let mut state = State {
        cfg,
        target,
        client,
        armed,
        sender,
        last_seq: None,
        last_command_at: None,
        reverted: false,
        valid_until: None,
        deadman_at: None,
        last_message: None,
        pending_next: PendingSlot::new(),
        pending_last_seq: None,
    };

    // Set when a re-subscribe is refused (request channel still full after an outage); retried on
    // the deadman tick, by which point `poll()` has drained the channel.
    let mut resubscribe = false;
    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    let mut heartbeat = tokio::time::interval(HEARTBEAT_REFRESH);
    let mut pending_check = tokio::time::interval(PENDING_CHECK_INTERVAL);
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                // rumqttc doesn't replay subscriptions after a reconnect — re-subscribe on every ConnAck.
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    let id = state.cfg.controller_id.clone();
                    // `try_*` — this arm runs inside the task polling the eventloop, and the
                    // awaiting forms send on rumqttc's bounded request channel, which only `poll()`
                    // drains. A channel filled during an outage would make this await a slot only
                    // this loop can free: a permanent deadlock of the controller.
                    // A refused subscribe (request channel still full after an outage) must be
                    // RETRIED, not discarded: otherwise the controller logs "(re)connected,
                    // subscribed" while being subscribed to nothing — deaf to every command.
                    let control_ok = state
                        .client
                        .try_subscribe(&control_topic, QoS::AtLeastOnce)
                        .is_ok();
                    let next_ok = state
                        .client
                        .try_subscribe(&next_topic, QoS::AtLeastOnce)
                        .is_ok();
                    if !control_ok || !next_ok {
                        resubscribe = true;
                        eprintln!(
                            "[{}] re-subscribe refused (request channel full) — retrying",
                            state.cfg.controller_id
                        );
                    } else {
                        resubscribe = false;
                    }
                    // Folded into the same retry as the subscribe: a refused `online` would
                    // otherwise leave the retained health topic saying "offline" (the last will)
                    // while the controller runs — a monitor then reads a live controller as down.
                    if state
                        .client
                        .try_publish(topics::health(&id), QoS::AtLeastOnce, true, "online")
                        .is_err()
                    {
                        resubscribe = true;
                    }
                    println!("[loxone] (re)connected to the broker");
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic == control_topic {
                        state.on_command(&p.payload).await;
                    } else if p.topic == next_topic {
                        state.on_next_command(&p.payload, Utc::now()).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[loxone] mqtt connection: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = deadman.tick() => {
                // Retry a re-subscribe the ConnAck arm could not place (the request channel was
                // still full); by now `poll()` has drained it. Without this the controller stays
                // subscribed to nothing until the NEXT reconnect, i.e. deaf indefinitely.
                if resubscribe
                    && state
                        .client
                        .try_subscribe(&control_topic, QoS::AtLeastOnce)
                        .is_ok()
                    && state
                        .client
                        .try_subscribe(&next_topic, QoS::AtLeastOnce)
                        .is_ok()
                    && state
                        .client
                        .try_publish(
                            topics::health(&state.cfg.controller_id),
                            QoS::AtLeastOnce,
                            true,
                            "online",
                        )
                        .is_ok()
                {
                    resubscribe = false;
                    println!("[loxone] re-subscribed to {control_topic} (+ {next_topic})");
                }
                state.check_deadman().await;
            }
            _ = heartbeat.tick() => state.heartbeat_refresh().await,
            _ = pending_check.tick() => state.check_pending(Utc::now()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_prepends_the_active_gate() {
        let w = vec![LoxoneWrite {
            key: "MPCHeatChodbaDole".into(),
            value: 1.0,
        }];
        // Live command: MPCActive=1 first, then the payload.
        let on = with_heartbeat("MPCActive", &w, true);
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].key, "MPCActive");
        assert_eq!(on[0].value, 1.0);
        assert_eq!(on[1].key, "MPCHeatChodbaDole");

        // Deadman release: gate off, no payload.
        let off = with_heartbeat("MPCActive", &[], false);
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].key, "MPCActive");
        assert_eq!(off[0].value, 0.0);

        // Disabled heartbeat: no gate prepended.
        let none = with_heartbeat("", &w, true);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].key, "MPCHeatChodbaDole");
    }

    // ---- item G: the pending NEXT command, end to end through `State` ----
    //
    // `adopt`/`on_next_command`/`check_pending` take `now` as a parameter rather than reading
    // `Utc::now()` themselves, so these tests drive the real production code path with a fully
    // synthetic clock — no real sleeping, no flakiness. `armed: false` (dry-run) throughout: no UDP
    // send is attempted, only `State`'s own bookkeeping is exercised, alongside `try_publish` calls
    // on an `AsyncClient` whose `EventLoop` is dropped (never connects; failures are logged and
    // ignored exactly as they are against a real but unreachable broker).

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn test_state() -> State {
        let (client, _eventloop) =
            AsyncClient::new(MqttOptions::new("test", "127.0.0.1", 1883), 64);
        State {
            cfg: json5::from_str("{}").unwrap(),
            target: "127.0.0.1:0".to_string(),
            client,
            armed: false,
            sender: None,
            last_seq: None,
            last_command_at: None,
            reverted: false,
            valid_until: None,
            deadman_at: None,
            last_message: None,
            pending_next: PendingSlot::new(),
            pending_last_seq: None,
        }
    }

    /// One loxone next-command envelope, serialized (what would arrive on the `/next` topic).
    fn next_cmd_bytes(
        seq: u64,
        apply_at: Option<DateTime<Utc>>,
        valid_until: DateTime<Utc>,
        value: f64,
    ) -> Vec<u8> {
        let cmd = ControlCommand {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: "loxone".to_string(),
            issued_at: utc("2026-09-22T12:00:00Z"),
            block_start: apply_at.unwrap_or(utc("2026-09-22T12:00:00Z")),
            valid_until,
            plan_id: "plan-1".to_string(),
            command_seq: seq,
            apply_at,
            payload: Payload::Loxone {
                writes: vec![LoxoneWrite {
                    key: "MPCHeatTest".to_string(),
                    value,
                }],
            },
        };
        serde_json::to_vec(&cmd).unwrap()
    }

    /// Acceptance G1a: a next command received 40s before the mark is not applied on receipt, and is
    /// only promoted once `check_pending` is called with `now` at (or past) the mark.
    #[tokio::test]
    async fn g1a_next_command_applies_at_the_mark_not_on_receipt() {
        let mut state = test_state();
        let mark = utc("2026-09-22T12:15:00Z");
        let valid_until = mark + chrono::Duration::minutes(15);
        let bytes = next_cmd_bytes(10, Some(mark), valid_until, 1.0);

        state
            .on_next_command(&bytes, mark - chrono::Duration::seconds(40))
            .await;
        assert!(state.pending_next.is_pending());
        assert_eq!(state.last_seq, None, "must not adopt on receipt");

        state
            .check_pending(mark - chrono::Duration::seconds(1))
            .await;
        assert_eq!(state.last_seq, None, "must not adopt before the mark");
        assert!(state.pending_next.is_pending());

        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(10), "must adopt at the mark");
        assert!(!state.pending_next.is_pending());
        assert!(state
            .last_message
            .as_deref()
            .is_some_and(|m| m.contains("MPCHeatTest=1")));
    }

    /// Acceptance G1b: a replacement next command received before the mark wins over the earlier one.
    #[tokio::test]
    async fn g1b_a_replacement_before_the_mark_wins() {
        let mut state = test_state();
        let mark = utc("2026-09-22T12:15:00Z");
        let valid_until = mark + chrono::Duration::minutes(15);

        state
            .on_next_command(
                &next_cmd_bytes(10, Some(mark), valid_until, 1.0),
                mark - chrono::Duration::seconds(40),
            )
            .await;
        state
            .on_next_command(
                &next_cmd_bytes(11, Some(mark), valid_until, 0.0),
                mark - chrono::Duration::seconds(10),
            )
            .await;

        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(11), "the replacement must win");
        assert!(state
            .last_message
            .as_deref()
            .is_some_and(|m| m.contains("MPCHeatTest=0")));
    }

    /// Acceptance G1c: with no next command ever received, ticking `check_pending` is a no-op — the
    /// controller keeps whatever the current-command/heartbeat path was already doing.
    #[tokio::test]
    async fn g1c_no_next_command_check_pending_is_a_no_op() {
        let mut state = test_state();
        state.check_pending(utc("2026-09-22T12:15:00Z")).await;
        assert_eq!(state.last_seq, None);
        assert!(!state.pending_next.is_pending());
    }

    /// Acceptance G1d: an `apply_at` already in the past (a late plan) is applied immediately by
    /// `on_next_command` itself — it need not wait for a `check_pending` tick.
    #[tokio::test]
    async fn g1d_a_past_apply_at_is_applied_immediately() {
        let mut state = test_state();
        let now = utc("2026-09-22T12:15:40Z");
        let apply_at = now - chrono::Duration::seconds(5);
        let valid_until = apply_at + chrono::Duration::minutes(15);
        state
            .on_next_command(&next_cmd_bytes(5, Some(apply_at), valid_until, 1.0), now)
            .await;
        assert_eq!(state.last_seq, Some(5));
        assert!(!state.pending_next.is_pending());
    }

    /// Acceptance G1e: a next-topic command with no `apply_at` at all (protocol default: "apply now")
    /// still parses and is applied immediately, same as an already-due one.
    #[tokio::test]
    async fn g1e_next_command_without_apply_at_applies_now() {
        let mut state = test_state();
        let now = utc("2026-09-22T12:15:40Z");
        let bytes = next_cmd_bytes(5, None, now + chrono::Duration::minutes(15), 1.0);
        state.on_next_command(&bytes, now).await;
        assert_eq!(state.last_seq, Some(5));
    }

    /// Safety beyond the lettered criteria: the deadman-triggered failsafe must discard any pending
    /// next command, so it can't later spring the controller back out of the failsafe at its own
    /// (possibly much later) `apply_at` mark.
    #[tokio::test]
    async fn deadman_revert_clears_a_pending_next_command() {
        let mut state = test_state();
        let mark = utc("2026-09-22T12:15:00Z");
        state
            .on_next_command(
                &next_cmd_bytes(1, Some(mark), mark + chrono::Duration::minutes(15), 1.0),
                mark - chrono::Duration::minutes(5),
            )
            .await;
        assert!(state.pending_next.is_pending());

        state.deadman_at = Some(Instant::now()); // already due
        state.check_deadman().await;
        assert!(
            !state.pending_next.is_pending(),
            "the deadman revert must discard a scheduled next command"
        );
    }
}
