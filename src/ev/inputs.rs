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
use crate::optimize::grid::BlockGrid;
use crate::optimize::unified::EvSpec;
use crate::source::SourceClients;

/// The EV slice of the optimizer inputs for one planning cycle.
pub struct EvInputs {
    /// Controllable chargers the LP schedules.
    pub specs: Vec<EvSpec>,
    /// Expected exogenous load (kW) from monitored chargers, per FINE step (empty ⇒ none) — like
    /// `ForecastContext::temperature_c`, aggregated onto the plan's block grid by the caller.
    pub monitored_kw: Vec<f64>,
    /// The fused live state of every configured charger, for the API and the plan report.
    pub states: Vec<EvState>,
}

/// Safety clamp on a LEARNED departure time: q20 is already conservative, but a SQL/timezone bug
/// must never yield a 02:00 (panic-charge overnight) or a 14:00 (car long gone) deadline.
const LEARNED_DEADLINE_MIN: (u32, u32) = (5, 0);
const LEARNED_DEADLINE_MAX: (u32, u32) = (10, 0);

/// Which local calendar day a `candidate` deadline lands on, given "today at candidate is still
/// ahead, else tomorrow" — the same day-roll rule `deadline_block` applies. Used to pick the
/// weekday-vs-weekend learned quantile. Pure for testability.
fn deadline_day(now_local: chrono::NaiveDateTime, candidate: (u32, u32)) -> chrono::Weekday {
    use chrono::Datelike;
    let today_at = now_local
        .date()
        .and_hms_opt(candidate.0, candidate.1, 0)
        .unwrap_or(now_local);
    let date = if today_at > now_local {
        now_local.date()
    } else {
        now_local.date() + Duration::days(1)
    };
    date.weekday()
}

/// The TeslaMate-learned departure deadline: a conservative quantile of the car's real first
/// departures (weekday vs weekend split), read through the charger's `departure_weekday` /
/// `departure_weekend` Postgres locators (minutes-of-local-day as float). `None` on any failure
/// — the config deadline is a complete answer, so this degrades silently (one log line).
async fn learned_deadline_hm(
    sources: &SourceClients,
    c: &EvChargerConfig,
    start: DateTime<Utc>,
    offset: FixedOffset,
) -> Option<(u32, u32)> {
    // Day-type probe: which day would the deadline land on if it were the CONFIG time? (A single
    // iteration — a learned Friday time that rolls onto Saturday picks the weekday quantile once;
    // bounded error, documented trade.)
    let probe = c.deadline_hm().unwrap_or((7, 0));
    let day = deadline_day(start.with_timezone(&offset).naive_local(), probe);
    let role = if matches!(day, chrono::Weekday::Sat | chrono::Weekday::Sun) {
        "departure_weekend"
    } else {
        "departure_weekday"
    };
    let loc = c.sources.get(role)?;
    let minutes = sources.read_locator(loc, 24 * 60).await?;
    if !minutes.is_finite() {
        return None;
    }
    let m = minutes.round().clamp(0.0, 24.0 * 60.0 - 1.0) as u32;
    let hm = (m / 60, m % 60);
    let clamped = hm.max(LEARNED_DEADLINE_MIN).min(LEARNED_DEADLINE_MAX);
    if clamped != hm {
        eprintln!(
            "[ev] charger {:?}: learned {role} departure {:02}:{:02} clamped to {:02}:{:02}",
            c.name, hm.0, hm.1, clamped.0, clamped.1
        );
    }
    Some(clamped)
}

/// The absolute instant a `HH:MM` **site-local** deadline resolves to: today if still ahead, else
/// tomorrow. Shared by `deadline_block` and the `deadline_at` the API reports, so the scheduling
/// maths and what the dashboard draws can never disagree about which instant the deadline is.
pub(crate) fn deadline_instant(
    hm: (u32, u32),
    start: DateTime<Utc>,
    offset: FixedOffset,
) -> DateTime<Utc> {
    let (h, m) = hm;
    let local = start.with_timezone(&offset);
    let mut target = local
        .naive_local()
        .date()
        .and_hms_opt(h, m, 0)
        .and_then(|nd| offset.from_local_datetime(&nd).single())
        .unwrap_or(local);
    if target <= local {
        // +1 day inside a FIXED offset is the same wall-clock time next day *in that frame*, which
        // is what we want. KNOWN LIMITATION: `offset` is resolved once, at `start`, so if a DST
        // changeover falls between now and the deadline the result is an hour out — bounded, and
        // only on the two changeover nights a year. Fixing it properly means threading the IANA
        // `site.timezone` (see `SiteConfig::tz`) down here instead of a pre-resolved offset, so
        // both this and `deadline_block` re-resolve at the target date.
        target += Duration::days(1);
    }
    target.with_timezone(&Utc)
}

/// The GRID BLOCK by which a local `HH:MM` deadline next falls, and the fraction `(0, 1]` of that
/// block usable before the deadline — generalizes the old uniform-grid `ceil(secs/block_seconds)-1`
/// to a variable-rate grid by scanning block ends directly. A deadline landing exactly on a block
/// boundary belongs to the block ENDING there (fully usable), not the one starting there: since the
/// grid's blocks are contiguous, that block's own end equals the next block's start, so the `<=`
/// scan below matches the earlier block first and never reaches the later one.
fn deadline_block(
    hm: Option<(u32, u32)>,
    start: DateTime<Utc>,
    grid: &BlockGrid,
    offset: FixedOffset,
) -> (usize, f64) {
    let n = grid.len();
    let Some(hm) = hm else {
        return (n.saturating_sub(1), 1.0);
    };
    let deadline = deadline_instant(hm, start, offset);
    for i in 0..n {
        let block_end = grid.block_end(i);
        if deadline <= block_end {
            let block_start = grid.block_start(i);
            let block_seconds = grid.dt_hours(i) * 3600.0;
            let frac = ((deadline - block_start).num_seconds() as f64 / block_seconds)
                .clamp(f64::EPSILON, 1.0);
            return (i, frac);
        }
    }
    // Past the horizon: clamp to the last block, fully usable.
    (n.saturating_sub(1), 1.0)
}

/// Fold an observed, **unschedulable** load (kW) into the per-block house-load forecast for a ~1 h
/// nowcast window. Shared by `monitored` chargers and untracked / no-SoC controllable chargers: the
/// future of an uncontrollable load is unknown, so assume the current rate persists near-term and let
/// the per-tick re-plan track changes. Sets `any` so the caller folds the vector into the load.
/// `monitored_kw` is on the FINE lattice (like `ForecastContext::temperature_c`) — the caller
/// aggregates it onto the block grid alongside the rest of the load forecast (`grid.mean`), so the
/// near-term window here is fine (15-min) steps, not grid blocks.
fn fold_nowcast_load(load: &mut [f64], any: &mut bool, power_kw: f64, fine_seconds: f64) {
    let near = ((3600.0 / fine_seconds).round() as usize).clamp(1, load.len());
    for slot in load.iter_mut().take(near) {
        *slot += power_kw;
        *any = true;
    }
}

/// Build the per-charger optimizer inputs from live fused state, applying the live dashboard
/// `prefs` (strategy / rate / target / deadline override config and the car's own limit).
///
/// `specs`' `deadline_block`/`plugged` are indexed by GRID BLOCK (`grid.len()`, what
/// `optimize_unified` wants directly); `monitored_kw` is indexed by FINE step (`grid.n_fine()`,
/// like the rest of the exogenous load forecast) — the caller aggregates it onto the grid.
pub async fn build_inputs(
    sources: &SourceClients,
    chargers: &[EvChargerConfig],
    start: DateTime<Utc>,
    grid: &BlockGrid,
    offset: FixedOffset,
    prefs: &EvPrefs,
) -> EvInputs {
    let n = grid.len();
    let n_fine = grid.n_fine();
    let mut specs = Vec::new();
    let mut monitored = vec![0.0; n_fine];
    let mut any_monitored = false;
    let mut states = Vec::new();

    for c in chargers {
        let pref = prefs.get(&c.name);
        let mut st = fuse_charger(sources, c, pref.and_then(|p| p.target_pct)).await;
        let strategy = pref.and_then(|p| p.strategy).unwrap_or(c.strategy);
        // The LP's overhead credit is `overhead_kw · dt / cap`, so a cap at or below
        // `overhead_kw / efficiency` makes each charged kWh cost MORE overhead than it delivers:
        // the delivery expressions turn negative in the charge legs and the solver is free to run
        // the charger flat out while "delivering" less than nothing. `validate` enforces
        // `overhead_kw < efficiency · max_kw` against the RATED cap, but the effective cap here can
        // be far smaller — a `max_rate_kw` preference is validated only as finite and ≥ 0. Floor it
        // so the invariant the LP relies on holds for the cap the LP actually receives; a rate this
        // low is below any real wallbox minimum anyway.
        let floor_kw = if c.efficiency > 0.0 {
            c.overhead_kw / c.efficiency * 1.01
        } else {
            0.0
        };
        let max_kw = pref
            .and_then(|p| p.max_rate_kw)
            .map(|r| r.clamp(0.0, c.max_kw))
            .unwrap_or_else(|| c.effective_max_kw());
        let max_kw = if max_kw > 0.0 {
            max_kw.max(floor_kw.min(c.max_kw))
        } else {
            max_kw
        };
        // Deadline precedence: an explicit dashboard preference > the TeslaMate-learned departure
        // quantile (when enabled) > the config constant. The learned value is read fresh per plan
        // tick (one bounded SELECT) and falls back to config SILENTLY on any failure — a missing
        // learned deadline is not degraded data, the config is a full answer.
        let learned = if c.learned_deadline && pref.and_then(|p| p.deadline_hm()).is_none() {
            learned_deadline_hm(sources, c, start, offset).await
        } else {
            None
        };
        let deadline_source = if pref.and_then(|p| p.deadline_hm()).is_some() {
            "pref"
        } else if learned.is_some() {
            "learned"
        } else {
            "config"
        };
        let hm = pref
            .and_then(|p| p.deadline_hm())
            .or(learned)
            .or_else(|| c.deadline_hm());
        st.deadline_source = Some(deadline_source.to_string());
        st.deadline_hm = hm.map(|(h, m)| format!("{h:02}:{m:02}"));
        // The same instant the scheduler uses, so a dashboard in ANY timezone marks the deadline
        // where the plan actually places it (resolving `HH:MM` browser-side put it hours off for a
        // viewer away from the site).
        st.deadline_at = hm.map(|hm| deadline_instant(hm, start, offset));

        match c.control {
            // Monitored: not scheduled. Its future is unknown, so fold the current measured draw as an
            // exogenous load (an idle charger reads 0) — like the untracked-car path below.
            EvControl::Monitored => {
                if st.on_our_charger && st.charger_power_kw > ON_CHARGER_KW {
                    fold_nowcast_load(
                        &mut monitored,
                        &mut any_monitored,
                        st.charger_power_kw,
                        grid.fine_seconds,
                    );
                }
            }
            // Modulating / on-off: schedule it only while it's controllable on our wallbox.
            _ => {
                let target_energy = st.energy_needed_kwh.unwrap_or(0.0).max(0.0);
                // A charger with zero remaining target can still absorb otherwise-wasted energy
                // (curtailed PV / negative-price blocks) up to the car's own limit — build the
                // spec whenever either is schedulable.
                if st.controllable_now
                    && max_kw > 0.0
                    && (target_energy > 0.0 || st.bonus_energy_kwh > 0.0)
                {
                    // `charge_now` collapses the deadline to the earliest block the target fits in at
                    // full power (whole blocks ⇒ `frac` 1.0); the others use the time-of-day deadline,
                    // which can land partway through its block.
                    let (deadline, deadline_frac) = if strategy == EvStrategy::ChargeNow {
                        // Size the window from target AND bonus: a car already at target but with
                        // headroom to its own limit exists purely to absorb curtailed-PV /
                        // negative-price energy, and sizing from target alone gave it a ONE-block
                        // plug window — the bonus was schedulable in name only.
                        let schedulable = target_energy + st.bonus_energy_kwh.max(0.0);
                        // Accumulate each block's own deliverable energy (generalizes the old
                        // uniform-grid `ceil(schedulable/per_block)-1`) until it covers what's
                        // schedulable; that block is the deadline (whole blocks ⇒ `frac` 1.0).
                        let mut acc = 0.0;
                        let mut b = 0usize;
                        for i in 0..n {
                            acc += (max_kw * c.efficiency * grid.dt_hours(i)).max(1e-9);
                            b = i;
                            if acc >= schedulable {
                                break;
                            }
                        }
                        (b, 1.0)
                    } else {
                        deadline_block(hm, start, grid, offset)
                    };
                    let plugged: Vec<bool> = (0..n).map(|i| i <= deadline).collect();
                    specs.push(EvSpec {
                        name: c.name.clone(),
                        on_off: c.control == EvControl::OnOff,
                        strategy,
                        max_kw,
                        // A rate override below the hardware floor means "as slow as possible".
                        min_kw: c.min_kw.min(max_kw),
                        overhead_kw: c.overhead_kw,
                        efficiency: c.efficiency,
                        allow_battery_to_ev: c.allow_battery_to_ev,
                        plugged,
                        target_energy_kwh: target_energy,
                        bonus_energy_kwh: st.bonus_energy_kwh,
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
                        grid.fine_seconds,
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
        let grid = BlockGrid::uniform(start_at(6, 45), 96, 900.0);
        assert_eq!(deadline_block(None, start_at(6, 45), &grid, utc), (95, 1.0));
    }

    #[test]
    fn deadline_on_block_boundary_is_fully_usable() {
        // start 06:45, deadline 07:00 → exactly one block away; the containing block (0) is full.
        let utc = FixedOffset::east_opt(0).unwrap();
        let grid = BlockGrid::uniform(start_at(6, 45), 96, 900.0);
        let (block, frac) = deadline_block(Some((7, 0)), start_at(6, 45), &grid, utc);
        assert_eq!(block, 0);
        assert!((frac - 1.0).abs() < 1e-9, "boundary deadline frac = {frac}");
    }

    #[test]
    fn deadline_mid_block_scales_the_final_block() {
        // start 06:45, deadline 07:07 → 1320 s in: block 1 ([07:00,07:15)), 420/900 of it usable.
        let utc = FixedOffset::east_opt(0).unwrap();
        let grid = BlockGrid::uniform(start_at(6, 45), 96, 900.0);
        let (block, frac) = deadline_block(Some((7, 7)), start_at(6, 45), &grid, utc);
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
        let grid = BlockGrid::uniform(start_at(6, 45), 1, 900.0);
        let (block, frac) = deadline_block(Some((7, 7)), start_at(6, 45), &grid, utc);
        assert_eq!(block, 0);
        assert!((frac - 1.0).abs() < 1e-9, "clamped deadline frac = {frac}");
    }

    /// The multi-rate generalization: a deadline landing inside an HOURLY block (not a fine one)
    /// gets that block's own (1h) duration for `frac`, not the fine 900s used before.
    #[test]
    fn deadline_inside_an_hourly_block_scales_by_its_own_duration() {
        let utc = FixedOffset::east_opt(0).unwrap();
        let start = start_at(0, 0);
        // 1h fine (4 fine blocks, 0..=3) + hourly blocks from there: block 4 is [01:00, 02:00).
        let grid = BlockGrid::multi_rate(start, 2, 1, 900.0);
        assert_eq!(grid.dt_hours(4), 1.0, "block 4 must be hourly");
        // Deadline 01:40 lands 40 minutes into block 4 ([01:00, 02:00)) → frac = 40/60.
        let (block, frac) = deadline_block(Some((1, 40)), start, &grid, utc);
        assert_eq!(block, 4);
        assert!(
            (frac - 40.0 / 60.0).abs() < 1e-9,
            "hourly-block frac = {frac}"
        );
    }
    #[test]
    fn deadline_day_rolls_correctly_across_midnight_and_weekends() {
        use chrono::{NaiveDate, Weekday};
        let at = |y, mo, d, h, mi| {
            NaiveDate::from_ymd_opt(y, mo, d)
                .unwrap()
                .and_hms_opt(h, mi, 0)
                .unwrap()
        };
        // Friday 06:00, deadline 07:00 → still today (Friday) → weekday quantile.
        assert_eq!(deadline_day(at(2026, 7, 10, 6, 0), (7, 0)), Weekday::Fri);
        // Friday 08:00, deadline 07:00 → rolls to Saturday → weekend quantile.
        assert_eq!(deadline_day(at(2026, 7, 10, 8, 0), (7, 0)), Weekday::Sat);
        // Sunday 23:59, deadline 07:00 → rolls to Monday.
        assert_eq!(deadline_day(at(2026, 7, 12, 23, 59), (7, 0)), Weekday::Mon);
        // Exactly AT the deadline counts as passed (today_at > now fails) → tomorrow.
        assert_eq!(deadline_day(at(2026, 7, 10, 7, 0), (7, 0)), Weekday::Sat);
    }
}
