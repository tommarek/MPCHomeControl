//! `mpc-controller-growatt` — the battery/inverter controller
//! (loxone_smart_home's own Growatt control is OFF — never two controllers on one inverter).
//!
//! Subscribes the north command topic, translates the battery intent into the Growatt MQTT command
//! vocabulary ([`translate`]), and — only when *both* the config `armed` flag and the
//! `MPC_CONTROLLER_ARM` env token are set — publishes it. Otherwise it logs the would-send messages.
//! A `valid_until` deadman reverts to `regular` (handing control back) if commands go silent.
//!
//! **Command acknowledgement (armed only).** Growatt drops/NAKs commands sent faster than ~1 Hz, so
//! a fire-and-forget publish can be silently lost. Each armed publish is therefore confirmed against
//! the bridge's `energy/solar/result` reply (`{command, success}`, matched by the command sub-path,
//! the same correlation loxone uses) and retried with backoff on failure/timeout. Because the event
//! loop must keep polling to *receive* those replies while a publish awaits its ack, the connection
//! is driven on its own task that forwards commands/telemetry/results to the worker via shared state.

mod config;
mod translate;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use controller_common::PendingSlot;
use controller_protocol::{
    actions_changed, topics, BatteryPayload, BatterySlot, ControlCommand, ControllerStatus, Mode,
    Payload, PlannedAction, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::config::GrowattConfig;
use crate::translate::{translate, SlotWindow, TranslateCfg};

/// The exact env token required (alongside `armed: true`) before anything is sent to hardware.
const ARM_TOKEN: &str = "i-understand-this-actuates";
/// How long to wait for a command's `energy/solar/result` ack before retrying.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times to (re)send a single command before giving up (and logging the loss).
const MAX_ACK_ATTEMPTS: u32 = 4;
/// item G: how often the pending NEXT command is checked against the clock. Well under the "≤1 s"
/// the spec asks for, so a command is promoted within a fraction of a second of its `apply_at` mark.
const PENDING_CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// item 6 (rework cycle 2, restart safety, finding 4): the widest `apply_at` lead this controller
/// ever trusts on a retained `/next` — block 1 is always a fine (15-min) block by design (§6), so a
/// legitimately-published next command's mark is never more than one block ahead of the poll that
/// sent it. See [`State::on_next_command`]'s restart-safety guard.
const ONE_BLOCK: chrono::Duration = chrono::Duration::minutes(15);

/// Hardware actuation needs BOTH the config flag and the env token — neither alone is enough.
fn resolve_armed(cfg: &GrowattConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// The block's 15-minute window as the inverter's local `HH:MM` (Growatt expects local time).
/// `offset` is the site-local offset **at the block start** (see `GrowattConfig::offset_at`) — with
/// an IANA zone configured, the window stays correct across DST changeovers.
fn slot_window(block_start: DateTime<Utc>, offset: chrono::FixedOffset) -> SlotWindow {
    let local = block_start.with_timezone(&offset);
    let stop = local + ChronoDuration::minutes(15);
    // The 23:45 block would wrap to stop="00:00" — an inverted same-day window whose firmware
    // handling is unverified (it may reject it or run it inverted). Clamp to 23:59; the next
    // block re-programs the slot at midnight anyway, so the lost minute is harmless.
    let stop = if stop.date_naive() != local.date_naive() {
        "23:59".to_string()
    } else {
        stop.format("%H:%M").to_string()
    };
    SlotWindow {
        start: local.format("%H:%M").to_string(),
        stop,
    }
}

/// Exponential-ish backoff between command resends (base 1 s, ×2, capped) — also honours Growatt's
/// ~1 Hz floor so a first retry never lands faster than the inverter accepts.
fn ack_backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(4)).clamp(1, 15))
}

/// Pending armed commands awaiting their `energy/solar/result` ack, keyed by command sub-path
/// (e.g. `batteryfirst/set/stopsoc`); the connection task fulfils the oneshot with the `success` flag.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

/// Live state shared with the connection-driver task: the freshest telemetry SoC (percent).
type SharedSoc = Arc<Mutex<Option<(f64, Instant)>>>;

/// Driver → worker messages: a received command's bytes (current or item G's next-command topic),
/// or "the MQTT session reconnected" (which invalidates the change-only skip — see the ConnAck arm).
enum WorkerMsg {
    Command(Vec<u8>),
    NextCommand(Vec<u8>),
    Reconnected,
}

/// How old the telemetry SoC may be before `battery_hold` falls back to the command's `soc_kwh`.
/// Telemetry normally arrives every few seconds–minutes; a partially-dead bridge (commands flow,
/// telemetry silent) used to pin the hold stop-SoC to an hours-old cached value.
const TELEMETRY_SOC_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// Bounded failsafe-revert retries (see `revert_attempts`) before backing off to
/// [`REVERT_RETRY_BACKOFF`].
const MAX_REVERT_ATTEMPTS: usize = 5;

/// How long to wait after exhausting [`MAX_REVERT_ATTEMPTS`] before trying the failsafe revert
/// again. The burst of fast retries is for a blip; this is for a durable outage. It must exist:
/// the ACKs the revert waits on come from the growatt MQTT **bridge**, not the broker, so the most
/// likely failure (bridge process down, modbus wedged, NAK storm) leaves our own MQTT session
/// perfectly healthy and no `ConnAck` ever fires — tying the re-arm to reconnection alone gave up
/// forever after ~2.5 minutes, latching the inverter in whatever the last brain command programmed
/// (grid-charging at a since-expired cheap window, or off through a day of PV) with nothing behind
/// it. One attempt per period is far too slow to wedge rumqttc's request channel.
const REVERT_RETRY_BACKOFF: Duration = Duration::from_secs(600);

struct State {
    cfg: GrowattConfig,
    tcfg: TranslateCfg,
    client: AsyncClient,
    /// Live broker connection? Set on ConnAck, cleared on a poll error. Publishing while
    /// disconnected only queues the message for a burst replay on reconnect — stale inverter
    /// programming applied long after its block, with no `valid_until` to stop it.
    connected: Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
    last_seq: Option<u64>,
    last_actions: Vec<PlannedAction>,
    last_command_at: Option<DateTime<Utc>>,
    soc: SharedSoc,
    pending: Pending,
    reverted: bool,
    /// Failed failsafe-revert attempts so far. The retry exists because the outage that trips a
    /// deadman is usually the one that makes the revert fail — but it must be BOUNDED: each attempt
    /// pushes a full action batch into rumqttc's fixed-size request channel, which the event loop
    /// does not drain while disconnected, so retrying every tick forever would fill it and wedge
    /// the controller (including its reconnect path) permanently.
    revert_attempts: usize,
    /// When the revert gave up after [`MAX_REVERT_ATTEMPTS`], for the [`REVERT_RETRY_BACKOFF`] retry.
    revert_gave_up_at: Option<Instant>,
    /// The deadman has expired and the failsafe path has run (or is running). Separate from
    /// `reverted`, which now means "the revert LANDED" and is only set at the very end: reporting
    /// `deadman_expired: self.reverted` meant the status published BY the failsafe revert itself —
    /// and by each of its retries, which return early — always said `false`. The one signal that
    /// the safety net fired was therefore never observable on the wire.
    deadman_fired: bool,
    /// Wall-clock validity, kept for logging only — the deadman compares `deadman_at`.
    valid_until: Option<DateTime<Utc>>,
    /// Monotonic copy of `valid_until` (via [`controller_common::monotonic_deadline`]), so a
    /// backward NTP/wall-clock step can't extend a stale battery command's validity. Mirrors the
    /// loxone controller's hardening.
    deadman_at: Option<Instant>,
    /// item G: the pending NEXT command, held until its `apply_at` mark (or dropped if it ages out
    /// first) — see [`State::on_next_command`] / [`State::check_pending`]. Named `pending_next`
    /// (not `pending`) to avoid colliding with the ACK-wait map above, which is an unrelated concept.
    pending_next: PendingSlot<ControlCommand>,
    /// Ordering high-water for the `/next` topic — tracked SEPARATELY from `last_seq` (the
    /// current-command channel), since a pending command hasn't been applied yet and must not let a
    /// stale/duplicate redelivery on this topic reject a genuinely newer one on the other.
    pending_last_seq: Option<u64>,
    /// item 2 (rework cycle 2, belt and braces for finding 1): the `block_start` of the last command
    /// actually adopted. A CURRENT command whose `block_start` is EARLIER than this is a stale poll
    /// — see the loxone controller's identical field for the full rationale. A repeated command for
    /// the SAME block still applies (not a switch); `actions_changed`'s own change-only skip already
    /// handles the "nothing to do" case for growatt.
    applied_block_start: Option<DateTime<Utc>>,
    /// rework cycle 3, rule 3: the exact payload most recently applied for `applied_block_start` —
    /// lets `on_command` reject a DIFFERENT payload for a block already applied (extends the guard
    /// above from "strictly earlier block" to "same block, already applied from a promoted
    /// snapshot"; belt and braces alongside the publisher's own `Promoted`-snapshot authority, which
    /// should already stop a diverged command from being sent at all). Unlike the loxone controller,
    /// growatt does NOT need this for the byte-identical-repeat case — `actions_changed`'s own
    /// change-only skip in `adopt` already covers "must not reprogram the inverter twice with the
    /// same content" — this field exists purely for the reject-on-divergence guard.
    applied_payload: Option<Payload>,
    /// item 6 (rework cycle 2, restart safety, finding 4): has a CURRENT command been accepted at
    /// least once in this process? A retained `/next` is trusted only once this is `true` — see
    /// [`State::on_next_command`]'s doc for why a fresh process can't rely on the `/next` channel's
    /// own (much looser) freshness window alone.
    current_command_seen: bool,
    /// Rework cycle 5, item 4 (refuter finding 4, probe R6): the `battery_hold` stop-SoC percent,
    /// pinned once per block — `(block_start, pct)` — when that block's `BatteryHold` command is
    /// first applied, from the promoted snapshot's telemetry/`soc_kwh` at that moment (see `adopt`).
    /// A later same-block adopt (fresh telemetry, or the publisher's `soc_kwh` drifting between
    /// polls now that item 3 lets those repeats through `actuation_eq`) reuses this stored percent
    /// instead of recomputing it, so `translate` reproduces the IDENTICAL actions and the inverter
    /// is never re-programmed mid-block. Cleared implicitly by simply being overwritten once
    /// `cmd.block_start` moves to a new block.
    held_pct: Option<(DateTime<Utc>, u32)>,
}

impl State {
    async fn soc_pct(&self) -> Option<f64> {
        match *self.soc.lock().await {
            Some((soc, at)) if at.elapsed() <= TELEMETRY_SOC_MAX_AGE => Some(soc),
            Some((soc, at)) => {
                eprintln!(
                    "[growatt] telemetry SoC {soc}% is {}s old — ignoring (command soc_kwh is fresher)",
                    at.elapsed().as_secs()
                );
                None
            }
            None => None,
        }
    }

    /// Adopt `cmd` as the controller's current decision — the change-only-skip/deadman bookkeeping
    /// and inverter send, identical whichever of three paths got here: an ordinary current-topic
    /// command, a next command that was already due on receipt, or one promoted by
    /// [`Self::check_pending`] at its mark. `reason` is a short label for the log line. `now` is the
    /// caller's clock reading, taken as a parameter (not read internally) so this method and the
    /// next-command path above it are testable with a synthetic clock.
    async fn adopt(&mut self, cmd: &ControlCommand, reason: &str, now: DateTime<Utc>) {
        let Payload::Battery(battery) = &cmd.payload else {
            println!(
                "[growatt] ignoring non-battery payload ({reason}, seq {})",
                cmd.command_seq
            );
            return;
        };
        let window = slot_window(cmd.block_start, self.cfg.offset_at(cmd.block_start));
        // Rework cycle 5, item 4: for `battery_hold`, pin the stop-SoC to the value computed the
        // FIRST time this block's command is applied, and keep reusing it for every later adopt in
        // the same block — never re-derive it from fresh telemetry or a drifted `soc_kwh` mid-block
        // (see `held_pct`'s doc). Every other slot is unaffected: it still gets live telemetry.
        let telemetry = self.soc_pct().await;
        let telemetry_soc_pct = if matches!(battery.slot, BatterySlot::BatteryHold) {
            let pct = match self.held_pct {
                Some((held_block, pct)) if held_block == cmd.block_start => pct,
                _ => {
                    let pct = telemetry
                        .map(|t| t.clamp(0.0, 100.0).round() as u32)
                        .or_else(|| {
                            battery
                                .soc_kwh
                                .map(|k| translate::soc_pct(k, self.tcfg.battery_capacity_kwh))
                        })
                        .unwrap_or_else(|| {
                            translate::soc_pct(battery.min_soc_kwh, self.tcfg.battery_capacity_kwh)
                        });
                    self.held_pct = Some((cmd.block_start, pct));
                    pct
                }
            };
            Some(f64::from(pct))
        } else {
            telemetry
        };
        let actions = translate(battery, &self.tcfg, &window, telemetry_soc_pct);

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(now);
        self.valid_until = Some(cmd.valid_until);
        self.deadman_at = Some(controller_common::monotonic_deadline(cmd.valid_until));
        self.reverted = false;
        self.revert_attempts = 0;
        self.revert_gave_up_at = None;
        self.deadman_fired = false;
        // item 2: record the block this adoption actually applies, for `on_command`'s monotonic-apply
        // guard — updated on every adopted command (current or next).
        self.applied_block_start = Some(cmd.block_start);
        self.applied_payload = Some(cmd.payload.clone());

        if !actions_changed(&self.last_actions, &actions) {
            println!(
                "[growatt] {reason} seq {} unchanged — skipping re-publish",
                cmd.command_seq
            );
            // Still refresh `mpc/status/growatt`: `translate` emits byte-identical actions for a
            // whole Regular/InverterOff stretch, so the skip path could otherwise keep the status
            // topic silent for HOURS while the controller is perfectly healthy — a monitor keying
            // on status freshness then reads an actively-commanded controller as dead. Publishes
            // the unchanged action list; nothing is re-sent to the inverter.
            self.publish_status(self.last_actions.clone()).await;
            return;
        }
        let ctx = format!("{reason} seq {} ({:?})", cmd.command_seq, battery.slot);
        self.apply(actions, &ctx).await;
    }

    /// `now` is the caller's clock reading, taken as a parameter (not read internally via
    /// `Utc::now()`) for the same reason `adopt`/`on_next_command`/`check_pending` do — so item 2's
    /// monotonic-apply guard below is testable with a synthetic clock.
    async fn on_command(&mut self, bytes: &[u8], now: DateTime<Utc>) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[growatt] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, now) {
            println!("[growatt] ignoring command: {why}");
            return;
        }
        // item 6: a genuinely fresh, accepted current command proves the current-topic channel — and
        // thus the brain/publisher — is actually alive right now. Set unconditionally from here on,
        // even if the monotonic-apply guard below still rejects THIS particular command.
        self.current_command_seen = true;
        if let Some(why) = self.same_block_guard_rejects(&cmd) {
            println!("[growatt] ignoring command: {why}");
            return;
        }
        self.adopt(&cmd, "command", now).await;
    }

    /// item 2 (belt and braces for finding 1) / rework cycle 4 item 3 (probe R4b): the monotonic
    /// same-block guard shared by the current-command path (`on_command`) and the next-command
    /// "due on receipt" path (`on_next_command` — a late `/next` whose `apply_at` has already
    /// passed used to skip straight to `adopt` with NO guard at all, the exact gap probe R4b
    /// exploited). Never let a command apply a block EARLIER than the one already applied (a stale
    /// poll of an old plan); a repeated command for the SAME block with the SAME actuation is a
    /// no-op (falls through to `adopt`, which refreshes the deadman); the SAME block with a
    /// DIFFERENT actuation (`Payload::actuation_eq`, item 2 — `soc_kwh` alone doesn't count) is
    /// rejected outright — a promoted block's content must not change mid-block. Returns the reject
    /// reason, or `None` if `cmd` may proceed to `adopt`.
    fn same_block_guard_rejects(&self, cmd: &ControlCommand) -> Option<String> {
        let applied = self.applied_block_start?;
        if cmd.block_start < applied {
            return Some(format!(
                "stale block_start {} < already-applied {applied} (a stale poll)",
                cmd.block_start
            ));
        }
        if cmd.block_start == applied {
            if let Some(p) = &self.applied_payload {
                if !p.actuation_eq(&cmd.payload) {
                    return Some(format!(
                        "block {} already applied with a DIFFERENT actuation — a promoted block's \
                         content must not change mid-block",
                        cmd.block_start
                    ));
                }
            }
        }
        None
    }

    /// item G: a NEXT command arrived on the `/next` topic. Gated exactly like the current-command
    /// path (`accept`, against the `/next` channel's OWN ordering high-water), then handed to the
    /// pending slot: applied right away if it's already due (a late plan, or an old producer that
    /// never set `apply_at`), otherwise held until [`Self::check_pending`] promotes it at the mark.
    ///
    /// item 6 (restart safety, finding 4): a controller that just (re)started reads its retained
    /// `/next` message immediately on subscribe — but the current-topic retained message may ALREADY
    /// be too stale to accept (its own tighter `valid_until`, ~`deadman_seconds`, may have already
    /// expired if the outage predates the restart), while the `/next` channel's own bound
    /// (`apply_at + deadman_seconds`, up to one block wide) can still look "fresh enough". A fresh
    /// process has no other way to know whether the current-topic channel is actually alive RIGHT
    /// NOW, so it must not act on a retained `/next` until it has proven that for itself — see
    /// `current_command_seen`. Additionally, a next command is trusted only when its mark is no more
    /// than [`ONE_BLOCK`] away (bounding how far in the future a retained message may still reach,
    /// belt and braces on top of the process-level check; a PAST `apply_at`, e.g. G1d's late plan,
    /// is unaffected — only how far AHEAD is bounded).
    async fn on_next_command(&mut self, bytes: &[u8], now: DateTime<Utc>) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[growatt] ignoring malformed next-command JSON: {e}");
                return;
            }
        };
        if !self.current_command_seen {
            println!(
                "[growatt] ignoring next command: no current command received yet this process \
                 (restart safety)"
            );
            return;
        }
        if let Some(at) = cmd.apply_at {
            if at - now > ONE_BLOCK {
                println!(
                    "[growatt] ignoring next command: apply_at {at} is {} ahead of {now} (more than \
                     one block — restart safety)",
                    at - now
                );
                return;
            }
        }
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.pending_last_seq, now) {
            println!("[growatt] ignoring next command: {why}");
            return;
        }
        self.pending_last_seq = Some(cmd.command_seq);
        let apply_at = cmd.apply_at;
        let valid_until = cmd.valid_until;
        match self.pending_next.receive(cmd, apply_at, valid_until, now) {
            Some(due) => {
                // rework cycle 4, item 3 (probe R4b): a late `/next` that's due on receipt must
                // pass the SAME same-block guard as the current-command path — see
                // `same_block_guard_rejects`'s doc.
                if let Some(why) = self.same_block_guard_rejects(&due) {
                    println!("[growatt] ignoring next command: {why}");
                    return;
                }
                self.adopt(&due, "next command (due on receipt)", now).await;
            }
            None => println!("[growatt] next command pending, apply_at={apply_at:?}"),
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

    /// Returns whether every action reached the inverter (always `true` in dry-run) — the deadman
    /// failsafe uses this to decide whether it may consider itself done.
    async fn apply(&mut self, mut actions: Vec<PlannedAction>, ctx: &str) -> bool {
        println!(
            "[growatt] {ctx} — {} action(s) [{}]:",
            actions.len(),
            if self.armed { "ARMED" } else { "dry-run" }
        );
        // Publish in order and STOP at the first failure. `translate` orders every mode
        // "params first, timeslot enable last" precisely so a truncated batch is fail-passive: a
        // half-applied slot with no enable is inert. Pushing on past a failure destroys that
        // guarantee — an unacked `stopsoc`/`acchargeenabled` followed by an ACKED `timeslot`
        // arms the slot against the PREVIOUS window's parameters (e.g. grid-charging to last
        // night's stop-soc, or discharging into a window that has since become expensive). The
        // skipped remainder is left `published: false`, so `fully_applied` is false and the whole
        // batch is re-applied on the next poll (~30 s).
        let mut aborted = false;
        for act in actions.iter_mut() {
            if aborted {
                println!(
                    "    SKIPPED {} {}  ({})",
                    act.target, act.message, act.reason
                );
                continue;
            }
            if self.armed {
                act.published = self.publish_with_ack(&act.target, &act.message).await;
                aborted = !act.published;
            }
            println!(
                "    {} {} {}  ({})",
                match (self.armed, act.published) {
                    (false, _) => "would-send",
                    (true, true) => "ACKED",
                    (true, false) => "UNACKED!",
                },
                act.target,
                act.message,
                act.reason
            );
        }
        if aborted {
            eprintln!(
                "[growatt] {ctx}: aborted the batch at the first unacked action — the remaining \
                 actions (incl. any timeslot enable) were NOT sent, leaving the inverter in its \
                 previous state"
            );
        }
        // A command that never fully reached the inverter must NOT satisfy the change-only skip:
        // `actions_changed` compares only target/message, so recording failed actions here would
        // make every identical re-send (the publisher's 30 s poll) skip as "unchanged" — a mode
        // whose bytes never change (`regular`, `inverter_off` carry no per-block window) would then
        // never be retried, leaving the inverter in the previous state indefinitely (e.g. powered
        // OFF for a whole day of PV after a bridge blip). Clearing forces a full re-application on
        // the next poll — naturally rate-limited, and idempotent on the inverter side.
        let fully_applied = !self.armed || actions.iter().all(|a| a.published);
        if fully_applied {
            self.last_actions = actions.clone();
        } else {
            eprintln!("[growatt] {ctx}: not all actions acked — will re-apply on the next command");
            self.last_actions = Vec::new();
        }
        self.publish_status(actions).await;
        fully_applied
    }

    /// Publish one armed command and confirm it against `energy/solar/result`, retrying with backoff.
    /// Returns `true` only on a positive ack; `false` if every attempt failed/timed out (logged, never
    /// silently dropped). The correlation key is the command sub-path after `command_base/`.
    async fn publish_with_ack(&self, target: &str, message: &str) -> bool {
        let sub = target
            .strip_prefix(&format!("{}/", self.cfg.command_base))
            .unwrap_or(target)
            .to_string();
        // An explicit NAK is a *definitive* rejection — the inverter parsed the command and said
        // no; resending the identical bytes rarely helps and each cycle costs ack-timeout+backoff
        // (which also delays the deadman tick, since apply() runs inline in the select loop). One
        // courtesy retry covers transient inverter states; timeouts/drops keep the full budget.
        const MAX_NAK_ATTEMPTS: u32 = 2;
        let mut naks = 0u32;
        for attempt in 0..MAX_ACK_ATTEMPTS {
            // A long retry sequence must not outlive the command it serves: without this bound a
            // dead bridge turns each ~7-action batch into ~3 min of blind retries, starving the
            // deadman tick in the same select loop and backing up the command queue.
            if self.deadman_at.is_some_and(|d| Instant::now() >= d) {
                eprintln!("[growatt] {sub}: command deadman passed mid-retry — giving up");
                return false;
            }
            if !self.connected.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "[growatt] broker disconnected — not queueing {sub} (it would be replayed \
                     stale on reconnect); the deadman and the next command decide"
                );
                return false;
            }
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(sub.clone(), tx);
            // `try_publish`: the blocking form queues on rumqttc's 64-slot request channel, which
            // is only drained while a connection exists. The failsafe revert alone can issue ~140
            // publishes (5 attempts x ~7 actions x 4 ack retries), so during a broker outage the
            // channel fills and `publish().await` blocks indefinitely — freezing the deadman loop,
            // and then replaying every queued (now stale) inverter command when the broker returns.
            // A refused send is simply an unacked action: `apply()` already treats that as failure,
            // clears `last_actions` and lets the bounded revert retry.
            match self.client.try_publish(
                target,
                QoS::AtLeastOnce,
                false,
                message.as_bytes().to_vec(),
            ) {
                // `on_result` removed the pending entry when it delivered the ack, so the success
                // arm has nothing to clean up. Every other arm falls through to the unified cleanup
                // below before retrying.
                Ok(()) => match tokio::time::timeout(ACK_TIMEOUT, rx).await {
                    Ok(Ok(true)) => return true,
                    Ok(Ok(false)) => {
                        naks += 1;
                        eprintln!("[growatt] {sub} NAKed (attempt {})", attempt + 1);
                        if naks >= MAX_NAK_ATTEMPTS {
                            self.pending.lock().await.remove(&sub);
                            eprintln!(
                                "[growatt] GAVE UP on {sub} after {naks} explicit NAKs \
                                 (definitive rejection — not retrying further)"
                            );
                            return false;
                        }
                    }
                    Ok(Err(_)) => {
                        eprintln!("[growatt] {sub} ack dropped (attempt {})", attempt + 1)
                    }
                    Err(_) => eprintln!("[growatt] {sub} ack timeout (attempt {})", attempt + 1),
                },
                Err(e) => eprintln!("[growatt] publish {target} failed: {e}"),
            }
            // Drop this attempt's pending entry before the next insert, so a late/duplicate ack can't
            // be misrouted to the next attempt's sender and the map can't accrue stale entries.
            self.pending.lock().await.remove(&sub);
            if attempt + 1 < MAX_ACK_ATTEMPTS {
                tokio::time::sleep(ack_backoff(attempt)).await;
            }
        }
        eprintln!("[growatt] GAVE UP on {sub} after {MAX_ACK_ATTEMPTS} attempts");
        false
    }

    async fn check_deadman(&mut self) {
        if self.reverted {
            // A revert that exhausted its fast retries is retried on a slow backoff rather than
            // abandoned — see `REVERT_RETRY_BACKOFF`.
            match self.revert_gave_up_at {
                Some(t) if t.elapsed() >= REVERT_RETRY_BACKOFF => {
                    eprintln!(
                        "[growatt] retrying the failed failsafe revert after {} s of backoff",
                        REVERT_RETRY_BACKOFF.as_secs()
                    );
                    self.reverted = false;
                    self.revert_attempts = 0;
                    self.revert_gave_up_at = None;
                    self.deadman_at = Some(Instant::now());
                }
                _ => return,
            }
        }
        let Some(deadman) = self.deadman_at else {
            return;
        };
        if Instant::now() < deadman {
            return;
        }
        println!(
            "[growatt] DEADMAN expired (valid_until {:?}) → failsafe '{}'",
            self.valid_until, self.cfg.failsafe
        );
        // Clear the expired deadline BEFORE applying the failsafe: publish_with_ack's mid-retry
        // guard gives up on any send whose deadman has passed — which at this point is every send.
        // The failsafe command is the controller's own (it has no deadman); without this clear the
        // revert would return UNACKED with zero publish attempts, leaving the inverter latched in
        // the last commanded slot precisely when the safety net is supposed to release it.
        self.deadman_at = None;
        self.valid_until = None;
        let first_expiry = !self.deadman_fired;
        self.deadman_fired = true;
        // item G: a pending next command can be scheduled well beyond the current command's deadman
        // window (its `apply_at` is routinely minutes out). Left in place, it would later spring the
        // controller back out of this very failsafe at its own mark — discard it now, same as any
        // other stale decision the deadman exists to invalidate.
        self.pending_next.clear();
        if self.cfg.failsafe == "revert_to_regular" {
            let regular = BatteryPayload {
                slot: BatterySlot::Regular,
                export_enabled: true,
                inverter_on: true,
                charge_kw: 0.0,
                discharge_kw: 0.0,
                min_soc_kwh: self.cfg.battery_capacity_kwh * 0.2,
                max_soc_kwh: self.cfg.battery_capacity_kwh,
                soc_kwh: None,
            };
            let window = slot_window(Utc::now(), self.cfg.offset_at(Utc::now()));
            let actions = translate(&regular, &self.tcfg, &window, self.soc_pct().await);
            // Latch `reverted` only once the revert has actually LANDED. The outage that trips a
            // deadman (broker/bridge down) is exactly the one that makes this publish fail, and
            // marking it done regardless left the inverter latched in the last commanded slot —
            // the safety net silently not firing, precisely when it is needed. On failure we stay
            // un-reverted so the next tick retries.
            if !self.apply(actions, "failsafe revert_to_regular").await {
                self.revert_attempts += 1;
                if self.revert_attempts < MAX_REVERT_ATTEMPTS {
                    eprintln!(
                        "[growatt] failsafe revert NOT acked (attempt {}/{MAX_REVERT_ATTEMPTS}) — \
                         retrying on the next tick (inverter still in its last commanded mode)",
                        self.revert_attempts
                    );
                    // Restore the (already-expired) deadline so the next tick re-enters this path;
                    // `reverted` stays false. The clear above is only needed for the duration of
                    // the publish itself, so re-arming it here is what makes the retry reachable.
                    self.deadman_at = Some(deadman);
                    return;
                }
                // Stop retrying rather than keep stuffing the MQTT request channel: past this
                // point the broker is durably unreachable, so more attempts cannot land and would
                // only risk wedging the client. Re-armed on the next ConnAck (see `Reconnected`),
                // so a broker that returns while the BRAIN is still down still gets the revert —
                // without that re-arm this branch latched the inverter in its last commanded slot
                // permanently, which is the exact failure the deadman exists to prevent.
                self.revert_gave_up_at = Some(Instant::now());
                eprintln!(
                    "[growatt] failsafe revert still NOT acked after {MAX_REVERT_ATTEMPTS} \
                     attempts — backing off for {} s before retrying (inverter remains in its last \
                     commanded mode meanwhile)",
                    REVERT_RETRY_BACKOFF.as_secs()
                );
            }
        }
        // "hold" → issue nothing; the inverter keeps its last mode / loxone resumes. Publish the
        // status once anyway: `publish_status` is otherwise reached only via `apply()`, i.e. only on
        // the revert path, so under `failsafe: "hold"` `deadman_expired: true` never appeared on
        // `mpc/status/growatt` — invisible in exactly the mode that leaves the inverter latched in
        // its last commanded slot. Same gap, same fix, as the loxone controller.
        else if first_expiry {
            self.publish_status(Vec::new()).await;
        }
        self.reverted = true;
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
            deadman_expired: self.deadman_fired,
            telemetry: json!({ "soc_pct": self.soc_pct().await }),
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

/// Parse an `energy/solar/result` reply and fulfil the matching pending command's ack.
async fn on_result(bytes: &[u8], pending: &Pending) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    let Some(command) = v.get("command").and_then(|c| c.as_str()) else {
        return;
    };
    // `success` may be a bool or 1/0; absent ⇒ NOT success (the bridge always includes it — the
    // loxone-side consumer has always read `result.get("success", False)` — so a reply without it
    // is malformed and must not confirm an armed hardware write). Delivering `false` routes it
    // through the NAK path, which retries.
    let success = match v.get("success") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    };
    // Correlation is by command sub-path only — the bridge's result carries no send token, so a
    // QoS-1 duplicate of an EARLIER result for the same sub-path could in principle ack a newer
    // send. Sends per sub-path are serialized and each attempt's pending entry is dropped before
    // the next insert, so the window is one in-flight command; a real fix needs the bridge to
    // echo a per-send token (protocol change, tracked outside this crate).
    if let Some(tx) = pending.lock().await.remove(command) {
        let _ = tx.send(success);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "growatt.json5".to_string());
    let cfg = GrowattConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    if armed {
        println!(
            "*** mpc-controller-growatt ARMED — WILL PUBLISH to {} on mqtt://{}:{} ***",
            cfg.command_base, cfg.mqtt.host, cfg.mqtt.port
        );
    } else if cfg.armed {
        println!("--- mpc-controller-growatt: config armed but MPC_CONTROLLER_ARM token absent → DRY-RUN ---");
    } else {
        println!(
            "--- mpc-controller-growatt DRY-RUN — logging only, the inverter is untouched ---"
        );
    }

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

    let control_topic = cfg.control_topic.clone();
    let telemetry_topic = cfg.telemetry_topic.clone();
    // The bridge replies to commands on `<telemetry>/result` (e.g. `energy/solar/result`).
    let result_topic = format!("{telemetry_topic}/result");
    let controller_id = cfg.controller_id.clone();
    // item G: the sibling NEXT-command topic — a separate retained topic (see
    // `topics::command_next`'s doc) so the current-command subscription/handling above is untouched.
    let next_topic = topics::command_next(&cfg.controller_id);

    client.subscribe(&control_topic, QoS::AtLeastOnce).await?;
    client.subscribe(&next_topic, QoS::AtLeastOnce).await?;
    client.subscribe(&telemetry_topic, QoS::AtMostOnce).await?;
    client.subscribe(&result_topic, QoS::AtLeastOnce).await?;
    client
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;
    println!(
        "[growatt] listening on {control_topic} (+ {next_topic}; telemetry {telemetry_topic}, acks {result_topic})"
    );

    let soc: SharedSoc = Arc::new(Mutex::new(None));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    // Is a broker connection live RIGHT NOW? Set on ConnAck, cleared on any poll error. Without it,
    // an inverter command issued while the broker is down sits in rumqttc's request channel and is
    // flushed the moment the connection returns — so a `modbus/set value=0`, an `export/disable`,
    // or a `batteryfirst` timeslot from ten minutes ago is actuated on the inverter long after the
    // block it belonged to. Unlike the north-side commands these carry no `valid_until` and no
    // sequence number, so the bridge applies them unconditionally, and the `pending` ack map has
    // been cleared by then, so the replays are untracked. Better to refuse and let the deadman and
    // the next command decide.
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkerMsg>(16);

    // Connection-driver task: keep polling so command acks/telemetry are received while the worker
    // (below) awaits a publish ack. It forwards commands to the worker and fulfils pending acks.
    let driver_task = {
        let driver = client.clone();
        let soc = Arc::clone(&soc);
        let pending = Arc::clone(&pending);
        let connected = Arc::clone(&connected);
        let (ct, nt, tt, rt, cid) = (
            control_topic.clone(),
            next_topic.clone(),
            telemetry_topic.clone(),
            result_topic.clone(),
            controller_id.clone(),
        );
        tokio::spawn(async move {
            // Set on ConnAck, cleared only once every subscription has actually been accepted.
            let mut resubscribe = true;
            loop {
                if resubscribe {
                    let subs = [
                        (&ct, QoS::AtLeastOnce),
                        (&nt, QoS::AtLeastOnce),
                        (&tt, QoS::AtMostOnce),
                        (&rt, QoS::AtLeastOnce),
                    ];
                    let ok = subs
                        .iter()
                        .all(|(t, q)| driver.try_subscribe(t.as_str(), *q).is_ok());
                    if ok {
                        let _ = driver.try_publish(
                            topics::health(&cid),
                            QoS::AtLeastOnce,
                            true,
                            "online",
                        );
                        println!("[growatt] subscribed to {ct} (+next, telemetry, acks)");
                        resubscribe = false;
                    } else {
                        eprintln!(
                            "[growatt] re-subscribe refused (request channel still full) — \
                             retrying on the next poll"
                        );
                    }
                }
                match eventloop.poll().await {
                    // rumqttc doesn't replay subscriptions after a reconnect — re-subscribe on ConnAck.
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        // `try_*`, NEVER the awaiting forms. This arm runs INSIDE the task that
                        // owns the eventloop, and `subscribe().await` is a send on rumqttc's bounded
                        // request channel — which only `poll()` drains. During a broker outage the
                        // failsafe revert can enqueue ~140 publishes and fill all 64 slots; on
                        // reconnect this arm would then await a slot that only the very loop it is
                        // blocking could free. The task deadlocks for good: no commands, no
                        // telemetry, no acks, the revert can never land, and the inverter stays
                        // latched in its last commanded slot until someone restarts the container —
                        // exactly what the deadman exists to prevent.
                        connected.store(true, std::sync::atomic::Ordering::Relaxed);
                        // `try_subscribe` REFUSES while the request channel is still full — which is
                        // exactly the state a broker outage leaves it in, since the eventloop has
                        // not drained it yet at ConnAck time. Discarding that error left the armed
                        // controller subscribed to nothing while logging "(re)connected, subscribed":
                        // permanently deaf to commands, with the deadman its only remaining defence.
                        // Track the failure and retry on later poll iterations, once poll() has
                        // drained the channel.
                        resubscribe = true;
                        println!("[growatt] (re)connected to the broker");
                        // Invalidate the worker's change-only skip: retained/redelivered messages
                        // around a reconnect make "same bytes as last time" unreliable (a stale
                        // redelivered ack could even have confirmed a command that never applied).
                        // Re-applying one full batch after reconnect is cheap and idempotent.
                        let _ = cmd_tx.try_send(WorkerMsg::Reconnected);
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        if p.topic == ct {
                            // NEVER block the eventloop on a full worker queue: the worker's
                            // ack-waits need this loop polling, so a blocking send here is a
                            // circular wait — during a bridge outage the queue fills (~10 min),
                            // the driver stalls, no acks/keepalive flow, and the armed controller
                            // wedges permanently with the deadman starved. Dropping is safe: the
                            // publisher re-sends every poll and stale seqs are rejected anyway.
                            if let Err(e) = cmd_tx.try_send(WorkerMsg::Command(p.payload.to_vec()))
                            {
                                eprintln!("[growatt] worker busy — dropping a command ({e})");
                            }
                        } else if p.topic == nt {
                            // Same drop-if-busy reasoning as the current-command branch above: never
                            // block the eventloop on a full worker queue.
                            if let Err(e) =
                                cmd_tx.try_send(WorkerMsg::NextCommand(p.payload.to_vec()))
                            {
                                eprintln!("[growatt] worker busy — dropping a next command ({e})");
                            }
                        } else if p.topic == tt {
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&p.payload) {
                                if let Some(s) = v.get("SOC").and_then(|x| x.as_f64()) {
                                    *soc.lock().await = Some((s, Instant::now()));
                                }
                            }
                        } else if p.topic == rt {
                            on_result(&p.payload, &pending).await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);
                        eprintln!("[growatt] mqtt connection: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        })
    };

    let tcfg = cfg.translate_cfg();
    let mut state = State {
        cfg,
        tcfg,
        client,
        connected: Arc::clone(&connected),
        armed,
        last_seq: None,
        last_actions: Vec::new(),
        last_command_at: None,
        soc,
        pending,
        reverted: false,
        revert_attempts: 0,
        revert_gave_up_at: None,
        deadman_fired: false,
        valid_until: None,
        deadman_at: None,
        pending_next: PendingSlot::new(),
        pending_last_seq: None,
        applied_block_start: None,
        applied_payload: None,
        current_command_seen: false,
        held_pct: None,
    };

    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    let mut pending_check = tokio::time::interval(PENDING_CHECK_INTERVAL);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(WorkerMsg::Command(bytes)) => state.on_command(&bytes, Utc::now()).await,
                Some(WorkerMsg::NextCommand(bytes)) => state.on_next_command(&bytes, Utc::now()).await,
                Some(WorkerMsg::Reconnected) => {
                    state.last_actions = Vec::new();
                    // If the failsafe revert exhausted its attempts during the outage, the broker
                    // being back is precisely the condition that makes a retry viable. Re-arm the
                    // (already-expired) deadman so the next tick re-fires it. A live brain would
                    // also clear this via `on_command`, but the brain may still be down.
                    if state.reverted && state.revert_attempts >= MAX_REVERT_ATTEMPTS {
                        eprintln!(
                            "[growatt] broker reconnected after a failed failsafe revert — \
                             re-arming the deadman to retry it"
                        );
                        state.reverted = false;
                        state.revert_attempts = 0;
                        state.revert_gave_up_at = None;
                        state.deadman_at = Some(Instant::now());
                    }
                }
                None => {
                    // The driver task owns the only sender, so `None` means it ended (panic/abort) —
                    // log it so the controller stopping isn't a silent exit.
                    eprintln!("[growatt] connection task ended — shutting down");
                    break;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
            _ = pending_check.tick() => state.check_pending(Utc::now()).await,
        }
    }
    // The driver owns the eventloop, so the loop above exits only once it's already gone (cmd_tx
    // dropped) — this abort is then a no-op, but it ties the task's lifetime to main rather than
    // leaking the handle to runtime-drop. The broker publishes our `offline` last-will on the dropped
    // connection regardless.
    driver_task.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn slot_window_is_local_block() {
        let cest = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let w = slot_window(utc("2026-06-23T10:00:00Z"), cest);
        assert_eq!(w.start, "12:00");
        assert_eq!(w.stop, "12:15");
    }

    #[test]
    fn offset_at_tracks_dst_with_an_iana_zone() {
        let mut cfg: GrowattConfig = json5::from_str("{}").unwrap();
        cfg.timezone = Some("Europe/Prague".to_string());
        // Summer (CEST, +2) vs winter (CET, +1) — the fixed utc_offset_hours can't do both.
        let summer = cfg.offset_at(utc("2026-06-23T10:00:00Z"));
        let winter = cfg.offset_at(utc("2026-01-15T10:00:00Z"));
        assert_eq!(summer.local_minus_utc(), 2 * 3600);
        assert_eq!(winter.local_minus_utc(), 3600);
        // The slot window follows: the same UTC block start is 12:00 local in summer, 11:00 in winter.
        assert_eq!(
            slot_window(utc("2026-06-23T10:00:00Z"), summer).start,
            "12:00"
        );
        assert_eq!(
            slot_window(utc("2026-01-15T10:00:00Z"), winter).start,
            "11:00"
        );
        // Without a zone, the fixed offset applies unchanged year-round.
        cfg.timezone = None;
        assert_eq!(
            cfg.offset_at(utc("2026-01-15T10:00:00Z")).local_minus_utc(),
            2 * 3600
        );
    }

    #[test]
    fn ack_backoff_grows_and_respects_the_1hz_floor() {
        assert_eq!(ack_backoff(0), Duration::from_secs(1));
        assert_eq!(ack_backoff(1), Duration::from_secs(2));
        assert_eq!(ack_backoff(2), Duration::from_secs(4));
        assert!(ack_backoff(10) <= Duration::from_secs(15)); // capped
    }

    #[tokio::test]
    async fn on_result_fulfils_pending_by_command_subpath() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert("batteryfirst/set/stopsoc".to_string(), tx);
        on_result(
            br#"{"command":"batteryfirst/set/stopsoc","success":true}"#,
            &pending,
        )
        .await;
        assert!(rx.await.unwrap());
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn on_result_decodes_numeric_failure() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("modbus/set".to_string(), tx);
        on_result(br#"{"command":"modbus/set","success":0}"#, &pending).await;
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn on_result_without_success_field_is_not_an_ack() {
        // A malformed reply must not confirm an armed hardware write — it NAKs (and retries).
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("modbus/set".to_string(), tx);
        on_result(br#"{"command":"modbus/set"}"#, &pending).await;
        assert!(!rx.await.unwrap());
    }

    // ---- item G: the pending NEXT command, end to end through `State` ----
    //
    // `adopt`/`on_next_command`/`check_pending` take `now` as a parameter rather than reading
    // `Utc::now()` themselves, so these tests drive the real production code path with a fully
    // synthetic clock — no real sleeping, no flakiness. `armed: false` (dry-run) throughout: no
    // ack-wait/network send is attempted (`apply` only awaits `publish_with_ack` when armed), only
    // `State`'s own bookkeeping is exercised, alongside `try_publish` calls on an `AsyncClient` whose
    // `EventLoop` is dropped (never connects; failures are logged and ignored exactly as they are
    // against a real but unreachable broker).

    fn test_state() -> State {
        let cfg: GrowattConfig = json5::from_str("{}").unwrap();
        let tcfg = cfg.translate_cfg();
        let (client, _eventloop) =
            AsyncClient::new(MqttOptions::new("test", "127.0.0.1", 1883), 64);
        State {
            cfg,
            tcfg,
            client,
            connected: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            armed: false,
            last_seq: None,
            last_actions: Vec::new(),
            last_command_at: None,
            soc: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            reverted: false,
            revert_attempts: 0,
            revert_gave_up_at: None,
            deadman_fired: false,
            valid_until: None,
            deadman_at: None,
            pending_next: PendingSlot::new(),
            pending_last_seq: None,
            applied_block_start: None,
            applied_payload: None,
            current_command_seen: false,
            held_pct: None,
        }
    }

    /// One battery CURRENT-command envelope, serialized (what would arrive on the plain control topic).
    fn cur_cmd_bytes(
        seq: u64,
        block_start: DateTime<Utc>,
        valid_until: DateTime<Utc>,
        slot: BatterySlot,
    ) -> Vec<u8> {
        let cmd = ControlCommand {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: "growatt".to_string(),
            issued_at: utc("2026-09-22T12:00:00Z"),
            block_start,
            valid_until,
            plan_id: "plan-1".to_string(),
            command_seq: seq,
            apply_at: None,
            payload: Payload::Battery(BatteryPayload {
                slot,
                export_enabled: true,
                inverter_on: true,
                charge_kw: 0.0,
                discharge_kw: 0.0,
                min_soc_kwh: 2.0,
                max_soc_kwh: 10.0,
                soc_kwh: None,
            }),
        };
        serde_json::to_vec(&cmd).unwrap()
    }

    /// One battery next-command envelope, serialized (what would arrive on the `/next` topic).
    fn next_cmd_bytes(
        seq: u64,
        apply_at: Option<DateTime<Utc>>,
        valid_until: DateTime<Utc>,
        slot: BatterySlot,
    ) -> Vec<u8> {
        let cmd = ControlCommand {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: "growatt".to_string(),
            issued_at: utc("2026-09-22T12:00:00Z"),
            block_start: apply_at.unwrap_or(utc("2026-09-22T12:00:00Z")),
            valid_until,
            plan_id: "plan-1".to_string(),
            command_seq: seq,
            apply_at,
            payload: Payload::Battery(BatteryPayload {
                slot,
                export_enabled: true,
                inverter_on: true,
                charge_kw: 0.0,
                discharge_kw: 0.0,
                min_soc_kwh: 2.0,
                max_soc_kwh: 10.0,
                soc_kwh: None,
            }),
        };
        serde_json::to_vec(&cmd).unwrap()
    }

    /// Acceptance G1a: a next command received 40s before the mark is not applied on receipt, and is
    /// only promoted once `check_pending` is called with `now` at (or past) the mark.
    #[tokio::test]
    async fn g1a_next_command_applies_at_the_mark_not_on_receipt() {
        let mut state = test_state();
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let mark = utc("2026-09-22T12:15:00Z");
        let valid_until = mark + ChronoDuration::minutes(15);
        let bytes = next_cmd_bytes(10, Some(mark), valid_until, BatterySlot::ChargeFromGrid);

        state
            .on_next_command(&bytes, mark - ChronoDuration::seconds(40))
            .await;
        assert!(state.pending_next.is_pending());
        assert_eq!(state.last_seq, None, "must not adopt on receipt");

        state.check_pending(mark - ChronoDuration::seconds(1)).await;
        assert_eq!(state.last_seq, None, "must not adopt before the mark");
        assert!(state.pending_next.is_pending());

        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(10), "must adopt at the mark");
        assert!(!state.pending_next.is_pending());
        assert!(!state.last_actions.is_empty());
    }

    /// Acceptance G1b: a replacement next command received before the mark wins over the earlier one.
    #[tokio::test]
    async fn g1b_a_replacement_before_the_mark_wins() {
        let mut state = test_state();
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let mark = utc("2026-09-22T12:15:00Z");
        let valid_until = mark + ChronoDuration::minutes(15);

        state
            .on_next_command(
                &next_cmd_bytes(10, Some(mark), valid_until, BatterySlot::ChargeFromGrid),
                mark - ChronoDuration::seconds(40),
            )
            .await;
        state
            .on_next_command(
                &next_cmd_bytes(11, Some(mark), valid_until, BatterySlot::DischargeToGrid),
                mark - ChronoDuration::seconds(10),
            )
            .await;

        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(11), "the replacement must win");
    }

    /// Acceptance G1c: with no next command ever received, ticking `check_pending` is a no-op — the
    /// controller keeps whatever the current-command/deadman path was already doing.
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
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let now = utc("2026-09-22T12:15:40Z");
        let apply_at = now - ChronoDuration::seconds(5);
        let valid_until = apply_at + ChronoDuration::minutes(15);
        state
            .on_next_command(
                &next_cmd_bytes(5, Some(apply_at), valid_until, BatterySlot::ChargeFromGrid),
                now,
            )
            .await;
        assert_eq!(state.last_seq, Some(5));
        assert!(!state.pending_next.is_pending());
    }

    /// Acceptance G1e: a next-topic command with no `apply_at` at all (protocol default: "apply now")
    /// still parses and is applied immediately, same as an already-due one.
    #[tokio::test]
    async fn g1e_next_command_without_apply_at_applies_now() {
        let mut state = test_state();
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let now = utc("2026-09-22T12:15:40Z");
        let bytes = next_cmd_bytes(
            5,
            None,
            now + ChronoDuration::minutes(15),
            BatterySlot::ChargeFromGrid,
        );
        state.on_next_command(&bytes, now).await;
        assert_eq!(state.last_seq, Some(5));
    }

    /// Safety beyond the lettered criteria: the deadman-triggered failsafe must discard any pending
    /// next command, so it can't later spring the controller back out of the failsafe at its own
    /// (possibly much later) `apply_at` mark.
    #[tokio::test]
    async fn deadman_revert_clears_a_pending_next_command() {
        let mut state = test_state();
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let mark = utc("2026-09-22T12:15:00Z");
        state
            .on_next_command(
                &next_cmd_bytes(
                    1,
                    Some(mark),
                    mark + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                mark - ChronoDuration::minutes(5),
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

    // ---- item 2 (rework cycle 2, belt and braces for finding 1): monotonic apply ----

    /// The growatt-side analogue of the loxone D2 regression: a promoted NEXT command switches the
    /// battery slot at the mark; the next poll's CURRENT command re-publishes the SAME (pre-mark)
    /// plan's OLD block with a stale `block_start` — this must be ignored, not applied.
    #[tokio::test]
    async fn g2_stale_current_command_does_not_undo_the_just_promoted_slot() {
        let mut state = test_state();
        state.current_command_seen = true; // item 6: assume a live current-command channel here
        let mark = utc("2026-09-22T12:15:00Z");

        state
            .on_next_command(
                &next_cmd_bytes(
                    1000,
                    Some(mark),
                    mark + ChronoDuration::minutes(15),
                    BatterySlot::DischargeToGrid,
                ),
                mark - ChronoDuration::seconds(30),
            )
            .await;
        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(1000));

        let now = mark + ChronoDuration::seconds(5);
        let stale_current = cur_cmd_bytes(
            2000,
            mark - ChronoDuration::minutes(15), // the OLD (pre-mark) block
            now + ChronoDuration::seconds(120),
            BatterySlot::Regular,
        );
        state.on_command(&stale_current, now).await;

        assert_eq!(
            state.last_seq,
            Some(1000),
            "the stale current command must not be adopted over the just-promoted next command"
        );
    }

    /// A current command for the block ALREADY applied (same `block_start`) is not a switch and must
    /// still apply normally.
    #[tokio::test]
    async fn a_repeated_current_command_for_the_same_block_still_applies() {
        let mut state = test_state();
        let now = utc("2026-09-22T12:15:05Z");
        state
            .on_command(
                &cur_cmd_bytes(
                    1,
                    utc("2026-09-22T12:15:00Z"),
                    now + ChronoDuration::seconds(120),
                    BatterySlot::ChargeFromGrid,
                ),
                now,
            )
            .await;
        assert_eq!(state.last_seq, Some(1));

        let now2 = now + ChronoDuration::seconds(30);
        state
            .on_command(
                &cur_cmd_bytes(
                    2,
                    utc("2026-09-22T12:15:00Z"), // SAME block_start
                    now2 + ChronoDuration::seconds(120),
                    BatterySlot::ChargeFromGrid,
                ),
                now2,
            )
            .await;
        assert_eq!(
            state.last_seq,
            Some(2),
            "a same-block repeat is not a switch and must still apply"
        );
    }

    /// A current command for a genuinely NEWER block (the normal rollover case) still applies.
    #[tokio::test]
    async fn a_current_command_for_a_newer_block_still_applies() {
        let mut state = test_state();
        let now = utc("2026-09-22T12:15:05Z");
        state
            .on_command(
                &cur_cmd_bytes(
                    1,
                    utc("2026-09-22T12:15:00Z"),
                    now + ChronoDuration::seconds(120),
                    BatterySlot::ChargeFromGrid,
                ),
                now,
            )
            .await;
        assert_eq!(state.last_seq, Some(1));

        let now2 = utc("2026-09-22T12:30:05Z");
        state
            .on_command(
                &cur_cmd_bytes(
                    2,
                    utc("2026-09-22T12:30:00Z"), // NEWER block_start
                    now2 + ChronoDuration::seconds(120),
                    BatterySlot::Regular,
                ),
                now2,
            )
            .await;
        assert_eq!(state.last_seq, Some(2), "a newer block must still apply");
    }

    // ---- item 6 (rework cycle 2, restart safety, finding 4) ----

    /// The core restart scenario: a FRESH process (no current command received yet) must not act on
    /// a retained `/next` message, even one whose `apply_at`/`valid_until` still look fresh enough on
    /// their own — the current-topic channel's own aliveness hasn't been proven yet.
    #[tokio::test]
    async fn retained_next_ignored_on_a_fresh_process() {
        let mut state = test_state(); // current_command_seen defaults false
        let mark = utc("2026-09-22T12:15:00Z");
        state
            .on_next_command(
                &next_cmd_bytes(
                    1,
                    Some(mark),
                    mark + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                mark - ChronoDuration::seconds(30),
            )
            .await;
        assert!(
            !state.pending_next.is_pending(),
            "a fresh process must not even hold the retained next command pending"
        );
        state.check_pending(mark).await;
        assert_eq!(
            state.last_seq, None,
            "a fresh process must not promote a next command it never trusted"
        );
    }

    /// Once a current command DOES land (proving the channel alive), a next command received
    /// afterward is trusted normally.
    #[tokio::test]
    async fn next_command_trusted_once_a_current_command_has_been_seen() {
        let mut state = test_state();
        let now = utc("2026-09-22T12:10:00Z");
        state
            .on_command(
                &cur_cmd_bytes(
                    1,
                    now,
                    now + ChronoDuration::seconds(120),
                    BatterySlot::Regular,
                ),
                now,
            )
            .await;
        assert!(state.current_command_seen);

        let mark = utc("2026-09-22T12:15:00Z");
        state
            .on_next_command(
                &next_cmd_bytes(
                    2,
                    Some(mark),
                    mark + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                mark - ChronoDuration::seconds(30),
            )
            .await;
        assert!(
            state.pending_next.is_pending(),
            "trusted once a current command has been seen this process"
        );
    }

    /// Even with a live current-command channel, a next command whose mark is MORE than one block
    /// ahead is not trusted.
    #[tokio::test]
    async fn next_command_with_apply_at_more_than_one_block_ahead_is_ignored() {
        let mut state = test_state();
        state.current_command_seen = true;
        let now = utc("2026-09-22T12:00:00Z");
        let apply_at = now + ChronoDuration::minutes(16); // > ONE_BLOCK (15 min)
        state
            .on_next_command(
                &next_cmd_bytes(
                    1,
                    Some(apply_at),
                    apply_at + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                now,
            )
            .await;
        assert!(
            !state.pending_next.is_pending(),
            "a mark more than one block ahead must not be trusted"
        );
    }

    /// A next command exactly at the ONE_BLOCK boundary, or with no `apply_at` at all (apply now,
    /// e.g. a late plan), is unaffected by the "how far ahead" bound.
    #[tokio::test]
    async fn next_command_within_one_block_or_apply_now_is_unaffected_by_the_lead_bound() {
        let mut state = test_state();
        state.current_command_seen = true;
        let now = utc("2026-09-22T12:00:00Z");

        let at_bound = now + ChronoDuration::minutes(15);
        state
            .on_next_command(
                &next_cmd_bytes(
                    1,
                    Some(at_bound),
                    at_bound + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                now,
            )
            .await;
        assert!(
            state.pending_next.is_pending(),
            "exactly one block ahead must still be trusted"
        );

        let mut state2 = test_state();
        state2.current_command_seen = true;
        state2
            .on_next_command(
                &next_cmd_bytes(
                    2,
                    None,
                    now + ChronoDuration::minutes(15),
                    BatterySlot::ChargeFromGrid,
                ),
                now,
            )
            .await;
        assert_eq!(state2.last_seq, Some(2));
    }

    /// rework cycle 3, rule 3 — the exact live-demonstrated defect (refuter `c2-mqtt-mark0615.log`):
    /// at 06:15:00 the controller promotes the frozen `/next` (`sell_production`); at 06:15:48 a
    /// LATER poll's current command for the SAME block (12:15, not strictly earlier — item 2's old
    /// guard didn't cover it) carries a DIFFERENT slot (`discharge_to_grid`). Must now be rejected,
    /// not reprogrammed onto the inverter a second time.
    #[tokio::test]
    async fn r1_same_block_current_command_with_a_different_slot_is_rejected() {
        let mut state = test_state();
        state.current_command_seen = true;
        let mark = utc("2026-09-22T12:15:00Z");

        state
            .on_next_command(
                &next_cmd_bytes(
                    1000,
                    Some(mark),
                    mark + ChronoDuration::seconds(120),
                    BatterySlot::SellProduction,
                ),
                mark - ChronoDuration::seconds(30),
            )
            .await;
        state.check_pending(mark).await;
        assert_eq!(state.last_seq, Some(1000));

        let diverged = cur_cmd_bytes(
            1001,
            mark, // SAME block_start as the promoted next command
            mark + ChronoDuration::seconds(168),
            BatterySlot::DischargeToGrid,
        );
        state
            .on_command(&diverged, mark + ChronoDuration::seconds(48))
            .await;
        assert_eq!(
            state.last_seq,
            Some(1000),
            "the diverged same-block command must be rejected, not reprogrammed onto the inverter"
        );
    }

    /// Rework cycle 4, item 2 (probe R4a): a publisher restart mid-block re-polls the SAME
    /// actuation for a block already applied, but with a DIFFERENT (fresher) `soc_kwh` — pure
    /// telemetry, re-measured every tick, not a decision. This must be ACCEPTED as identical (the
    /// deadman refreshed, nothing re-sent to the inverter), not rejected as a diverged
    /// reprogramming — the defect that tripped both armed controllers' deadmen for up to
    /// `deadman_seconds` after every publisher restart (refuter finding 2, cycle 3).
    #[tokio::test]
    async fn r4a_same_block_soc_drift_alone_is_accepted_and_refreshes_the_deadman() {
        let mut state = test_state();
        state.current_command_seen = true;
        let block_start = utc("2026-09-22T12:15:00Z");
        let now = utc("2026-09-22T12:15:05Z");

        let cmd_with_soc = |seq: u64, valid_until: DateTime<Utc>, soc_kwh: f64| {
            let cmd = ControlCommand {
                schema_version: SCHEMA_VERSION.to_string(),
                controller_id: "growatt".to_string(),
                issued_at: utc("2026-09-22T12:00:00Z"),
                block_start,
                valid_until,
                plan_id: "plan-1".to_string(),
                command_seq: seq,
                apply_at: None,
                payload: Payload::Battery(BatteryPayload {
                    slot: BatterySlot::ChargeFromGrid,
                    export_enabled: true,
                    inverter_on: true,
                    charge_kw: 2.0,
                    discharge_kw: 0.0,
                    min_soc_kwh: 2.0,
                    max_soc_kwh: 10.0,
                    soc_kwh: Some(soc_kwh),
                }),
            };
            serde_json::to_vec(&cmd).unwrap()
        };

        state
            .on_command(
                &cmd_with_soc(1, now + ChronoDuration::seconds(120), 5.00),
                now,
            )
            .await;
        assert_eq!(state.last_seq, Some(1));
        let first_actions = state.last_actions.clone();
        let first_valid_until = state.valid_until.expect("set by the first command");

        let now2 = now + ChronoDuration::seconds(30);
        state
            .on_command(
                &cmd_with_soc(2, now2 + ChronoDuration::seconds(120), 5.03),
                now2,
            )
            .await;

        assert_eq!(
            state.last_seq,
            Some(2),
            "a same-block repeat differing only in soc_kwh must be accepted, not rejected"
        );
        assert_eq!(
            state.last_actions, first_actions,
            "no re-programming: soc_kwh does not affect this slot's translated actions"
        );
        assert!(
            state.valid_until.expect("set by the second command") > first_valid_until,
            "the deadman must be refreshed by the accepted repeat"
        );
    }

    /// Rework cycle 5, item 3 (refuter finding 3, probe R4c): a publisher restart re-seeds from the
    /// brain's unpinned block 0, whose `charge_kw` is a fresh LP float that can drift by ~1e-4 kW
    /// between two solves of the SAME economic decision. On a `Regular` slot — where `translate`
    /// never reads `charge_kw`/`discharge_kw` at all — that drift must be accepted as a no-op
    /// repeat (deadman refreshed), not rejected as a diverged reprogramming (which, before this fix,
    /// left the controller's actuation frozen until the ~120 s deadman fired).
    #[tokio::test]
    async fn r4c_same_block_regular_slot_chargekw_float_drift_is_accepted_and_refreshes_the_deadman(
    ) {
        let mut state = test_state();
        state.current_command_seen = true;
        let block_start = utc("2026-09-23T10:15:00Z");
        let now = utc("2026-09-23T10:15:05Z");

        let cmd_with_charge = |seq: u64, valid_until: DateTime<Utc>, charge_kw: f64| {
            let cmd = ControlCommand {
                schema_version: SCHEMA_VERSION.to_string(),
                controller_id: "growatt".to_string(),
                issued_at: block_start,
                block_start,
                valid_until,
                plan_id: "plan-1".to_string(),
                command_seq: seq,
                apply_at: None,
                payload: Payload::Battery(BatteryPayload {
                    slot: BatterySlot::Regular,
                    export_enabled: true,
                    inverter_on: true,
                    charge_kw,
                    discharge_kw: 0.0,
                    min_soc_kwh: 2.0,
                    max_soc_kwh: 10.0,
                    soc_kwh: Some(5.67),
                }),
            };
            serde_json::to_vec(&cmd).unwrap()
        };

        state
            .on_command(
                &cmd_with_charge(1, now + ChronoDuration::seconds(120), 0.7609135912257146),
                now,
            )
            .await;
        assert_eq!(state.last_seq, Some(1));
        let first_valid_until = state.valid_until.expect("set by the first command");

        let now2 = now + ChronoDuration::seconds(30);
        state
            .on_command(
                &cmd_with_charge(2, now2 + ChronoDuration::seconds(120), 0.7612),
                now2,
            )
            .await;

        assert_eq!(
            state.last_seq,
            Some(2),
            "a same-block Regular-slot repeat differing only by charge_kw float drift must be \
             accepted, not rejected"
        );
        assert!(
            state.valid_until.expect("set by the second command") > first_valid_until,
            "the deadman must be refreshed by the accepted repeat"
        );
    }

    /// Rework cycle 5, item 4 (refuter finding 4, probe R6): a same-block `battery_hold` repeat
    /// whose `soc_kwh` has drifted (the publisher's `merge_actuation` now stamps fresh telemetry
    /// into `soc_kwh` on every poll, and item 3 lets a same-block repeat through `actuation_eq`
    /// regardless) must NOT re-program the inverter's stop-SoC. The stop-SoC is pinned to whatever
    /// was computed the FIRST time this block was applied and held for the rest of the block — one
    /// programming per block, not one per poll.
    #[tokio::test]
    async fn r6_battery_hold_stop_soc_is_programmed_once_per_block_despite_soc_kwh_drift() {
        let mut state = test_state();
        state.current_command_seen = true;
        let block_start = utc("2026-09-22T12:15:00Z");
        let now = utc("2026-09-22T12:15:05Z");
        let mk = |seq: u64, vu: DateTime<Utc>, soc: f64| {
            serde_json::to_vec(&ControlCommand {
                schema_version: SCHEMA_VERSION.to_string(),
                controller_id: "growatt".to_string(),
                issued_at: block_start,
                block_start,
                valid_until: vu,
                plan_id: "plan-1".to_string(),
                command_seq: seq,
                apply_at: None,
                payload: Payload::Battery(BatteryPayload {
                    slot: BatterySlot::BatteryHold,
                    export_enabled: true,
                    inverter_on: true,
                    charge_kw: 0.0,
                    discharge_kw: 0.0,
                    min_soc_kwh: 2.0,
                    max_soc_kwh: 10.0,
                    soc_kwh: Some(soc),
                }),
            })
            .unwrap()
        };
        let stop = |a: &[PlannedAction]| {
            a.iter()
                .filter(|x| x.target.ends_with("stopsoc"))
                .map(|x| x.message.clone())
                .collect::<Vec<_>>()
        };

        state
            .on_command(&mk(1, now + ChronoDuration::seconds(120), 5.0), now)
            .await;
        assert_eq!(state.last_seq, Some(1));
        let first_stop = stop(&state.last_actions);
        assert!(
            !first_stop.is_empty(),
            "sanity: hold must program a stopsoc"
        );

        let now2 = now + ChronoDuration::seconds(60);
        state
            .on_command(&mk(2, now2 + ChronoDuration::seconds(120), 6.0), now2)
            .await;
        assert_eq!(
            state.last_seq,
            Some(2),
            "a same-block soc_kwh drift is accepted (actuation_eq already ignores soc_kwh)"
        );
        assert_eq!(
            stop(&state.last_actions),
            first_stop,
            "the stop-SoC must be pinned to the FIRST application's value for the whole block, not \
             re-programmed by the drifted soc_kwh"
        );
    }

    /// Rework cycle 4, item 3 (probe R4b): a `/next` command that's due ON RECEIPT (its `apply_at`
    /// already passed) for a block ALREADY applied via the current-command path, carrying a
    /// DIFFERENT slot, must be rejected by the same same-block guard `on_command` uses — before
    /// this fix, `on_next_command`'s due-on-receipt branch skipped straight to `adopt` with no
    /// guard at all, so a late `/next` could reprogram the inverter a second time for a block the
    /// mark already committed to.
    #[tokio::test]
    async fn r4b_a_late_next_command_due_on_receipt_for_an_applied_block_with_a_different_slot_is_rejected(
    ) {
        let mut state = test_state();
        state.current_command_seen = true;
        let mark = utc("2026-09-22T12:15:00Z");

        state
            .on_command(
                &cur_cmd_bytes(
                    1,
                    mark,
                    mark + ChronoDuration::seconds(120),
                    BatterySlot::ChargeFromGrid,
                ),
                mark,
            )
            .await;
        assert_eq!(state.last_seq, Some(1));

        // A `/next` for the SAME block, with `apply_at` already in the past relative to `now` — due
        // on receipt — carrying a DIFFERENT slot.
        let late_next = next_cmd_bytes(
            2,
            Some(mark),
            mark + ChronoDuration::seconds(168),
            BatterySlot::DischargeToGrid,
        );
        state
            .on_next_command(&late_next, mark + ChronoDuration::seconds(48))
            .await;

        assert_eq!(
            state.last_seq,
            Some(1),
            "the diverged, due-on-receipt next command must be rejected, not adopted"
        );
    }
}
