//! Exclusive heating / cooling **mode per room**, decided from the house's own thermal tendency.
//!
//! A room that is losing heat coasts down for free, so it never needs cooling; a room gaining heat
//! never needs floor heating. Each plan, before the LP is built, every comfort room gets one
//! [`RoomMode`]: the LP then keeps only that side's band and forces the other side's actuators to 0
//! (the LP stays pure — the mode is data). The selection is pure and IO-free:
//!
//! - **Tendency** = the sign of the *free drift*: the room's predicted air temperature with no
//!   heating and no cooling (the very free response the LP uses) at +24 h (or the horizon end if
//!   shorter) minus its temperature now, expressed in K per day.
//! - **Safety rule**: a room below its in-force heat target is always `Heating`.
//! - **Hysteresis**: otherwise a room changes mode only after the opposite tendency has been seen
//!   continuously for `comfort.mode_switch_hours` (default 24 h). A gap of more than
//!   [`MAX_OBSERVATION_GAP_HOURS`] without an observation (downtime, failed or degraded plans, which
//!   the caller does not commit) restarts the run.
//!
//! The per-room state ([`RoomModeState`]) is persisted by the caller (`AppState::room_modes`) so a
//! restart does not flip modes.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::grid::BlockGrid;

/// The longest gap between two observations of the opposite tendency that still counts as one
/// continuous run. Sized for the slowest legitimate cadence: an hourly `mpc_tick_minutes`, and the
/// comfort backtest's `--step-hours` (at most `horizon.fine_hours`, 6 in the live config).
pub const MAX_OBSERVATION_GAP_HOURS: i64 = 6;

/// Which side of the comfort band a room is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomMode {
    /// Floor heating / HVAC air-heating active; the cooling side is forced off.
    #[default]
    Heating,
    /// HVAC cooling active; floor heating and air-heating are forced off.
    Cooling,
}

impl RoomMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RoomMode::Heating => "heating",
            RoomMode::Cooling => "cooling",
        }
    }
}

/// One room's persisted mode state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoomModeState {
    pub mode: RoomMode,
    /// When the room entered the current mode.
    pub since: DateTime<Utc>,
    /// When the opposite tendency was first seen in an unbroken run (`None` ⇒ the tendency agrees
    /// with the mode).
    #[serde(default)]
    pub opposite_since: Option<DateTime<Utc>>,
    /// When the opposite tendency was last seen in that run. Absent from stores written before it
    /// existed; the run then counts as last seen at `opposite_since`.
    #[serde(default)]
    pub opposite_seen_at: Option<DateTime<Utc>>,
    /// The free-drift slope behind the latest decision (K per day, + = gaining heat).
    #[serde(default)]
    pub drift_k_per_day: f64,
}

/// What the selection looks at for one room.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoomObservation {
    /// Current (estimated) air temperature, °C.
    pub temp_c: f64,
    /// The heat target in force now, °C.
    pub heat_target_c: f64,
    /// Free-drift slope, K per day (+ = gaining).
    pub drift_k_per_day: f64,
}

/// The free-drift slope (K per day, + = gaining heat) from a zone's free response.
///
/// `free_response_k[b]` is the air temperature (K) at the END of grid block `b` with every actuator
/// off; `now_k` the current temperature. The look-ahead is the last block ending within 24 h of the
/// grid start (the first block if even that is longer, the last block if the horizon is shorter).
pub fn drift_k_per_day(grid: &BlockGrid, free_response_k: &[f64], now_k: f64) -> f64 {
    let n = free_response_k.len().min(grid.len());
    if n == 0 {
        return 0.0;
    }
    let start = grid.block_start(0);
    let day = Duration::hours(24);
    let b = (0..n)
        .rev()
        .find(|&b| grid.block_end(b) - start <= day)
        .unwrap_or(0);
    let hours = (grid.block_end(b) - start).num_seconds() as f64 / 3600.0;
    if hours <= 0.0 {
        return 0.0;
    }
    (free_response_k[b] - now_k) / hours * 24.0
}

/// Choose a room's mode and next state. Pure.
///
/// `prev` is the persisted state (`None` on first sight: start in heating and let a
/// gaining room earn cooling through the hysteresis), `now` the decision instant, `switch_hours` the hysteresis (`comfort.mode_switch_hours`).
pub fn select_mode(
    prev: Option<&RoomModeState>,
    obs: RoomObservation,
    now: DateTime<Utc>,
    switch_hours: f64,
) -> RoomModeState {
    let tendency = if obs.drift_k_per_day > 0.0 {
        RoomMode::Cooling
    } else {
        RoomMode::Heating
    };
    let mut next = match prev {
        // No history (first start): begin in heating — the conservative side for an armed heating
        // system — and let a gaining room earn cooling through the usual hysteresis.
        None => RoomModeState {
            mode: RoomMode::Heating,
            since: now,
            opposite_since: (tendency == RoomMode::Cooling).then_some(now),
            opposite_seen_at: (tendency == RoomMode::Cooling).then_some(now),
            drift_k_per_day: obs.drift_k_per_day,
        },
        Some(p) if p.mode == tendency => RoomModeState {
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: obs.drift_k_per_day,
            ..*p
        },
        Some(p) => {
            let last_seen = p.opposite_seen_at.or(p.opposite_since);
            let opposite_since = match (p.opposite_since, last_seen) {
                (Some(since), Some(seen))
                    if now - seen <= Duration::hours(MAX_OBSERVATION_GAP_HOURS) =>
                {
                    since
                }
                _ => now,
            };
            let held_s = (now - opposite_since).num_seconds() as f64;
            if held_s >= switch_hours * 3600.0 {
                RoomModeState {
                    mode: tendency,
                    since: now,
                    opposite_since: None,
                    opposite_seen_at: None,
                    drift_k_per_day: obs.drift_k_per_day,
                }
            } else {
                RoomModeState {
                    opposite_since: Some(opposite_since),
                    opposite_seen_at: Some(now),
                    drift_k_per_day: obs.drift_k_per_day,
                    ..*p
                }
            }
        }
    };
    // Safety rule: a room below its heat target is never left unheated.
    if next.mode == RoomMode::Cooling && obs.temp_c < obs.heat_target_c {
        next = RoomModeState {
            mode: RoomMode::Heating,
            since: now,
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: obs.drift_k_per_day,
        };
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap() + Duration::hours(h)
    }

    fn obs(drift: f64, temp: f64) -> RoomObservation {
        RoomObservation {
            temp_c: temp,
            heat_target_c: 21.5,
            drift_k_per_day: drift,
        }
    }

    fn heating_since(h: i64) -> RoomModeState {
        RoomModeState {
            mode: RoomMode::Heating,
            since: t(h),
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: -1.0,
        }
    }

    /// Observe `o` hourly over `hours`, as an unbroken run of plans does.
    fn observe(
        mut s: RoomModeState,
        o: RoomObservation,
        hours: std::ops::RangeInclusive<i64>,
    ) -> RoomModeState {
        for h in hours {
            s = select_mode(Some(&s), o, t(h), 24.0);
        }
        s
    }

    #[test]
    fn first_sight_starts_heating_and_a_gaining_room_earns_cooling() {
        assert_eq!(
            select_mode(None, obs(-2.0, 22.0), t(0), 24.0).mode,
            RoomMode::Heating
        );
        let s = select_mode(None, obs(1.5, 24.0), t(0), 24.0);
        assert_eq!(s.mode, RoomMode::Heating);
        assert_eq!(s.opposite_since, Some(t(0)));
        assert_eq!(observe(s, obs(1.5, 24.0), 1..=24).mode, RoomMode::Cooling);
        // Safety rule applies at start-up too: gaining but below the heat target.
        assert_eq!(
            select_mode(None, obs(1.5, 20.8), t(0), 24.0).mode,
            RoomMode::Heating
        );
    }

    #[test]
    fn losing_keeps_or_returns_to_heating() {
        let mut s = heating_since(0);
        s = select_mode(Some(&s), obs(-3.0, 30.0), t(1), 24.0);
        assert_eq!(
            s.mode,
            RoomMode::Heating,
            "even at 30 C a losing room heats"
        );
    }

    #[test]
    fn gaining_for_a_full_day_switches_to_cooling_but_not_before() {
        let mut s = heating_since(0);
        s = select_mode(Some(&s), obs(2.0, 24.0), t(1), 24.0);
        assert_eq!(s.mode, RoomMode::Heating);
        assert_eq!(s.opposite_since, Some(t(1)));
        s = observe(s, obs(2.0, 24.0), 2..=24);
        assert_eq!(s.mode, RoomMode::Heating, "23 h of gaining is not enough");
        s = select_mode(Some(&s), obs(2.0, 24.0), t(25), 24.0);
        assert_eq!(s.mode, RoomMode::Cooling, "24 h observed continuously");
        assert_eq!(s.since, t(25));
        assert_eq!(s.opposite_since, None);
    }

    #[test]
    fn an_interrupted_run_restarts_the_clock() {
        let mut s = heating_since(0);
        s = select_mode(Some(&s), obs(2.0, 24.0), t(1), 24.0);
        s = select_mode(Some(&s), obs(-0.5, 24.0), t(12), 24.0);
        assert_eq!(s.opposite_since, None, "a losing reading resets the timer");
        s = observe(s, obs(2.0, 24.0), 13..=30);
        assert_eq!(s.mode, RoomMode::Heating, "only 17 h since the restart");
    }

    #[test]
    fn cooling_returns_to_heating_only_after_a_day_of_losing() {
        let mut s = RoomModeState {
            mode: RoomMode::Cooling,
            since: t(0),
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: 1.0,
        };
        s = select_mode(Some(&s), obs(-1.0, 25.0), t(2), 24.0);
        assert_eq!(s.mode, RoomMode::Cooling);
        s = observe(s, obs(-1.0, 25.0), 3..=25);
        assert_eq!(s.mode, RoomMode::Cooling);
        s = select_mode(Some(&s), obs(-1.0, 25.0), t(26), 24.0);
        assert_eq!(s.mode, RoomMode::Heating);
    }

    #[test]
    fn a_gap_in_observation_restarts_the_run() {
        let mut s = select_mode(Some(&heating_since(0)), obs(2.0, 24.0), t(1), 24.0);
        // An outage (or a stretch of degraded plans) longer than the bound sees nothing.
        s = select_mode(Some(&s), obs(2.0, 24.0), t(31), 24.0);
        assert_eq!(
            s.mode,
            RoomMode::Heating,
            "one reading either side is no day"
        );
        assert_eq!(s.opposite_since, Some(t(31)));
        // A gap within the bound still counts as continuous.
        s = observe(s, obs(2.0, 24.0), 32..=48);
        s = select_mode(
            Some(&s),
            obs(2.0, 24.0),
            t(48 + MAX_OBSERVATION_GAP_HOURS),
            24.0,
        );
        assert_eq!(s.mode, RoomMode::Heating);
        assert_eq!(s.opposite_since, Some(t(31)));
        s = observe(s, obs(2.0, 24.0), 49 + MAX_OBSERVATION_GAP_HOURS..=55);
        assert_eq!(s.mode, RoomMode::Cooling);
    }

    #[test]
    fn a_store_without_the_last_seen_field_loads_and_ages_from_the_run_start() {
        let json = r#"{"mode":"heating","since":"2026-07-01T00:00:00Z",
            "opposite_since":"2026-07-01T01:00:00Z","drift_k_per_day":2.0}"#;
        let old: RoomModeState = serde_json::from_str(json).unwrap();
        assert_eq!(old.opposite_seen_at, None);
        assert_eq!(old.opposite_since, Some(t(1)));
        let kept = select_mode(Some(&old), obs(2.0, 24.0), t(2), 24.0);
        assert_eq!(kept.opposite_since, Some(t(1)));
        assert_eq!(kept.opposite_seen_at, Some(t(2)));
        let restarted = select_mode(Some(&old), obs(2.0, 24.0), t(30), 24.0);
        assert_eq!(restarted.mode, RoomMode::Heating);
        assert_eq!(restarted.opposite_since, Some(t(30)));
    }

    #[test]
    fn below_the_heat_target_is_always_heating_at_once() {
        let s = RoomModeState {
            mode: RoomMode::Cooling,
            since: t(0),
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: 1.0,
        };
        let n = select_mode(Some(&s), obs(2.0, 20.8), t(5), 24.0);
        assert_eq!(n.mode, RoomMode::Heating);
        assert_eq!(n.since, t(5));
        // And it needs a fresh full day of gaining above target to go back.
        let n = select_mode(Some(&n), obs(2.0, 22.0), t(6), 24.0);
        assert_eq!(n.mode, RoomMode::Heating);
        assert_eq!(n.opposite_since, Some(t(6)));
    }

    #[test]
    fn state_survives_a_simulated_restart() {
        let s0 = RoomModeState {
            mode: RoomMode::Cooling,
            since: t(0),
            opposite_since: None,
            opposite_seen_at: None,
            drift_k_per_day: 2.0,
        };
        let json = serde_json::to_string(&s0).unwrap();
        let restored: RoomModeState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, s0);
        // A cooling room with a pending (short) losing run keeps cooling across the restart.
        let s1 = select_mode(Some(&restored), obs(-1.0, 25.0), t(3), 24.0);
        let restored: RoomModeState =
            serde_json::from_str(&serde_json::to_string(&s1).unwrap()).unwrap();
        let s2 = select_mode(Some(&restored), obs(-1.0, 25.0), t(5), 24.0);
        assert_eq!(s2.mode, RoomMode::Cooling);
        assert_eq!(s2.opposite_since, Some(t(3)));
    }

    #[test]
    fn drift_uses_the_last_block_within_a_day() {
        let grid = BlockGrid::uniform(t(0), 36, 3600.0);
        // +0.1 K per hour, 24 h look-ahead ⇒ +2.4 K/day.
        let fr: Vec<f64> = (1..=36).map(|h| 293.15 + 0.1 * f64::from(h)).collect();
        let d = drift_k_per_day(&grid, &fr, 293.15);
        assert!((d - 2.4).abs() < 1e-9, "{d}");
        // Short horizon: 6 h of -0.5 K/h ⇒ -12 K/day (scaled to a day).
        let grid = BlockGrid::uniform(t(0), 6, 3600.0);
        let fr: Vec<f64> = (1..=6).map(|h| 293.15 - 0.5 * f64::from(h)).collect();
        let d = drift_k_per_day(&grid, &fr, 293.15);
        assert!((d + 12.0).abs() < 1e-9, "{d}");
    }
}
