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
use controller_common::UdpClient;
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
}

impl State {
    async fn on_command(&mut self, bytes: &[u8]) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[loxone] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, Utc::now()) {
            println!("[loxone] ignoring command: {why}");
            return;
        }
        let Payload::Loxone { writes } = &cmd.payload else {
            println!("[loxone] ignoring non-loxone payload");
            return;
        };

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(Utc::now());
        self.valid_until = Some(cmd.valid_until);
        self.deadman_at = Some(controller_common::monotonic_deadline(cmd.valid_until));
        self.reverted = false;

        // Send immediately on every command (no change-only skip) so new setpoints land at once; the
        // heartbeat timer re-sends between commands to keep `MPCActive` fresh and self-heal dropped UDP.
        let full = with_heartbeat(&self.cfg.heartbeat_key, writes, true);
        let actions: Vec<PlannedAction> = translate(&full, &self.target).into_iter().collect();
        // Remember the live datagram so the heartbeat can re-send it between commands.
        self.last_message = actions.first().map(|a| a.message.clone());
        let ctx = format!("command seq {}", cmd.command_seq);
        self.apply(actions, &ctx).await;
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
    client
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;

    let control_topic = cfg.control_topic.clone();
    println!("[loxone] listening on {control_topic} → UDP {target}");
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
    };

    // Set when a re-subscribe is refused (request channel still full after an outage); retried on
    // the deadman tick, by which point `poll()` has drained the channel.
    let mut resubscribe = false;
    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    let mut heartbeat = tokio::time::interval(HEARTBEAT_REFRESH);
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
                    if state
                        .client
                        .try_subscribe(&control_topic, QoS::AtLeastOnce)
                        .is_err()
                    {
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
                        .try_publish(
                            topics::health(&state.cfg.controller_id),
                            QoS::AtLeastOnce,
                            true,
                            "online",
                        )
                        .is_ok()
                {
                    resubscribe = false;
                    println!("[loxone] re-subscribed to {control_topic}");
                }
                state.check_deadman().await;
            }
            _ = heartbeat.tick() => state.heartbeat_refresh().await,
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
}
