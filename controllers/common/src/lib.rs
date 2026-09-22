//! Shared helpers for the hardware-controller crates.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Convert a wall-clock `valid_until` into a monotonic deadline for the deadman, immune to later
/// clock steps. Read both clocks adjacently and anchor to the monotonic one: computing `remaining`
/// from one `Utc::now()` and then adding it to a *later* `Instant::now()` would push the deadline out
/// by the gap between the two reads. (An already-past `valid_until` yields `ZERO` → fire on the next
/// check, the intended fail-safe; the next command re-arms it.)
pub fn monotonic_deadline(valid_until: DateTime<Utc>) -> Instant {
    let now_utc = Utc::now();
    let now_mono = Instant::now();
    let remaining = (valid_until - now_utc).to_std().unwrap_or(Duration::ZERO);
    now_mono + remaining
}

/// **item G** (switch exactly on the quarter-hour marks): a single-slot scheduler for a controller's
/// NEXT command — the pending decision that must be held, never applied early, and applied exactly
/// once the clock reaches its `apply_at`. Shared by every hardware controller (growatt, loxone) so the
/// "hold one pending command, promote it at its mark, drop it if it ages out unapplied" state machine
/// exists — and is tested — exactly once. Pure and clock-injected (every method takes `now` rather
/// than reading it itself), so it is fully unit-testable with synthetic instants — no real sleeping,
/// no fake `Instant`/tokio-time plumbing needed.
struct Scheduled<T> {
    apply_at: DateTime<Utc>,
    /// The command's own freshness bound (mirrors `ControlCommand::valid_until`): once `now` reaches
    /// this, the command is too old to apply at all, not just early — see [`PendingSlot::poll`].
    valid_until: DateTime<Utc>,
    command: T,
}

/// See the module-level doc above [`Scheduled`]. Holds at most one not-yet-applied command.
pub struct PendingSlot<T>(Option<Scheduled<T>>);

impl<T> Default for PendingSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PendingSlot<T> {
    pub fn new() -> Self {
        Self(None)
    }

    /// Record a newly-received next command, valid only in `[apply_at, valid_until)` (`apply_at`
    /// absent means "apply now", matching the protocol's `ControlCommand::apply_at` documentation).
    ///
    /// Returns `Some(command)` immediately when it is already due (`apply_at` absent, or `<= now`)
    /// AND still fresh (`now < valid_until`) — the caller applies it right away instead of waiting for
    /// a later tick to notice (covers a late-arriving plan: acceptance G1d/e). An already-expired
    /// command (`now >= valid_until` — e.g. a very late/stale delivery) is DROPPED outright: not
    /// applied, not stored — the same fail-safe direction as the protocol's own freshness check, never
    /// reviving a decision old enough that acting on it now would be wrong. Otherwise the command is
    /// stored as pending, replacing any earlier one (acceptance G1b: a replacement before the mark
    /// wins) — a single slot, not a queue.
    pub fn receive(
        &mut self,
        command: T,
        apply_at: Option<DateTime<Utc>>,
        valid_until: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<T> {
        if now >= valid_until {
            self.0 = None;
            return None;
        }
        match apply_at {
            Some(at) if at > now => {
                self.0 = Some(Scheduled {
                    apply_at: at,
                    valid_until,
                    command,
                });
                None
            }
            _ => {
                self.0 = None;
                Some(command)
            }
        }
    }

    /// Called on every clock tick (recommended: ≤1 s resolution, so a command is promoted within
    /// about a second of its mark). If a pending command's mark has arrived (`now >= apply_at`) AND it
    /// hasn't expired (`now < valid_until`), take and return it, clearing the slot (acceptance G1a: a
    /// command received well before the mark is applied AT the mark, not on receipt). An
    /// expired-before-being-noticed command (the controller was busy, or a long outage) is silently
    /// dropped rather than applied — a stale decision must never spring back to life once the deadman
    /// window it belonged to has passed. No pending command, or one not yet due: `None`, a no-op.
    pub fn poll(&mut self, now: DateTime<Utc>) -> Option<T> {
        match &self.0 {
            Some(s) if now >= s.valid_until => {
                self.0 = None;
                None
            }
            Some(s) if now >= s.apply_at => self.0.take().map(|s| s.command),
            _ => None,
        }
    }

    /// Discard any pending command without applying it — called when a controller's own deadman fires
    /// and it reverts to its failsafe, so a pending command scheduled further out than the CURRENT
    /// command's deadman window can't later spring the controller back out of that failsafe at its
    /// `apply_at` mark (a next command's `apply_at` is routinely minutes out, comfortably longer than
    /// a ~2-minute current-command deadman).
    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// Whether a command is currently held pending (not yet due). For controller-crate tests that
    /// assert on scheduling state without reaching into the private `Scheduled`.
    pub fn is_pending(&self) -> bool {
        self.0.is_some()
    }
}

/// A UDP datagram sink to a `host:port` target (the Loxone Miniserver virtual inputs). Binds an
/// ephemeral local port; the controller runtime only constructs one when armed.
pub struct UdpClient {
    socket: UdpSocket,
    target: SocketAddr,
}

impl UdpClient {
    /// Resolve the target ONCE, here. `send_to(&String)` re-ran `ToSocketAddrs` on every datagram —
    /// a cheap parse for the default IP literal, but `loxone.host` is a free-form config string, so
    /// pointing it at a hostname turned every send into a blocking `getaddrinfo`. `send` is called
    /// from `apply` and `heartbeat_refresh`, both driven straight off the `tokio::select!` loop, so
    /// a slow or unreachable resolver would stall the MQTT event loop, the command handler and the
    /// deadman tick — on the controller that gates the whole house. An unresolvable target is now a
    /// startup error instead of a per-datagram hazard.
    pub fn bind(target: String) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        let target = target
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("loxone target {target:?} resolved to no address"))?;
        Ok(Self { socket, target })
    }

    pub fn send(&self, datagram: &str) -> Result<()> {
        self.socket.send_to(datagram.as_bytes(), self.target)?;
        Ok(())
    }

    /// The resolved target, for logging.
    pub fn target(&self) -> SocketAddr {
        self.target
    }
}

// Shared `#[serde(default)]` infrastructure constants, single-sourced here so every controller's
// config agrees (each controller still defines its own `default_client_id`, which must be distinct).

/// Default MQTT broker host (the loxone broker, reached on localhost).
pub fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}

/// Default MQTT broker port.
pub fn default_mqtt_port() -> u16 {
    1883
}

/// Default Loxone Miniserver host for the UDP virtual-input controllers.
pub fn default_loxone_host() -> String {
    "192.168.0.200".to_string()
}

/// Default Loxone Miniserver UDP port.
pub fn default_loxone_port() -> u16 {
    4000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // A next command's usual validity window: apply_at + one 15-min block, mirroring how the
    // publisher sets `ControlCommand::valid_until` for the next command (item G).
    fn valid_until_of(apply_at: DateTime<Utc>) -> DateTime<Utc> {
        apply_at + chrono::Duration::minutes(15)
    }

    /// Acceptance G1a: a next command received well before the mark is stored, NOT applied on
    /// receipt, and is only promoted once a later tick's `now` reaches `apply_at` (within ~1 s, the
    /// recommended poll resolution — here exact, since the test supplies the instant directly).
    #[test]
    fn g1a_next_command_applies_at_the_mark_not_on_receipt() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let mark = utc("2026-09-22T12:15:00Z");
        let received_at = mark - chrono::Duration::seconds(40);

        let immediate = slot.receive("block1-on", Some(mark), valid_until_of(mark), received_at);
        assert_eq!(immediate, None, "must not apply on receipt");
        assert!(slot.is_pending());

        // Ticks before the mark: still nothing.
        assert_eq!(slot.poll(mark - chrono::Duration::seconds(1)), None);
        // At (or within 1s after) the mark: promoted.
        assert_eq!(slot.poll(mark), Some("block1-on"));
        assert!(!slot.is_pending());
        // A second poll after promotion finds nothing left to do.
        assert_eq!(slot.poll(mark + chrono::Duration::seconds(1)), None);
    }

    /// Acceptance G1b: a replacement next command received before the mark wins — the slot holds at
    /// most one pending command, and only the latest survives to be promoted.
    #[test]
    fn g1b_a_replacement_before_the_mark_wins() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let mark = utc("2026-09-22T12:15:00Z");
        let t0 = mark - chrono::Duration::seconds(40);
        let t1 = mark - chrono::Duration::seconds(10);

        assert_eq!(
            slot.receive("plan-a", Some(mark), valid_until_of(mark), t0),
            None
        );
        // A fresher poll replan supersedes it, same mark.
        assert_eq!(
            slot.receive("plan-b", Some(mark), valid_until_of(mark), t1),
            None
        );
        assert_eq!(slot.poll(mark), Some("plan-b"), "the replacement must win");
    }

    /// Acceptance G1c: with no next command ever received, ticking the clock is a pure no-op — the
    /// caller's existing "keep repeating the current value" behaviour is untouched.
    #[test]
    fn g1c_no_next_command_is_a_no_op() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        assert!(!slot.is_pending());
        assert_eq!(slot.poll(utc("2026-09-22T12:15:00Z")), None);
        assert_eq!(slot.poll(utc("2026-09-22T18:00:00Z")), None);
    }

    /// Acceptance G1d: an `apply_at` already in the past (a late plan) is applied immediately on
    /// receipt — the caller need not wait for the next tick.
    #[test]
    fn g1d_a_past_apply_at_is_applied_immediately() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let now = utc("2026-09-22T12:15:40Z");
        let apply_at = now - chrono::Duration::seconds(5); // the mark already passed
        let applied = slot.receive("late-block", Some(apply_at), valid_until_of(apply_at), now);
        assert_eq!(applied, Some("late-block"));
        assert!(!slot.is_pending(), "must not also sit pending");
    }

    /// Acceptance G1e: a command with `apply_at: None` (the protocol's "absent = apply now", e.g. an
    /// old producer or the current-command channel) is applied immediately, same as an already-due one.
    #[test]
    fn g1e_no_apply_at_means_apply_now() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let now = utc("2026-09-22T12:15:40Z");
        let applied = slot.receive("cmd", None, now + chrono::Duration::minutes(15), now);
        assert_eq!(applied, Some("cmd"));
    }

    /// Safety beyond the lettered criteria: a pending command that ages past its OWN `valid_until`
    /// without ever being promoted (a long outage, or the controller too busy to tick) is dropped, not
    /// applied — a stale decision must never spring back to life once its window has passed. Checked
    /// both at `receive` (arrives already expired) and at `poll` (was pending, then expired unnoticed).
    #[test]
    fn an_expired_pending_command_is_dropped_not_applied() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let apply_at = utc("2026-09-22T12:15:00Z");
        let valid_until = valid_until_of(apply_at); // 12:30:00Z

        // Arrives already past its own valid_until (a very late/duplicate delivery): dropped outright.
        let very_late = valid_until + chrono::Duration::seconds(1);
        assert_eq!(
            slot.receive("stale", Some(apply_at), valid_until, very_late),
            None
        );
        assert!(!slot.is_pending());

        // Was pending, then nobody polled until after valid_until: dropped on the next poll, not
        // promoted even though `now >= apply_at` also holds.
        slot.receive(
            "will-expire",
            Some(apply_at),
            valid_until,
            apply_at - chrono::Duration::seconds(30),
        );
        assert!(slot.is_pending());
        assert_eq!(slot.poll(valid_until + chrono::Duration::seconds(1)), None);
        assert!(!slot.is_pending());
    }

    /// `clear()` is what a controller's deadman-triggered failsafe revert calls: a pending command
    /// scheduled further out than the current command's (shorter) deadman window must not later
    /// re-arm the controller at its own `apply_at` mark.
    #[test]
    fn clear_discards_a_pending_command() {
        let mut slot: PendingSlot<&str> = PendingSlot::new();
        let mark = utc("2026-09-22T12:15:00Z");
        slot.receive(
            "would-rearm",
            Some(mark),
            valid_until_of(mark),
            mark - chrono::Duration::minutes(5),
        );
        assert!(slot.is_pending());
        slot.clear();
        assert!(!slot.is_pending());
        assert_eq!(slot.poll(mark), None, "cleared — nothing to promote");
    }
}
