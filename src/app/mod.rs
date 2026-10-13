//! Application-level assembly of the live whole-house state and plan.
//!
//! One source of truth shared by the CLI demos and the web API: it pulls the live data (estimated
//! thermal state, the self-corrected Solcast PV forecast, prices) and runs the unified optimizer,
//! returning serializable reports. The data layer (InfluxDB) and the models are passed in.
//!
//! This module holds the entry points ([`current_state`], [`current_plan`], [`PlanExtras`]); the
//! pieces they assemble live in submodules, re-exported here: `report` (the output types), `solve`
//! (fix-and-round and the bounded solve), `cache` ([`PlanCache`]), `pricing` (tariff + price fill),
//! `pv` (arrays, nowcast, p10), `terminal` (terminal SoC value), `mode` (`classify_mode`) and
//! `loads` (scheduled-load run hours).

mod cache;
mod loads;
mod mode;
mod pricing;
mod pv;
mod report;
mod solve;
mod terminal;

pub use cache::{build_cache, fit_live_internal_gains, PlanCache};
pub(crate) use cache::{CALIBRATION_MIN_BAND_HOURS, CALIBRATION_MIN_SCORED_HOURS};
pub use loads::measured_run_hours;
pub use mode::ModeStep;
use mode::{classify_mode, BlockFlows};
use pricing::price_fallback_label;
pub(crate) use pricing::{fill_block_prices, placeholder_price_curve, tariff_prices};
use pv::{apply_pv_nowcast, clearsky_pv_kw, next_solar_day_mask, p10_curtailment};
pub use pv::{default_pv_array, pv_arrays, PvNowcast};
pub(crate) use report::pull_break_even_eur_per_kwh;
pub use report::{
    EvChargerPlan, FirstStep, GainsSnapshot, PlanReport, ScheduledFit, StateReport, TimelineBlock,
    TimestampedPlan, ZoneSeries, ZoneTemp,
};
pub use solve::build_kernel_cache;
#[cfg(test)]
pub(crate) use solve::fix_and_round_inner;
pub(crate) use solve::{fix_and_round, on_demand_solver_idle, run_solve, SolveGrade, SolveJob};
use solve::{
    solve_bounded, FALLBACK_PER_LP_HIGHS_TIME_LIMIT_S, FALLBACK_SOLVE_TIMEOUT,
    PER_LP_HIGHS_TIME_LIMIT_S, STRICT_SOLVE_TIMEOUT,
};
pub(crate) use terminal::{select_terminal_value, terminal_soc_value};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{ensure, Result};
use chrono::{DateTime, Duration, Timelike, Utc};
use nalgebra::DVector;
use uom::si::f64::Angle;

use crate::estimate::estimate_initial_state;
use crate::forecast::calibration::{Calibration, PvBandCalibration};
use crate::forecast::consumption::ConsumptionModel;
use crate::live_inputs::{
    battery_soc_kwh, block_prices, train_consumption, weather_forecast, BlockPrices,
    WeatherForecast,
};
use crate::optimize::battery::BatterySpec;
use crate::optimize::config::{BatteryConfig, ControlConfig};
use crate::optimize::coordinator::{ForecastContext, Outlook};
use crate::optimize::thermal::KernelSet;
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::solar_forecast::pv_forecast_kw;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::sun::SolarInput;
use crate::tools::{c_to_k, k_to_c};

/// Planning horizon in hours (the span the weather/PV/consumption feeds are read over).
/// 36 h: after the ~14:00 OTE auction the market publishes ~34 h of REAL prices, and the most
/// valuable daily decision — how much SoC and slab heat to carry through the evening into
/// tomorrow's morning peak — is exactly what a 24 h horizon truncated. Open-meteo covers 48 h;
/// PV day+2 (plan starts after ~12:00 local) falls back to the flagged clear-sky splice; the
/// pre-auction placeholder tail is defused by the arbitrage ban (price_is_placeholder).
/// REVERT TO 30 if the live strict solve routinely exceeds ~15 s (watch the [mpc] tick logs).
pub(crate) const HORIZON_HOURS: usize = 36;
/// Dispatch/mode resolution: 15-minute blocks, matching the OTE day-ahead price grid.
pub(crate) const BLOCKS_PER_HOUR: usize = 4;
const HORIZON_BLOCKS: usize = HORIZON_HOURS * BLOCKS_PER_HOUR;
pub(crate) const BLOCK_SECONDS: f64 = 900.0;

/// Floor `now` to the current 15-minute block boundary — the SAME alignment [`current_plan`] uses
/// for its own `start`/block 0, exposed so `mpc_loop`'s pre-adoption (adopt `committed_next` into
/// `committed` BEFORE calling `current_plan`, so the first post-mark LP is pinned from the start)
/// can compute the anticipated new block without duplicating — and risking
/// drifting from — this formula.
pub fn block_align(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_minute((now.minute() / 15) * 15)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now)
}

/// Map each 15-minute block to the hourly value of the **calendar hour containing the block's
/// midpoint**. Hourly feeds (weather, PV) are keyed to calendar hours, but a plan can start
/// mid-hour (3 ticks out of 4) — naively repeating `hourly[h]` from the start would put hour
/// boundaries at e.g. :45, so the 15:00/15:15/15:30 blocks of a 14:45 plan would carry the
/// 14:00–15:00 value (up to 45 min of skew). Indexing by the midpoint's calendar hour keeps every
/// block on the value of the hour it actually lies in. The last hourly value covers any tail.
pub(crate) fn hourly_to_blocks(start: DateTime<Utc>, hourly: &[f64]) -> Vec<f64> {
    let start_hour = start.timestamp().div_euclid(3600);
    (0..hourly.len() * BLOCKS_PER_HOUR)
        .map(|b| {
            let midpoint =
                start.timestamp() + b as i64 * BLOCK_SECONDS as i64 + BLOCK_SECONDS as i64 / 2;
            let idx = (midpoint.div_euclid(3600) - start_hour).max(0) as usize;
            hourly[idx.min(hourly.len() - 1)]
        })
        .collect()
}

/// [`hourly_to_blocks`] for the per-hour [`SolarInput`] chain — same calendar-hour-midpoint
/// alignment (the inputs are hourly forecast values; each block takes its own hour's).
pub(crate) fn hourly_solar_to_blocks(
    start: DateTime<Utc>,
    hourly: &[SolarInput],
) -> Vec<SolarInput> {
    let start_hour = start.timestamp().div_euclid(3600);
    (0..hourly.len() * BLOCKS_PER_HOUR)
        .map(|b| {
            let midpoint =
                start.timestamp() + b as i64 * BLOCK_SECONDS as i64 + BLOCK_SECONDS as i64 / 2;
            let idx = (midpoint.div_euclid(3600) - start_hour).max(0) as usize;
            hourly[idx.min(hourly.len() - 1)]
        })
        .collect()
}

/// Build the post-horizon [`Outlook`] from a raw [`WeatherForecast`] read PAST the horizon,
/// truncated to only the hours the forecast ACTUALLY covers (`WeatherForecast::covered_hours`) —
/// never forward-filling a flat guess over days the stored forecast doesn't reach (a short-lived
/// weather scraper window must shrink the outlook, not silently invent a multi-day plateau).
/// `covered_hours` COUNTS real samples rather than locating the last one, but the scraper stores a
/// contiguous future window (a real sample is never followed by a gap then another real sample),
/// so the count equals the true covered prefix length in practice. `None` when nothing at all is
/// covered — same as no outlook.
fn outlook_from_weather(outlook_start: DateTime<Utc>, owf: &WeatherForecast) -> Option<Outlook> {
    let covered = owf.covered_hours.min(owf.temperature_c.len());
    (covered > 0).then(|| Outlook {
        temperature_c: hourly_to_blocks(outlook_start, &owf.temperature_c[..covered]),
        cloud_cover: hourly_to_blocks(outlook_start, &owf.cloud_cover[..covered]),
        solar: hourly_solar_to_blocks(outlook_start, &owf.solar[..covered]),
    })
}

/// A small home battery spec for the offline demos (real runs build the spec from `config.battery`).
pub fn default_battery_spec() -> BatterySpec {
    BatterySpec {
        max_charge_kw: 3.0,
        max_discharge_kw: 3.0,
        charge_efficiency: 0.95,
        discharge_efficiency: 0.95,
        min_soc_kwh: 0.5,
        max_soc_kwh: 10.0,
        initial_soc_kwh: 3.0,
    }
}

/// Build the battery spec from configuration (capacity, SoC floor, charge/discharge power,
/// round-trip efficiency split evenly across the two directions).
pub fn battery_spec(cfg: &BatteryConfig) -> BatterySpec {
    let one_way_efficiency = cfg.round_trip_efficiency.clamp(1e-3, 1.0).sqrt();
    let max_soc_kwh = cfg.capacity_kwh.max(0.0);
    let min_soc_kwh = (cfg.min_soc_pct / 100.0 * cfg.capacity_kwh).clamp(0.0, max_soc_kwh);
    BatterySpec {
        max_charge_kw: cfg.charge_kw,
        max_discharge_kw: cfg.discharge_kw,
        charge_efficiency: one_way_efficiency,
        discharge_efficiency: one_way_efficiency,
        min_soc_kwh,
        max_soc_kwh,
        // Overwritten from live telemetry each cycle; the floor is a safe seed.
        initial_soc_kwh: min_soc_kwh,
    }
}

/// Placeholder consumption model — a flat 0.4 kWh/h across all hours, used when no training data is
/// available so the planner still has a sane baseline load.
fn flat_consumption() -> ConsumptionModel {
    let mut m = ConsumptionModel::new();
    for h in 0..24u32 {
        m.add_sample(22.0, h, false, 0.4);
    }
    m.build();
    m
}

/// Cross-check every zone name the config references against the model — called once at serve
/// startup, where Model/RcNetwork and ControlConfig first meet.
///
/// The optimizer *intersects* the config's zones with the model's (`heated_zones` ∩
/// `heating.zones` ∩ state rows) rather than erroring, so a typo'd or renamed zone silently drops
/// that room from heating/HVAC/gain control — no relay, no comfort constraint, no violation
/// penalty. Unknown names are a **hard error**; a heated zone that exists but has no `"heating"`
/// marker yet is only a loud warning (config-first dormant zones are a legitimate workflow — the
/// room activates when its floor boundary lands in the model).
pub fn validate_config_zones(config: &ControlConfig, net: &RcNetwork) -> Result<()> {
    let known = |zone: &str| net.zone_indices.contains_key(zone);
    for zone in config.heating.zones.keys() {
        anyhow::ensure!(
            known(zone),
            "config heating.zones[{zone:?}] does not exist in the model — a typo'd zone would \
             silently never heat"
        );
        if !net
            .marker_indices
            .contains_key(&(zone.clone(), "heating".to_string()))
        {
            eprintln!(
                "[config] WARNING: heated zone {zone:?} has no \"heating\" marker in the model — \
                 it stays DORMANT (no heating scheduled) until its floor boundary lands"
            );
        }
    }
    // Gain groups name zones too, and a typo there is silent in a nastier way than most: the zone
    // simply never joins a group, so the fit quietly falls back to per-zone (or to nothing) with no
    // symptom beyond a bias no one connects to the config.
    for group in &config.heating.gain_groups {
        for zone in group {
            anyhow::ensure!(
                known(zone),
                "config heating.gain_groups names {zone:?}, which does not exist in the model"
            );
        }
    }
    for zone in config.heating.comfort.rooms.keys() {
        anyhow::ensure!(
            known(zone),
            "config comfort.rooms[{zone:?}] does not exist in the model"
        );
    }
    if let Some(hvac) = &config.hvac {
        for (unit, u) in &hvac.units {
            for zone in &u.zones {
                anyhow::ensure!(
                    known(zone),
                    "config hvac.units[{unit:?}] serves unknown zone {zone:?}"
                );
            }
        }
    }
    for load in &config.scheduled_loads {
        anyhow::ensure!(
            known(&load.zone),
            "config scheduled_loads[{:?}] references unknown zone {:?}",
            load.label,
            load.zone
        );
    }
    Ok(())
}

/// Estimate the current thermal state (per-zone air temperature) from measured history.
pub async fn current_state(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    config: &ControlConfig,
    kalman: Option<&crate::kalman::KalmanFilter>,
) -> Result<StateReport> {
    // No cache on this on-demand path — the estimator uses the config-baseline gains.
    let est =
        estimate_initial_state(db, net, ss, latitude, longitude, 72, config, None, kalman).await?;
    let mut zones: Vec<ZoneTemp> = net
        .zone_indices
        .iter()
        .filter(|(z, _)| z.as_str() != "outside" && z.as_str() != "ground")
        .filter_map(|(zone, &node)| {
            ss.state_index(node).map(|s| ZoneTemp {
                zone: zone.clone(),
                temp_c: k_to_c(est.x0[s]),
            })
        })
        .collect();
    zones.sort_by(|a, b| a.zone.cmp(&b.zone));
    Ok(StateReport {
        zones,
        disturbance_w: est.disturbance_w,
    })
}

/// Recent **measured** per-zone air-temperature series, for the Indoor comfort sparklines. Unlike
/// [`current_state`] (a model estimate, anchored to the latest reading), this is the raw sensor
/// history — so it shows reality (e.g. an overnight open-window dip) as a trend. Zones without
/// measured data are omitted; a failed read for one zone never fails the whole call.
pub async fn zone_temp_history(
    db: &SourceClients,
    net: &RcNetwork,
    hours: i64,
) -> Result<Vec<ZoneSeries>> {
    let start = format!("-{hours}h");
    let mut out = Vec::new();
    for zone in net.zone_indices.keys() {
        if zone == "outside" || zone == "ground" {
            continue;
        }
        if let Ok(series) = db
            .read_zone_temperature_series(zone, &start, "now()", "30m")
            .await
        {
            if !series.is_empty() {
                out.push(ZoneSeries {
                    zone: zone.clone(),
                    series: series
                        .iter()
                        .map(|s| (s.time.to_rfc3339(), s.value))
                        .collect(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.zone.cmp(&b.zone));
    Ok(out)
}

/// Cross-cutting plan inputs beyond the raw data sources, bundled to keep `current_plan`'s
/// signature stable as features accrue. `Default` = the plain on-demand behaviour.
#[derive(Default)]
pub struct PlanExtras<'a> {
    /// The loop's slow-input cache (consumption model, PV calibration, live gains); `None` reads
    /// them fresh (the on-demand web path).
    pub cache: Option<&'a PlanCache>,
    /// The loop's block-0 heating commitment `(block_start, relays)`: fixed INTO the LP when the
    /// committed block is the plan's own block 0 or exactly ONE block later (the bounded
    /// backward-clock hold). An OLDER committed block (a forward rollover between the loop's
    /// clock read and this plan's) is stale, and one MORE than a block ahead means a large
    /// backward step — both optimize freely instead.
    pub committed_heat: Option<(DateTime<Utc>, HashMap<String, f64>)>,
    /// The startup-built kernel cache (x0-independent; see [`KernelSet`]). `None` builds fresh.
    pub kernels: Option<Arc<KernelSet>>,
    /// `true` for the MPC loop's own re-plan: it gets a solver permit RESERVED for the loop, so an
    /// on-demand `/api/plan` recompute can never displace the actuated plan onto the relaxed
    /// fallback (which the publisher would then skip for that tick).
    pub loop_caller: bool,
    /// The startup-built Kalman filter (config `estimator.mode` shadow/kalman); `None` = anchor.
    pub kalman: Option<Arc<crate::kalman::KalmanFilter>>,
    /// Hours each CONTROLLABLE load has already run inside the window occurrence in progress right
    /// now — the loop's own tally of what it actuated. Lets the LP re-plan that occurrence for its
    /// remainder instead of from scratch, which is what stops a per-minute re-plan from either
    /// dropping the requirement or re-running the appliance. Empty on the on-demand path.
    pub load_run_hours: HashMap<String, f64>,
    /// The persisted per-room mode state (heating / cooling, with its hysteresis clock) the plan's
    /// mode selection starts from — `AppState::room_modes` on the live paths. Empty ⇒ first sight.
    pub room_mode_prior: HashMap<String, crate::optimize::room_mode::RoomModeState>,
}

/// Block 1 of `timeline` (the NEXT block), for [`PlanReport::next_step`] — `None` when the plan has
/// fewer than 2 blocks (a very short horizon/grid). Pure, so it's directly unit-testable.
fn next_timeline_step(timeline: &[TimelineBlock]) -> Option<TimelineBlock> {
    timeline.get(1).cloned()
}

/// Build the live whole-house plan: self-corrected Solcast PV + estimated state → unified optimizer.
/// `extras.cache` supplies the slow inputs (consumption + calibration) when the loop has them;
/// `None` reads them fresh (the on-demand web path).
pub async fn current_plan(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
    extras: PlanExtras<'_>,
) -> Result<PlanReport> {
    let cache = extras.cache;
    let ground_temperature_c = config.site.ground_temperature_c;
    let mut placeholders: Vec<String> = Vec::new();
    // A cache built on fallbacks (neutral calibration, flat consumption) must surface in this
    // plan's placeholders too — the loop path has no other way to expose a degraded cache.
    if let Some(c) = cache {
        placeholders.extend(c.fallbacks.iter().cloned());
    }

    // Align the plan to the current 15-minute block boundary, so block 0 is the block we're in.
    let now = Utc::now();
    let start = block_align(now);
    // The plan's local offset, derived at the plan start (per-block where it matters:
    // tariff_prices derives per block; the consumption-bin/scheduled-window uses accept <=1 h of
    // far-horizon drift on the two DST transition days — see ForecastContext::local_offset).
    let local_offset = config.site.offset_at(start);

    // The multi-rate planning grid: fine (15-min) blocks for `config.horizon.fine_hours`,
    // hourly beyond that, out to `config.horizon.hours` — see `docs/configuration.md`'s `horizon`
    // section. `start` is always block-aligned (just truncated above), matching `BlockGrid::
    // multi_rate`'s alignment requirement. Block 0 is always a fine block (`fine_hours >= 1`).
    let grid = crate::optimize::grid::BlockGrid::multi_rate(
        start,
        config.horizon.hours,
        config.horizon.fine_hours,
        BLOCK_SECONDS,
    );

    // Safety-critical degradation: set when a fallback is bad enough that ACTUATING the plan is
    // worse than letting the controllers deadman-revert to their failsafe (a fictional thermal
    // state, or no idea of the outside temperature). The publisher refuses to publish a degraded
    // plan; lesser fallbacks stay advisory placeholders.
    let mut degraded = false;

    // Seed the thermal state from measured history; fall back to a flat guess — FLAGGED: the
    // heating decision from a fictional uniform 22 °C house must never look like a clean plan.
    // The Kalman observer's per-zone constant disturbance flux (offset-free estimation) rides
    // along when it ran; folded into `ctx.internal_gain_w` below so the forward prediction keeps
    // tracking a measured unmodelled loss/gain instead of dropping it after this instant.
    let mut disturbance_w: HashMap<String, f64> = HashMap::new();
    let x0 = match estimate_initial_state(
        db,
        net,
        ss,
        latitude,
        longitude,
        72,
        config,
        cache,
        extras.kalman.as_deref(),
    )
    .await
    {
        Ok(est) => {
            if let Some(d) = est.disturbance_w {
                disturbance_w = d;
            }
            est.x0
        }
        Err(_) => {
            placeholders.push("thermal state (history unavailable; flat 22 °C seed)".to_string());
            degraded = true;
            DVector::from_element(ss.n_states(), c_to_k(22.0))
        }
    };

    // Outside temperature + cloud forecast (also feeds the clear-sky PV fallback below, so it is
    // read first). Degraded modes, safest first: a forecast covering only part of the horizon is
    // used but flagged; no forecast at all falls back to the LAST MEASURED outside temperature
    // held flat (an independent feed — in winter a flat +24 °C guess would plan zero heating and
    // the armed controllers would actuate it); flat 24 °C is the last resort with both feeds down.
    let (temperature_c, cloud_cover, solar) = match weather_forecast(db, start, HORIZON_HOURS)
        .await?
    {
        Some(wf) => {
            if wf.covered_hours * 5 < HORIZON_HOURS * 4 {
                // Under 80 % of the horizon has a real sample — the tail is forward-filled flat.
                placeholders.push(format!(
                    "weather forecast (covers {}/{HORIZON_HOURS} h; tail held flat)",
                    wf.covered_hours
                ));
            }
            if wf.cloud_covered_hours == 0 {
                placeholders.push("cloud cover (forecast unavailable; flat 30 %)".to_string());
            }
            // Advisory only when radiation covers part of the horizon — an all-cloud-model plan
            // is the ordinary state until the writer stores radiation fields.
            if wf.radiation_covered_hours > 0 && wf.radiation_covered_hours < HORIZON_HOURS {
                placeholders.push(format!(
                    "solar radiation ({}/{HORIZON_HOURS} h; cloud model for the rest)",
                    wf.radiation_covered_hours
                ));
            }
            (
                hourly_to_blocks(start, &wf.temperature_c),
                hourly_to_blocks(start, &wf.cloud_cover),
                hourly_solar_to_blocks(start, &wf.solar),
            )
        }
        None => {
            let measured = db
                .read_zone_temperature_series("outside", "-3h", "now()", "15m")
                .await
                .ok()
                .and_then(|s| s.last().map(|x| x.value));
            match measured {
                Some(t) => {
                    placeholders.push(format!(
                        "outside temperature (forecast unavailable; last measured {t:.1} °C held flat)"
                    ));
                    (
                        vec![t; HORIZON_BLOCKS],
                        vec![0.3; HORIZON_BLOCKS],
                        Vec::new(),
                    )
                }
                None => {
                    placeholders.push(
                        "outside temperature + cloud (forecast and measurement unavailable; flat 24 °C)"
                            .to_string(),
                    );
                    // In winter a flat 24 °C guess plans zero heating — never actuate it.
                    degraded = true;
                    (
                        vec![24.0; HORIZON_BLOCKS],
                        vec![0.3; HORIZON_BLOCKS],
                        Vec::new(),
                    )
                }
            }
        }
    };

    // The post-horizon outlook: a separate read starting where the horizon TRULY ends, so the
    // horizon's own weather-coverage flags above are unaffected. Advisory only (never feeds the
    // LP) — best-effort, no placeholder flag: an unavailable outlook just reverts
    // `heating_demanded`/the terminal credit to horizon-only, today's behaviour, not a degraded
    // plan.
    //
    // `grid.block_end(grid.len() - 1)`, NOT a fixed `start + HORIZON_HOURS`: on the multi-rate grid
    // the trailing partial hour is dropped, so the grid's true end can be up to ~45 min before
    // `start + HORIZON_HOURS` — `coordinator::outlook_thermal_ inputs` already anchors the
    // outlook's KNOWN INPUTS at exactly `ctx.start + step_seconds * n_fine` (the true fine-lattice
    // end) when it continues the free-response simulation past the horizon; anchoring the weather
    // FETCH at the fixed offset instead would read `Outlook. temperature_c[0]` for the wrong
    // instant (up to 45 min of skew) relative to what the simulation actually treats it as
    // covering.
    let outlook_start = grid.block_end(grid.len() - 1);
    // `config.horizon.outlook_hours` (0 disables the outlook entirely — same as a fetch failure).
    let outlook = if config.horizon.outlook_hours == 0 {
        None
    } else {
        match weather_forecast(db, outlook_start, config.horizon.outlook_hours).await {
            Ok(Some(owf)) => outlook_from_weather(outlook_start, &owf),
            Ok(None) | Err(_) => None,
        }
    };

    // PV: prefer the self-corrected Solcast forecast (it already covers every array); fall back to
    // the clear-sky model over the configured arrays when Solcast is unavailable. The calibration
    // is fit from the last week's Solcast-vs-actual and recomputed each cycle.
    let calibration = match cache {
        Some(c) => c.calibration,
        None => match backtest_pv(db, &config.site, 7, &config.pv.nowcast).await {
            // Same evidence gate as `build_cache`: don't trust a ratio fit from a few hours.
            Ok(bt) if bt.scored_hours >= CALIBRATION_MIN_SCORED_HOURS => {
                PvBandCalibration::from_backtest(
                    bt.band_solcast_kwh,
                    bt.band_actual_kwh,
                    bt.band_clean_hours,
                    Calibration::from_totals_default(bt.total_solcast_kwh, bt.total_actual_kwh),
                    CALIBRATION_MIN_BAND_HOURS,
                )
            }
            Ok(_) | Err(_) => {
                placeholders.push("PV calibration (insufficient evidence; neutral)".to_string());
                PvBandCalibration::neutral()
            }
        },
    };
    let solcast = pv_forecast_kw(db, start, HORIZON_HOURS, &config.site)
        .await
        .ok()
        .filter(|f| f.hourly_kw.iter().sum::<f64>() > 0.0);
    let (raw_pv, pv_kw, pv_calibration_scale, pv_p10_kw, pv_nowcast) = match solcast {
        Some(f) => {
            let raw = hourly_to_blocks(start, &f.hourly_kw);
            // Band-aware application: each block calibrated by its own local hour's ratio.
            let mut calibrated: Vec<f64> = raw
                .iter()
                .enumerate()
                .map(|(b, &kw)| {
                    let at = start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64);
                    // Pick the band from the block's hour-END, because that is the key the value
                    // itself carries: `solar_forecast` reads the stored curve at `at + 1h`, and
                    // `pv_backtest` FITS the band ratios against that same hour-ending key. Using
                    // the block-START hour mismatched the fit at the band edges (11 and 15), so
                    // the 10–11 and 14–15 local blocks were scaled by a neighbouring band's ratio
                    // — exactly the shoulder-of-day timing the band split exists to correct.
                    let end = at + Duration::seconds(3600);
                    calibration.apply_at(kw, end.with_timezone(&config.site.offset_at(end)).hour())
                })
                .collect();
            let mut raw = raw;
            // Splice the clear-sky model into the hours whose DATE has no stored curve (a
            // snapshotter gap, not night) — the horizon always crosses midnight, so a missing
            // tomorrow would otherwise plan phantom 0 kW mornings and the optimizer would
            // grid-charge overnight against them. Spliced blocks are the clear-sky model
            // (uncalibrated — the Solcast-vs-actual ratio doesn't apply to it), and flagged.
            if !f.missing_dates.is_empty() {
                let missing_mask: Vec<f64> = f
                    .hours_missing
                    .iter()
                    .map(|&m| if m { 1.0 } else { 0.0 })
                    .collect();
                let missing_blocks = hourly_to_blocks(start, &missing_mask);
                let clear_sky = clearsky_pv_kw(
                    &pv_arrays(&config.pv),
                    latitude,
                    longitude,
                    start,
                    &cloud_cover,
                );
                let mut spliced = 0usize;
                for b in 0..raw.len().min(clear_sky.len()) {
                    if missing_blocks[b] > 0.5 {
                        raw[b] = clear_sky[b];
                        calibrated[b] = clear_sky[b];
                        spliced += 1;
                    }
                }
                let dates = f
                    .missing_dates
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                placeholders.push(format!(
                    "PV (no snapshot for {dates}; clear-sky for {spliced}/{HORIZON_BLOCKS} blocks)"
                ));
            }
            // p10 stays UNCALIBRATED (it is already the conservative percentile; scaling it by
            // the p50 ratio would double-count) and un-spliced (a clear-sky fill is not a p10).
            let p10 = f.hourly_p10_kw.as_ref().map(|h| hourly_to_blocks(start, h));
            // Nowcast AFTER the clear-sky splice above, so spliced blocks (a missing-snapshot
            // date) are nowcast too — their actual weather is just as real as a Solcast block's.
            let dc_peak_kw = (!config.pv.arrays.is_empty())
                .then(|| config.pv.arrays.iter().map(|a| a.kwp).sum());
            let nowcast = apply_pv_nowcast(
                db,
                &config.pv.nowcast,
                &config.site,
                &f,
                &calibration,
                &mut calibrated,
                start,
                now,
                dc_peak_kw,
                &mut placeholders,
            )
            .await;
            (raw, calibrated, calibration.overall_scale(), p10, nowcast)
        }
        None => {
            let arrays_desc = if config.pv.arrays.is_empty() {
                "default array".to_string()
            } else {
                config
                    .pv
                    .arrays
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            placeholders.push(format!(
                "PV (Solcast unavailable; clear-sky model over {arrays_desc})"
            ));
            let clear_sky = clearsky_pv_kw(
                &pv_arrays(&config.pv),
                latitude,
                longitude,
                start,
                &cloud_cover,
            );
            // No Solcast curve to blend measured PV against this cycle — not one of the nowcast's
            // own gating reasons, so no placeholder (the "PV (Solcast unavailable…)" one above
            // already flags the degraded cycle).
            let nowcast = if config.pv.nowcast.enabled {
                PvNowcast::skip(config.pv.nowcast.window_minutes, "no forecast")
            } else {
                PvNowcast::skip(config.pv.nowcast.window_minutes, "disabled")
            };
            (clear_sky.clone(), clear_sky, 1.0, None, nowcast)
        }
    };
    // raw_pv / pv_kw are per-block kW; sum × block-hours = kWh over the horizon.
    let pv_raw_kwh: f64 = raw_pv.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);
    let pv_calibrated_kwh: f64 = pv_kw.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);

    // Parsed once, reused by the day-ahead fallback chain below AND the post-horizon terminal-value
    // curve (`terminal_soc_value_outlook`'s `day_type_median_curve` call) — both need the SAME
    // holiday calendar the day-type median estimator classifies days by.
    let public_holidays: Vec<(u32, u32)> = config
        .site
        .public_holidays
        .iter()
        .filter_map(|md| crate::optimize::price_forecast::parse_month_day(md))
        .collect();
    // Day-ahead spot prices (EUR/kWh) from OTE — fall back to the placeholder curve if not yet
    // published or unreadable (a transient DB error must not fail the whole planning cycle).
    let (spot_price, price_is_placeholder): (Vec<f64>, Vec<bool>) =
        match block_prices(db, start, HORIZON_BLOCKS).await {
            Ok(Some(BlockPrices { current, day_ago })) => {
                // Use real prices where published; fill the unpublished tail (e.g. tomorrow before
                // the ~14:00 auction) FIRST with the day-type median estimate (the
                // backtested-better predictor — median price at the same local clock slot over the
                // most recent same-day-type days, from the cached ≤30-day history), else the real
                // price of the same clock block a day earlier (persistence), else the fixed
                // placeholder curve. The per-block MASK is unchanged either way (the LP must not
                // commit battery arbitrage against an invented spread, and an estimated or day-old
                // price is exactly that) — flag how much fell back and by which route.
                let placeholder = placeholder_price_curve(start, local_offset, HORIZON_BLOCKS);
                let price_history: &[(DateTime<Utc>, f64)] = cache
                    .map(|c| c.price_history_eur_kwh.as_slice())
                    .unwrap_or_default();
                let estimated: Vec<Option<f64>> = (0..current.len())
                    .map(|b| {
                        let at = start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64);
                        crate::optimize::price_forecast::day_type_median_price(
                            price_history,
                            at,
                            |t| config.site.offset_at(t), // real per-instant offset: DST-safe
                            &public_holidays,
                            config.site.easter_holidays,
                        )
                    })
                    .collect();
                let (price, is_placeholder, missing, persisted, estimated_count) =
                    fill_block_prices(&current, &estimated, &day_ago, &placeholder);
                if missing > 0 {
                    let source = price_fallback_label(missing, estimated_count, persisted);
                    placeholders.push(format!(
                        "day-ahead prices ({missing}/{HORIZON_BLOCKS} blocks unpublished; {source})"
                    ));
                }
                (price, is_placeholder)
            }
            Ok(None) | Err(_) => {
                placeholders.push("day-ahead prices (unavailable; placeholder curve)".to_string());
                (
                    placeholder_price_curve(start, local_offset, HORIZON_BLOCKS),
                    vec![true; HORIZON_BLOCKS],
                )
            }
        };
    // Apply the real Czech tariff: import = spot + distribution (VT/NT by local hour); export =
    // spot − sell fee. This is the same economics the live loxone controller sees.
    let (import_price, export_price) =
        tariff_prices(&config.tariff, &config.site, &spot_price, start);

    // Per-block grid gates from the spot price vs the tariff thresholds (EUR/kWh): no export below
    // the export floor, and the inverter off in deeply-negative blocks — the loxone behaviour.
    let export_floor = config.tariff.czk_to_eur(config.tariff.export_price_min_czk);
    let inverter_off = config
        .tariff
        .czk_to_eur(config.tariff.inverter_off_price_czk);
    let export_allowed: Vec<bool> = spot_price.iter().map(|&s| s >= export_floor).collect();
    let inverter_on: Vec<bool> = spot_price.iter().map(|&s| s >= inverter_off).collect();
    // Battery wear (EUR/kWh discharged). The terminal value of leftover SoC is computed below, once
    // the battery's round-trip efficiency is known (see `terminal_soc_value`).
    let battery_amortisation = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);

    // Consumption model trained from the trailing window (self-correcting), else a flat fallback.
    let consumption = match cache {
        Some(c) => c.consumption.clone(),
        None => match train_consumption(db, net, config).await? {
            Some(m) => m,
            None => {
                placeholders.push("consumption (history unavailable; flat 0.4 kWh/h)".to_string());
                flat_consumption()
            }
        },
    };

    // Battery: seed the current SoC from live telemetry, else the default spec's value.
    let mut battery = battery_spec(&config.battery);
    match battery_soc_kwh(db, battery.max_soc_kwh).await? {
        Some(soc) => {
            let clamped = soc.clamp(battery.min_soc_kwh, battery.max_soc_kwh);
            if (clamped - soc).abs() > 1e-6 {
                // The live SoC fell outside the optimizer's [min, max] band — the physical battery is
                // below our economic floor, or a capacity/config mismatch. Plan from the clamped value
                // (the LP needs `initial_soc` within bounds), but flag it rather than presenting a
                // silently-corrected reading as a clean one.
                placeholders.push(format!(
                    "battery SoC (telemetry {soc:.2} kWh outside [{:.2}, {:.2}]; clamped)",
                    battery.min_soc_kwh, battery.max_soc_kwh
                ));
            }
            battery.initial_soc_kwh = clamped;
        }
        None => placeholders.push("battery SoC (telemetry unavailable; default)".to_string()),
    }
    // Terminal value from REAL prices only (given enough of them): with a long pre-auction
    // placeholder tail the median would be dominated by the synthetic curve. ≥16 real blocks
    // (4 h) is always satisfied in practice — today's full published day gives ≥40.
    let real_import: Vec<f64> = import_price
        .iter()
        .zip(&price_is_placeholder)
        .filter(|(_, &ph)| !ph)
        .map(|(&p, _)| p)
        .collect();
    let terminal_basis: &[f64] = if real_import.len() >= 16 {
        &real_import
    } else {
        &import_price
    };
    let round_trip_eta = battery.charge_efficiency * battery.discharge_efficiency;
    // The in-horizon-median value — the fallback for a cold-start/thin price history (and the
    // slab-heat credit's basis).
    let horizon_median_terminal_value =
        terminal_soc_value(terminal_basis, battery_amortisation, round_trip_eta);
    // The day-type median (`price_history`, SPOT EUR/kWh) must land on the SAME scale as
    // `import_price` (spot + VT/NT distribution) — see `ForecastContext::distribution_eur_by_local_
    // hour`'s doc. Precomputed once, reused by `ctx` below.
    let distribution_eur_by_local_hour: [f64; 24] = {
        let mask = config.tariff.low_tariff_mask();
        std::array::from_fn(|h| config.tariff.distribution_eur(h as u32, &mask))
    };
    // The post-horizon import curve (24 h, 96 × 900 s) from the TRUE grid end (`outlook_start`, the
    // same anchor the heat-credit outlook uses) — `terminal_soc_value_outlook` prices leftover SoC
    // at what it displaces AFTER the horizon, not the in-horizon median. Used only when the
    // day-type median covers EVERY post-horizon block; a thin/cold-start history falls back to the
    // horizon-median value unchanged, today's behaviour, and is flagged as a placeholder.
    const POST_HORIZON_BLOCKS: usize = 96;
    let post_horizon_curve = crate::optimize::price_forecast::day_type_median_curve(
        cache
            .map(|c| c.price_history_eur_kwh.as_slice())
            .unwrap_or_default(),
        outlook_start,
        BLOCK_SECONDS,
        POST_HORIZON_BLOCKS,
        |t| config.site.offset_at(t),
        &public_holidays,
        config.site.easter_holidays,
        &distribution_eur_by_local_hour,
    );
    let (terminal_value, terminal_soc_value_source, terminal_value_note) = select_terminal_value(
        &post_horizon_curve,
        horizon_median_terminal_value,
        battery_amortisation,
        round_trip_eta,
    );
    if let Some(note) = terminal_value_note {
        placeholders.push(note);
    }

    let mut ctx = ForecastContext {
        latitude,
        longitude,
        start,
        step_seconds: BLOCK_SECONDS,
        grid: grid.clone(),
        local_offset,
        temperature_c,
        ground_temperature_c,
        cloud_cover,
        solar,
        // Live-fitted gains from the loop's cache; the on-demand path (no cache) uses the config baseline.
        internal_gain_w: cache
            .map(|c| c.internal_gains.clone())
            .unwrap_or_else(|| config.heating.internal_gains()),
        scheduled_loads: config.scheduled_loads.clone(),
        load_run_hours: {
            // Start from the loop's planned-actuation tally, then override any load that has a
            // `sensor` with what the appliance MEASURABLY drew this occurrence — evidence beats a
            // guess, and the guess cannot see an actuation-chain outage (see `measured_run_hours`).
            let mut run = extras.load_run_hours.clone();
            for load in config.scheduled_loads.iter().filter(|l| l.controllable) {
                if let Some(h) = measured_run_hours(db, &config.site, load, start).await {
                    run.insert(crate::optimize::coordinator::load_name(load), h);
                }
            }
            run
        },
        // Live-fitted scheduled-load magnitudes from the cache; the on-demand path (no cache) seeds
        // them from the configured magnitudes (a fixed `power_w` takes effect immediately; a fitted
        // load is 0, no effect) until the loop's first re-fit lands.
        scheduled_w: cache
            .map(|c| c.scheduled_w.clone())
            .filter(|w| w.len() == config.scheduled_loads.len())
            .unwrap_or_else(|| {
                config
                    .scheduled_loads
                    .iter()
                    .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
                    .collect()
            }),
        export_price,
        export_allowed: export_allowed.clone(),
        inverter_on: inverter_on.clone(),
        battery_amortisation,
        export_needs_pv: true,
        min_dispatch_kw: config.battery.min_dispatch_kw,
        terminal_value,
        // The flat heat-credit fallback stays on the in-horizon-median value regardless of
        // which valuation fed `terminal_value` — see `ForecastContext::terminal_heat_basis`'s doc.
        terminal_heat_basis: horizon_median_terminal_value,
        import_price,
        min_final_soc_kwh: Some(battery.min_soc_kwh),
        price_is_placeholder: price_is_placeholder.clone(),
        // Physical grid-connection limits from config (`grid` block); None ⇒ unconstrained.
        max_import_kw: config.grid.max_import_kw,
        max_export_kw: config.grid.max_export_kw,
        pv_kw_override: Some(pv_kw),
        load_kw_override: None,
        load_scale: 1.0,
        outlook,
        // On-demand (no cache) gets no day-type median history — falls back to plain persistence,
        // same as before this feature existed, rather than a fresh bounded Influx read per call.
        price_history: cache
            .map(|c| c.price_history_eur_kwh.clone())
            .unwrap_or_default(),
        public_holidays: public_holidays.clone(),
        easter_holidays: config.site.easter_holidays,
        distribution_eur_by_local_hour,
        room_mode_prior: extras.room_mode_prior.clone(),
    };

    // Offset-free MPC: fold the disturbance observer's per-zone constant flux into the forecast's
    // internal gains, AFTER the live gain re-fit above so both corrections apply — the forward
    // prediction stops reverting to the model's own bias and instead keeps tracking today's
    // measured unmodelled loss/gain over the whole horizon. Re-clamped here even though the filter
    // already clamps it (belt-and-suspenders against a future caller bypassing the filter).
    for (zone, &d) in &disturbance_w {
        let clamped = d.clamp(
            -config.estimator.max_disturbance_w,
            config.estimator.max_disturbance_w,
        );
        let gain = ctx
            .internal_gain_w
            .entry(zone.clone())
            .or_insert_with(|| crate::optimize::config::GainProfile::flat(0.0));
        gain.night += clamped;
        gain.day += clamped;
        gain.evening += clamped;
    }

    // Ignored while `pv_kw_override` is set; pass the configured array so the non-override path stays
    // consistent with the live forecast.
    let primary_pv = pv_arrays(&config.pv)
        .first()
        .copied()
        .unwrap_or_else(default_pv_array);
    let hvac = config.hvac.clone().unwrap_or_default();

    // Curtailment-risk metric from the Solcast p10 percentile (None until the writer stores it):
    // even the conservatively-LOW forecast's surplus over the NEXT SOLAR DAY's load (today's
    // remaining daylight before local noon, tomorrow's from noon on — `next_solar_day_mask`), vs
    // the battery headroom. Optionally (config `battery.p10_precharge_guard`) halve the terminal
    // SoC value when even p10 fills the battery — the pre-charge would be squeezed out (or
    // curtailed) by that coming daylight anyway.
    let (p10_surplus_kwh, curtailment_risk_kwh) = match &pv_p10_kw {
        Some(p10) => {
            let next_solar_day =
                next_solar_day_mask(start, local_offset, HORIZON_BLOCKS, BLOCK_SECONDS);
            let load_kw = crate::optimize::coordinator::forecast_pv_load(
                &primary_pv,
                &consumption,
                &ctx,
                HORIZON_BLOCKS,
            )
            .map(|(_pv, load)| load)
            .unwrap_or_default();
            let headroom = battery.max_soc_kwh - battery.initial_soc_kwh;
            let (surplus, risk) = p10_curtailment(
                p10,
                &load_kw,
                &next_solar_day,
                headroom,
                BLOCK_SECONDS / 3600.0,
            );
            if config.battery.p10_precharge_guard && risk > 0.0 {
                ctx.terminal_value *= 0.5;
                ctx.terminal_heat_basis *= 0.5;
                placeholders.push(format!(
                    "terminal value halved (p10 precharge guard: next solar day's p10 surplus \
                     {surplus:.1} kWh exceeds battery headroom {headroom:.1} kWh)"
                ));
            }
            (Some(surplus), Some(risk))
        }
        None => (None, None),
    };
    // EV chargers: fuse each charger's live state + config + dashboard prefs into optimizer inputs.
    // Off the runtime: this is plain synchronous file IO against a bind-mounted store, and
    // `current_plan` is awaited on a tokio worker by BOTH the MPC tick and `/api/plan` — a stalled
    // volume would block unrelated handlers (`/livez`, `/readyz`) behind it. web.rs already wraps
    // every other access to this same store; this was the outlier.
    let ev_prefs = tokio::task::spawn_blocking(crate::ev::prefs::load)
        .await
        .unwrap_or_default();
    let ev =
        crate::ev::build_inputs(db, &config.chargers, start, &grid, local_offset, &ev_prefs).await;
    // The block-0 commitment applies when the committed block is this plan's block 0 — or ONE
    // block later (a small backward wall-clock step, e.g. NTP: the loop keeps its latch on
    // `block <= b` and expects the relays to actually be held, so filtering on strict equality
    // would let the LP re-decide them every minute while the loop believed them latched). An
    // OLDER committed block (a forward rollover between the loop's clock read and ours) is stale,
    // and a commitment MORE than one block ahead means a large backward step — honoring it would
    // freeze the relays regardless of zone temperature until wall-clock caught up, so optimize
    // freely and let the loop re-latch.
    let committed = extras
        .committed_heat
        .as_ref()
        .filter(|(block, _)| {
            *block >= start && *block - start <= Duration::seconds(BLOCK_SECONDS as i64)
        })
        .map(|(_, relays)| relays.clone());
    // `u_{-1}` for the relay switching cost: the relays held in the block immediately BEFORE this
    // plan's block 0. The loop's latch still holds exactly that on the first plan of a new block
    // (before it adopts/decides the new block's relays), when it is older than `start` by one
    // block. When the latch already covers block 0 the block-0 relays are pinned and the entry
    // switch is a constant, so no `u_{-1}` is needed (`None`: not counted).
    let prev_heat = extras
        .committed_heat
        .as_ref()
        .filter(|(block, _)| {
            *block < start && start - *block <= Duration::seconds(BLOCK_SECONDS as i64)
        })
        .map(|(_, relays)| relays.clone());

    // `ctx`'s fine-lattice vectors above were all built at the full HORIZON_BLOCKS span (the
    // `p10_curtailment` read just above needs that full length) — but `grid.n_fine()` can be
    // slightly SHORTER (a non-hour-aligned `start` drops a trailing partial hour off the far end;
    // see `BlockGrid::multi_rate`'s doc). Truncate them to match before the grid-aware plan path
    // reads them (a no-op whenever `start` is hour-aligned or `horizon.fine_hours >= horizon.hours`).
    let n_fine = grid.n_fine();
    ensure!(
        n_fine <= HORIZON_BLOCKS,
        "config horizon.hours ({}) needs {n_fine} fine steps, more than the {HORIZON_BLOCKS}-block \
         fine-lattice assembly (HORIZON_HOURS={HORIZON_HOURS} h) provides — reduce horizon.hours \
         or raise HORIZON_HOURS",
        config.horizon.hours
    );
    ctx.temperature_c.truncate(n_fine);
    ctx.cloud_cover.truncate(n_fine);
    if !ctx.solar.is_empty() {
        ctx.solar.truncate(n_fine);
    }
    ctx.import_price.truncate(n_fine);
    ctx.export_price.truncate(n_fine);
    ctx.export_allowed.truncate(n_fine);
    ctx.inverter_on.truncate(n_fine);
    if !ctx.price_is_placeholder.is_empty() {
        ctx.price_is_placeholder.truncate(n_fine);
    }
    if let Some(pv) = &mut ctx.pv_kw_override {
        pv.truncate(n_fine);
    }

    let job = Arc::new(SolveJob {
        pv: primary_pv,
        consumption: consumption.clone(),
        battery: battery.clone(),
        heating: config.heating.clone(),
        hvac: hvac.clone(),
        ss: ss.clone(),
        net: net.clone(),
        ctx: ctx.clone(),
        x0: x0.clone(),
        ev_specs: ev.specs.clone(),
        ev_monitored: ev.monitored_kw.clone(),
        committed,
        prev_heat,
        kernels: extras.kernels.clone(),
    });
    let strict_job = Arc::clone(&job);
    let fallback_job = Arc::clone(&job);
    let per_lp_budget = crate::optimize::unified::SolveBudget {
        time_limit_s: Some(PER_LP_HIGHS_TIME_LIMIT_S),
    };
    let fallback_per_lp_budget = crate::optimize::unified::SolveBudget {
        time_limit_s: Some(FALLBACK_PER_LP_HIGHS_TIME_LIMIT_S),
    };
    // Filled by the strict closure as soon as its relaxed LP succeeds (see `fix_and_round`'s doc);
    // `solve_bounded` salvages it on the outer strict timeout instead of starting a fresh fallback
    // LP.
    let salvage: Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>> =
        Arc::new(Mutex::new(None));
    let strict_salvage = Arc::clone(&salvage);
    let (plan, grade, fallback_cause) = solve_bounded(
        // Strict = fix-and-round (see `fix_and_round`'s own doc) — the NORMAL plan path; HiGHS
        // never runs branch-and-bound.
        move || fix_and_round(&strict_job, per_lp_budget, &strict_salvage),
        // Fallback: a single plain relaxed LP — used only when the strict pipeline above times out
        // or its permit is busy. Its own (looser) per-LP budget: one LP inside
        // FALLBACK_SOLVE_TIMEOUT, unlike the strict pipeline's two inside STRICT_SOLVE_TIMEOUT.
        move || run_solve(&fallback_job, None, fallback_per_lp_budget),
        STRICT_SOLVE_TIMEOUT,
        FALLBACK_SOLVE_TIMEOUT,
        extras.loop_caller,
        salvage,
    )
    .await?;
    let relaxed = matches!(grade, SolveGrade::Relaxed);
    let rounded = matches!(grade, SolveGrade::Rounded);
    if let Some(cause) = fallback_cause {
        placeholders.push(format!("plan ({cause}; binaries relaxed)"));
    }

    // The full plan as timestamped per-block rows: the optimizer's flows + the inverter slot mode
    // (classified from those flows) + the price-gated export / inverter levers, with the forecast
    // prices/PV that fed the block and the predicted per-zone temperature it produced.
    //
    // `ctx`'s own vectors (import_price, pv_kw_override, export_allowed, …) are on the FINE lattice
    // — `plan`'s are per GRID BLOCK. Aggregate them here the SAME way `plan_unified` does
    // internally before the solve, so the timeline reports exactly what the LP saw (not a
    // fine-index-read-as-block-index bug: block `b`'s fine index and grid index coincide only in
    // the fine section).
    let import_price_blocks = ctx.grid.mean(&ctx.import_price);
    let export_price_blocks = ctx.grid.mean(&ctx.export_price);
    let price_is_placeholder_blocks = if ctx.price_is_placeholder.is_empty() {
        Vec::new()
    } else {
        ctx.grid.any(&ctx.price_is_placeholder)
    };
    // Outside-temperature forecast per block; the length guard keeps `GridSpec::mean` (which slices)
    // from panicking on a context whose series is shorter than the lattice.
    let outside_blocks = if ctx.temperature_c.len() >= ctx.grid.n_fine() {
        ctx.grid.mean(&ctx.temperature_c)
    } else {
        Vec::new()
    };
    let pv_series = ctx
        .pv_kw_override
        .as_deref()
        .map(|pv| ctx.grid.mean(pv))
        .unwrap_or_default();
    let export_allowed_blocks = ctx.grid.all(&export_allowed);
    let inverter_on_blocks = ctx.grid.all(&inverter_on);
    let at_block = |map: &HashMap<String, Vec<f64>>, b: usize| -> HashMap<String, f64> {
        map.iter()
            .map(|(z, v)| (z.clone(), v.get(b).copied().unwrap_or(0.0)))
            .collect()
    };
    let timeline: Vec<TimelineBlock> = (0..plan.charge_kw.len())
        .map(|b| {
            let at = |v: &[f64]| v.get(b).copied().unwrap_or(0.0);
            let (charge, discharge) = (at(&plan.charge_kw), at(&plan.discharge_kw));
            let (grid_import, grid_export) = (at(&plan.grid_import_kw), at(&plan.grid_export_kw));
            // Mode classification keys on the BATTERY grid legs, not the EV-inclusive totals: a
            // solar-charging battery + EV-on-grid block is NOT charge_from_grid (forcing AC
            // charge), and battery→EV during solar export is NOT discharge_to_grid (draining the
            // battery to the grid). The totals stay in the reported metrics.
            let (batt_grid_charge, batt_to_grid) =
                (at(&plan.batt_grid_charge_kw), at(&plan.batt_to_grid_kw));
            let soc = at(&plan.soc_kwh);
            // Asymmetric safe defaults for a missing block: inverter ON (off is the rare
            // deeply-negative-price state), but export OFF (an unknown gate must not claim export).
            let inverter = inverter_on_blocks.get(b).copied().unwrap_or(true);
            TimelineBlock {
                t: ctx.grid.block_start(b),
                dt_minutes: (ctx.grid.dt_hours(b) * 60.0).round() as u32,
                import_price: import_price_blocks.get(b).copied().unwrap_or(0.0),
                export_price: export_price_blocks.get(b).copied().unwrap_or(0.0),
                price_is_placeholder: price_is_placeholder_blocks.get(b).copied().unwrap_or(false),
                pv_kw: pv_series.get(b).copied().unwrap_or(0.0),
                load_kw: at(&plan.load_kw),
                soc_kwh: soc,
                charge_kw: charge,
                discharge_kw: discharge,
                grid_import_kw: grid_import,
                grid_export_kw: grid_export,
                curtail_kw: at(&plan.curtail_kw),
                heat_kw: at_block(&plan.heat_kw, b),
                cool_kw: at_block(&plan.cool_kw, b),
                hvac_heat_kw: at_block(&plan.hvac_heat_kw, b),
                controllable_load_kw: at_block(&plan.controllable_load_kw, b),
                ev_charge_kw: at_block(&plan.ev_charge_kw, b),
                temp_c: at_block(&plan.zone_temp_c, b),
                outside_c: outside_blocks.get(b).copied().filter(|v| v.is_finite()),
                slot: classify_mode(
                    &BlockFlows {
                        charge_kw: charge,
                        discharge_kw: discharge,
                        batt_grid_charge_kw: batt_grid_charge,
                        batt_to_grid_kw: batt_to_grid,
                        grid_import_kw: grid_import,
                        grid_export_kw: grid_export,
                        soc_kwh: soc,
                        inverter_on: inverter,
                    },
                    battery.min_soc_kwh,
                    battery.max_soc_kwh,
                    config.battery.min_dispatch_kw,
                )
                .to_string(),
                // Safe default: export disabled if the per-block gate is unavailable.
                export_enabled: export_allowed_blocks.get(b).copied().unwrap_or(false),
                inverter_on: inverter,
                // Every ordinary `timeline` row reports the tick's own fresh LP output, never frozen
                // — only `mpc_loop`'s post-hoc override of `next_step` (a separate, cloned copy) ever
                // sets this true. See `TimelineBlock::frozen`'s doc.
                frozen: false,
            }
        })
        .collect();

    let first = |v: &[f64]| v.first().copied().unwrap_or(0.0);
    let first_of = |map: &HashMap<String, Vec<f64>>| -> HashMap<String, f64> {
        map.iter().map(|(z, v)| (z.clone(), first(v))).collect()
    };
    let first_step = FirstStep {
        hour_start: start,
        heat_kw: first_of(&plan.heat_kw),
        cool_kw: first_of(&plan.cool_kw),
        hvac_heat_kw: first_of(&plan.hvac_heat_kw),
        controllable_load_kw: first_of(&plan.controllable_load_kw),
        battery_charge_kw: first(&plan.charge_kw),
        battery_discharge_kw: first(&plan.discharge_kw),
        grid_import_kw: first(&plan.grid_import_kw),
        grid_export_kw: first(&plan.grid_export_kw),
        mode: timeline
            .first()
            .map(|b| ModeStep {
                slot: b.slot.clone(),
                export_enabled: b.export_enabled,
                inverter_on: b.inverter_on,
                charge_kw: b.charge_kw,
                discharge_kw: b.discharge_kw,
            })
            .unwrap_or(ModeStep {
                slot: "regular".into(),
                export_enabled: true,
                inverter_on: true,
                charge_kw: 0.0,
                discharge_kw: 0.0,
            }),
    };

    // Per-block duration (the multi-rate grid is not uniform) — every kWh total below weights each
    // block's kW by ITS OWN duration, not a flat BLOCK_SECONDS.
    let dt_vec = ctx.grid.dt_hours_vec();
    let sum_kwh = |v: &[f64]| -> f64 { v.iter().zip(&dt_vec).map(|(&p, &dt)| p * dt).sum() };
    let battery_discharge_kwh = sum_kwh(&plan.discharge_kw);
    // Per-charger EV plan: the live fused state joined to the optimizer's schedule + source split.
    let ev_plan: Vec<EvChargerPlan> = ev
        .states
        .iter()
        .map(|st| {
            let charge_kw = plan.ev_charge_kw.get(&st.name).cloned().unwrap_or_default();
            let charger_cfg = config.chargers.iter().find(|c| c.name == st.name);
            // AC→DC: `charge_kw` is house AC draw, so `charged_kwh` is DC energy into the car —
            // η per kWh minus the fixed onboard overhead per hour the session is on.
            let efficiency = charger_cfg.map(|c| c.efficiency).unwrap_or(1.0);
            let overhead_kw = charger_cfg.map(|c| c.overhead_kw).unwrap_or(0.0);
            EvChargerPlan {
                name: st.name.clone(),
                status: st.status().to_string(),
                on_our_charger: st.on_our_charger,
                controllable_now: st.controllable_now,
                charging_elsewhere: st.charging_elsewhere,
                soc_pct: st.soc_pct,
                target_pct: st.target_pct,
                target_capped: st.target_capped,
                capacity_kwh: st.capacity_kwh,
                active_car: st.active_car.clone(),
                strategy: ev_prefs
                    .get(&st.name)
                    .and_then(|p| p.strategy)
                    .or_else(|| charger_cfg.map(|c| c.strategy))
                    .unwrap_or_default(),
                charger_power_kw: st.charger_power_kw,
                deadline_source: st.deadline_source.clone(),
                deadline_hm: st.deadline_hm.clone(),
                deadline_at: st.deadline_at,
                charged_kwh: charge_kw
                    .iter()
                    .zip(&dt_vec)
                    .map(|(&kw, &dt)| {
                        if kw > 1e-6 {
                            (kw * efficiency - overhead_kw).max(0.0) * dt
                        } else {
                            0.0
                        }
                    })
                    .sum(),
                charge_kw,
                solar_kw: plan.ev_solar_kw.get(&st.name).cloned().unwrap_or_default(),
                grid_kw: plan.ev_grid_kw.get(&st.name).cloned().unwrap_or_default(),
                batt_kw: plan.ev_batt_kw.get(&st.name).cloned().unwrap_or_default(),
            }
        })
        .collect();

    // On-the-mark switching: block 1 with its start instant, computed once so the borrow below completes before
    // `timeline` is moved into the struct literal — see `next_timeline_step`'s doc.
    let next_step = next_timeline_step(&timeline);

    // The kernels this plan was actually built with — reused from `extras.kernels` (the startup
    // cache) when present, else rebuilt exactly as `unified_lp_inputs` would have internally
    // (same cost either way; `pull_break_even_eur_per_kwh` only runs this report-side lookup,
    // never a second solve).
    let built_kernel_cache;
    let pull_kernels: &HashMap<(String, String), Vec<f64>> = match extras.kernels.as_deref() {
        Some(k) => &k.kernels,
        None => {
            built_kernel_cache = build_kernel_cache(config, net, ss);
            &built_kernel_cache.kernels
        }
    };

    Ok(PlanReport {
        horizon_hours: HORIZON_HOURS,
        total_cost_eur: plan.total_cost,
        total_cost_czk: config.tariff.eur_to_czk(plan.total_cost),
        eur_czk_rate: config.tariff.eur_czk_rate,
        grid_import_kwh: sum_kwh(&plan.grid_import_kw),
        grid_export_kwh: sum_kwh(&plan.grid_export_kw),
        pv_curtailed_kwh: sum_kwh(&plan.curtail_kw),
        heating_kwh: plan.heat_kw.values().map(|v| sum_kwh(v)).sum(),
        cooling_kwh: plan.cool_kw.values().map(|v| sum_kwh(v)).sum(),
        hvac_heating_kwh: plan.hvac_heat_kw.values().map(|v| sum_kwh(v)).sum(),
        battery_charge_kwh: sum_kwh(&plan.charge_kw),
        battery_discharge_kwh,
        battery_wear_czk: battery_discharge_kwh * config.tariff.battery_amortisation_czk,
        final_soc_kwh: plan.soc_kwh.last().copied().unwrap_or_default(),
        pv_raw_kwh,
        pv_calibrated_kwh,
        pv_calibration_scale,
        pv_nowcast,
        placeholder_inputs: placeholders,
        degraded,
        relaxed,
        rounded,
        first_step,
        timeline,
        next_step,
        ev: ev_plan,
        p10_surplus_kwh,
        curtailment_risk_kwh,
        disturbance_w,
        terminal_heat_credit_eur_per_kwh: plan.terminal_heat_credit.clone(),
        export_pv_gated_blocks: plan.export_pv_gated_blocks,
        terminal_soc_value_eur_per_kwh: ctx.terminal_value,
        terminal_soc_value_source: terminal_soc_value_source.to_string(),
        pull_cost_eur: plan.pull_cost,
        pull_kh: plan.pull_kh.clone(),
        relay_switches: plan.relay_switches.values().sum(),
        relay_switches_by_zone: plan.relay_switches.clone(),
        room_modes: plan.room_modes.clone(),
        pull_break_even_eur_per_kwh: pull_break_even_eur_per_kwh(&config.heating, pull_kernels),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimize::config::{SiteConfig, TariffConfig};
    use chrono::FixedOffset;

    pub(super) fn tariff() -> TariffConfig {
        TariffConfig::default() // eur_czk 25; dist 0.919/0.281; sell_fee/export_min 0.5; amort 1.0; inv_off -2.0
    }

    pub(super) fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn hourly_to_blocks_aligns_on_calendar_hours() {
        use chrono::TimeZone;
        // On-the-hour start: plain repeat-4, last hour covers the tail.
        let at = |h: i64, m: i64| Utc.timestamp_opt(h * 3600 + m * 60, 0).single().unwrap();
        let start = at(14, 0);
        let blocks = hourly_to_blocks(start, &[1.0, 2.0, 3.0]);
        assert_eq!(blocks.len(), 3 * BLOCKS_PER_HOUR);
        assert!(blocks[0..BLOCKS_PER_HOUR].iter().all(|&v| v == 1.0));
        assert!(blocks[BLOCKS_PER_HOUR..2 * BLOCKS_PER_HOUR]
            .iter()
            .all(|&v| v == 2.0));

        // Mid-hour start (14:45): hourly[0] is the 14:00 calendar-hour value, so only block 0
        // (midpoint 14:52) carries it; blocks 1..4 lie in the 15:00 hour → hourly[1]. Repeating
        // from the start would wrongly stretch hourly[0] to 15:30.
        let start = at(14, 45);
        let blocks = hourly_to_blocks(start, &[1.0, 2.0, 3.0]);
        assert_eq!(blocks[0], 1.0); // 14:45–15:00 → hour 14
        assert!(blocks[1..5].iter().all(|&v| v == 2.0)); // 15:00–16:00 → hour 15
        assert_eq!(blocks[5], 3.0); // 16:00 hour begins
        assert_eq!(*blocks.last().unwrap(), 3.0); // tail clamps to the last hourly value
    }

    /// A stored forecast shorter than the requested outlook must truncate
    /// the outlook to the covered hours — never forward-fill a flat guess over the uncovered tail.
    #[test]
    fn outlook_from_weather_truncates_to_covered_hours() {
        use chrono::TimeZone;
        let start = Utc.timestamp_opt(0, 0).single().unwrap();
        // 10 requested hours, only 4 actually backed by a real sample (the rest of `temperature_c`
        // is `weather_forecast`'s own forward-filled flat tail, which must NOT reach the outlook).
        let owf = WeatherForecast {
            temperature_c: vec![1.0, 2.0, 3.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0],
            cloud_cover: vec![0.1, 0.2, 0.3, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4],
            covered_hours: 4,
            cloud_covered_hours: 4,
            solar: vec![SolarInput::Cloud { cloud: 0.1 }; 10],
            radiation_covered_hours: 0,
        };
        let outlook = outlook_from_weather(start, &owf).expect("some coverage");
        assert_eq!(
            outlook.temperature_c.len(),
            4 * BLOCKS_PER_HOUR,
            "must truncate to the 4 covered hours, not the requested 10"
        );
        assert_eq!(outlook.cloud_cover.len(), 4 * BLOCKS_PER_HOUR);
        assert_eq!(outlook.solar.len(), 4 * BLOCKS_PER_HOUR);
        // The covered values themselves must be exactly the real (non-forward-filled) samples.
        assert_eq!(outlook.temperature_c[0], 1.0);
        assert!((*outlook.temperature_c.last().unwrap() - 4.0).abs() < 1e-9);
    }

    /// Zero covered hours (an unusable forecast) yields no outlook at all, not an empty-but-`Some`
    /// one.
    #[test]
    fn outlook_from_weather_zero_coverage_yields_none() {
        use chrono::TimeZone;
        let start = Utc.timestamp_opt(0, 0).single().unwrap();
        let owf = WeatherForecast {
            temperature_c: vec![24.0; 6],
            cloud_cover: vec![0.3; 6],
            covered_hours: 0,
            cloud_covered_hours: 0,
            solar: vec![SolarInput::Cloud { cloud: 0.3 }; 6],
            radiation_covered_hours: 0,
        };
        assert!(outlook_from_weather(start, &owf).is_none());
    }

    /// A UTC site (fixed offset 0, no IANA zone) so test hour bins map 1:1.
    pub(super) fn utc_site() -> SiteConfig {
        SiteConfig {
            latitude: 49.5,
            longitude: 17.4,
            utc_offset_hours: 0,
            timezone: None,
            ground_temperature_c: 16.0,
            public_holidays: Vec::new(),
            easter_holidays: false,
        }
    }

    #[test]
    fn horizon_constants_are_consistent() {
        // 36 h × 4 blocks/h; everything downstream derives from these two.
        assert_eq!(HORIZON_BLOCKS, HORIZON_HOURS * BLOCKS_PER_HOUR);
        assert_eq!(BLOCK_SECONDS, 3600.0 / BLOCKS_PER_HOUR as f64);
        // The placeholder price curve spans the whole horizon (per-block local-hour keyed).
        let curve = placeholder_price_curve(
            utc("2024-01-15T00:00:00Z"),
            FixedOffset::east_opt(0).unwrap(),
            HORIZON_BLOCKS,
        );
        assert_eq!(curve.len(), HORIZON_BLOCKS);
    }
    /// A minimal, otherwise-zeroed [`TimelineBlock`] at `t`, for tests that only care about block
    /// identity/ordering (e.g. `next_timeline_step`).
    pub(super) fn test_block(t: DateTime<Utc>) -> TimelineBlock {
        TimelineBlock {
            t,
            dt_minutes: 15,
            import_price: 0.0,
            export_price: 0.0,
            price_is_placeholder: false,
            pv_kw: 0.0,
            load_kw: 0.0,
            soc_kwh: 0.0,
            charge_kw: 0.0,
            discharge_kw: 0.0,
            grid_import_kw: 0.0,
            grid_export_kw: 0.0,
            curtail_kw: 0.0,
            heat_kw: HashMap::new(),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            temp_c: HashMap::new(),
            outside_c: None,
            slot: "regular".to_string(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    // `next_step` is block 1 of the timeline, with `t = grid.block_start(1)`.
    // `TimelineBlock::t` is already `grid.block_start(b)` by construction (see the timeline-building
    // loop above) for every block including b=1, so this proves `next_timeline_step` SELECTS that
    // exact block rather than re-deriving `t` some other way.
    #[test]
    fn next_timeline_step_is_block_1() {
        let t0 = utc("2026-01-15T00:15:00Z");
        let t1 = utc("2026-01-15T00:30:00Z");
        let t2 = utc("2026-01-15T00:45:00Z");
        let timeline = vec![test_block(t0), test_block(t1), test_block(t2)];

        let step = next_timeline_step(&timeline);

        assert_eq!(step.map(|b| b.t), Some(t1));
    }

    // `next_step` is absent when the plan has < 2 blocks.
    #[test]
    fn next_timeline_step_is_absent_with_fewer_than_2_blocks() {
        assert!(next_timeline_step(&[]).is_none());
        assert!(next_timeline_step(&[test_block(utc("2026-01-15T00:15:00Z"))]).is_none());
    }
}
