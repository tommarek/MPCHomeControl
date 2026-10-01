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
use crate::optimize::unified::{optimize_unified, FlowParams, SolveBudget, UnifiedPlan};
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
fn forward_fill(v: Vec<Option<f64>>) -> (Vec<f64>, usize) {
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

fn floor_to_hour(t: DateTime<Utc>) -> DateTime<Utc> {
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

// --- IO: bounded reads ----------------------------------------------------------------------------

async fn read_prices_chunked(
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

async fn read_growatt_chunked(
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

async fn read_soc_seed(
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
        None,
        SolveBudget::default(),
    )
    .context("terminal backtest: battery-only LP solve failed")
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
        let grid = BlockGrid::multi_rate(
            t,
            config.horizon.hours,
            config.horizon.fine_hours,
            FINE_SECONDS,
        );
        let n_fine = grid.n_fine();

        // Built ONCE per hour, shared by the fallback chain below AND the NEW arm's post-horizon
        // curve (finding 2, rework cycle 1) — see `history_known_at`'s doc.
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), publish_hour);
        let history = history_known_at(array_start, &real_spot_fine, known_until, offset);

        let (current, estimated, day_ago) = known_at_t_price_inputs(
            known_until,
            &history,
            array_start,
            &real_spot_fine,
            t,
            n_fine,
            offset,
            &public_holidays,
            config.site.easter_holidays,
        );
        let placeholder = placeholder_price_curve(t, offset(t), n_fine);
        let (spot_fine, mask_fine, _missing, _persisted, _estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        let (import_fine, export_fine) = tariff_prices(&config.tariff, &config.site, &spot_fine, t);
        let export_allowed_fine: Vec<bool> = spot_fine.iter().map(|&s| s >= export_floor).collect();
        let inverter_on_fine: Vec<bool> =
            spot_fine.iter().map(|&s| s >= inverter_off_price).collect();

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

        // OLD: in-horizon median on the FINE import/mask arrays — mirrors `app::current_plan`'s own
        // `terminal_basis` rule exactly (computed before any grid aggregation there too).
        let real_import_fine: Vec<f64> = import_fine
            .iter()
            .zip(&mask_fine)
            .filter(|(_, &m)| !m)
            .map(|(&p, _)| p)
            .collect();
        let old_basis: &[f64] = if real_import_fine.len() >= 16 {
            &real_import_fine
        } else {
            &import_fine
        };
        let old_terminal_value = terminal_soc_value(old_basis, amortisation, round_trip_eta);

        // NEW: the post-horizon 24 h curve from the TRUE grid end, the SAME `history` known at `t`
        // built once above.
        let outlook_start = grid.block_end(grid.len() - 1);
        let post_curve = day_type_median_curve(
            &history,
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

        let minutes: Vec<u32> = (0..grid.len())
            .map(|b| {
                let at = grid.block_start(b);
                let local = at.with_timezone(&offset(at));
                local.hour() * 60 + local.minute()
            })
            .collect();
        let outdoor = vec![15.0; grid.len()];

        let old_plan = solve_arm(
            &battery_spec0,
            &heating,
            &grid,
            &import_blocks,
            &export_blocks,
            &pv_blocks,
            &load_blocks,
            &export_allowed_blocks,
            &inverter_on_blocks,
            &placeholder_blocks,
            amortisation,
            old_terminal_value,
            config,
            old_soc,
            min_final_soc,
            &outdoor,
            &minutes,
        )
        .with_context(|| format!("OLD arm at {t}"))?;
        let new_plan = solve_arm(
            &battery_spec0,
            &heating,
            &grid,
            &import_blocks,
            &export_blocks,
            &pv_blocks,
            &load_blocks,
            &export_allowed_blocks,
            &inverter_on_blocks,
            &placeholder_blocks,
            amortisation,
            new_terminal_value,
            config,
            new_soc,
            min_final_soc,
            &outdoor,
            &minutes,
        )
        .with_context(|| format!("NEW arm at {t}"))?;

        let t_plus_1h = t + Duration::hours(1);
        let old_exec = execute_first_hour(
            &grid,
            t_plus_1h,
            &import_blocks,
            &export_blocks,
            &old_plan.grid_import_kw,
            &old_plan.grid_export_kw,
            &old_plan.discharge_kw,
            &old_plan.soc_kwh,
            amortisation,
            old_soc,
        );
        let new_exec = execute_first_hour(
            &grid,
            t_plus_1h,
            &import_blocks,
            &export_blocks,
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
