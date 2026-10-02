//! `backtest-terminal <days> [--publish-hour H] [--live]` — rolling-horizon backtest: OLD
//! (`app::terminal_soc_value`, in-horizon median) vs NEW (`app::terminal_soc_value_outlook`,
//! post-horizon day-type-median curve) terminal SoC valuation, on real measured history.
//!
//! Each LOCAL hour `T` in the window, both arms plan a battery-only LP (config `horizon`) from
//! measured PV/load (perfect foresight — the item under test is the terminal valuation, not the
//! forecast) and prices reconstructed exactly as they were KNOWN at `T`: a block is REAL iff its
//! delivery's local date is published by `T` (day-ahead day-D known from D-1 at `--publish-hour`,
//! default 14:00); otherwise the day-type median estimate from history known at `T`, then
//! persistence, then the fixed placeholder — the SAME `fill_block_prices` chain the live plan uses.
//! Both arms execute only the FIRST hour at the real prices and carry their own SoC forward — a
//! receding-horizon replay, not a single day-ahead solve.
//!
//! `--live` instead compares the two terminal values on the CURRENT on-demand plan
//! (`app::current_plan`, `PlanExtras::legacy_terminal_value`), mirroring `export_audit`'s live
//! comparison.
//!
//! Bounded reads throughout: OTE prices and Growatt PV/load/SoC are read in ≤7-day chunks with a
//! pause between (`export_audit::INTER_DAY_PAUSE_S`) — never one unbounded multi-week query.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Days, Duration, FixedOffset, NaiveDate, Timelike, Utc};
use uom::si::{angle::degree, f64::Angle};

use crate::app::{
    battery_spec, current_plan, fill_block_prices, placeholder_price_curve, select_terminal_value,
    tariff_prices, terminal_soc_value, PlanExtras,
};
use crate::export_audit::{shared_plan_cache, INTER_DAY_PAUSE_S};
use crate::influxdb::{PriceSample, TimeSample};
use crate::live_inputs::align_blocks_15min;
use crate::optimize::battery::{BatterySpec, DispatchInputs};
use crate::optimize::config::ControlConfig;
use crate::optimize::grid::BlockGrid;
use crate::optimize::price_forecast::{day_type_median_curve, day_type_median_price};
use crate::optimize::unified::{
    optimize_unified, FlowParams, SolveBudget, UnifiedPlan, DISPATCH_TOL,
};
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::what_if::{align_15min, empty_thermal, inert_heating_config};

const FINE_SECONDS: f64 = 900.0;
const FINE_SECONDS_I: i64 = 900;
/// The post-horizon window `terminal_soc_value_outlook` prices against — 24 h at the fine (15-min)
/// resolution, independent of the config horizon (see `app::current_plan`'s own constant of the
/// same shape).
const POST_HORIZON_BLOCKS: usize = 96;
const DEFAULT_PUBLISH_HOUR: u32 = 14;
/// `read_prices_range`/`growatt_series` chunk size (days) — COMMON.md's ≤7-day Influx-read bound.
const CHUNK_DAYS: i64 = 7;

// --- Pure parts (unit-tested) -------------------------------------------------------------------

/// The latest LOCAL calendar date whose day-ahead prices are published as of `t_local` — OTE
/// publishes day D's curve on D-1 at `publish_hour` local (default ~14:00): `t_local`'s own date
/// before that hour, the NEXT date from it on.
pub(crate) fn known_until_local_date(
    t_local: DateTime<FixedOffset>,
    publish_hour: u32,
) -> NaiveDate {
    if t_local.hour() >= publish_hour {
        t_local.date_naive() + Days::new(1)
    } else {
        t_local.date_naive()
    }
}

/// Is a block whose delivery falls on `delivery_local_date` REAL (published), given everything up
/// to and including `known_until` is published?
pub(crate) fn is_real_as_of(delivery_local_date: NaiveDate, known_until: NaiveDate) -> bool {
    delivery_local_date <= known_until
}

/// Which of the three named local-hour windows `local_hour` falls in — `05:00` (a point sample),
/// the morning import window `[5, 9)`, and the evening export window `[15, 21)`. The morning window
/// INCLUDES hour 5 (`05:00` is both a sample point and the start of "morning").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HourBuckets {
    pub(crate) is_0500: bool,
    pub(crate) is_morning_import_window: bool,
    pub(crate) is_evening_export_window: bool,
}

pub(crate) fn classify_local_hour(local_hour: u32) -> HourBuckets {
    HourBuckets {
        is_0500: local_hour == 5,
        is_morning_import_window: (5..9).contains(&local_hour),
        is_evening_export_window: (15..21).contains(&local_hour),
    }
}

/// What executing the FIRST HOUR of a plan (the fine blocks whose start is `< t_plus_1h`) actually
/// realizes, at the plan's own (real, by construction — the first hour is always known) prices.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecutedHour {
    pub(crate) cost_eur: f64,
    pub(crate) import_kwh: f64,
    pub(crate) export_kwh: f64,
    pub(crate) discharge_kwh: f64,
    /// SoC (kWh) at the end of the last executed block — `initial_soc_kwh` unchanged if nothing
    /// executed (an empty/degenerate grid).
    pub(crate) end_soc_kwh: f64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_first_hour(
    grid: &BlockGrid,
    t_plus_1h: DateTime<Utc>,
    import_price: &[f64],
    export_price: &[f64],
    grid_import_kw: &[f64],
    grid_export_kw: &[f64],
    discharge_kw: &[f64],
    soc_kwh: &[f64],
    amortisation: f64,
    initial_soc_kwh: f64,
) -> ExecutedHour {
    let mut exec = ExecutedHour {
        end_soc_kwh: initial_soc_kwh,
        ..Default::default()
    };
    for b in 0..grid.len() {
        if grid.block_start(b) >= t_plus_1h {
            break;
        }
        let dt = grid.dt_hours(b);
        let imp = grid_import_kw.get(b).copied().unwrap_or(0.0);
        let exp = grid_export_kw.get(b).copied().unwrap_or(0.0);
        let dis = discharge_kw.get(b).copied().unwrap_or(0.0);
        let pi = import_price.get(b).copied().unwrap_or(0.0);
        let pe = export_price.get(b).copied().unwrap_or(0.0);
        exec.cost_eur += dt * (imp * pi - exp * pe) + amortisation * dt * dis;
        exec.import_kwh += dt * imp;
        exec.export_kwh += dt * exp;
        exec.discharge_kwh += dt * dis;
        exec.end_soc_kwh = soc_kwh.get(b).copied().unwrap_or(exec.end_soc_kwh);
    }
    exec
}

/// One demoted leg (for the first-≤-20-OLD-blocks sample `run_floor_window` prints).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DemotedLeg {
    pub(crate) t: DateTime<Utc>,
    pub(crate) dt_hours: f64,
    pub(crate) leg: &'static str, // "export" or "grid_charge"
    pub(crate) kw: f64,
    pub(crate) price: f64,
}

/// [`execute_first_hour`]'s dispatch-floor twin: the ACTUATOR RULE applied block by block — a
/// sub-floor battery<->grid leg (`0 < v < floor`) is never actuated (the controller rounds it up,
/// which `classify_mode` demotes to `regular` instead — see `app::classify_mode`), so neither its
/// revenue/cost nor its wear is realized, and the forgone/retained energy stays in (discharge
/// blocked) or out of (charge blocked) the battery. Applies the PHYSICAL actuator rule, not a
/// leg-only one: without a COMMANDED `batt_to_grid`/`batt_grid_charge` (at/above the floor), a
/// load-first inverter only ever discharges up to the real house deficit (`served_load + EV −
/// pv`) or charges up to its real solar surplus (`pv − served_load − EV`) — the excess beyond
/// that is fiction regardless of which leg the LP's own accounting routed it through (the SAME
/// kWh can surface as extra `batt_to_load`/`solar_to_batt` while solar/grid makes up the
/// difference, same cost — see `optimize::unified`'s `ROUTING_EPSILON` doc). `booked_cost_eur` is
/// the as-PLANNED total (identical to what [`execute_first_hour`] would report); `realized_cost_eur`
/// is what actually happens once the fiction is stripped out. The SoC correction accumulates block
/// to block (`delta`, same style as `optimize::unified::round_dispatch_legs`'s SoC guard) since
/// retained/forgone energy persists for the REST of the executed hour, not just the block it arose
/// in; the final `end_soc_kwh` is clamped to `[min_soc_kwh, max_soc_kwh]` (the executor, unlike the
/// planner, has no SoC guard of its own — a demoted leg is a fact, not a choice). `floor <= 0.0`
/// makes every leg committed by construction (no demotion), matching [`execute_first_hour`] exactly.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutedHourFloor {
    pub(crate) booked_cost_eur: f64,
    pub(crate) realized_cost_eur: f64,
    pub(crate) import_kwh: f64,
    pub(crate) export_kwh: f64,
    pub(crate) discharge_kwh: f64,
    pub(crate) end_soc_kwh: f64,
    pub(crate) demoted_blocks: usize,
    pub(crate) demoted_export_kwh: f64,
    pub(crate) demoted_charge_kwh: f64,
    pub(crate) demoted: Vec<DemotedLeg>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_first_hour_floor(
    grid: &BlockGrid,
    t_plus_1h: DateTime<Utc>,
    import_price: &[f64],
    export_price: &[f64],
    grid_import_kw: &[f64],
    grid_export_kw: &[f64],
    discharge_kw: &[f64],
    charge_kw: &[f64],
    batt_to_grid_kw: &[f64],
    batt_grid_charge_kw: &[f64],
    pv_kw: &[f64],
    served_load_kw: &[f64],
    ev_total_kw: &[f64],
    soc_kwh: &[f64],
    amortisation: f64,
    initial_soc_kwh: f64,
    floor: f64,
    charge_efficiency: f64,
    discharge_efficiency: f64,
    min_soc_kwh: f64,
    max_soc_kwh: f64,
) -> ExecutedHourFloor {
    const EPS: f64 = 1e-9;
    // A demoted BLOCK is only counted above this fiction (kW); the kWh/EUR accounting below
    // stays exact for any amount. Dusk/dawn trickles of a few W would otherwise dominate the
    // count without moving the economics — 0.05 kW is `classify_mode`'s own "solver dust" edge.
    const COUNT_KW: f64 = 0.05;
    let mut exec = ExecutedHourFloor {
        end_soc_kwh: initial_soc_kwh,
        ..Default::default()
    };
    let mut delta = 0.0; // retained-energy correction (kWh), carried block to block
    for b in 0..grid.len() {
        let t = grid.block_start(b);
        if t >= t_plus_1h {
            break;
        }
        let dt = grid.dt_hours(b);
        let imp = grid_import_kw.get(b).copied().unwrap_or(0.0);
        let exp = grid_export_kw.get(b).copied().unwrap_or(0.0);
        let dis = discharge_kw.get(b).copied().unwrap_or(0.0);
        let chg = charge_kw.get(b).copied().unwrap_or(0.0);
        let bg = batt_to_grid_kw.get(b).copied().unwrap_or(0.0);
        let gc = batt_grid_charge_kw.get(b).copied().unwrap_or(0.0);
        let pi = import_price.get(b).copied().unwrap_or(0.0);
        let pe = export_price.get(b).copied().unwrap_or(0.0);
        let pv = pv_kw.get(b).copied().unwrap_or(0.0);
        let served_load = served_load_kw.get(b).copied().unwrap_or(0.0);
        let ev_total = ev_total_kw.get(b).copied().unwrap_or(0.0);
        let deficit = (served_load + ev_total - pv).max(0.0);
        let surplus = (pv - served_load - ev_total).max(0.0);

        exec.booked_cost_eur += dt * (imp * pi - exp * pe) + amortisation * dt * dis;

        let mut realized_imp = imp;
        let mut realized_exp = exp;
        let mut realized_dis = dis;

        if floor > 0.0 && bg < floor - DISPATCH_TOL {
            let fiction_dis = (dis - deficit).max(0.0);
            if fiction_dis > EPS {
                realized_exp -= fiction_dis;
                realized_dis -= fiction_dis;
                exec.demoted_export_kwh += fiction_dis * dt;
                delta += dt * fiction_dis / discharge_efficiency;
                if fiction_dis > COUNT_KW {
                    exec.demoted_blocks += 1;
                    exec.demoted.push(DemotedLeg {
                        t,
                        dt_hours: dt,
                        leg: "discharge",
                        kw: fiction_dis,
                        price: pe,
                    });
                }
            }
        }
        if floor > 0.0 && gc < floor - DISPATCH_TOL {
            let fiction_chg = (chg - surplus).max(0.0);
            if fiction_chg > EPS {
                realized_imp -= fiction_chg;
                exec.demoted_charge_kwh += fiction_chg * dt;
                delta -= dt * charge_efficiency * fiction_chg;
                if fiction_chg > COUNT_KW {
                    exec.demoted_blocks += 1;
                    exec.demoted.push(DemotedLeg {
                        t,
                        dt_hours: dt,
                        leg: "charge",
                        kw: fiction_chg,
                        price: pi,
                    });
                }
            }
        }

        exec.realized_cost_eur +=
            dt * (realized_imp * pi - realized_exp * pe) + amortisation * dt * realized_dis;
        exec.import_kwh += dt * realized_imp;
        exec.export_kwh += dt * realized_exp;
        exec.discharge_kwh += dt * realized_dis;
        let planned_soc = soc_kwh.get(b).copied().unwrap_or(exec.end_soc_kwh - delta);
        exec.end_soc_kwh = (planned_soc + delta).clamp(min_soc_kwh, max_soc_kwh);
    }
    exec
}

/// `(current, estimated, day_ago)` — the same shape `app::fill_block_prices` consumes.
type PriceFallbackInputs = (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>);

/// Real (published-as-of-`known_until`) spot-price samples from the FULL window array, as
/// `(time, price)` pairs — the day-type-median estimator's `history` argument. Built ONCE per hour
/// by the caller and shared by [`known_at_t_price_inputs`]'s per-block fallback AND the NEW arm's
/// post-horizon curve, instead of each independently re-scanning `real_spot_fine`.
fn history_known_at(
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    known_until: NaiveDate,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset,
) -> Vec<(DateTime<Utc>, f64)> {
    real_spot_fine
        .iter()
        .enumerate()
        .filter_map(|(i, &p)| {
            let at = array_start + Duration::seconds(FINE_SECONDS_I * i as i64);
            let real = is_real_as_of(at.with_timezone(&offset(at)).date_naive(), known_until);
            real.then_some(()).and(p).map(|v| (at, v))
        })
        .collect()
}

/// Build this plan's spot-price fallback inputs (`current`/`estimated`/`day_ago` — the same shape
/// `app::fill_block_prices` consumes live) from the FULL window's real (ground-truth) spot-price
/// array, respecting only what was KNOWN at `t` — a later day's real price never leaks into an
/// earlier plan. `real_spot_fine[i]` is the real spot price (EUR/kWh) `i` fine (900 s) steps after
/// `array_start`; `None` is a genuine data gap. `known_until`/`history` are the caller's own
/// per-hour [`history_known_at`] outputs, shared with the NEW arm's post-horizon curve.
#[allow(clippy::too_many_arguments)]
fn known_at_t_price_inputs(
    known_until: NaiveDate,
    history: &[(DateTime<Utc>, f64)],
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    t: DateTime<Utc>,
    n_fine: usize,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset,
    public_holidays: &[(u32, u32)],
    easter_holidays: bool,
) -> PriceFallbackInputs {
    let t_fine_index = (t - array_start).num_seconds().div_euclid(FINE_SECONDS_I);
    let at_index = |idx: i64| -> Option<f64> {
        usize::try_from(idx)
            .ok()
            .and_then(|i| real_spot_fine.get(i).copied().flatten())
    };

    let mut current = Vec::with_capacity(n_fine);
    let mut estimated = Vec::with_capacity(n_fine);
    let mut day_ago = Vec::with_capacity(n_fine);
    for b in 0..n_fine {
        let idx = t_fine_index + b as i64;
        let at = t + Duration::seconds(FINE_SECONDS_I * b as i64);
        let real = is_real_as_of(at.with_timezone(&offset(at)).date_naive(), known_until);
        current.push(if real { at_index(idx) } else { None });
        estimated.push(day_type_median_price(
            history,
            at,
            &offset,
            public_holidays,
            easter_holidays,
        ));
        // The day-ago candidate (the SAME clock block one day earlier) must ALSO have been
        // published as of `t` — live `block_prices`' `day_ago` only ever holds published prices,
        // and near the far edge of the horizon the day-ago delivery day can itself still be in the
        // future relative to `t` (a target 2+ days out). Without this gate the backtest would leak
        // a real price from a day that, at `t`, hadn't been auctioned yet.
        let at_day_ago = at - Duration::hours(24);
        let day_ago_real = is_real_as_of(
            at_day_ago.with_timezone(&offset(at_day_ago)).date_naive(),
            known_until,
        );
        day_ago.push(if day_ago_real {
            at_index(idx - 96)
        } else {
            None
        });
    }
    (current, estimated, day_ago)
}

/// Forward-fill `None` gaps with the last known (or first-available, before any value has been
/// seen) sample; returns the filled series and how many blocks were filled.
pub(crate) fn forward_fill(v: Vec<Option<f64>>) -> (Vec<f64>, usize) {
    let mut filled = 0usize;
    let mut last = v.iter().find_map(|&x| x).unwrap_or(0.0);
    let out = v
        .into_iter()
        .map(|x| match x {
            Some(val) => {
                last = val;
                val
            }
            None => {
                filled += 1;
                last
            }
        })
        .collect();
    (out, filled)
}

fn safe_mean(sum: f64, n: usize) -> f64 {
    if n > 0 {
        sum / n as f64
    } else {
        0.0
    }
}

/// Acceptance 2's paired metric: kWh sold on day `d`'s evening (`evening_export_kwh[d]`) that were
/// re-imported the VERY NEXT morning (`morning_import_kwh[d + 1]`) — `min` of the two, since only
/// the smaller of "sold" and "bought back" can actually be the SAME re-imported energy. Both slices
/// must be indexed by consecutive LOCAL calendar day (one entry per day, sorted, no gaps — the
/// caller's `day_rows` loop touches every date in the window, so this always holds there); the last
/// day pairs with nothing. Returns the per-day paired kWh (same length, last entry always `0.0`)
/// and the total.
fn paired_sold_then_reimported(
    evening_export_kwh: &[f64],
    morning_import_kwh: &[f64],
) -> (Vec<f64>, f64) {
    let n = evening_export_kwh.len().min(morning_import_kwh.len());
    let mut per_day = vec![0.0; evening_export_kwh.len()];
    for d in 0..n.saturating_sub(1) {
        per_day[d] = evening_export_kwh[d].min(morning_import_kwh[d + 1]);
    }
    let total = per_day.iter().sum();
    (per_day, total)
}

fn chunk_windows(start: DateTime<Utc>, stop: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut s = start;
    while s < stop {
        let e = (s + Duration::days(CHUNK_DAYS)).min(stop);
        out.push((s, e));
        s = e;
    }
    out
}

pub(crate) fn floor_to_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    t - Duration::minutes(t.minute() as i64)
        - Duration::seconds(t.second() as i64)
        - Duration::nanoseconds(t.nanosecond() as i64)
}

// --- Per-arm accounting --------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct ArmTotals {
    cost_eur: f64,
    import_kwh: f64,
    export_kwh: f64,
    discharge_kwh: f64,
    terminal_value_sum: f64,
    terminal_value_n: usize,
    fallback_count: usize,
    soc_0500_sum: f64,
    soc_0500_n: usize,
    evening_export_kwh: f64,
    morning_import_kwh: f64,
    planned_end_soc_sum: f64,
    planned_end_soc_n: usize,
    end_soc_kwh: f64,
}

#[allow(clippy::too_many_arguments)]
fn add_hour(
    totals: &mut ArmTotals,
    exec: &ExecutedHour,
    terminal_value: f64,
    fallback: bool,
    soc_before: f64,
    buckets: HourBuckets,
    planned_end_soc: f64,
) {
    totals.cost_eur += exec.cost_eur;
    totals.import_kwh += exec.import_kwh;
    totals.export_kwh += exec.export_kwh;
    totals.discharge_kwh += exec.discharge_kwh;
    totals.terminal_value_sum += terminal_value;
    totals.terminal_value_n += 1;
    if fallback {
        totals.fallback_count += 1;
    }
    if buckets.is_0500 {
        totals.soc_0500_sum += soc_before;
        totals.soc_0500_n += 1;
    }
    if buckets.is_evening_export_window {
        totals.evening_export_kwh += exec.export_kwh;
    }
    if buckets.is_morning_import_window {
        totals.morning_import_kwh += exec.import_kwh;
    }
    totals.planned_end_soc_sum += planned_end_soc;
    totals.planned_end_soc_n += 1;
    totals.end_soc_kwh = exec.end_soc_kwh;
}

/// Per-arm totals for `run_floor_window`. The NEW-only solve-timing/pin fields stay `0`/empty on
/// the OLD arm (it never pins — `flow.min_dispatch_kw = 0`, the live loop's already-integral path).
#[derive(Debug, Default, Clone)]
struct FloorArmTotals {
    realized_cost_eur: f64,
    booked_cost_eur: f64,
    import_kwh: f64,
    export_kwh: f64,
    discharge_kwh: f64,
    end_soc_kwh: f64,
    /// Hourly PLANS (whole horizon, not just the executed hour) with ≥ 1 sub-floor leg anywhere.
    plans_with_sub_floor: usize,
    /// EXECUTED blocks with a demoted leg (the actuator-rule executor's own count).
    demoted_blocks: usize,
    demoted_export_kwh: f64,
    demoted_charge_kwh: f64,
    /// Successful STAGE 1 (un-guarded pin) resolves — the normal case.
    stage1_pinned_resolves: usize,
    /// Hours where stage 1's pinned re-solve failed and stage 2 (the SoC guard) was actually
    /// attempted (`guard_freed > 0`; a `guard_freed == 0` retry would be the identical LP and is
    /// skipped straight to the relaxed fallback, same as `app::fix_and_round_inner`).
    stage2_retries: usize,
    /// Blocks the SoC guard left free, SUMMED OVER stage-2 attempts only.
    stage2_guard_freed: usize,
    /// Successful STAGE 2 resolves (a subset of `stage2_retries`).
    stage2_pinned_resolves: usize,
    /// Hours that ended on the RELAXED (unpinned) plan — stage 1 failed and (stage 2 was skipped
    /// because it would be identical, OR stage 2 itself failed).
    relaxed_fallbacks: usize,
    /// Σ over all FINAL (post-stage) NEW plans of the number of sub-floor legs still present
    /// anywhere in the horizon — should be ≈ `stage2_guard_freed` (the guard-freed legs are the
    /// only ones a final plan can still carry; a plain-relaxed fallback hour can carry more).
    final_sub_floor_legs: usize,
    relaxed_solve_ms_sum: f64,
    relaxed_solve_ms_max: f64,
    relaxed_solve_n: usize,
    pinned_solve_ms_sum: f64,
    pinned_solve_ms_max: f64,
    pinned_solve_n: usize,
}

fn add_floor_hour(totals: &mut FloorArmTotals, exec: &ExecutedHourFloor) {
    totals.realized_cost_eur += exec.realized_cost_eur;
    totals.booked_cost_eur += exec.booked_cost_eur;
    totals.import_kwh += exec.import_kwh;
    totals.export_kwh += exec.export_kwh;
    totals.discharge_kwh += exec.discharge_kwh;
    totals.end_soc_kwh = exec.end_soc_kwh;
    totals.demoted_blocks += exec.demoted_blocks;
    totals.demoted_export_kwh += exec.demoted_export_kwh;
    totals.demoted_charge_kwh += exec.demoted_charge_kwh;
}

// --- IO: bounded reads ----------------------------------------------------------------------------

pub(crate) async fn read_prices_chunked(
    db: &SourceClients,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Result<Vec<PriceSample>> {
    let mut all = Vec::new();
    let windows = chunk_windows(start, stop);
    let n = windows.len();
    for (i, (s, e)) in windows.into_iter().enumerate() {
        let mut samples = db
            .read_prices_range(&s.to_rfc3339(), &e.to_rfc3339())
            .await
            .with_context(|| format!("reading OTE prices {s}..{e}"))?;
        all.append(&mut samples);
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    Ok(all)
}

pub(crate) async fn read_growatt_chunked(
    db: &SourceClients,
    metric: &str,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Vec<TimeSample> {
    let mut all = Vec::new();
    let windows = chunk_windows(start, stop);
    let n = windows.len();
    for (i, (s, e)) in windows.into_iter().enumerate() {
        let mut samples = db
            .growatt_series(metric, &s.to_rfc3339(), &e.to_rfc3339(), "15m")
            .await
            .unwrap_or_default();
        all.append(&mut samples);
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    all
}

pub(crate) async fn read_soc_seed(
    db: &SourceClients,
    window_start: DateTime<Utc>,
    config: &ControlConfig,
) -> f64 {
    let stop = window_start + Duration::hours(1);
    db.growatt_series("SOC", &window_start.to_rfc3339(), &stop.to_rfc3339(), "15m")
        .await
        .ok()
        .and_then(|s| {
            s.first()
                .map(|x| x.value / 100.0 * config.battery.capacity_kwh)
        })
        .unwrap_or_else(|| config.battery.min_soc_pct / 100.0 * config.battery.capacity_kwh)
}

// --- The battery-only LP solve, shared by both arms ------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn solve_arm(
    battery: &BatterySpec,
    heating: &crate::optimize::config::HeatingConfig,
    grid: &BlockGrid,
    import_blocks: &[f64],
    export_blocks: &[f64],
    pv_blocks: &[f64],
    load_blocks: &[f64],
    export_allowed_blocks: &[bool],
    inverter_on_blocks: &[bool],
    placeholder_blocks: &[bool],
    amortisation: f64,
    terminal_value: f64,
    config: &ControlConfig,
    initial_soc: f64,
    min_final_soc: f64,
    outdoor: &[f64],
    minutes: &[u32],
    fixed_binaries: Option<&crate::optimize::unified::FixedBinaries>,
) -> Result<UnifiedPlan> {
    let mut battery = battery.clone();
    battery.initial_soc_kwh = initial_soc.clamp(battery.min_soc_kwh, battery.max_soc_kwh);
    let flow = FlowParams {
        export_allowed: export_allowed_blocks.to_vec(),
        inverter_on: inverter_on_blocks.to_vec(),
        price_placeholder: placeholder_blocks.to_vec(),
        amortisation,
        terminal_value,
        terminal_heat_value: 0.0,
        terminal_heat_value_by_zone: HashMap::new(),
        terminal_heat_budget_kwh: HashMap::new(),
        max_import_kw: config.grid.max_import_kw,
        max_export_kw: config.grid.max_export_kw,
        export_needs_pv: config.battery.export_needs_pv,
        min_dispatch_kw: config.battery.min_dispatch_kw,
    };
    let inputs = DispatchInputs {
        dt_hours: grid.dt_hours(0),
        import_price: import_blocks.to_vec(),
        export_price: export_blocks.to_vec(),
        pv_kw: pv_blocks.to_vec(),
        load_kw: load_blocks.to_vec(),
        min_final_soc_kwh: Some(min_final_soc),
    };
    optimize_unified(
        &battery,
        heating,
        &crate::optimize::config::HvacConfig::default(),
        &empty_thermal(grid.clone()),
        &inputs,
        &flow,
        outdoor,
        &[],
        &[],
        None,
        minutes,
        fixed_binaries,
        SolveBudget::default(),
    )
    .context("terminal backtest: battery-only LP solve failed")
}

// --- Per-hour input preparation, shared by `run_window` and `run_floor_window` -------------------

/// Everything one hour `t`'s plan needs from the market/weather side (prices known-as-of `t`, the
/// measured PV/load perfect-foresight slice, the gates, the block grid) — common to both the
/// terminal-value backtest and the dispatch-floor backtest; only the ARMS (what each solves/
/// executes) differ. `history`/`import_fine`/`mask_fine` are exposed (not folded into the block
/// aggregates) because each caller's own terminal-value computation needs them at FINE resolution
/// (the in-horizon-median basis) and as the day-type-median estimator's history input.
pub(crate) struct HourPrep {
    pub(crate) grid: BlockGrid,
    pub(crate) history: Vec<(DateTime<Utc>, f64)>,
    pub(crate) import_fine: Vec<f64>,
    /// Export price on the SAME fine lattice as `import_fine` — exposed (not folded into
    /// `export_blocks` alone) for `warmth_backtest`'s `ForecastContext`, which is built on the
    /// fine lattice directly rather than through this module's block-aggregated `solve_arm` path.
    pub(crate) export_fine: Vec<f64>,
    pub(crate) mask_fine: Vec<bool>,
    /// Fine-lattice export-allowed/inverter-on gates — see `export_fine`'s doc.
    pub(crate) export_allowed_fine: Vec<bool>,
    pub(crate) inverter_on_fine: Vec<bool>,
    /// Measured PV/load (kW) on the fine lattice, perfect foresight — see `export_fine`'s doc;
    /// `warmth_backtest` feeds these straight into `ForecastContext::pv_kw_override`/
    /// `load_kw_override` instead of re-slicing the measured arrays itself.
    pub(crate) pv_fine: Vec<f64>,
    pub(crate) load_fine: Vec<f64>,
    pub(crate) import_blocks: Vec<f64>,
    pub(crate) export_blocks: Vec<f64>,
    pub(crate) pv_blocks: Vec<f64>,
    pub(crate) load_blocks: Vec<f64>,
    pub(crate) export_allowed_blocks: Vec<bool>,
    pub(crate) inverter_on_blocks: Vec<bool>,
    pub(crate) placeholder_blocks: Vec<bool>,
    pub(crate) minutes: Vec<u32>,
    pub(crate) outdoor: Vec<f64>,
}

/// Build [`HourPrep`] for hour `t`, shared by `run_window` and `run_floor_window` so both
/// backtests use exactly one known-at-`T` price/foresight/gate construction — they can never see
/// two different ideas of what hour `t` knew. `run_window`'s own numbers are unaffected: same
/// functions, same order, same inputs as computing them inline.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_hour(
    t: DateTime<Utc>,
    config: &ControlConfig,
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    pv_kw: &[f64],
    load_kw: &[f64],
    window_start: DateTime<Utc>,
    publish_hour: u32,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset + Copy,
    public_holidays: &[(u32, u32)],
    export_floor: f64,
    inverter_off_price: f64,
) -> HourPrep {
    let grid = BlockGrid::multi_rate(
        t,
        config.horizon.hours,
        config.horizon.fine_hours,
        FINE_SECONDS,
    );
    let n_fine = grid.n_fine();

    // Built ONCE per hour, shared by the fallback chain below AND each caller's own NEW-arm
    // post-horizon curve — see `history_known_at`'s doc.
    let known_until = known_until_local_date(t.with_timezone(&offset(t)), publish_hour);
    let history = history_known_at(array_start, real_spot_fine, known_until, offset);

    let (current, estimated, day_ago) = known_at_t_price_inputs(
        known_until,
        &history,
        array_start,
        real_spot_fine,
        t,
        n_fine,
        offset,
        public_holidays,
        config.site.easter_holidays,
    );
    let placeholder = placeholder_price_curve(t, offset(t), n_fine);
    let (spot_fine, mask_fine, _missing, _persisted, _estimated_count) =
        fill_block_prices(&current, &estimated, &day_ago, &placeholder);
    let (import_fine, export_fine) = tariff_prices(&config.tariff, &config.site, &spot_fine, t);
    let export_allowed_fine: Vec<bool> = spot_fine.iter().map(|&s| s >= export_floor).collect();
    let inverter_on_fine: Vec<bool> = spot_fine.iter().map(|&s| s >= inverter_off_price).collect();

    // Perfect foresight: the measured PV/load series stand in for the forecast directly.
    let meas_offset = ((t - window_start).num_seconds() / FINE_SECONDS_I) as usize;
    let pv_slice: Vec<f64> = (0..n_fine)
        .map(|i| pv_kw.get(meas_offset + i).copied().unwrap_or(0.0))
        .collect();
    let load_slice: Vec<f64> = (0..n_fine)
        .map(|i| load_kw.get(meas_offset + i).copied().unwrap_or(0.0))
        .collect();

    let import_blocks = grid.mean(&import_fine);
    let export_blocks = grid.mean(&export_fine);
    let pv_blocks = grid.mean(&pv_slice);
    let load_blocks = grid.mean(&load_slice);
    let export_allowed_blocks = grid.all(&export_allowed_fine);
    let inverter_on_blocks = grid.all(&inverter_on_fine);
    let placeholder_blocks = grid.any(&mask_fine);
    let minutes: Vec<u32> = (0..grid.len())
        .map(|b| {
            let at = grid.block_start(b);
            let local = at.with_timezone(&offset(at));
            local.hour() * 60 + local.minute()
        })
        .collect();
    let outdoor = vec![15.0; grid.len()];

    HourPrep {
        grid,
        history,
        import_fine,
        export_fine,
        mask_fine,
        export_allowed_fine,
        inverter_on_fine,
        pv_fine: pv_slice,
        load_fine: load_slice,
        import_blocks,
        export_blocks,
        pv_blocks,
        load_blocks,
        export_allowed_blocks,
        inverter_on_blocks,
        placeholder_blocks,
        minutes,
        outdoor,
    }
}

// --- The rolling-horizon window backtest ------------------------------------------------------------

async fn run_window(
    db: &SourceClients,
    config: &ControlConfig,
    days: i64,
    publish_hour: u32,
) -> Result<()> {
    let now = Utc::now();
    let end = floor_to_hour(now - Duration::hours(37));
    let window_start = end - Duration::hours(24 * days);
    let array_start = window_start - Duration::days(28);
    let read_stop = end + Duration::days(2);

    println!(
        "backtest-terminal: window {window_start} .. {end} ({days} day(s)), publish_hour \
         {publish_hour}, history from {array_start}"
    );

    let price_samples = read_prices_chunked(db, array_start, read_stop).await?;
    let n_array = ((read_stop - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    // `align_blocks_15min` already converts EUR/MWh -> EUR/kWh.
    let real_spot_fine = align_blocks_15min(&price_samples, array_start, n_array)
        .unwrap_or_else(|| vec![None; n_array]);

    let core_start = ((window_start - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let core_end = ((end - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let missing_core = real_spot_fine[core_start..core_end]
        .iter()
        .filter(|p| p.is_none())
        .count();
    ensure!(
        missing_core == 0,
        "backtest-terminal: {missing_core} OTE price block(s) missing in the core window \
         [{window_start}, {end}) — cannot execute at real prices"
    );

    let pv_samples = read_growatt_chunked(db, "InputPower", window_start, read_stop).await;
    let load_samples =
        read_growatt_chunked(db, "INVPowerToLocalLoad", window_start, read_stop).await;
    let n_meas = ((read_stop - window_start).num_seconds() / FINE_SECONDS_I) as usize;
    let pv_raw = align_15min(&pv_samples, window_start, n_meas);
    let load_raw = align_15min(&load_samples, window_start, n_meas);
    let (pv_kw, pv_filled) = forward_fill(
        pv_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );
    let (load_kw, load_filled) = forward_fill(
        load_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );
    println!(
        "  measured gaps forward-filled: PV {pv_filled}/{n_meas} blocks, load {load_filled}/{n_meas} \
         blocks"
    );

    let soc0 = read_soc_seed(db, window_start, config).await;

    let battery_spec0 = battery_spec(&config.battery);
    let heating = inert_heating_config();
    let offset = |t: DateTime<Utc>| config.site.offset_at(t);
    let public_holidays: Vec<(u32, u32)> = config
        .site
        .public_holidays
        .iter()
        .filter_map(|md| crate::optimize::price_forecast::parse_month_day(md))
        .collect();
    let distribution_eur_by_local_hour: [f64; 24] = {
        let mask = config.tariff.low_tariff_mask();
        std::array::from_fn(|h| config.tariff.distribution_eur(h as u32, &mask))
    };
    let export_floor = config.tariff.czk_to_eur(config.tariff.export_price_min_czk);
    let inverter_off_price = config
        .tariff
        .czk_to_eur(config.tariff.inverter_off_price_czk);
    let amortisation = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);
    let round_trip_eta = battery_spec0.charge_efficiency * battery_spec0.discharge_efficiency;
    let min_final_soc = battery_spec0.min_soc_kwh;

    let mut old_soc = soc0;
    let mut new_soc = soc0;
    let mut old_totals = ArmTotals::default();
    let mut new_totals = ArmTotals::default();
    let mut day_rows: BTreeMap<NaiveDate, (ArmTotals, ArmTotals)> = BTreeMap::new();

    let n_hours = days * 24;
    for h in 0..n_hours {
        let t = window_start + Duration::hours(h);
        if t >= end {
            break;
        }
        let hp = prepare_hour(
            t,
            config,
            array_start,
            &real_spot_fine,
            &pv_kw,
            &load_kw,
            window_start,
            publish_hour,
            offset,
            &public_holidays,
            export_floor,
            inverter_off_price,
        );

        // OLD: in-horizon median on the FINE import/mask arrays — mirrors `app::current_plan`'s own
        // `terminal_basis` rule exactly (computed before any grid aggregation there too).
        let real_import_fine: Vec<f64> = hp
            .import_fine
            .iter()
            .zip(&hp.mask_fine)
            .filter(|(_, &m)| !m)
            .map(|(&p, _)| p)
            .collect();
        let old_basis: &[f64] = if real_import_fine.len() >= 16 {
            &real_import_fine
        } else {
            &hp.import_fine
        };
        let old_terminal_value = terminal_soc_value(old_basis, amortisation, round_trip_eta);

        // NEW: the post-horizon 24 h curve from the TRUE grid end, the SAME `history` known at `t`
        // built once above.
        let outlook_start = hp.grid.block_end(hp.grid.len() - 1);
        let post_curve = day_type_median_curve(
            &hp.history,
            outlook_start,
            FINE_SECONDS,
            POST_HORIZON_BLOCKS,
            offset,
            &public_holidays,
            config.site.easter_holidays,
            &distribution_eur_by_local_hour,
        );
        // `select_terminal_value(false, ...)` is the SAME outlook-vs-horizon-median choice
        // `app::current_plan` makes live — never the `legacy` branch (that's `--live`'s own
        // OLD/NEW toggle via `PlanExtras::legacy_terminal_value`, not this arm's own valuation).
        let (new_terminal_value, _source, note) = select_terminal_value(
            false,
            &post_curve,
            old_terminal_value,
            amortisation,
            round_trip_eta,
        );
        let new_fallback = note.is_some();

        let old_plan = solve_arm(
            &battery_spec0,
            &heating,
            &hp.grid,
            &hp.import_blocks,
            &hp.export_blocks,
            &hp.pv_blocks,
            &hp.load_blocks,
            &hp.export_allowed_blocks,
            &hp.inverter_on_blocks,
            &hp.placeholder_blocks,
            amortisation,
            old_terminal_value,
            config,
            old_soc,
            min_final_soc,
            &hp.outdoor,
            &hp.minutes,
            None,
        )
        .with_context(|| format!("OLD arm at {t}"))?;
        let new_plan = solve_arm(
            &battery_spec0,
            &heating,
            &hp.grid,
            &hp.import_blocks,
            &hp.export_blocks,
            &hp.pv_blocks,
            &hp.load_blocks,
            &hp.export_allowed_blocks,
            &hp.inverter_on_blocks,
            &hp.placeholder_blocks,
            amortisation,
            new_terminal_value,
            config,
            new_soc,
            min_final_soc,
            &hp.outdoor,
            &hp.minutes,
            None,
        )
        .with_context(|| format!("NEW arm at {t}"))?;

        let t_plus_1h = t + Duration::hours(1);
        let old_exec = execute_first_hour(
            &hp.grid,
            t_plus_1h,
            &hp.import_blocks,
            &hp.export_blocks,
            &old_plan.grid_import_kw,
            &old_plan.grid_export_kw,
            &old_plan.discharge_kw,
            &old_plan.soc_kwh,
            amortisation,
            old_soc,
        );
        let new_exec = execute_first_hour(
            &hp.grid,
            t_plus_1h,
            &hp.import_blocks,
            &hp.export_blocks,
            &new_plan.grid_import_kw,
            &new_plan.grid_export_kw,
            &new_plan.discharge_kw,
            &new_plan.soc_kwh,
            amortisation,
            new_soc,
        );

        let t_local = t.with_timezone(&offset(t));
        let buckets = classify_local_hour(t_local.hour());
        let date = t_local.date_naive();
        let day = day_rows.entry(date).or_default();
        add_hour(
            &mut old_totals,
            &old_exec,
            old_terminal_value,
            false,
            old_soc,
            buckets,
            old_plan.soc_kwh.last().copied().unwrap_or(old_soc),
        );
        add_hour(
            &mut day.0,
            &old_exec,
            old_terminal_value,
            false,
            old_soc,
            buckets,
            old_plan.soc_kwh.last().copied().unwrap_or(old_soc),
        );
        add_hour(
            &mut new_totals,
            &new_exec,
            new_terminal_value,
            new_fallback,
            new_soc,
            buckets,
            new_plan.soc_kwh.last().copied().unwrap_or(new_soc),
        );
        add_hour(
            &mut day.1,
            &new_exec,
            new_terminal_value,
            new_fallback,
            new_soc,
            buckets,
            new_plan.soc_kwh.last().copied().unwrap_or(new_soc),
        );

        old_soc = old_exec.end_soc_kwh;
        new_soc = new_exec.end_soc_kwh;
    }

    let core_spot: Vec<f64> = real_spot_fine[core_start..core_end]
        .iter()
        .map(|p| p.unwrap_or(0.0))
        .collect();
    let (core_import, _) = tariff_prices(&config.tariff, &config.site, &core_spot, window_start);
    let mean_real_import = safe_mean(core_import.iter().sum(), core_import.len());

    // Acceptance 2's paired metric: evening-export kWh re-imported the very next morning.
    let old_evening: Vec<f64> = day_rows
        .values()
        .map(|(o, _)| o.evening_export_kwh)
        .collect();
    let old_morning: Vec<f64> = day_rows
        .values()
        .map(|(o, _)| o.morning_import_kwh)
        .collect();
    let new_evening: Vec<f64> = day_rows
        .values()
        .map(|(_, n)| n.evening_export_kwh)
        .collect();
    let new_morning: Vec<f64> = day_rows
        .values()
        .map(|(_, n)| n.morning_import_kwh)
        .collect();
    let (old_paired_by_day, old_paired_total) =
        paired_sold_then_reimported(&old_evening, &old_morning);
    let (new_paired_by_day, new_paired_total) =
        paired_sold_then_reimported(&new_evening, &new_morning);

    println!(
        "\nOLD vs NEW terminal SoC valuation — {} hourly plan(s) over {window_start} .. {end}",
        old_totals.terminal_value_n
    );
    println!(
        "  {:<6}{:>10}{:>10}{:>10}{:>11}{:>12}{:>10}{:>10}",
        "arm", "cost EUR", "imp kWh", "exp kWh", "disch kWh", "mean term", "fallback", "end SoC"
    );
    for (label, t) in [("OLD", &old_totals), ("NEW", &new_totals)] {
        println!(
            "  {:<6}{:>10.3}{:>10.1}{:>10.1}{:>11.1}{:>12.4}{:>10}{:>10.2}",
            label,
            t.cost_eur,
            t.import_kwh,
            t.export_kwh,
            t.discharge_kwh,
            safe_mean(t.terminal_value_sum, t.terminal_value_n),
            t.fallback_count,
            t.end_soc_kwh,
        );
    }
    println!(
        "  05:00 SoC mean: OLD {:.2} kWh ({} samples) / NEW {:.2} kWh ({} samples)",
        safe_mean(old_totals.soc_0500_sum, old_totals.soc_0500_n),
        old_totals.soc_0500_n,
        safe_mean(new_totals.soc_0500_sum, new_totals.soc_0500_n),
        new_totals.soc_0500_n,
    );
    println!(
        "  evening export kWh: OLD {:.2} / NEW {:.2}  |  morning import kWh: OLD {:.2} / NEW {:.2}  \
         |  paired (sold evening, re-imported next morning) kWh: OLD {:.2} / NEW {:.2}",
        old_totals.evening_export_kwh,
        new_totals.evening_export_kwh,
        old_totals.morning_import_kwh,
        new_totals.morning_import_kwh,
        old_paired_total,
        new_paired_total,
    );
    println!(
        "  mean end-of-horizon planned SoC: OLD {:.2} kWh / NEW {:.2} kWh",
        safe_mean(old_totals.planned_end_soc_sum, old_totals.planned_end_soc_n),
        safe_mean(new_totals.planned_end_soc_sum, new_totals.planned_end_soc_n),
    );
    let soc_delta = new_totals.end_soc_kwh - old_totals.end_soc_kwh;
    let adjusted_new_cost =
        new_totals.cost_eur - soc_delta * mean_real_import * battery_spec0.discharge_efficiency;
    println!(
        "  end SoC: OLD {:.2} kWh / NEW {:.2} kWh (delta {:+.2} kWh); NEW cost valuing that delta \
         at the window's mean real import ({:.4} EUR/kWh) x eta_d: {:.3} EUR (raw {:.3} EUR)",
        old_totals.end_soc_kwh,
        new_totals.end_soc_kwh,
        soc_delta,
        mean_real_import,
        adjusted_new_cost,
        new_totals.cost_eur,
    );

    println!(
        "\nper local day: cost OLD/NEW EUR, 05:00 SoC OLD/NEW kWh, evening-export OLD/NEW kWh, \
         morning-import OLD/NEW kWh, paired OLD/NEW kWh (this day's evening export re-imported the \
         NEXT morning)"
    );
    for (i, (date, (o, n))) in day_rows.iter().enumerate() {
        println!(
            "  {date}: cost {:.3}/{:.3}  05:00 {:.2}/{:.2}  evening-exp {:.2}/{:.2}  \
             morning-imp {:.2}/{:.2}  paired {:.2}/{:.2}",
            o.cost_eur,
            n.cost_eur,
            safe_mean(o.soc_0500_sum, o.soc_0500_n),
            safe_mean(n.soc_0500_sum, n.soc_0500_n),
            o.evening_export_kwh,
            n.evening_export_kwh,
            o.morning_import_kwh,
            n.morning_import_kwh,
            old_paired_by_day.get(i).copied().unwrap_or(0.0),
            new_paired_by_day.get(i).copied().unwrap_or(0.0),
        );
    }
    Ok(())
}

// --- The dispatch-floor rolling-horizon backtest -------------------------------------------------

/// OLD (today's behaviour: `min_dispatch_kw = 0`) vs NEW (the configured floor, via fix-and-round)
/// under the ACTUATOR RULE: in each arm's executed first hour, a sub-floor battery<->grid leg (`0 <
/// v < floor`) is demoted (not actuated) by [`execute_first_hour_floor`] — the SAME executor for
/// both arms, so NEW is scored by exactly what OLD already had to contend with. Window setup
/// mirrors `run_window`'s own (same bounded reads); only the per-hour arms and accounting differ.
async fn run_floor_window(
    db: &SourceClients,
    config: &ControlConfig,
    days: i64,
    publish_hour: u32,
) -> Result<()> {
    let floor = config.battery.min_dispatch_kw;
    ensure!(
        floor > 0.0,
        "backtest-dispatch-floor: config.battery.min_dispatch_kw must be > 0 to backtest the floor \
         mechanism (got 0 — nothing for the floor to demote)"
    );

    let now = Utc::now();
    let end = floor_to_hour(now - Duration::hours(37));
    let window_start = end - Duration::hours(24 * days);
    let array_start = window_start - Duration::days(28);
    let read_stop = end + Duration::days(2);

    println!(
        "backtest-dispatch-floor: window {window_start} .. {end} ({days} day(s)), floor {floor:.2} \
         kW, publish_hour {publish_hour}, history from {array_start}"
    );

    let price_samples = read_prices_chunked(db, array_start, read_stop).await?;
    let n_array = ((read_stop - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let real_spot_fine = align_blocks_15min(&price_samples, array_start, n_array)
        .unwrap_or_else(|| vec![None; n_array]);

    let core_start = ((window_start - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let core_end = ((end - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let missing_core = real_spot_fine[core_start..core_end]
        .iter()
        .filter(|p| p.is_none())
        .count();
    ensure!(
        missing_core == 0,
        "backtest-dispatch-floor: {missing_core} OTE price block(s) missing in the core window \
         [{window_start}, {end}) — cannot execute at real prices"
    );

    let pv_samples = read_growatt_chunked(db, "InputPower", window_start, read_stop).await;
    let load_samples =
        read_growatt_chunked(db, "INVPowerToLocalLoad", window_start, read_stop).await;
    let n_meas = ((read_stop - window_start).num_seconds() / FINE_SECONDS_I) as usize;
    let pv_raw = align_15min(&pv_samples, window_start, n_meas);
    let load_raw = align_15min(&load_samples, window_start, n_meas);
    let (pv_kw, pv_filled) = forward_fill(
        pv_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );
    let (load_kw, load_filled) = forward_fill(
        load_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );
    println!(
        "  measured gaps forward-filled: PV {pv_filled}/{n_meas} blocks, load {load_filled}/{n_meas} \
         blocks"
    );

    let soc0 = read_soc_seed(db, window_start, config).await;

    let battery_spec0 = battery_spec(&config.battery);
    let heating = inert_heating_config();
    let offset = |t: DateTime<Utc>| config.site.offset_at(t);
    let public_holidays: Vec<(u32, u32)> = config
        .site
        .public_holidays
        .iter()
        .filter_map(|md| crate::optimize::price_forecast::parse_month_day(md))
        .collect();
    let distribution_eur_by_local_hour: [f64; 24] = {
        let mask = config.tariff.low_tariff_mask();
        std::array::from_fn(|h| config.tariff.distribution_eur(h as u32, &mask))
    };
    let export_floor = config.tariff.czk_to_eur(config.tariff.export_price_min_czk);
    let inverter_off_price = config
        .tariff
        .czk_to_eur(config.tariff.inverter_off_price_czk);
    let amortisation = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);
    let round_trip_eta = battery_spec0.charge_efficiency * battery_spec0.discharge_efficiency;
    let min_final_soc = battery_spec0.min_soc_kwh;

    // OLD = today's behaviour: the solve itself sees no floor at all (`min_dispatch_kw: 0`); the
    // floor only ever acts at EXECUTION, via the actuator-rule executor shared with NEW.
    let mut old_cfg = config.clone();
    old_cfg.battery.min_dispatch_kw = 0.0;

    let mut old_soc = soc0;
    let mut new_soc = soc0;
    let mut old_totals = FloorArmTotals::default();
    let mut new_totals = FloorArmTotals::default();
    let mut day_rows: BTreeMap<NaiveDate, (FloorArmTotals, FloorArmTotals)> = BTreeMap::new();
    let mut demoted_sample: Vec<DemotedLeg> = Vec::new();
    let mut hours_executed = 0usize;

    let n_hours = days * 24;
    for h in 0..n_hours {
        let t = window_start + Duration::hours(h);
        if t >= end {
            break;
        }
        let hp = prepare_hour(
            t,
            config,
            array_start,
            &real_spot_fine,
            &pv_kw,
            &load_kw,
            window_start,
            publish_hour,
            offset,
            &public_holidays,
            export_floor,
            inverter_off_price,
        );

        // Both arms use the SAME (live) terminal valuation — only the dispatch floor differs.
        let real_import_fine: Vec<f64> = hp
            .import_fine
            .iter()
            .zip(&hp.mask_fine)
            .filter(|(_, &m)| !m)
            .map(|(&p, _)| p)
            .collect();
        let basis: &[f64] = if real_import_fine.len() >= 16 {
            &real_import_fine
        } else {
            &hp.import_fine
        };
        let horizon_median_fallback = terminal_soc_value(basis, amortisation, round_trip_eta);
        let outlook_start = hp.grid.block_end(hp.grid.len() - 1);
        let post_curve = day_type_median_curve(
            &hp.history,
            outlook_start,
            FINE_SECONDS,
            POST_HORIZON_BLOCKS,
            offset,
            &public_holidays,
            config.site.easter_holidays,
            &distribution_eur_by_local_hour,
        );
        let (terminal_value, _source, _note) = select_terminal_value(
            false,
            &post_curve,
            horizon_median_fallback,
            amortisation,
            round_trip_eta,
        );

        let old_plan = solve_arm(
            &battery_spec0,
            &heating,
            &hp.grid,
            &hp.import_blocks,
            &hp.export_blocks,
            &hp.pv_blocks,
            &hp.load_blocks,
            &hp.export_allowed_blocks,
            &hp.inverter_on_blocks,
            &hp.placeholder_blocks,
            amortisation,
            terminal_value,
            &old_cfg,
            old_soc,
            min_final_soc,
            &hp.outdoor,
            &hp.minutes,
            None,
        )
        .with_context(|| format!("OLD(floor=0) arm at {t}"))?;

        // One closure for every NEW-arm solve this hour (relaxed + either pinned stage) — the 17
        // args beyond `fixed_binaries` never change within an hour (only `new_soc` changes, and
        // only BETWEEN hours).
        let solve_new = |fixed_binaries: Option<&crate::optimize::unified::FixedBinaries>| {
            solve_arm(
                &battery_spec0,
                &heating,
                &hp.grid,
                &hp.import_blocks,
                &hp.export_blocks,
                &hp.pv_blocks,
                &hp.load_blocks,
                &hp.export_allowed_blocks,
                &hp.inverter_on_blocks,
                &hp.placeholder_blocks,
                amortisation,
                terminal_value,
                config,
                new_soc,
                min_final_soc,
                &hp.outdoor,
                &hp.minutes,
                fixed_binaries,
            )
        };

        let relaxed_t0 = std::time::Instant::now();
        let new_relaxed =
            solve_new(None).with_context(|| format!("NEW(floor) relaxed arm at {t}"))?;
        let relaxed_ms = relaxed_t0.elapsed().as_secs_f64() * 1000.0;
        new_totals.relaxed_solve_n += 1;
        new_totals.relaxed_solve_ms_sum += relaxed_ms;
        new_totals.relaxed_solve_ms_max = new_totals.relaxed_solve_ms_max.max(relaxed_ms);

        // STAGE 1 is the un-guarded pin (every sub-floor leg pinned OFF unconditionally) tried
        // FIRST; STAGE 2 (the SoC guard) is the RETRY only if stage 1's pinned re-solve itself
        // fails AND the guard would actually free something (otherwise it's the identical LP and
        // would fail again) — a local mirror of `app::fix_and_round_inner`'s own two-stage logic,
        // kept separate since the two don't share a signature. Both stages also merge the
        // routing-loophole caps (`batt_to_load_cap`/`solar_to_batt_cap`), identical in each stage
        // since they depend only on `new_relaxed`, not on the guard.
        let new_plan = if crate::optimize::unified::dispatch_legs_integral(&new_relaxed, floor) {
            new_relaxed
        } else {
            let dt_vec = hp.grid.dt_hours_vec();
            let stage1_pins = crate::optimize::unified::round_dispatch_legs(
                &new_relaxed,
                &battery_spec0,
                floor,
                &dt_vec,
                &[],
                false,
            );
            let fixed1 = crate::optimize::unified::FixedBinaries {
                batt_to_grid_on: stage1_pins.batt_to_grid_on,
                grid_charge_on: stage1_pins.grid_charge_on,
                batt_to_load_cap: stage1_pins.batt_to_load_cap,
                solar_to_batt_cap: stage1_pins.solar_to_batt_cap,
                ..Default::default()
            };
            let stage1_t0 = std::time::Instant::now();
            let stage1 = solve_new(Some(&fixed1));
            let stage1_ms = stage1_t0.elapsed().as_secs_f64() * 1000.0;
            new_totals.pinned_solve_n += 1;
            new_totals.pinned_solve_ms_sum += stage1_ms;
            new_totals.pinned_solve_ms_max = new_totals.pinned_solve_ms_max.max(stage1_ms);
            match stage1 {
                Ok(p) => {
                    new_totals.stage1_pinned_resolves += 1;
                    p
                }
                Err(_) => {
                    let stage2_pins = crate::optimize::unified::round_dispatch_legs(
                        &new_relaxed,
                        &battery_spec0,
                        floor,
                        &dt_vec,
                        &[],
                        true,
                    );
                    if stage2_pins.guard_freed == 0 {
                        new_totals.relaxed_fallbacks += 1;
                        new_relaxed
                    } else {
                        new_totals.stage2_retries += 1;
                        new_totals.stage2_guard_freed += stage2_pins.guard_freed;
                        let fixed2 = crate::optimize::unified::FixedBinaries {
                            batt_to_grid_on: stage2_pins.batt_to_grid_on,
                            grid_charge_on: stage2_pins.grid_charge_on,
                            batt_to_load_cap: stage2_pins.batt_to_load_cap,
                            solar_to_batt_cap: stage2_pins.solar_to_batt_cap,
                            ..Default::default()
                        };
                        let stage2_t0 = std::time::Instant::now();
                        let stage2 = solve_new(Some(&fixed2));
                        let stage2_ms = stage2_t0.elapsed().as_secs_f64() * 1000.0;
                        new_totals.pinned_solve_n += 1;
                        new_totals.pinned_solve_ms_sum += stage2_ms;
                        new_totals.pinned_solve_ms_max =
                            new_totals.pinned_solve_ms_max.max(stage2_ms);
                        match stage2 {
                            Ok(p) => {
                                new_totals.stage2_pinned_resolves += 1;
                                p
                            }
                            Err(_) => {
                                new_totals.relaxed_fallbacks += 1;
                                new_relaxed
                            }
                        }
                    }
                }
            }
        };

        let sub_floor_legs = |p: &UnifiedPlan| -> usize {
            p.batt_to_grid_kw
                .iter()
                .chain(&p.batt_grid_charge_kw)
                .filter(|&&v| v > 1e-9 && v < floor)
                .count()
        };
        let old_sub_floor = sub_floor_legs(&old_plan);
        let new_sub_floor = sub_floor_legs(&new_plan);
        if old_sub_floor > 0 {
            old_totals.plans_with_sub_floor += 1;
        }
        if new_sub_floor > 0 {
            new_totals.plans_with_sub_floor += 1;
        }
        old_totals.final_sub_floor_legs += old_sub_floor;
        new_totals.final_sub_floor_legs += new_sub_floor;

        // The executor's physical rule needs each plan's own total EV draw per block (not carried
        // on `UnifiedPlan` as a flat vector — summed from the per-charger map here).
        let ev_total_of = |p: &UnifiedPlan| -> Vec<f64> {
            let n = p.charge_kw.len();
            (0..n)
                .map(|i| {
                    p.ev_charge_kw
                        .values()
                        .map(|v| v.get(i).copied().unwrap_or(0.0))
                        .sum()
                })
                .collect()
        };
        let old_ev_total = ev_total_of(&old_plan);
        let new_ev_total = ev_total_of(&new_plan);

        let t_plus_1h = t + Duration::hours(1);
        let old_exec = execute_first_hour_floor(
            &hp.grid,
            t_plus_1h,
            &hp.import_blocks,
            &hp.export_blocks,
            &old_plan.grid_import_kw,
            &old_plan.grid_export_kw,
            &old_plan.discharge_kw,
            &old_plan.charge_kw,
            &old_plan.batt_to_grid_kw,
            &old_plan.batt_grid_charge_kw,
            &old_plan.pv_kw,
            &old_plan.served_load_kw,
            &old_ev_total,
            &old_plan.soc_kwh,
            amortisation,
            old_soc,
            floor,
            battery_spec0.charge_efficiency,
            battery_spec0.discharge_efficiency,
            battery_spec0.min_soc_kwh,
            battery_spec0.max_soc_kwh,
        );
        let new_exec = execute_first_hour_floor(
            &hp.grid,
            t_plus_1h,
            &hp.import_blocks,
            &hp.export_blocks,
            &new_plan.grid_import_kw,
            &new_plan.grid_export_kw,
            &new_plan.discharge_kw,
            &new_plan.charge_kw,
            &new_plan.batt_to_grid_kw,
            &new_plan.batt_grid_charge_kw,
            &new_plan.pv_kw,
            &new_plan.served_load_kw,
            &new_ev_total,
            &new_plan.soc_kwh,
            amortisation,
            new_soc,
            floor,
            battery_spec0.charge_efficiency,
            battery_spec0.discharge_efficiency,
            battery_spec0.min_soc_kwh,
            battery_spec0.max_soc_kwh,
        );

        if demoted_sample.len() < 20 {
            let room = 20 - demoted_sample.len();
            demoted_sample.extend(old_exec.demoted.iter().copied().take(room));
        }

        let date = t.with_timezone(&offset(t)).date_naive();
        let day = day_rows.entry(date).or_default();
        add_floor_hour(&mut old_totals, &old_exec);
        add_floor_hour(&mut day.0, &old_exec);
        add_floor_hour(&mut new_totals, &new_exec);
        add_floor_hour(&mut day.1, &new_exec);

        old_soc = old_exec.end_soc_kwh;
        new_soc = new_exec.end_soc_kwh;
        hours_executed += 1;
    }

    println!(
        "\nOLD(floor=0) vs NEW(floor {floor:.2} kW) — {hours_executed} hourly plan(s) over \
         {window_start} .. {end}"
    );
    println!(
        "  {:<6}{:>12}{:>12}{:>10}{:>10}{:>11}{:>10}{:>9}{:>9}{:>9}",
        "arm",
        "realized€",
        "booked€",
        "imp kWh",
        "exp kWh",
        "disch kWh",
        "end SoC",
        "plans≥1",
        "demoted",
        "unreal€"
    );
    for (label, t) in [("OLD", &old_totals), ("NEW", &new_totals)] {
        println!(
            "  {:<6}{:>12.4}{:>12.4}{:>10.1}{:>10.1}{:>11.1}{:>10.2}{:>9}{:>9}{:>9.4}",
            label,
            t.realized_cost_eur,
            t.booked_cost_eur,
            t.import_kwh,
            t.export_kwh,
            t.discharge_kwh,
            t.end_soc_kwh,
            t.plans_with_sub_floor,
            t.demoted_blocks,
            t.booked_cost_eur - t.realized_cost_eur,
        );
    }
    println!(
        "  demoted kWh: OLD export {:.2} charge {:.2}  |  NEW export {:.2} charge {:.2}",
        old_totals.demoted_export_kwh,
        old_totals.demoted_charge_kwh,
        new_totals.demoted_export_kwh,
        new_totals.demoted_charge_kwh,
    );
    println!(
        "  NEW: stage-1 (un-guarded) resolves {}, stage-2 retries {} (guard-freed {} block(s) \
         total, {} resolved), relaxed fallbacks {}",
        new_totals.stage1_pinned_resolves,
        new_totals.stage2_retries,
        new_totals.stage2_guard_freed,
        new_totals.stage2_pinned_resolves,
        new_totals.relaxed_fallbacks,
    );
    println!(
        "  final-plan sub-floor legs (should be ≈ stage-2 guard-freed): OLD {}  |  NEW {}",
        old_totals.final_sub_floor_legs, new_totals.final_sub_floor_legs,
    );
    println!(
        "  NEW solve wall-clock (ms): relaxed mean {:.1} max {:.1} (n={})  |  pinned mean {:.1} max \
         {:.1} (n={})",
        safe_mean(new_totals.relaxed_solve_ms_sum, new_totals.relaxed_solve_n),
        new_totals.relaxed_solve_ms_max,
        new_totals.relaxed_solve_n,
        safe_mean(new_totals.pinned_solve_ms_sum, new_totals.pinned_solve_n),
        new_totals.pinned_solve_ms_max,
        new_totals.pinned_solve_n,
    );

    println!(
        "\nfirst {} demoted OLD block(s) (t, dt, leg, kW, price, slot):",
        demoted_sample.len()
    );
    for d in &demoted_sample {
        println!(
            "  {} dt={:.2}h leg={} kw={:.3} price={:.4}  slot=regular (demoted)",
            d.t, d.dt_hours, d.leg, d.kw, d.price,
        );
    }

    println!(
        "\nper local day: realized cost OLD/NEW EUR, booked OLD/NEW EUR, demoted blocks OLD/NEW"
    );
    for (date, (o, n)) in &day_rows {
        println!(
            "  {date}: realized {:.4}/{:.4}  booked {:.4}/{:.4}  demoted {}/{}",
            o.realized_cost_eur,
            n.realized_cost_eur,
            o.booked_cost_eur,
            n.booked_cost_eur,
            o.demoted_blocks,
            n.demoted_blocks,
        );
    }
    Ok(())
}

// --- `--live`: compare the two valuations on the current on-demand plan --------------------------

async fn run_live(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
) -> Result<()> {
    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);
    let (cache, kernels) = shared_plan_cache(db, net, config, ss).await;
    let extras = |legacy: bool| PlanExtras {
        cache: Some(&cache),
        kernels: Some(kernels.clone()),
        legacy_terminal_value: legacy,
        ..Default::default()
    };

    let old = current_plan(db, net, ss, config, latitude, longitude, extras(true))
        .await
        .context("solving OLD (legacy terminal value) plan")?;
    let new = current_plan(db, net, ss, config, latitude, longitude, extras(false))
        .await
        .context("solving NEW (outlook terminal value) plan")?;

    println!(
        "backtest-terminal --live: OLD terminal_value {:.4} EUR/kWh (source {})  |  NEW \
         terminal_value {:.4} EUR/kWh (source {})",
        old.terminal_soc_value_eur_per_kwh,
        old.terminal_soc_value_source,
        new.terminal_soc_value_eur_per_kwh,
        new.terminal_soc_value_source,
    );
    println!(
        "total cost: OLD {:.3} EUR / {:.2} CZK  |  NEW {:.3} EUR / {:.2} CZK",
        old.total_cost_eur, old.total_cost_czk, new.total_cost_eur, new.total_cost_czk,
    );
    println!(
        "grid import/export kWh: OLD {:.2}/{:.2}  |  NEW {:.2}/{:.2}",
        old.grid_import_kwh, old.grid_export_kwh, new.grid_import_kwh, new.grid_export_kwh,
    );
    for (label, plan) in [("OLD", &old), ("NEW", &new)] {
        println!("\n{label} — last 12 timeline blocks (t, slot, charge/discharge kW, soc kWh):");
        let n = plan.timeline.len();
        for b in plan.timeline.iter().skip(n.saturating_sub(12)) {
            println!(
                "  {} slot={:<18} charge={:.2} discharge={:.2} soc={:.2}",
                b.t, b.slot, b.charge_kw, b.discharge_kw, b.soc_kwh,
            );
        }
    }
    Ok(())
}

// --- CLI entry point --------------------------------------------------------------------------

fn parse_args(args: &[String]) -> Result<(i64, u32, bool)> {
    let days: i64 = args
        .first()
        .context("usage: backtest-terminal <days> [--publish-hour H] [--live]")?
        .parse()
        .context("backtest-terminal: <days> must be an integer")?;
    ensure!(
        (1..=60).contains(&days),
        "backtest-terminal: days must be 1..=60"
    );
    let mut publish_hour = DEFAULT_PUBLISH_HOUR;
    let mut live = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--publish-hour" => {
                publish_hour = args
                    .get(i + 1)
                    .context("--publish-hour needs a value")?
                    .parse()
                    .context("--publish-hour: an integer 0..24")?;
                ensure!(publish_hour < 24, "--publish-hour must be 0..24");
                i += 2;
            }
            "--live" => {
                live = true;
                i += 1;
            }
            other => bail!("backtest-terminal: unknown argument {other}"),
        }
    }
    Ok((days, publish_hour, live))
}

pub async fn run(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    args: &[String],
) -> Result<()> {
    let (days, publish_hour, live) = parse_args(args)?;
    if live {
        run_live(db, config, net, ss).await
    } else {
        run_window(db, config, days, publish_hour).await
    }
}

/// `backtest-dispatch-floor <days> [--publish-hour H]` argument parsing — `days` capped at 14 (vs
/// `backtest-terminal`'s 60): each hour now runs up to 3 LP solves (OLD relaxed, NEW relaxed, NEW
/// pinned) instead of 2, and an UNBOUNDED multi-week run is exactly what COMMON.md's Influx-query
/// bound rule exists to prevent on the live server.
fn parse_floor_args(args: &[String]) -> Result<(i64, u32)> {
    let days: i64 = args
        .first()
        .context("usage: backtest-dispatch-floor <days> [--publish-hour H]")?
        .parse()
        .context("backtest-dispatch-floor: <days> must be an integer")?;
    ensure!(
        (1..=14).contains(&days),
        "backtest-dispatch-floor: days must be 1..=14"
    );
    let mut publish_hour = DEFAULT_PUBLISH_HOUR;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--publish-hour" => {
                publish_hour = args
                    .get(i + 1)
                    .context("--publish-hour needs a value")?
                    .parse()
                    .context("--publish-hour: an integer 0..24")?;
                ensure!(publish_hour < 24, "--publish-hour must be 0..24");
                i += 2;
            }
            other => bail!("backtest-dispatch-floor: unknown argument {other}"),
        }
    }
    Ok((days, publish_hour))
}

/// `backtest-dispatch-floor <days> [--publish-hour H]` entry point (read-only, bounded reads).
pub async fn run_floor(db: &SourceClients, config: &ControlConfig, args: &[String]) -> Result<()> {
    let (days, publish_hour) = parse_floor_args(args)?;
    run_floor_window(db, config, days, publish_hour).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn known_until_rolls_over_at_the_publish_hour() {
        let plus1 = FixedOffset::east_opt(3600).unwrap();
        // 13:00 local (before the 14:00 publish hour): only today's own date is known.
        let before = utc(2026, 9, 30, 12, 0).with_timezone(&plus1);
        assert_eq!(
            known_until_local_date(before, 14),
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
        // 14:00 local (at the publish hour): tomorrow is known too.
        let after = utc(2026, 9, 30, 13, 0).with_timezone(&plus1);
        assert_eq!(
            known_until_local_date(after, 14),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
    }

    #[test]
    fn is_real_as_of_matches_the_known_until_cutoff() {
        let known = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        assert!(is_real_as_of(
            NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(),
            known
        ));
        assert!(is_real_as_of(known, known));
        assert!(!is_real_as_of(
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            known
        ));
    }

    #[test]
    fn classify_local_hour_flags_the_three_windows() {
        let b5 = classify_local_hour(5);
        assert!(b5.is_0500 && b5.is_morning_import_window && !b5.is_evening_export_window);
        let b8 = classify_local_hour(8);
        assert!(!b8.is_0500 && b8.is_morning_import_window);
        let b9 = classify_local_hour(9);
        assert!(
            !b9.is_morning_import_window,
            "09:00 is the exclusive upper bound"
        );
        let b17 = classify_local_hour(17);
        assert!(b17.is_evening_export_window && !b17.is_morning_import_window);
        let b21 = classify_local_hour(21);
        assert!(
            !b21.is_evening_export_window,
            "21:00 is the exclusive upper bound"
        );
        let b12 = classify_local_hour(12);
        assert!(!b12.is_0500 && !b12.is_evening_export_window && !b12.is_morning_import_window);
    }

    #[test]
    fn execute_first_hour_sums_only_blocks_before_the_cutoff() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 8, 900.0); // 8 x 15-min = 2 h
        let import_price = vec![0.10; 8];
        let export_price = vec![0.05; 8];
        let grid_import_kw = vec![4.0; 8];
        let grid_export_kw = vec![0.0; 8];
        let discharge_kw = vec![0.0; 8];
        let soc_kwh = vec![1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7];
        let t_plus_1h = start + Duration::hours(1); // only the first 4 blocks execute
        let exec = execute_first_hour(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &soc_kwh,
            0.0,
            0.5,
        );
        // 4 blocks * 0.25 h * 4 kW = 4 kWh imported, cost = 4 kWh * 0.10 EUR/kWh.
        assert!((exec.import_kwh - 4.0).abs() < 1e-9);
        assert!((exec.cost_eur - 0.40).abs() < 1e-9);
        assert!(
            (exec.end_soc_kwh - 1.3).abs() < 1e-9,
            "soc at the 4th executed block"
        );
    }

    #[test]
    fn execute_first_hour_keeps_the_initial_soc_when_nothing_executes() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 4, 900.0);
        let v = vec![0.0; 4];
        let exec = execute_first_hour(&grid, start, &v, &v, &v, &v, &v, &v, 0.0, 2.5);
        assert_eq!(exec.end_soc_kwh, 2.5);
        assert_eq!(exec.import_kwh, 0.0);
    }

    /// A sub-floor battery→grid export (`0 < b < floor`) is demoted: no export revenue, no wear,
    /// `discharge_kwh`/`export_kwh` drop by the leg, and the energy stays in the battery (SoC ends
    /// back where it started — zeroing the export exactly undoes its own planned SoC draw).
    #[test]
    fn execute_first_hour_floor_demotes_sub_floor_export() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 1, 900.0); // one 15-min block
        let t_plus_1h = start + Duration::hours(1);
        let import_price = vec![0.30];
        let export_price = vec![0.25];
        let grid_import_kw = vec![0.0];
        let grid_export_kw = vec![1.0];
        let discharge_kw = vec![1.0];
        let batt_to_grid_kw = vec![1.0]; // below the 2.0 kW floor
        let batt_grid_charge_kw = vec![0.0];
        let charge_kw = vec![0.0];
        let pv_kw = vec![0.0];
        let served_load_kw = vec![0.0]; // deficit 0: the whole discharge is fiction
        let ev_total_kw = vec![0.0];
        let eta_d = 0.95;
        let initial_soc = 5.0;
        let soc_kwh = vec![initial_soc - 1.0 * 0.25 / eta_d];

        let exec = execute_first_hour_floor(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &charge_kw,
            &batt_to_grid_kw,
            &batt_grid_charge_kw,
            &pv_kw,
            &served_load_kw,
            &ev_total_kw,
            &soc_kwh,
            0.02,
            initial_soc,
            2.0,
            0.95,
            eta_d,
            0.0,
            10.0,
        );
        assert_eq!(exec.demoted_blocks, 1);
        assert!((exec.demoted_export_kwh - 0.25).abs() < 1e-9);
        assert_eq!(exec.export_kwh, 0.0, "the sub-floor export is not realized");
        assert_eq!(exec.discharge_kwh, 0.0);
        assert!(
            (exec.realized_cost_eur - 0.0).abs() < 1e-9,
            "no revenue, no wear: {}",
            exec.realized_cost_eur
        );
        assert!(
            (exec.booked_cost_eur - (-0.0575)).abs() < 1e-9,
            "booked (as-planned) cost unaffected by demotion: {}",
            exec.booked_cost_eur
        );
        assert!(
            (exec.end_soc_kwh - initial_soc).abs() < 1e-9,
            "the retained energy exactly undoes the planned draw: {}",
            exec.end_soc_kwh
        );
    }

    /// A sub-floor grid→battery charge (`0 < g < floor`) is demoted: the import cost is not paid,
    /// `import_kwh` drops by the leg, and the SoC ends BELOW the plan (the charge never happened).
    #[test]
    fn execute_first_hour_floor_demotes_sub_floor_grid_charge() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 1, 900.0);
        let t_plus_1h = start + Duration::hours(1);
        let import_price = vec![0.30];
        let export_price = vec![0.25];
        let grid_import_kw = vec![1.0];
        let grid_export_kw = vec![0.0];
        let discharge_kw = vec![0.0];
        let batt_to_grid_kw = vec![0.0];
        let batt_grid_charge_kw = vec![1.0]; // below the 2.0 kW floor
        let charge_kw = vec![1.0]; // no solar_to_batt component: total charge == grid_charge
        let pv_kw = vec![0.0];
        let served_load_kw = vec![0.0]; // surplus 0: the whole charge is fiction
        let ev_total_kw = vec![0.0];
        let eta_c = 0.95;
        let initial_soc = 5.0;
        let soc_kwh = vec![initial_soc + 1.0 * 0.25 * eta_c];

        let exec = execute_first_hour_floor(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &charge_kw,
            &batt_to_grid_kw,
            &batt_grid_charge_kw,
            &pv_kw,
            &served_load_kw,
            &ev_total_kw,
            &soc_kwh,
            0.0,
            initial_soc,
            2.0,
            eta_c,
            0.95,
            0.0,
            10.0,
        );
        assert_eq!(exec.demoted_blocks, 1);
        assert!((exec.demoted_charge_kwh - 0.25).abs() < 1e-9);
        assert_eq!(exec.import_kwh, 0.0, "the sub-floor charge is not realized");
        assert!(
            (exec.realized_cost_eur - 0.0).abs() < 1e-9,
            "import cost not paid"
        );
        assert!(
            (exec.end_soc_kwh - initial_soc).abs() < 1e-9,
            "the charge never happened: SoC ends back at its initial value"
        );
    }

    /// A leg AT OR ABOVE the floor is executed exactly as planned — `execute_first_hour_floor`
    /// reproduces [`execute_first_hour`]'s own accounting bit-for-bit (no demotion at all).
    #[test]
    fn execute_first_hour_floor_at_or_above_floor_matches_as_planned() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 1, 900.0);
        let t_plus_1h = start + Duration::hours(1);
        let import_price = vec![0.30];
        let export_price = vec![0.25];
        let grid_import_kw = vec![0.0];
        let grid_export_kw = vec![3.0];
        let discharge_kw = vec![3.0];
        let batt_to_grid_kw = vec![3.0]; // at/above the 2.0 kW floor
        let batt_grid_charge_kw = vec![0.0];
        let charge_kw = vec![0.0];
        let pv_kw = vec![0.0];
        let served_load_kw = vec![0.0];
        let ev_total_kw = vec![0.0];
        let initial_soc = 5.0;
        let soc_kwh = vec![initial_soc - 3.0 * 0.25 / 0.95];

        let plain = execute_first_hour(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &soc_kwh,
            0.02,
            initial_soc,
        );
        let floor_exec = execute_first_hour_floor(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &charge_kw,
            &batt_to_grid_kw,
            &batt_grid_charge_kw,
            &pv_kw,
            &served_load_kw,
            &ev_total_kw,
            &soc_kwh,
            0.02,
            initial_soc,
            2.0,
            0.95,
            0.95,
            0.0,
            10.0,
        );
        assert_eq!(floor_exec.demoted_blocks, 0);
        assert!((floor_exec.realized_cost_eur - plain.cost_eur).abs() < 1e-9);
        assert!((floor_exec.booked_cost_eur - plain.cost_eur).abs() < 1e-9);
        assert_eq!(floor_exec.export_kwh, plain.export_kwh);
        assert_eq!(floor_exec.discharge_kwh, plain.discharge_kwh);
        assert!((floor_exec.end_soc_kwh - plain.end_soc_kwh).abs() < 1e-9);
    }

    /// The routing loophole: `batt_to_grid` pinned to `0` doesn't mean nothing happened — a
    /// load-first inverter still can't discharge MORE than the house's real deficit, so any
    /// discharge beyond that (laundered through `batt_to_load` while extra solar exports instead)
    /// is demoted too, even though the leg itself was already `0` (PV 3 kW / load 1 kW / floor 2 kW:
    /// a relaxed `batt_to_grid` of 0.35 kW re-routed into `batt_to_load`). Discharge
    /// exactly AT the deficit is real load-serving and is NOT demoted.
    #[test]
    fn execute_first_hour_floor_demotes_discharge_beyond_the_real_deficit() {
        let start = utc(2026, 9, 30, 10, 0);
        let grid = BlockGrid::uniform(start, 1, 900.0);
        let t_plus_1h = start + Duration::hours(1);
        let import_price = vec![0.30];
        let export_price = vec![0.25];
        let grid_import_kw = vec![0.0];
        let charge_kw = vec![0.0];
        let batt_to_grid_kw = vec![0.0]; // no commanded export at all
        let batt_grid_charge_kw = vec![0.0];
        let ev_total_kw = vec![0.0];
        let eta_d = 0.95;
        let initial_soc = 5.0;

        // Laundered: PV(3) already covers load(1) in full (deficit 0), yet the plan discharges
        // 0.35 kW anyway, routed through `batt_to_load` while 0.35 kW of extra solar exports in
        // its place — `grid_export_kw` is unchanged by the re-routing (2.0 real surplus + 0.35
        // laundered).
        let pv_kw = vec![3.0];
        let served_load_kw = vec![1.0];
        let discharge_kw = vec![0.35];
        let grid_export_kw = vec![2.35];
        let soc_kwh = vec![initial_soc - 0.35 * 0.25 / eta_d];
        let exec = execute_first_hour_floor(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_kw,
            &discharge_kw,
            &charge_kw,
            &batt_to_grid_kw,
            &batt_grid_charge_kw,
            &pv_kw,
            &served_load_kw,
            &ev_total_kw,
            &soc_kwh,
            0.0,
            initial_soc,
            2.0,
            0.95,
            eta_d,
            0.0,
            10.0,
        );
        assert_eq!(
            exec.demoted_blocks, 1,
            "discharge beyond the real deficit is demoted"
        );
        assert!((exec.demoted_export_kwh - 0.0875).abs() < 1e-9); // 0.35 kW * 0.25 h
        assert!(
            (exec.export_kwh - 0.5).abs() < 1e-9,
            "only the real 2.0 kW solar surplus realizes: {}",
            exec.export_kwh
        );
        assert_eq!(
            exec.discharge_kwh, 0.0,
            "the laundered discharge never happens either"
        );

        // Discharge EXACTLY at the deficit (no solar at all: load 1, pv 0) is real load-serving.
        let pv_at_deficit = vec![0.0];
        let served_load_at_deficit = vec![1.0];
        let discharge_at_deficit = vec![1.0];
        let grid_export_none = vec![0.0];
        let soc_at_deficit = vec![initial_soc - 1.0 * 0.25 / eta_d];
        let exec2 = execute_first_hour_floor(
            &grid,
            t_plus_1h,
            &import_price,
            &export_price,
            &grid_import_kw,
            &grid_export_none,
            &discharge_at_deficit,
            &charge_kw,
            &batt_to_grid_kw,
            &batt_grid_charge_kw,
            &pv_at_deficit,
            &served_load_at_deficit,
            &ev_total_kw,
            &soc_at_deficit,
            0.0,
            initial_soc,
            2.0,
            0.95,
            eta_d,
            0.0,
            10.0,
        );
        assert_eq!(
            exec2.demoted_blocks, 0,
            "discharge exactly at the deficit is real load-serving, not fiction"
        );
        assert!((exec2.discharge_kwh - 0.25).abs() < 1e-9); // 1.0 kW * 0.25 h, realized in full
    }

    #[test]
    fn forward_fill_counts_gaps_and_holds_the_last_value() {
        let (v, n) = forward_fill(vec![None, Some(1.0), None, None, Some(4.0)]);
        assert_eq!(v, vec![1.0, 1.0, 1.0, 1.0, 4.0]);
        assert_eq!(n, 3);
    }

    #[test]
    fn chunk_windows_splits_into_at_most_seven_day_spans() {
        let start = utc(2026, 1, 1, 0, 0);
        let stop = start + Duration::days(16);
        let chunks = chunk_windows(start, stop);
        assert_eq!(chunks.len(), 3); // 7 + 7 + 2
        for (s, e) in &chunks {
            assert!((*e - *s) <= Duration::days(7));
        }
        assert_eq!(chunks[0].0, start);
        assert_eq!(chunks.last().unwrap().1, stop);
    }

    /// Acceptance (a): T before the publish hour — only today's own blocks are real; tomorrow's
    /// `current` is None even though our (hindsight) dataset already has tomorrow's actual price.
    #[test]
    fn known_at_t_price_inputs_before_publish_hour_only_today_is_real() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0); // day D midnight
        let t = array_start; // T = D 00:00, before the 14:00 publish hour
        let mut real_spot_fine = vec![Some(0.10); 96]; // day D
        real_spot_fine.extend(vec![Some(0.20); 96]); // day D+1 (unpublished as of T)
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let history = Vec::new();
        let (current, _estimated, _day_ago) = known_at_t_price_inputs(
            known_until,
            &history,
            array_start,
            &real_spot_fine,
            t,
            96 * 2,
            offset,
            &[],
            false,
        );
        assert!(current[0].is_some(), "today's own first block must be real");
        assert!(current[95].is_some(), "today's own last block must be real");
        assert!(
            current[96].is_none(),
            "tomorrow must NOT be real before the publish hour, even though the dataset has it"
        );
        assert!(current[191].is_none());
    }

    /// Acceptance (b): T at/after the publish hour — tomorrow is now real too.
    #[test]
    fn known_at_t_price_inputs_at_publish_hour_tomorrow_is_real() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0);
        let t = utc(2026, 9, 28, 14, 0); // T = D 14:00, AT the publish hour
        let mut real_spot_fine = vec![Some(0.10); 96];
        real_spot_fine.extend(vec![Some(0.20); 96]);
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let history = Vec::new();
        // A few fine steps past T is enough to cross the D -> D+1 midnight boundary.
        let idx_midnight = 96 - 56; // t is 14:00 (fine step 56 of the day); midnight is 40 steps on
        let (current, _estimated, _day_ago) = known_at_t_price_inputs(
            known_until,
            &history,
            array_start,
            &real_spot_fine,
            t,
            idx_midnight + 4,
            offset,
            &[],
            false,
        );
        assert!(
            current[idx_midnight].is_some(),
            "tomorrow must be real at/after the publish hour"
        );
    }

    /// Acceptance (c): a D+2 block's `day_ago` (the D+1 clock-twin) is None when D+1 is
    /// unpublished as of T, Some when it's published — regardless of the dataset having D+1's
    /// actual value either way (hindsight must never leak ahead of what T actually knew).
    #[test]
    fn known_at_t_price_inputs_day_ago_for_a_d_plus_2_block_respects_the_knowledge_cutoff() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0); // day D midnight
        let t = array_start; // T = D 00:00
        let mut real_spot_fine = vec![Some(0.10); 96]; // D
        real_spot_fine.extend(vec![Some(0.20); 96]); // D+1
        real_spot_fine.extend(vec![Some(0.30); 96]); // D+2
        let history = Vec::new();
        let n_fine = 96 * 2 + 1; // reach the D+2 00:00 block
        let d_plus_2_idx = 192;

        // D+1 UNPUBLISHED as of T (only D itself is known): day_ago must be None.
        let known_until_unpublished = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let (_current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until_unpublished,
            &history,
            array_start,
            &real_spot_fine,
            t,
            n_fine,
            offset,
            &[],
            false,
        );
        assert_eq!(
            day_ago[d_plus_2_idx], None,
            "D+1 isn't known at T yet; day_ago must not leak its real price"
        );

        // D+1 PUBLISHED as of T: day_ago may use it.
        let known_until_published = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(); // D+1
        let (_current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until_published,
            &history,
            array_start,
            &real_spot_fine,
            t,
            n_fine,
            offset,
            &[],
            false,
        );
        assert_eq!(day_ago[d_plus_2_idx], Some(0.20));
    }

    /// The `day_ago` gate classifies by LOCAL date: at UTC+2 a block at D+1 22:30 UTC is D+2 00:30
    /// local, so its day-ago (D+1 00:30 local) is unpublished when only D is known — a UTC-date
    /// check would have leaked it.
    #[test]
    fn known_at_t_price_inputs_day_ago_gate_uses_the_local_date() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(2 * 3600).unwrap();
        let array_start = utc(2026, 9, 27, 22, 0); // D 00:00 local
        let t = array_start;
        let mut real_spot_fine = vec![Some(0.10); 96]; // D
        real_spot_fine.extend(vec![Some(0.20); 96]); // D+1
        real_spot_fine.extend(vec![Some(0.30); 96]); // D+2
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let (current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until,
            &[],
            array_start,
            &real_spot_fine,
            t,
            96 * 2 + 2,
            offset,
            &[],
            false,
        );
        let d_plus_2_0030_local = 96 * 2 + 1; // 2026-09-29 22:15 UTC
        assert_eq!(current[95], Some(0.10), "D 23:45 local is published");
        assert_eq!(current[96], None, "D+1 00:00 local is not");
        assert_eq!(day_ago[d_plus_2_0030_local], None);
    }

    /// Acceptance 2: the paired metric only credits the SMALLER of what a day sold in the evening
    /// and what the NEXT morning imported; the last day pairs with nothing.
    #[test]
    fn paired_sold_then_reimported_pairs_each_day_with_the_next_mornings_import() {
        let evening = vec![5.0, 2.0];
        let morning = vec![1.0, 3.0]; // day 0's own morning import doesn't pair with day 0
        let (per_day, total) = paired_sold_then_reimported(&evening, &morning);
        assert_eq!(per_day, vec![3.0, 0.0]);
        assert_eq!(total, 3.0);
    }
}
