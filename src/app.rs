//! Application-level assembly of the live whole-house state and plan.
//!
//! One source of truth shared by the CLI demos and the web API: it pulls the live data (estimated
//! thermal state, the self-corrected Solcast PV forecast, prices) and runs the unified optimizer,
//! returning serializable reports. The data layer (InfluxDB) and the models are passed in.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, Timelike, Utc};
use nalgebra::DVector;
use serde::Serialize;
use uom::si::{
    angle::degree,
    f64::ThermodynamicTemperature,
    f64::{Angle, Power, Ratio},
    power::kilowatt,
    ratio::ratio,
    thermodynamic_temperature::{degree_celsius, kelvin},
};

use crate::estimate::estimate_initial_state;
use crate::forecast::calibration::Calibration;
use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::influxdb::InfluxDB;
use crate::live_inputs::{battery_soc_kwh, block_prices, train_consumption, weather_forecast};
use crate::optimize::battery::BatterySpec;
use crate::optimize::config::{BatteryConfig, ControlConfig, PvConfig, TariffConfig};
use crate::optimize::coordinator::{plan_unified, ForecastContext};
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::solar_forecast::pv_forecast_kw;
use crate::state_space::StateSpace;
use crate::validate::{self, BacktestConfig};

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

fn k_to_c(kelvin_value: f64) -> f64 {
    ThermodynamicTemperature::new::<kelvin>(kelvin_value).get::<degree_celsius>()
}

fn c_to_k(celsius: f64) -> f64 {
    ThermodynamicTemperature::new::<degree_celsius>(celsius).get::<kelvin>()
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

/// The recommended Growatt configuration for one 15-min block (shadow): the inverter **slot mode**
/// plus the two **independent, price-gated toggles** — export enable/disable and the inverter
/// on/off master switch — mirroring how the live controller is actually set up. It is a read-off of
/// the recommended intent, not a literal echo of the controller's register, and nothing is actuated.
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
/// (`growatt_status.current_mode`), so the dashboard and `/api/compare` line up directly with the
/// running controller: `regular` (self-consumption — including passive solar-charge / load-discharge,
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

/// A small home battery — the fallback when no battery is configured.
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

/// The estimated current air temperature of one zone.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneTemp {
    pub zone: String,
    pub temp_c: f64,
}

/// The model's estimated current thermal state.
#[derive(Debug, Clone, Serialize)]
pub struct StateReport {
    pub zones: Vec<ZoneTemp>,
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
    /// The controls the optimizer chose for the coming block — what a controller *would* apply
    /// (shadow only; nothing is actuated).
    pub first_step: FirstStep,
    /// The full per-15-min-block plan as **timestamped rows** — prices, PV, SoC, battery, grid,
    /// heating and predicted temperature per controlled zone, plus the recommended Growatt mode.
    /// Chart-ready (one object per block) and the source for `/api/plan/timeline`.
    pub timeline: Vec<TimelineBlock>,
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
    /// **Predicted** air temperature (°C) per controlled zone at the end of the block.
    pub temp_c: HashMap<String, f64>,
    /// Recommended Growatt slot mode and the price-gated export / inverter levers (shadow only).
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
}

/// The live internal-gain self-correction, published by the shadow loop after each re-fit so the
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
}

/// A plan with the instant it was computed — what the shadow loop publishes for the API.
#[derive(Debug, Clone, Serialize)]
pub struct TimestampedPlan {
    pub computed_at: DateTime<Utc>,
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
    pub battery_charge_kw: f64,
    pub battery_discharge_kw: f64,
    pub grid_import_kw: f64,
    pub grid_export_kw: f64,
    /// Recommended Growatt setup for the coming block (shadow only): slot mode + the two toggles.
    pub mode: ModeStep,
}

/// The placeholder day-ahead price curve, used until real OTE prices are published.
fn placeholder_price_curve(start: DateTime<Utc>) -> Vec<f64> {
    (0..HORIZON_BLOCKS)
        .map(
            |b| match (b / BLOCKS_PER_HOUR + start.hour() as usize) % 24 {
                17..=20 => 0.45,
                1..=5 => 0.10,
                _ => 0.25,
            },
        )
        .collect()
}

/// A flat fallback consumption model (used until real history can train one).
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
    db: &InfluxDB,
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

/// The slow-changing plan inputs cached across the per-minute re-plans: the consumption model and
/// PV calibration are trained from days of history and don't change minute to minute. The loop
/// refreshes this every few minutes and reuses it, so only the fast state (zone temps, SoC) and the
/// horizon-aligned forecasts (weather, prices, PV) are re-read each minute.
#[derive(Debug, Clone)]
pub struct PlanCache {
    pub consumption: ConsumptionModel,
    pub calibration: Calibration,
    /// Per-zone internal gains (W) used by the plan. The shadow loop re-fits these from a trailing
    /// window (see [`fit_live_internal_gains`]) on its own slow cadence and writes them here; absent
    /// that, [`build_cache`] seeds them from the calibrated `heating` config values.
    pub internal_gains: HashMap<String, f64>,
}

/// Build the cacheable slow inputs — the 7-day PV-calibration backtest and the trailing-window
/// consumption training (the two heaviest reads). Refreshed periodically by the shadow loop. The
/// internal gains start at the config baseline; the loop overwrites them with its live re-fit.
pub async fn build_cache(db: &InfluxDB, config: &ControlConfig) -> PlanCache {
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
    }
}

/// Re-fit the live per-zone internal gains from a trailing window of measured temperatures and the
/// recorded heating relays — the self-correction that lets the model track changes in occupant
/// behaviour (more/fewer people, appliance and fireplace use) without any config or model edit.
///
/// Returns `Some(gains)` on a successful fit — including an **empty** map, which is the legitimate
/// answer "the data shows no extra gain is needed" (e.g. summer, or a fireplace that's stopped being
/// used), so the caller should trust it. Returns `None` only when the fit can't run (no data /
/// sensors down), so the caller keeps its last-good gains rather than discarding them.
pub async fn fit_live_internal_gains(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
) -> Option<HashMap<String, f64>> {
    let window_days = config.internal_gain_window_days.max(3);
    let cfg = BacktestConfig {
        warmup_hours: 48, // relax the unknown slab seed before scoring
        window_hours: (window_days * 24 - 48).max(24),
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    let start = format!("-{window_days}d");
    match validate::fit_internal_gains(
        db,
        net,
        ss,
        &config.heating,
        latitude,
        longitude,
        &cfg,
        &start,
        "now()",
    )
    .await
    {
        Ok(gains) => Some(gains),
        Err(e) => {
            eprintln!("[mpc shadow] internal-gain re-fit failed ({e}); keeping previous gains");
            None
        }
    }
}

/// Build the live whole-house plan: self-corrected Solcast PV + estimated state → unified optimizer.
/// `cache` supplies the slow inputs (consumption + calibration) when the loop has them; `None` reads
/// them fresh (the on-demand web path).
pub async fn current_plan(
    db: &InfluxDB,
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
    // Align the plan to the current 15-minute block, so block 0 is the block we're in. The loop
    // re-plans every minute from fresh measurements; the horizon shifts a block at each :00/:15/
    // :30/:45 boundary, and within a block the re-plans refine the same grid against the latest data.
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
            let placeholder = placeholder_price_curve(start);
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
            placeholder_price_curve(start)
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
    // Battery wear (EUR/kWh discharged), and the value of energy left in the battery at the horizon
    // end — the cheapest import over the horizon, lightly discounted, so it isn't drained at the edge.
    let battery_amortisation = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);
    let cheapest_import = import_price.iter().cloned().fold(f64::INFINITY, f64::min);
    let terminal_value = if cheapest_import.is_finite() {
        cheapest_import * 0.99
    } else {
        0.0
    };

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
        Some(soc) => battery.initial_soc_kwh = soc.clamp(battery.min_soc_kwh, battery.max_soc_kwh),
        None => placeholders.push("battery SoC (telemetry unavailable; default)".to_string()),
    }

    let ctx = ForecastContext {
        latitude,
        longitude,
        start,
        step_seconds: BLOCK_SECONDS,
        local_offset,
        temperature_c,
        ground_temperature_c,
        cloud_cover,
        // The live-fitted gains from the loop's cache; the on-demand path (no cache) uses the config
        // baseline. This is the self-correction that tracks occupant-behaviour changes over time.
        internal_gain_w: cache
            .map(|c| c.internal_gains.clone())
            .unwrap_or_else(|| config.heating.internal_gains()),
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

    // The optimizer ignores this array while `pv_kw_override` is set; pass the primary configured
    // array so any future non-override path stays consistent with the live forecast.
    let primary_pv = pv_arrays(&config.pv)
        .first()
        .copied()
        .unwrap_or_else(default_pv_array);
    // The HVAC block is optional (the house has none today); an absent block ⇒ no HVAC actuators.
    let hvac = config.hvac.clone().unwrap_or_default();
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
        .map(|b| TimelineBlock {
            t: start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64),
            import_price: ctx.import_price.get(b).copied().unwrap_or(0.0),
            export_price: ctx.export_price.get(b).copied().unwrap_or(0.0),
            pv_kw: pv_series.get(b).copied().unwrap_or(0.0),
            soc_kwh: plan.soc_kwh.get(b).copied().unwrap_or(0.0),
            charge_kw: plan.charge_kw[b],
            discharge_kw: plan.discharge_kw[b],
            grid_import_kw: plan.grid_import_kw[b],
            grid_export_kw: plan.grid_export_kw[b],
            curtail_kw: plan.curtail_kw.get(b).copied().unwrap_or(0.0),
            heat_kw: at_block(&plan.heat_kw, b),
            cool_kw: at_block(&plan.cool_kw, b),
            hvac_heat_kw: at_block(&plan.hvac_heat_kw, b),
            temp_c: at_block(&plan.zone_temp_c, b),
            slot: classify_mode(
                plan.charge_kw[b],
                plan.discharge_kw[b],
                plan.grid_import_kw[b],
                plan.grid_export_kw[b],
                plan.soc_kwh.get(b).copied().unwrap_or(0.0),
                battery.min_soc_kwh,
                battery.max_soc_kwh,
                inverter_on.get(b).copied().unwrap_or(true),
            )
            .to_string(),
            export_enabled: export_allowed.get(b).copied().unwrap_or(true),
            inverter_on: inverter_on.get(b).copied().unwrap_or(true),
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

    // Per-block kW summed over the horizon → kWh needs the block duration (0.25 h at 15 min).
    let dt_h = BLOCK_SECONDS / 3600.0;
    let sum_kwh = |v: &[f64]| v.iter().sum::<f64>() * dt_h;
    let battery_discharge_kwh = sum_kwh(&plan.discharge_kw);
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
