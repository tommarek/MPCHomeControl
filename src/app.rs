//! Application-level assembly of the live whole-house state and plan.
//!
//! One source of truth shared by the CLI demos and the web API: it pulls the live data (estimated
//! thermal state, the self-corrected Solcast PV forecast, prices) and runs the unified optimizer,
//! returning serializable reports. The data layer (InfluxDB) and the models are passed in.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, Timelike, Utc};
use nalgebra::DVector;
use serde::Serialize;
use uom::si::{
    angle::degree,
    f64::{Angle, Power, Ratio},
    power::kilowatt,
    ratio::ratio,
};

use crate::estimate::estimate_initial_state;
use crate::forecast::calibration::Calibration;
use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::live_inputs::{battery_soc_kwh, block_prices, train_consumption, weather_forecast};
use crate::optimize::battery::BatterySpec;
use crate::optimize::config::{BatteryConfig, ControlConfig, PvConfig, TariffConfig};
use crate::optimize::coordinator::{plan_unified, ForecastContext};
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::solar_forecast::pv_forecast_kw;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::{c_to_k, k_to_c};
use crate::validate::{self, BacktestConfig, GainFit};

/// Planning horizon in hours (the span the weather/PV/consumption feeds are read over).
const HORIZON_HOURS: usize = 24;
/// Dispatch/mode resolution: 15-minute blocks, matching the OTE day-ahead price grid.
const BLOCKS_PER_HOUR: usize = 4;
const HORIZON_BLOCKS: usize = HORIZON_HOURS * BLOCKS_PER_HOUR;
const BLOCK_SECONDS: f64 = 900.0;

/// Repeat each hourly value across its `BLOCKS_PER_HOUR` 15-minute blocks (forward-fill within the
/// hour) so an hourly feed (weather, PV) aligns to the block grid.
fn expand_to_blocks(hourly: &[f64]) -> Vec<f64> {
    hourly
        .iter()
        .flat_map(|&v| std::iter::repeat_n(v, BLOCKS_PER_HOUR))
        .collect()
}

/// Value (EUR/kWh) of the energy left in the battery at the horizon end — the avoided future import
/// that stored charge represents. Mirrors the loxone MILP terminal value, which is what actually
/// keeps the battery from draining at the horizon edge (loxone's reserve-SoC floor is computed for
/// transparency but penalised at **zero** — the overnight hold is emergent from the objective, not a
/// per-block floor, which would double-count and force "grid-charge at the evening peak").
///
/// = **median** import (spot+dist) net of wear over the horizon (the *typical* worth of stored
/// energy, not the cheapest, which under-values it), **capped** at the cheapest grid-charge
/// break-even (`min_import / round_trip_η`) so it can never alone justify buying grid power to hoard
/// SoC, floored at 0. Apply it to leftover SoC times the discharge-leg efficiency at the call site.
fn terminal_soc_value(import_price: &[f64], amortisation: f64, round_trip_eta: f64) -> f64 {
    if import_price.is_empty() {
        return 0.0;
    }
    let mut net: Vec<f64> = import_price.iter().map(|&p| p - amortisation).collect();
    net.sort_by(f64::total_cmp);
    // The true statistical median (averaged two middle elements on an even horizon) — the principled
    // "typical" avoided-import value, with no upward bias toward the higher middle element.
    let mid = net.len() / 2;
    let median = if net.len() % 2 == 1 {
        net[mid]
    } else {
        (net[mid - 1] + net[mid]) / 2.0
    }
    .max(0.0);
    let cheapest = import_price.iter().cloned().fold(f64::INFINITY, f64::min);
    let break_even = if cheapest.is_finite() {
        cheapest / round_trip_eta.max(1e-3)
    } else {
        0.0
    };
    (median.min(break_even)).max(0.0) * 0.99
}

/// A single 10 kWp south-facing array — the fallback when no PV arrays are configured.
pub fn default_pv_array() -> PvArray {
    PvArray {
        peak_power: Power::new::<kilowatt>(10.0),
        tilt: Angle::new::<degree>(30.0),
        azimuth: Angle::new::<degree>(180.0),
        system_efficiency: Ratio::new::<ratio>(0.85),
    }
}

/// Build the configured PV arrays (used for the clear-sky fallback forecast). Falls back to a
/// single [`default_pv_array`] when none are configured.
pub fn pv_arrays(cfg: &PvConfig) -> Vec<PvArray> {
    if cfg.arrays.is_empty() {
        return vec![default_pv_array()];
    }
    let efficiency = Ratio::new::<ratio>(cfg.system_efficiency.clamp(0.0, 1.0));
    cfg.arrays
        .iter()
        .map(|a| PvArray {
            peak_power: Power::new::<kilowatt>(a.kwp),
            tilt: Angle::new::<degree>(a.tilt),
            azimuth: Angle::new::<degree>(a.azimuth),
            system_efficiency: efficiency,
        })
        .collect()
}

/// Clear-sky PV forecast (kW per 15-min block) summed across `arrays`, derated by the cloud cover.
/// Used only when the live Solcast forecast is unavailable.
fn clearsky_pv_kw(
    arrays: &[PvArray],
    latitude: Angle,
    longitude: Angle,
    start: DateTime<Utc>,
    cloud_cover: &[f64],
) -> Vec<f64> {
    (0..HORIZON_BLOCKS)
        .map(|b| {
            // Sample at the block midpoint to share the block-average convention of the load.
            let t = start + Duration::seconds((BLOCK_SECONDS * (b as f64 + 0.5)) as i64);
            let cloud =
                Ratio::new::<ratio>(cloud_cover.get(b).copied().unwrap_or(0.3).clamp(0.0, 1.0));
            arrays
                .iter()
                .map(|a| a.predict(latitude, longitude, &t, cloud).get::<kilowatt>())
                .sum()
        })
        .collect()
}

/// Apply the real tariff to a spot-price series, returning `(import_price, export_price)` in
/// EUR/kWh per 15-min block. Import adds the VT/NT distribution surcharge for each block's local hour;
/// export is the spot minus the sell fee, floored at 0 (no benefit to exporting below the fee) and
/// capped at the import price so the dispatch LP's `export ≤ import` precondition always holds.
fn tariff_prices(
    tariff: &TariffConfig,
    spot_price: &[f64],
    start: DateTime<Utc>,
    local_offset: FixedOffset,
) -> (Vec<f64>, Vec<f64>) {
    let mask = tariff.low_tariff_mask();
    let sell_fee = tariff.sell_fee_eur();
    spot_price
        .iter()
        .enumerate()
        .map(|(b, &spot)| {
            let local_hour = (start + Duration::seconds((BLOCK_SECONDS * b as f64) as i64))
                .with_timezone(&local_offset)
                .hour();
            let import = spot + tariff.distribution_eur(local_hour, &mask);
            // `.min(import)` is load-bearing: it guarantees export ≤ import for ALL spot prices
            // (including deeply-negative hours where `import` itself goes negative), which the
            // dispatch LP requires to stay bounded. Do not "simplify" it to a bare `.max(0.0)`.
            let export = (spot - sell_fee).max(0.0).min(import);
            (import, export)
        })
        .unzip()
}

/// The recommended Growatt configuration for one 15-min block: the inverter **slot mode** plus the
/// two **independent, price-gated toggles** — export enable/disable and the inverter on/off master
/// switch — mirroring how the live controller is actually set up. It is a read-off of the recommended
/// intent, not a literal echo of the controller's register; the brain itself never actuates — the
/// **armed** controllers apply it downstream (Growatt the battery, loxone the heating/EV).
#[derive(Debug, Clone, Serialize)]
pub struct ModeStep {
    /// Battery action in `loxone_smart_home`'s vocabulary: `regular` / `charge_from_grid` /
    /// `discharge_to_grid` / `sell_production` / `battery_hold` / `inverter_off` (see [`classify_mode`]).
    pub slot: String,
    /// Whether grid export is enabled this block — an **orthogonal toggle** (the inverter can export
    /// in any battery mode), off below the export-floor spot price.
    pub export_enabled: bool,
    /// Whether the inverter is powered on (off only in deeply-negative-price blocks).
    pub inverter_on: bool,
    /// Recommended battery **charge** power this block (kW; 0 when not charging) — the explicit
    /// power the inverter's charge powerRate would be set to.
    pub charge_kw: f64,
    /// Recommended battery **discharge** power this block (kW; 0 when not discharging).
    pub discharge_kw: f64,
}

/// The battery action for one block, in `loxone_smart_home`'s published vocabulary
/// (`growatt_status.current_mode`), so the dashboard speaks the same battery-mode language the house
/// has always used: `regular` (self-consumption — including passive solar-charge / load-discharge,
/// which loxone also reports as `regular`), `charge_from_grid`, `discharge_to_grid`, `sell_production`
/// (exporting surplus solar with the battery passive), `battery_hold` (importing while the battery is
/// held for a pricier block), `inverter_off`.
///
/// **Export-enabled and inverter-on are orthogonal toggles** (settable in any mode) — they are tracked
/// separately on [`ModeStep`] and are NOT folded into this status.
#[allow(clippy::too_many_arguments)] // the flows, SoC band and inverter state are all distinct
fn classify_mode(
    charge_kw: f64,
    discharge_kw: f64,
    grid_import_kw: f64,
    grid_export_kw: f64,
    soc_kwh: f64,
    min_soc_kwh: f64,
    max_soc_kwh: f64,
    inverter_on: bool,
) -> &'static str {
    const EPS: f64 = 0.05; // kW — ignore solver dust
    if !inverter_on {
        "inverter_off"
    } else if charge_kw > EPS && grid_import_kw > EPS {
        "charge_from_grid"
    } else if discharge_kw > EPS && grid_export_kw > EPS {
        "discharge_to_grid"
    } else if charge_kw > EPS || discharge_kw > EPS {
        // Battery active without grid involvement (solar-charging / covering the load) — loxone
        // reports this as `regular`; the spill of a little surplus to grid is incidental, not a sale.
        "regular"
    } else if grid_export_kw > EPS && soc_kwh < max_soc_kwh - 0.2 {
        // Exporting surplus although the battery has room to store it — loxone forces the inverter to
        // grid-first here (otherwise load_first would quietly charge), which it reports as
        // `sell_production`. With a full battery the surplus exports passively in `regular` (below).
        "sell_production"
    } else if grid_import_kw > EPS && soc_kwh > min_soc_kwh + 0.2 {
        "battery_hold" // importing while the battery is available — held for a pricier block
    } else {
        "regular" // self-consumption / idle (incl. passive surplus export with a full battery)
    }
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

/// The current air temperature of one zone — the model estimate re-anchored to the latest measured
/// reading (see [`current_state`]), so it tracks reality including unmodelled disturbances.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneTemp {
    pub zone: String,
    pub temp_c: f64,
}

/// The current per-zone thermal state (measured-anchored).
#[derive(Debug, Clone, Serialize)]
pub struct StateReport {
    pub zones: Vec<ZoneTemp>,
}

/// One zone's recent **measured** temperature history, for the dashboard comfort-grid sparklines.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneSeries {
    pub zone: String,
    /// `(iso8601, °C)` samples, oldest first.
    pub series: Vec<(String, f64)>,
}

/// The whole-house dispatch plan over the horizon, plus the PV self-correction that fed it.
#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub horizon_hours: usize,
    pub total_cost_eur: f64,
    /// The same horizon cost converted to CZK (via the tariff's exchange rate) for local reporting.
    pub total_cost_czk: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    /// PV energy curtailed over the horizon (kWh) — solar neither used, stored, nor exported.
    pub pv_curtailed_kwh: f64,
    pub heating_kwh: f64,
    /// HVAC cooling energy delivered over the horizon (kWh) — 0 when no HVAC is configured.
    pub cooling_kwh: f64,
    /// HVAC air-side heating energy delivered over the horizon (kWh) — 0 when no HVAC is configured.
    pub hvac_heating_kwh: f64,
    pub battery_charge_kwh: f64,
    pub battery_discharge_kwh: f64,
    /// Implied battery wear cost over the horizon (CZK) = discharged kWh × amortisation.
    pub battery_wear_czk: f64,
    pub final_soc_kwh: f64,
    pub pv_raw_kwh: f64,
    pub pv_calibrated_kwh: f64,
    pub pv_calibration_scale: f64,
    /// Which **data feeds** fell back to placeholders this cycle (empty = all data feeds live).
    /// PV-array and battery hardware specs come from `config.json5`; a "PV (Solcast unavailable…)"
    /// entry here means the clear-sky model over those arrays stood in for the Solcast forecast.
    pub placeholder_inputs: Vec<String>,
    /// The controls the optimizer chose for the coming block — the battery plan drives the **armed**
    /// Growatt controller and the heating decisions the **armed** loxone controller (downstream).
    pub first_step: FirstStep,
    /// The full per-15-min-block plan as **timestamped rows** — prices, PV, SoC, battery, grid,
    /// heating and predicted temperature per controlled zone, plus the recommended Growatt mode.
    /// Chart-ready (one object per block) and the source for `/api/plan/timeline`.
    pub timeline: Vec<TimelineBlock>,
    /// Per-EV-charger live state + the optimizer's charge schedule. Empty when no charger is
    /// configured; the source for `/api/ev` and the dashboard EV screen.
    #[serde(default)]
    pub ev: Vec<EvChargerPlan>,
}

/// One EV charger's live fused state and the plan's charge schedule (per block) with its source
/// breakdown — the brain only reports it; the **armed** loxone controller drives the wallbox downstream.
#[derive(Debug, Clone, Serialize)]
pub struct EvChargerPlan {
    pub name: String,
    /// `charging` | `connected` | `charging_away` | `away` (see [`crate::ev::EvState::status`]).
    pub status: String,
    pub on_our_charger: bool,
    pub controllable_now: bool,
    pub charging_elsewhere: bool,
    /// Car state of charge (%), if a source provides it.
    pub soc_pct: Option<f64>,
    pub target_pct: f64,
    /// Usable battery capacity (kWh) used for %↔kWh (a `capacity` source, or the `battery_kwh` fallback).
    pub capacity_kwh: f64,
    /// Which car is on the wallbox (multi-car chargers); `None` for a single-car charger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_car: Option<String>,
    pub strategy: crate::optimize::config::EvStrategy,
    /// Live charge power our wallbox is delivering (kW).
    pub charger_power_kw: f64,
    /// Planned charge power (kW) per block, and its solar / grid / battery split.
    pub charge_kw: Vec<f64>,
    pub solar_kw: Vec<f64>,
    pub grid_kw: Vec<f64>,
    pub batt_kw: Vec<f64>,
    /// Energy the plan delivers to the car over the horizon (kWh).
    pub charged_kwh: f64,
}

/// One 15-minute block of the plan, as a flat timestamped row for charting and to verify the heat
/// model's forward prediction against measured data later. All powers are kW, prices price-units/kWh.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineBlock {
    /// Block start instant (UTC).
    pub t: DateTime<Utc>,
    pub import_price: f64,
    pub export_price: f64,
    /// Forecast PV generation (kW) — the calibrated Solcast curve, or the clear-sky fallback.
    pub pv_kw: f64,
    /// Forecast base house load (kW) — the consumption model's prediction the optimizer planned
    /// around (the predicted `INVPowerToLocalLoad`, charted vs the measured `house_kw`).
    pub load_kw: f64,
    /// Battery state of charge (kWh) at the end of the block.
    pub soc_kwh: f64,
    pub charge_kw: f64,
    pub discharge_kw: f64,
    pub grid_import_kw: f64,
    pub grid_export_kw: f64,
    /// PV curtailed this block (kW) — solar neither used, stored, nor exported.
    pub curtail_kw: f64,
    /// Underfloor-heating power (kW) per heated zone.
    pub heat_kw: HashMap<String, f64>,
    /// HVAC cooling power (kW) per HVAC zone.
    pub cool_kw: HashMap<String, f64>,
    /// HVAC air-side heating power (kW) per HVAC zone.
    pub hvac_heat_kw: HashMap<String, f64>,
    /// Controllable scheduled-load draw (kW) per load this block (`on · rated_kw`) — the load-shift
    /// schedule. Empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    /// **Predicted** air temperature (°C) per controlled zone at the end of the block.
    pub temp_c: HashMap<String, f64>,
    /// Recommended Growatt slot mode and the price-gated export / inverter levers — applied by the
    /// Growatt controller for the live block.
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
}

/// The live internal-gain self-correction, published by the MPC loop after each re-fit so the
/// `/api/calibration/gains` endpoint can report what the model is currently assuming and when it
/// was learnt (see [`crate::validate::fit_internal_gains`]).
#[derive(Debug, Clone, Serialize)]
pub struct GainsSnapshot {
    /// When this fit landed (UTC).
    pub fitted_at: DateTime<Utc>,
    /// Trailing window (days) the fit was run over.
    pub window_days: i64,
    /// The fitted per-zone internal gains (W) now in use by the plan.
    pub gains_w: HashMap<String, f64>,
    /// Per scheduled-load magnitude (W) now in use, aligned to `config.scheduled_loads` — each tagged
    /// `configured` (`power_w` set), `fitted` (learnt from data), or `measured` (driven by a `sensor`).
    pub scheduled: Vec<ScheduledFit>,
}

/// One scheduled load's magnitude as the plan currently sees it, for `/api/calibration/gains`.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledFit {
    /// The load's label, or its zone when the label is empty.
    pub label: String,
    /// The zone whose air node the load acts on.
    pub zone: String,
    /// The magnitude in use (W, ≥ 0); the sign comes from the load's `kind`. For a `"measured"` load
    /// this is the configured **forecast** magnitude (`power_w`, or 0) — the live flux tracks the sensor.
    pub magnitude_w: f64,
    /// `"measured"` if a `sensor` drives the flux from the real draw, else `"configured"` if `power_w`
    /// was set, else `"fitted"` (learnt from data).
    pub source: String,
}

/// A plan with the instant it was computed — what the MPC loop publishes for the API.
#[derive(Debug, Clone, Serialize)]
pub struct TimestampedPlan {
    pub computed_at: DateTime<Utc>,
    /// Monotonic instant the plan was published, for clock-jump-proof freshness checks (`/readyz`):
    /// a wall-clock step (NTP) mustn't make a fresh plan look stale. Skipped in serialization —
    /// `Instant` isn't serializable, and the wall-clock `computed_at` is what the API exposes.
    #[serde(skip)]
    pub published: Instant,
    pub plan: PlanReport,
}

/// The first-step (next 15-min block) decisions extracted from the plan.
#[derive(Debug, Clone, Serialize)]
pub struct FirstStep {
    /// Start instant of the first block (UTC).
    pub hour_start: DateTime<Utc>,
    /// Underfloor-heating power (kW) per heated zone for the coming block.
    pub heat_kw: HashMap<String, f64>,
    /// HVAC cooling power (kW) per HVAC zone for the coming block.
    pub cool_kw: HashMap<String, f64>,
    /// HVAC air-side heating power (kW) per HVAC zone for the coming block.
    pub hvac_heat_kw: HashMap<String, f64>,
    /// Controllable scheduled-load draw (kW) per load for the coming block (`on · rated_kw`, 0 when
    /// off) — the boiler controller's setpoint. Empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    pub battery_charge_kw: f64,
    pub battery_discharge_kw: f64,
    pub grid_import_kw: f64,
    pub grid_export_kw: f64,
    /// Recommended Growatt setup for the coming block — slot mode + the two toggles, applied by the
    /// Growatt controller.
    pub mode: ModeStep,
}

fn placeholder_price_curve(start: DateTime<Utc>, local_offset: FixedOffset) -> Vec<f64> {
    // The peak (17–20) / off-peak (1–5) windows are local-time tariff hours, so classify by the
    // *local* hour (cf. `tariff_prices`), not the UTC hour — otherwise the curve is shifted by the
    // site's UTC offset.
    let start_hour = start.with_timezone(&local_offset).hour() as usize;
    (0..HORIZON_BLOCKS)
        .map(|b| match (b / BLOCKS_PER_HOUR + start_hour) % 24 {
            17..=20 => 0.45,
            1..=5 => 0.10,
            _ => 0.25,
        })
        .collect()
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

/// Estimate the current thermal state (per-zone air temperature) from measured history.
pub async fn current_state(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    ground_temperature_c: f64,
) -> Result<StateReport> {
    let x0 =
        estimate_initial_state(db, net, ss, latitude, longitude, 72, ground_temperature_c).await?;
    let mut zones: Vec<ZoneTemp> = net
        .zone_indices
        .iter()
        .filter(|(z, _)| z.as_str() != "outside" && z.as_str() != "ground")
        .filter_map(|(zone, &node)| {
            ss.state_index(node).map(|s| ZoneTemp {
                zone: zone.clone(),
                temp_c: k_to_c(x0[s]),
            })
        })
        .collect();
    zones.sort_by(|a, b| a.zone.cmp(&b.zone));
    Ok(StateReport { zones })
}

/// Recent **measured** per-zone air-temperature series, for the comfort-grid sparklines. Unlike
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

/// The slow-changing plan inputs cached across the per-minute re-plans: the consumption model and
/// PV calibration are trained from days of history and don't change minute to minute. The loop
/// refreshes this every few minutes and reuses it, so only the fast state (zone temps, SoC) and the
/// horizon-aligned forecasts (weather, prices, PV) are re-read each minute.
#[derive(Debug, Clone)]
pub struct PlanCache {
    pub consumption: ConsumptionModel,
    pub calibration: Calibration,
    /// Per-zone internal gains (W) used by the plan. The MPC loop re-fits these from a trailing
    /// window (see [`fit_live_internal_gains`]) on its own slow cadence and writes them here; absent
    /// that, [`build_cache`] seeds them from the calibrated `heating` config values.
    pub internal_gains: HashMap<String, f64>,
    /// Fitted scheduled-load magnitudes (W, ≥ 0), aligned 1:1 to `config.scheduled_loads`. The MPC
    /// loop writes its live re-fit here; [`build_cache`] seeds them to zero (no effect) until the
    /// first fit lands.
    pub scheduled_w: Vec<f64>,
}

/// Build the cacheable slow inputs — the 7-day PV-calibration backtest and the trailing-window
/// consumption training (the two heaviest reads). Refreshed periodically by the MPC loop. The
/// internal gains start at the config baseline; the loop overwrites them with its live re-fit.
pub async fn build_cache(db: &SourceClients, config: &ControlConfig) -> PlanCache {
    let offset = config.site.utc_offset_hours;
    let calibration = match backtest_pv(db, offset, 7).await {
        Ok(bt) => Calibration::from_totals_default(bt.total_solcast_kwh, bt.total_actual_kwh),
        Err(_) => Calibration::neutral(),
    };
    let consumption = match train_consumption(db, config.consumption_history_days, offset).await {
        Ok(Some(m)) => m,
        _ => flat_consumption(),
    };
    PlanCache {
        consumption,
        calibration,
        internal_gains: config.heating.internal_gains(),
        // Configured magnitudes (fixed `power_w` used as-is, fitted loads 0) until the live re-fit
        // overwrites the fitted ones.
        scheduled_w: config
            .scheduled_loads
            .iter()
            .map(|l| l.power_w.unwrap_or(0.0))
            .collect(),
    }
}

/// Re-fit the live per-zone internal gains from a trailing window of measured temperatures and the
/// recorded heating relays — the self-correction that lets the model track changes in occupant
/// behaviour (more/fewer people, appliance and fireplace use) without any config or model edit.
///
/// Returns `Some(fit)` on a successful fit — including **empty** gains, which is the legitimate answer
/// "the data shows no extra gain is needed" (e.g. summer, or a fireplace that's stopped being used),
/// so the caller should trust it. The fit also carries the per-scheduled-load magnitudes (W, aligned
/// to `config.scheduled_loads`). Returns `None` only when the fit can't run (no data / sensors down),
/// so the caller keeps its last-good values rather than discarding them.
pub async fn fit_live_internal_gains(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
) -> Option<GainFit> {
    let window_days = config.internal_gain_window_days.max(3);
    let cfg = BacktestConfig {
        warmup_hours: 48, // relax the unknown slab seed before scoring
        window_hours: (window_days * 24 - 48).max(24),
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    let local_offset = match FixedOffset::east_opt(config.site.utc_offset_hours * 3600) {
        Some(o) => o,
        None => FixedOffset::east_opt(0).unwrap(),
    };
    let start = format!("-{window_days}d");
    match validate::fit_internal_gains(
        db,
        net,
        ss,
        &config.heating,
        &config.scheduled_loads,
        local_offset,
        latitude,
        longitude,
        &cfg,
        &start,
        "now()",
    )
    .await
    {
        Ok(fit) => Some(fit),
        Err(e) => {
            eprintln!("[mpc] internal-gain re-fit failed ({e}); keeping previous gains");
            None
        }
    }
}

/// Build the live whole-house plan: self-corrected Solcast PV + estimated state → unified optimizer.
/// `cache` supplies the slow inputs (consumption + calibration) when the loop has them; `None` reads
/// them fresh (the on-demand web path).
pub async fn current_plan(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
    cache: Option<&PlanCache>,
) -> Result<PlanReport> {
    let offset = config.site.utc_offset_hours;
    let ground_temperature_c = config.site.ground_temperature_c;
    let mut placeholders: Vec<String> = Vec::new();

    let local_offset =
        FixedOffset::east_opt(offset * 3600).context("invalid site.utc_offset_hours")?;
    // Align the plan to the current 15-minute block boundary, so block 0 is the block we're in.
    let now = Utc::now();
    let start = now
        .with_minute((now.minute() / 15) * 15)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);

    // Seed the thermal state from measured history; fall back to a flat guess.
    let x0 = estimate_initial_state(db, net, ss, latitude, longitude, 72, ground_temperature_c)
        .await
        .unwrap_or_else(|_| DVector::from_element(ss.n_states(), c_to_k(22.0)));

    // Outside temperature + cloud forecast — fall back to flat if unavailable (also feeds the
    // clear-sky PV fallback below, so it is read first).
    let (temperature_c, cloud_cover) = match weather_forecast(db, start, HORIZON_HOURS).await? {
        Some((temp, cloud)) => (expand_to_blocks(&temp), expand_to_blocks(&cloud)),
        None => {
            placeholders
                .push("outside temperature + cloud (forecast unavailable; flat)".to_string());
            (vec![24.0; HORIZON_BLOCKS], vec![0.3; HORIZON_BLOCKS])
        }
    };

    // PV: prefer the self-corrected Solcast forecast (it already covers every array); fall back to
    // the clear-sky model over the configured arrays when Solcast is unavailable. The calibration
    // is fit from the last week's Solcast-vs-actual and recomputed each cycle.
    let calibration = match cache {
        Some(c) => c.calibration,
        None => match backtest_pv(db, offset, 7).await {
            Ok(bt) => Calibration::from_totals_default(bt.total_solcast_kwh, bt.total_actual_kwh),
            Err(_) => Calibration::neutral(),
        },
    };
    let solcast = pv_forecast_kw(db, start, HORIZON_HOURS, offset)
        .await
        .ok()
        .filter(|v| v.iter().sum::<f64>() > 0.0);
    let (raw_pv, pv_kw, pv_calibration_scale) = match solcast {
        Some(hourly) => {
            let raw = expand_to_blocks(&hourly);
            let calibrated = calibration.apply_series(&raw);
            (raw, calibrated, calibration.scale())
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
            (clear_sky.clone(), clear_sky, 1.0)
        }
    };
    // raw_pv / pv_kw are per-block kW; sum × block-hours = kWh over the horizon.
    let pv_raw_kwh: f64 = raw_pv.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);
    let pv_calibrated_kwh: f64 = pv_kw.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);

    // Day-ahead spot prices (EUR/kWh) from OTE — fall back to the placeholder curve if not yet
    // published or unreadable (a transient DB error must not fail the whole planning cycle).
    let spot_price = match block_prices(db, start, HORIZON_BLOCKS).await {
        Ok(Some(blocks)) => {
            // Use real prices where published; fill only the unpublished tail (e.g. tomorrow before
            // the ~14:00 auction) with the placeholder curve, and flag how much fell back.
            let placeholder = placeholder_price_curve(start, local_offset);
            let missing = blocks.iter().filter(|p| p.is_none()).count();
            if missing > 0 {
                placeholders.push(format!(
                    "day-ahead prices ({missing}/{HORIZON_BLOCKS} blocks unpublished; placeholder)"
                ));
            }
            blocks
                .iter()
                .enumerate()
                .map(|(b, p)| p.unwrap_or(placeholder[b]))
                .collect()
        }
        Ok(None) | Err(_) => {
            placeholders.push("day-ahead prices (unavailable; placeholder curve)".to_string());
            placeholder_price_curve(start, local_offset)
        }
    };
    // Apply the real Czech tariff: import = spot + distribution (VT/NT by local hour); export =
    // spot − sell fee. This is the same economics the live loxone controller sees.
    let (import_price, export_price) =
        tariff_prices(&config.tariff, &spot_price, start, local_offset);

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
        None => match train_consumption(db, config.consumption_history_days, offset).await? {
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
    let terminal_value = terminal_soc_value(
        &import_price,
        battery_amortisation,
        battery.charge_efficiency * battery.discharge_efficiency,
    );

    let ctx = ForecastContext {
        latitude,
        longitude,
        start,
        step_seconds: BLOCK_SECONDS,
        local_offset,
        temperature_c,
        ground_temperature_c,
        cloud_cover,
        // Live-fitted gains from the loop's cache; the on-demand path (no cache) uses the config baseline.
        internal_gain_w: cache
            .map(|c| c.internal_gains.clone())
            .unwrap_or_else(|| config.heating.internal_gains()),
        scheduled_loads: config.scheduled_loads.clone(),
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
                    .map(|l| l.power_w.unwrap_or(0.0))
                    .collect()
            }),
        export_price,
        export_allowed: export_allowed.clone(),
        inverter_on: inverter_on.clone(),
        battery_amortisation,
        terminal_value,
        import_price,
        min_final_soc_kwh: Some(battery.min_soc_kwh),
        pv_kw_override: Some(pv_kw),
        load_scale: 1.0,
    };

    // Ignored while `pv_kw_override` is set; pass the configured array so the non-override path stays
    // consistent with the live forecast.
    let primary_pv = pv_arrays(&config.pv)
        .first()
        .copied()
        .unwrap_or_else(default_pv_array);
    let hvac = config.hvac.clone().unwrap_or_default();
    // EV chargers: fuse each charger's live state + config + dashboard prefs into optimizer inputs.
    let ev_prefs = crate::ev::prefs::load();
    let ev = crate::ev::build_inputs(
        db,
        &config.chargers,
        start,
        HORIZON_BLOCKS,
        BLOCK_SECONDS,
        local_offset,
        &ev_prefs,
    )
    .await;
    let plan = plan_unified(
        &primary_pv,
        &consumption,
        &battery,
        &config.heating,
        &hvac,
        ss,
        net,
        &ctx,
        &x0,
        &ev.specs,
        &ev.monitored_kw,
    )?;

    // The full plan as timestamped per-block rows: the optimizer's flows + the inverter slot mode
    // (classified from those flows) + the price-gated export / inverter levers, with the forecast
    // prices/PV that fed the block and the predicted per-zone temperature it produced.
    let pv_series = ctx.pv_kw_override.as_deref().unwrap_or(&[]);
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
            let soc = at(&plan.soc_kwh);
            // Asymmetric safe defaults for a missing block: inverter ON (off is the rare
            // deeply-negative-price state), but export OFF (an unknown gate must not claim export).
            let inverter = inverter_on.get(b).copied().unwrap_or(true);
            TimelineBlock {
                t: start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64),
                import_price: ctx.import_price.get(b).copied().unwrap_or(0.0),
                export_price: ctx.export_price.get(b).copied().unwrap_or(0.0),
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
                temp_c: at_block(&plan.zone_temp_c, b),
                slot: classify_mode(
                    charge,
                    discharge,
                    grid_import,
                    grid_export,
                    soc,
                    battery.min_soc_kwh,
                    battery.max_soc_kwh,
                    inverter,
                )
                .to_string(),
                // Safe default: export disabled if the per-block gate is unavailable.
                export_enabled: export_allowed.get(b).copied().unwrap_or(false),
                inverter_on: inverter,
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

    let dt_h = BLOCK_SECONDS / 3600.0; // block duration in hours

    let sum_kwh = |v: &[f64]| v.iter().sum::<f64>() * dt_h;
    let battery_discharge_kwh = sum_kwh(&plan.discharge_kw);
    // Per-charger EV plan: the live fused state joined to the optimizer's schedule + source split.
    let ev_plan: Vec<EvChargerPlan> = ev
        .states
        .iter()
        .map(|st| {
            let charge_kw = plan.ev_charge_kw.get(&st.name).cloned().unwrap_or_default();
            let charger_cfg = config.chargers.iter().find(|c| c.name == st.name);
            // AC→DC: `charge_kw` is house AC draw, so `charged_kwh` is DC energy into the car.
            let efficiency = charger_cfg.map(|c| c.efficiency).unwrap_or(1.0);
            EvChargerPlan {
                name: st.name.clone(),
                status: st.status().to_string(),
                on_our_charger: st.on_our_charger,
                controllable_now: st.controllable_now,
                charging_elsewhere: st.charging_elsewhere,
                soc_pct: st.soc_pct,
                target_pct: st.target_pct,
                capacity_kwh: st.capacity_kwh,
                active_car: st.active_car.clone(),
                strategy: ev_prefs
                    .get(&st.name)
                    .and_then(|p| p.strategy)
                    .or_else(|| charger_cfg.map(|c| c.strategy))
                    .unwrap_or_default(),
                charger_power_kw: st.charger_power_kw,
                charged_kwh: charge_kw.iter().sum::<f64>() * dt_h * efficiency,
                charge_kw,
                solar_kw: plan.ev_solar_kw.get(&st.name).cloned().unwrap_or_default(),
                grid_kw: plan.ev_grid_kw.get(&st.name).cloned().unwrap_or_default(),
                batt_kw: plan.ev_batt_kw.get(&st.name).cloned().unwrap_or_default(),
            }
        })
        .collect();

    Ok(PlanReport {
        horizon_hours: HORIZON_HOURS,
        total_cost_eur: plan.total_cost,
        total_cost_czk: config.tariff.eur_to_czk(plan.total_cost),
        grid_import_kwh: sum_kwh(&plan.grid_import_kw),
        grid_export_kwh: sum_kwh(&plan.grid_export_kw),
        pv_curtailed_kwh: sum_kwh(&plan.curtail_kw),
        heating_kwh: plan.heat_kw.values().flatten().sum::<f64>() * dt_h,
        cooling_kwh: plan.cool_kw.values().flatten().sum::<f64>() * dt_h,
        hvac_heating_kwh: plan.hvac_heat_kw.values().flatten().sum::<f64>() * dt_h,
        battery_charge_kwh: sum_kwh(&plan.charge_kw),
        battery_discharge_kwh,
        battery_wear_czk: battery_discharge_kwh * config.tariff.battery_amortisation_czk,
        final_soc_kwh: plan.soc_kwh.last().copied().unwrap_or_default(),
        pv_raw_kwh,
        pv_calibrated_kwh,
        pv_calibration_scale,
        placeholder_inputs: placeholders,
        first_step,
        timeline,
        ev: ev_plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tariff() -> TariffConfig {
        TariffConfig::default() // eur_czk 25; dist 0.919/0.281; sell_fee/export_min 0.5; amort 1.0; inv_off -2.0
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn placeholder_curve_classifies_by_local_hour() {
        // 23:00 UTC. In UTC+2 that is 01:00 local → off-peak (0.10); the curve must use the local hour.
        let start = utc("2024-01-01T23:00:00Z");
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert!((placeholder_price_curve(start, plus2)[0] - 0.10).abs() < 1e-9);
        // The same instant is 23:00 in UTC → the regular band (0.25), not off-peak.
        let utc0 = FixedOffset::east_opt(0).unwrap();
        assert!((placeholder_price_curve(start, utc0)[0] - 0.25).abs() < 1e-9);
    }

    #[test]
    fn terminal_value_uses_median_capped_at_break_even() {
        // Break-even cap binds (median 0.30 > cheapest/η = 0.10/0.85 ≈ 0.1176): terminal ≈ 0.1176·0.99.
        let t = terminal_soc_value(&[0.10, 0.20, 0.30, 0.40], 0.0, 0.85);
        assert!((t - 0.10 / 0.85 * 0.99).abs() < 1e-9, "got {t}");
        // The break-even cap (cheapest/η) values leftover SoC above a bare cheapest×0.99 floor.
        assert!(t > 0.10 * 0.99);

        // Median binds when it's the smaller (flat prices, high wear): median (0.40−0.30)=0.10 < cap 0.40.
        let t = terminal_soc_value(&[0.40, 0.40, 0.40, 0.40], 0.30, 1.0);
        assert!((t - 0.10 * 0.99).abs() < 1e-9, "got {t}");

        // True (averaged) median on an even horizon where the median binds under the cap:
        // [.10,.11,.12,.50] → (.11+.12)/2 = .115 < cap .10/.8 = .125 (loxone's upper-middle would be .12).
        let t = terminal_soc_value(&[0.10, 0.11, 0.12, 0.50], 0.0, 0.80);
        assert!((t - 0.115 * 0.99).abs() < 1e-9, "got {t}");
    }

    #[test]
    fn terminal_value_floors_at_zero_on_negative_prices() {
        // Cheapest negative ⇒ break-even cap is negative ⇒ floored at 0 (never hoard via grid charge).
        assert_eq!(terminal_soc_value(&[-0.10, 0.20], 0.0, 0.85), 0.0);
        assert_eq!(terminal_soc_value(&[], 0.0, 0.85), 0.0);
    }

    #[test]
    fn expand_to_blocks_repeats_each_hour() {
        let blocks = expand_to_blocks(&[1.0, 2.0, 3.0]);
        assert_eq!(blocks.len(), 3 * BLOCKS_PER_HOUR);
        assert!(blocks[0..BLOCKS_PER_HOUR].iter().all(|&v| v == 1.0));
        assert!(blocks[BLOCKS_PER_HOUR..2 * BLOCKS_PER_HOUR]
            .iter()
            .all(|&v| v == 2.0));
    }

    #[test]
    fn classify_mode_uses_loxone_vocabulary() {
        // Args: charge, discharge, grid_import, grid_export, soc_kwh, min_soc=2, max_soc=10, inverter.
        let m = |c, d, gi, ge, soc, inv| classify_mode(c, d, gi, ge, soc, 2.0, 10.0, inv);
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 5.0, false), "inverter_off"); // inverter paused
        assert_eq!(m(2.0, 0.0, 2.0, 0.0, 5.0, true), "charge_from_grid"); // grid-charging
        assert_eq!(m(0.0, 2.0, 0.0, 2.0, 5.0, true), "discharge_to_grid"); // battery → grid
        assert_eq!(m(0.0, 0.0, 0.0, 2.0, 5.0, true), "sell_production"); // export though battery has room
        assert_eq!(m(0.0, 0.0, 0.0, 2.0, 10.0, true), "regular"); // full battery — surplus exports passively
        assert_eq!(m(0.0, 0.0, 1.0, 0.0, 5.0, true), "battery_hold"); // importing, battery saved
        assert_eq!(m(0.0, 0.0, 1.0, 0.0, 2.0, true), "regular"); // importing at the SoC floor — not a hold
        assert_eq!(m(2.0, 0.0, 0.0, 1.0, 5.0, true), "regular"); // solar charge + tiny spill = self-use
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 5.0, true), "regular"); // self-consume / idle
    }

    #[test]
    fn tariff_prices_apply_distribution_and_sell_fee() {
        let t = tariff();
        let start = utc("2024-01-15T00:00:00Z");
        let offset = FixedOffset::east_opt(0).unwrap(); // local == UTC
                                                        // 48 15-min blocks = 12 h. Block 0 = 00:00 (NT/low), block 40 = 10:00 (VT/high).
        let spot = vec![0.10; 48]; // EUR/kWh
        let (import, export) = tariff_prices(&t, &spot, start, offset);
        assert!((import[0] - (0.10 + 0.281 / 25.0)).abs() < 1e-12); // NT
        assert!((import[40] - (0.10 + 0.919 / 25.0)).abs() < 1e-12); // VT at 10:00
                                                                     // Export = spot − sell fee, and never exceeds import (the LP precondition).
        assert!((export[0] - (0.10 - 0.5 / 25.0)).abs() < 1e-12);
        for h in 0..spot.len() {
            assert!(
                export[h] <= import[h] + 1e-12,
                "export must not exceed import at {h}"
            );
        }
    }

    #[test]
    fn tariff_prices_floor_export_below_the_sell_fee() {
        let t = tariff();
        let start = utc("2024-01-15T00:00:00Z");
        let offset = FixedOffset::east_opt(0).unwrap();
        // Spot below the export floor (0.5/25 = 0.02 EUR): export floored to 0, still ≤ import.
        let (import, export) = tariff_prices(&t, &[0.005; 24], start, offset);
        for h in 0..24 {
            assert_eq!(export[h], 0.0);
            assert!(export[h] <= import[h] + 1e-12);
        }
    }
}
