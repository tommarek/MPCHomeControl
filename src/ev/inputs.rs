//! Turn each configured charger's live [`EvState`] into the optimizer's per-charger inputs.
//!
//! Controllable chargers (on our wallbox now) become [`EvSpec`]s the LP schedules; `monitored`
//! chargers contribute an expected exogenous load the plan reacts around; everything else (the car
//! away, or charging elsewhere) is observed but not scheduled. The deadline (a local time-of-day) is
//! projected onto the block grid, and the energy-to-target comes from the fused SoC.

use chrono::{DateTime, Duration, FixedOffset, TimeZone, Utc};

use crate::ev::prefs::EvPrefs;
use crate::ev::state::{fuse_charger, EvState, ON_CHARGER_KW};
use crate::optimize::config::{EvChargerConfig, EvControl, EvStrategy};
use crate::optimize::unified::EvSpec;
use crate::source::SourceClients;

/// The EV slice of the optimizer inputs for one planning cycle.
pub struct EvInputs {
    /// Controllable chargers the LP schedules.
    pub specs: Vec<EvSpec>,
    /// Expected exogenous load (kW) from monitored chargers, per block (empty ⇒ none).
    pub monitored_kw: Vec<f64>,
    /// The fused live state of every configured charger, for the API and the plan report.
    pub states: Vec<EvState>,
}

/// The block index by which a local `HH:MM` deadline next falls (clamped to `[0, n-1]`) **and** the
/// fraction `(0, 1]` of that block usable before the deadline. A deadline earlier in the day than
/// `start` rolls to tomorrow. `None` ⇒ the end of the horizon, fully usable.
fn deadline_block(
    hm: Option<(u32, u32)>,
    start: DateTime<Utc>,
    n: usize,
    block_seconds: f64,
    offset: FixedOffset,
) -> (usize, f64) {
    let Some((h, m)) = hm else {
        return (n.saturating_sub(1), 1.0);
    };
    let local = start.with_timezone(&offset);
    let mut target = local
        .naive_local()
        .date()
        .and_hms_opt(h, m, 0)
        .and_then(|nd| offset.from_local_datetime(&nd).single())
        .unwrap_or(local);
    if target <= local {
        target += Duration::days(1);
    }
    let secs = (target.with_timezone(&Utc) - start).num_seconds().max(0) as f64;
    // `ceil - 1` is the block *containing* the deadline: for a deadline strictly inside a block it's
    // that block; for one landing exactly on a boundary it's the *previous* block (the next block
    // starts at the deadline, so charging there would finish after it). This keeps the charge from
    // deferring past the deadline at a block boundary.
    let raw = (secs / block_seconds).ceil() as usize;
    let block = raw.saturating_sub(1).min(n.saturating_sub(1));
    // The deadline can fall partway through that block (a `HH:MM` deadline has minute granularity);
    // `frac` is the share of it before the deadline, so the rate cap there is scaled down. A deadline
    // past the horizon clamps to the last block, fully usable.
    let frac = if raw.saturating_sub(1) >= n {
        1.0
    } else {
        ((secs - block as f64 * block_seconds) / block_seconds).clamp(f64::EPSILON, 1.0)
    };
    (block, frac)
}

/// Fold an observed, **unschedulable** load (kW) into the per-block house-load forecast for a ~1 h
/// nowcast window. Shared by `monitored` chargers and untracked / no-SoC controllable chargers: the
/// future of an uncontrollable load is unknown, so assume the current rate persists near-term and let
/// the per-tick re-plan track changes. Sets `any` so the caller folds the vector into the load.
fn fold_nowcast_load(load: &mut [f64], any: &mut bool, power_kw: f64, block_seconds: f64) {
    let near = ((3600.0 / block_seconds).round() as usize).clamp(1, load.len());
    for slot in load.iter_mut().take(near) {
        *slot += power_kw;
        *any = true;
    }
}

/// Build the per-charger optimizer inputs from live fused state, applying the live dashboard
/// `prefs` (strategy / rate / target / deadline override config and the car's own limit).
pub async fn build_inputs(
    sources: &SourceClients,
    chargers: &[EvChargerConfig],
    start: DateTime<Utc>,
    n: usize,
    block_seconds: f64,
    offset: FixedOffset,
    prefs: &EvPrefs,
) -> EvInputs {
    // `block_seconds` is the planner's block width (the constant `BLOCK_SECONDS`). A 0/negative value
    // is a programming error: assert it loudly in debug, and clamp in release so the `3600 /
    // block_seconds` divisions below can't silently produce inf/NaN nowcast windows.
    debug_assert!(block_seconds > 0.0, "block_seconds must be positive");
    let block_seconds = block_seconds.max(1.0);
    let dt_h = block_seconds / 3600.0;
    let mut specs = Vec::new();
    let mut monitored = vec![0.0; n];
    let mut any_monitored = false;
    let mut states = Vec::new();

    for c in chargers {
        let pref = prefs.get(&c.name);
        let st = fuse_charger(sources, c, pref.and_then(|p| p.target_pct)).await;
        let strategy = pref.and_then(|p| p.strategy).unwrap_or(c.strategy);
        let max_kw = pref
            .and_then(|p| p.max_rate_kw)
            .map(|r| r.clamp(0.0, c.max_kw))
            .unwrap_or_else(|| c.effective_max_kw());
        let hm = pref
            .and_then(|p| p.deadline_hm())
            .or_else(|| c.deadline_hm());

        match c.control {
            // Monitored: not scheduled. Its future is unknown, so fold the current measured draw as an
            // exogenous load (an idle charger reads 0) — like the untracked-car path below.
            EvControl::Monitored => {
                if st.on_our_charger && st.charger_power_kw > ON_CHARGER_KW {
                    fold_nowcast_load(
                        &mut monitored,
                        &mut any_monitored,
                        st.charger_power_kw,
                        block_seconds,
                    );
                }
            }
            // Modulating / on-off: schedule it only while it's controllable on our wallbox.
            _ => {
                let target_energy = st.energy_needed_kwh.unwrap_or(0.0).max(0.0);
                if st.controllable_now && max_kw > 0.0 && target_energy > 0.0 {
                    // `charge_now` collapses the deadline to the earliest block the target fits in at
                    // full power (whole blocks ⇒ `frac` 1.0); the others use the time-of-day deadline,
                    // which can land partway through its block.
                    let (deadline, deadline_frac) = if strategy == EvStrategy::ChargeNow {
                        let per_block = (max_kw * c.efficiency * dt_h).max(1e-9);
                        let b = ((target_energy / per_block).ceil() as usize)
                            .saturating_sub(1)
                            .min(n.saturating_sub(1));
                        (b, 1.0)
                    } else {
                        deadline_block(hm, start, n, block_seconds, offset)
                    };
                    let plugged: Vec<bool> = (0..n).map(|i| i <= deadline).collect();
                    specs.push(EvSpec {
                        name: c.name.clone(),
                        on_off: c.control == EvControl::OnOff,
                        strategy,
                        max_kw,
                        efficiency: c.efficiency,
                        allow_battery_to_ev: c.allow_battery_to_ev,
                        plugged,
                        target_energy_kwh: target_energy,
                        deadline_block: deadline,
                        deadline_frac,
                    });
                } else if st.on_our_charger && st.charger_power_kw > ON_CHARGER_KW {
                    // Connected and drawing, but no SoC → can't optimize the charge (an untracked /
                    // unknown car, or a stale SoC feed). Fold the *measured* draw as an exogenous load so
                    // the plan accounts for it — protecting the home battery from being scheduled to
                    // discharge into a charge it can't see.
                    fold_nowcast_load(
                        &mut monitored,
                        &mut any_monitored,
                        st.charger_power_kw,
                        block_seconds,
                    );
                } else if st.controllable_now && st.energy_needed_kwh.is_none() {
                    // Controllable on our wallbox but no car SoC — there's no target to schedule toward,
                    // and (not drawing power) nothing to fold as load, so it silently leaves the plan.
                    // Surface it: a missing SoC is usually a stale/unparseable feed the operator should
                    // see (an already-at-target charger, by contrast, is benign and shows up in `states`).
                    eprintln!(
                        "[ev] charger {:?} controllable but unscheduled: car SoC unavailable",
                        c.name
                    );
                }
            }
        }
        states.push(st);
    }

    EvInputs {
        specs,
        monitored_kw: if any_monitored { monitored } else { Vec::new() },
        states,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A `start` at `h:m:00` on 2024-01-01 UTC (the live planner always aligns to a block boundary).
    fn start_at(h: i64, m: i64) -> DateTime<Utc> {
        // 1_704_067_200 = 2024-01-01T00:00:00Z.
        Utc.timestamp_opt(1_704_067_200 + h * 3600 + m * 60, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn deadline_block_none_is_horizon_end_fully_usable() {
        let utc = FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            deadline_block(None, start_at(6, 45), 96, 900.0, utc),
            (95, 1.0)
        );
    }

    #[test]
    fn deadline_on_block_boundary_is_fully_usable() {
        // start 06:45, deadline 07:00 → exactly one block away; the containing block (0) is full.
        let utc = FixedOffset::east_opt(0).unwrap();
        let (block, frac) = deadline_block(Some((7, 0)), start_at(6, 45), 96, 900.0, utc);
        assert_eq!(block, 0);
        assert!((frac - 1.0).abs() < 1e-9, "boundary deadline frac = {frac}");
    }

    #[test]
    fn deadline_mid_block_scales_the_final_block() {
        // start 06:45, deadline 07:07 → 1320 s in: block 1 ([07:00,07:15)), 420/900 of it usable.
        let utc = FixedOffset::east_opt(0).unwrap();
        let (block, frac) = deadline_block(Some((7, 7)), start_at(6, 45), 96, 900.0, utc);
        assert_eq!(block, 1);
        assert!(
            (frac - 420.0 / 900.0).abs() < 1e-9,
            "mid-block deadline frac = {frac}"
        );
    }

    #[test]
    fn deadline_past_horizon_clamps_to_last_block_fully_usable() {
        // A deadline far beyond a 1-block horizon clamps to block 0, fully usable.
        let utc = FixedOffset::east_opt(0).unwrap();
        let (block, frac) = deadline_block(Some((7, 7)), start_at(6, 45), 1, 900.0, utc);
        assert_eq!(block, 0);
        assert!((frac - 1.0).abs() < 1e-9, "clamped deadline frac = {frac}");
    }
}
