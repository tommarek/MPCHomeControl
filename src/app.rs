//! Application-level assembly of the live whole-house state and plan.
//!
//! One source of truth shared by the CLI demos and the web API: it pulls the live data (estimated
//! thermal state, the self-corrected Solcast PV forecast, prices) and runs the unified optimizer,
//! returning serializable reports. The data layer (InfluxDB) and the models are passed in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{ensure, Result};
use chrono::{DateTime, Datelike, Duration, FixedOffset, Timelike, Utc};
use nalgebra::DVector;
use serde::Serialize;
use uom::si::{
    angle::degree,
    f64::{Angle, Power, Ratio},
    power::kilowatt,
    ratio::ratio,
};

use crate::estimate::estimate_initial_state;
use crate::forecast::calibration::{Calibration, PvBandCalibration};
use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::live_inputs::{
    battery_soc_kwh, block_prices, train_consumption, weather_forecast, BlockPrices,
    WeatherForecast,
};
use crate::optimize::battery::BatterySpec;
use crate::optimize::config::{BatteryConfig, ControlConfig, PvConfig, SiteConfig, TariffConfig};
use crate::optimize::coordinator::{
    kernel_inputs, plan_unified, ForecastContext, Outlook, PlanOptions,
};
use crate::optimize::thermal::{build_kernels, KernelSet};
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::solar_forecast::pv_forecast_kw;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::sun::SolarInput;
use crate::tools::{c_to_k, k_to_c};
use crate::validate::{self, BacktestConfig, GainFit};

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
/// for its own `start`/block 0, exposed so `mpc_loop`'s rule-1 pre-adoption (rework cycle 3: adopt
/// `committed_next` into `committed` BEFORE calling `current_plan`, so the first post-mark LP is
/// pinned from the start) can compute the anticipated new block without duplicating — and risking
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
fn hourly_to_blocks(start: DateTime<Utc>, hourly: &[f64]) -> Vec<f64> {
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
fn hourly_solar_to_blocks(start: DateTime<Utc>, hourly: &[SolarInput]) -> Vec<SolarInput> {
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

/// Mask of the horizon blocks belonging to the NEXT solar day — the daylight that will refill the
/// battery after tonight's candidate pre-charge. Before local noon that is *today* (an 02:00 plan
/// is squeezed by the sun rising a few hours later); from noon on it is tomorrow. Masking
/// "start's date + 1" unconditionally excluded today's daylight during exactly the overnight
/// cheap hours the pre-charge guard exists for, collapsing the p10 surplus toward zero there.
fn next_solar_day_mask(
    start: DateTime<Utc>,
    local_offset: FixedOffset,
    n_blocks: usize,
    block_seconds: f64,
) -> Vec<bool> {
    let local_start = start.with_timezone(&local_offset);
    let target = if local_start.hour() >= 12 {
        local_start.date_naive() + Duration::days(1)
    } else {
        local_start.date_naive()
    };
    (0..n_blocks)
        .map(|b| {
            let at = start
                + Duration::seconds(block_seconds as i64 * b as i64 + block_seconds as i64 / 2);
            at.with_timezone(&local_offset).date_naive() == target
        })
        .collect()
}

/// The next solar day's p10 PV surplus over the forecast house load, and the curtailment risk —
/// the part of that surplus the battery's current headroom cannot absorb. `next_solar_day` masks
/// the blocks of the coming daylight period (see `next_solar_day_mask`); the p10 percentile is conservatively LOW, so a positive risk means "even a bad
/// solar day fills the battery" — the trigger for the optional pre-charge guard.
fn p10_curtailment(
    p10_kw: &[f64],
    load_kw: &[f64],
    next_solar_day: &[bool],
    headroom_kwh: f64,
    dt_h: f64,
) -> (f64, f64) {
    let surplus: f64 = next_solar_day
        .iter()
        .enumerate()
        .filter(|&(_, &t)| t)
        .map(|(b, _)| {
            let p10 = p10_kw.get(b).copied().unwrap_or(0.0);
            let load = load_kw.get(b).copied().unwrap_or(0.0);
            (p10 - load).max(0.0) * dt_h
        })
        .sum();
    (surplus, (surplus - headroom_kwh.max(0.0)).max(0.0))
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
pub(crate) fn terminal_soc_value(
    import_price: &[f64],
    amortisation: f64,
    round_trip_eta: f64,
) -> f64 {
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
    // The break-even cap ("leftover SoC is worth at most re-acquiring it at the cheapest block")
    // only makes sense for a NON-NEGATIVE cheapest price: at a negative price the LP charges to
    // capacity there regardless (it is *paid* to), so the cap stops guarding a phantom
    // charge-and-credit loop and instead collapses the whole plan's terminal value to zero —
    // draining the battery at the horizon edge on any day with one negative-price block.
    let break_even = if cheapest.is_finite() && cheapest >= 0.0 {
        cheapest / round_trip_eta.max(1e-3)
    } else if cheapest.is_finite() {
        f64::INFINITY // negative cheapest: the median alone values the leftover SoC
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
/// EUR/kWh per 15-min block. Import adds the VT/NT distribution surcharge for each block's local hour
/// — the offset is derived **per block** ([`SiteConfig::offset_at`]), so the VT/NT classification
/// stays correct across a DST changeover inside the horizon (exactly the hours that shift);
/// export is the spot minus the sell fee, floored at 0 (no benefit to exporting below the fee) and
/// capped at the import price so the dispatch LP's `export ≤ import` precondition always holds.
pub(crate) fn tariff_prices(
    tariff: &TariffConfig,
    site: &SiteConfig,
    spot_price: &[f64],
    start: DateTime<Utc>,
) -> (Vec<f64>, Vec<f64>) {
    let mask = tariff.low_tariff_mask();
    let sell_fee = tariff.sell_fee_eur();
    spot_price
        .iter()
        .enumerate()
        .map(|(b, &spot)| {
            let at = start + Duration::seconds((BLOCK_SECONDS * b as f64) as i64);
            let local_hour = at.with_timezone(&site.offset_at(at)).hour();
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
/// One block's flows for [`classify_mode`]. The BATTERY grid legs are separate from the totals:
/// `charge_from_grid`/`discharge_to_grid` must key on what the *battery* exchanges with the grid —
/// the totals also carry EV grid charging and solar export, and using them let a
/// solar-charging-battery + EV-import block actuate forced AC charge (and battery→EV during solar
/// export actuate a battery drain to grid). The totals still drive `sell_production` (PV export)
/// and `battery_hold` (house importing while the battery sits).
struct BlockFlows {
    charge_kw: f64,
    discharge_kw: f64,
    /// Battery AC-charge from the grid only (no EV leg).
    batt_grid_charge_kw: f64,
    /// Battery→grid export only (no solar, no EV).
    batt_to_grid_kw: f64,
    /// Total grid import (incl. EV charging).
    grid_import_kw: f64,
    /// Total grid export (incl. solar).
    grid_export_kw: f64,
    soc_kwh: f64,
    inverter_on: bool,
}

fn classify_mode(
    f: &BlockFlows,
    min_soc_kwh: f64,
    max_soc_kwh: f64,
    min_dispatch_kw: f64,
) -> &'static str {
    const EPS: f64 = 0.05; // kW — ignore solver dust
                           // The ACTUATOR's floor (config `battery.min_dispatch_kw`): the Growatt controller rounds any
                           // nonzero powerrate UP to its minimum (~2.45 kW), so commanding a grid-charge/-discharge the LP
                           // planned at, say, 0.3 kW would actuate ~8× the planned energy at a price justified only for
                           // the smaller amount — and skew the SoC every following tick re-plans from. Demote sub-floor
                           // grid dispatch to `regular` instead: no dispatch tracks the plan far closer than 8× of it.
    let eps = EPS.max(min_dispatch_kw);
    let BlockFlows {
        charge_kw,
        discharge_kw,
        batt_grid_charge_kw,
        batt_to_grid_kw,
        grid_import_kw,
        grid_export_kw,
        soc_kwh,
        inverter_on,
    } = *f;
    if !inverter_on {
        "inverter_off"
    } else if batt_grid_charge_kw > eps {
        "charge_from_grid"
    } else if batt_to_grid_kw > eps {
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
    /// The disturbance observer's per-zone constant flux (W, + heats); present only when
    /// `estimator.disturbance` is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disturbance_w: Option<HashMap<String, f64>>,
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
    /// The CZK/EUR rate this plan was priced with (`tariff.eur_czk_rate`). Reported so a client
    /// converts with the SAME rate the optimizer used, instead of dividing the two cost totals —
    /// which is undefined on a near-zero-cost horizon and previously fell back to a hardcoded 25.
    pub eur_czk_rate: f64,
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
    /// `true` when a **safety-critical** input fell back (fictional thermal seed, or no outside
    /// temperature at all): the plan is still served for inspection, but the publisher refuses to
    /// actuate it — letting the controllers' deadman revert to their failsafe is safer than
    /// heating decisions computed from a made-up house state.
    pub degraded: bool,
    /// `true` when this plan came from the binary-RELAXED fallback LP (the fix-and-round
    /// re-solve itself failed). Its relays/on-off decisions may be fractional; the publisher's
    /// threshold would round them up to full power and the loop's latch would then pin that
    /// rounding into the next strict solves — so the publisher skips actuation for relaxed plans,
    /// and the loop neither latches nor snapshots from them.
    pub relaxed: bool,
    /// `true` when the strict MILP stalled and this plan is the FIX-AND-ROUND result: relaxed LP
    /// → deterministic rounding → fully-pinned re-solve. Integral and self-consistent, so it is
    /// actuated/latched/snapshotted like a strict plan — the flag is transparency only.
    pub rounded: bool,
    /// The controls the optimizer chose for the coming block — the battery plan drives the **armed**
    /// Growatt controller and the heating decisions the **armed** loxone controller (downstream).
    pub first_step: FirstStep,
    /// The full per-15-min-block plan as **timestamped rows** — prices, PV, SoC, battery, grid,
    /// heating and predicted temperature per controlled zone, plus the recommended Growatt mode.
    /// Chart-ready (one object per block) and the source for `/api/plan/timeline`.
    pub timeline: Vec<TimelineBlock>,
    /// **Block 1** (the NEXT block) with its start instant `t` — item G ("switch exactly on the
    /// quarter-hour marks"): the publisher applies this exact block as the next command at
    /// `apply_at = t`, and the dashboard can show "next block: …" without indexing `timeline`
    /// itself. `None` when the plan has fewer than 2 blocks. From `mark − 120 s` the loop overrides
    /// its `heat_kw`/`cool_kw`/`hvac_heat_kw` to the value FROZEN at the first tick inside that
    /// window and sets `frozen: true` (item 3, rework cycle 2) — the publisher emits a next command
    /// ONLY then, so what it promotes always matches what the loop itself latches at rollover.
    pub next_step: Option<TimelineBlock>,
    /// Per-EV-charger live state + the optimizer's charge schedule. Empty when no charger is
    /// configured; the source for `/api/ev` and the dashboard EV screen.
    #[serde(default)]
    pub ev: Vec<EvChargerPlan>,
    /// The NEXT SOLAR DAY's PV surplus (today's remaining daylight before local noon, tomorrow's
    /// from noon on — see `next_solar_day_mask`) over the house load under the **p10** (conservatively low) Solcast
    /// percentile (kWh); `None` until the forecast writer stores the p10 curve.
    #[serde(default)]
    pub p10_surplus_kwh: Option<f64>,
    /// The part of `p10_surplus_kwh` the battery cannot absorb (kWh) — energy at risk of
    /// curtailment even under the conservative forecast. `None` when p10 is unavailable.
    #[serde(default)]
    pub curtailment_risk_kwh: Option<f64>,
    /// The Kalman disturbance observer's per-zone constant flux (W, + heats) as folded into this
    /// plan's forecast (see `ForecastContext.internal_gain_w`); empty when the observer didn't run
    /// (`estimator.mode` is `anchor`, `estimator.disturbance` is off, or the filter degenerated to
    /// open-loop with no updates applied).
    #[serde(default)]
    pub disturbance_w: HashMap<String, f64>,
    /// The terminal slab-heat credit ACTUALLY applied per zone this solve (EUR per kWh thermal) —
    /// the displaced future-heating price from the outlook when one was available, else the flat
    /// median-based value (see `optimize::coordinator::displaced_price_by_zone`,
    /// `optimize::unified::FlowParams::terminal_heat_value_by_zone`). Empty when no zone got a
    /// positive credit (no heating demand this cycle).
    #[serde(default)]
    pub terminal_heat_credit_eur_per_kwh: HashMap<String, f64>,
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
    /// The stored preference exceeded the car's own charge limit and was capped to it — the
    /// dashboard's pending-save overlay reconciles on this (a capped answer is final, not
    /// cache lag). See `EvState::target_capped`.
    pub target_capped: bool,
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
    /// Where the charge-by deadline came from: "pref" / "learned" / "config".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_source: Option<String>,
    /// The effective deadline, local `"HH:MM"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_hm: Option<String>,
    /// The same deadline as an absolute instant (see [`crate::ev::state::EvState::deadline_at`]).
    /// Consumers must prefer this over re-resolving `deadline_hm`, which would use THEIR timezone
    /// rather than the site's — the dashboard's "ready by" marker reads exactly this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
}

/// One block of the plan (15 min near-term, 1 h beyond `horizon.fine_hours` — see `dt_minutes`), as
/// a flat timestamped row for charting and to verify the heat model's forward prediction against
/// measured data later. All powers are kW, prices price-units/kWh.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineBlock {
    /// Block start instant (UTC).
    pub t: DateTime<Utc>,
    /// This block's duration in minutes — 15 for a fine (near-term) block, 60 for an hourly one
    /// (item F's multi-rate grid). The publisher derives each command's `valid_until` from it; the
    /// dashboard plots an hourly block four times as wide as a fine one.
    pub dt_minutes: u32,
    pub import_price: f64,
    pub export_price: f64,
    /// This block's price is the PLACEHOLDER curve (unpublished day-ahead tail) — battery
    /// arbitrage is forbidden here and the dashboard hatches the tail.
    pub price_is_placeholder: bool,
    /// Forecast PV generation (kW) — the calibrated Solcast curve, or the clear-sky fallback.
    pub pv_kw: f64,
    /// Forecast base house load (kW) — the consumption model's prediction the optimizer planned
    /// around — the BASE load, excluding heating/EV electricity (LP decision variables). The
    /// dashboard charts it against the measured total `house_kw` with the distinction labeled.
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
    /// Planned EV charge power (kW) per charger this block — the same schedule reported per-charger
    /// in [`PlanReport::ev`]'s `charge_kw`, folded into the timeline row too (rework cycle 4, item
    /// 4) so [`Self::frozen`] can pin it exactly like every other actuation field: before this, the
    /// publisher read the EV setpoint from `PlanReport::ev` directly (a separate, never-frozen
    /// array), so a later tick's `/next` could still change the EV rate inside the freeze window
    /// even though the loop's own decision was pinned. Empty when no EV charger is configured.
    #[serde(default)]
    pub ev_charge_kw: HashMap<String, f64>,
    /// **Predicted** air temperature (°C) per controlled zone at the end of the block.
    pub temp_c: HashMap<String, f64>,
    /// Recommended Growatt slot mode and the price-gated export / inverter levers — applied by the
    /// Growatt controller for the live block.
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
    /// item 3 (rework cycle 2, findings 5/2): `true` only on [`PlanReport::next_step`] once the loop's
    /// pre-mark freeze window has pinned its `heat_kw`/`cool_kw`/`hvac_heat_kw` to the value decided
    /// by the FIRST tick inside that window (`mpc_loop`'s `committed_next`) — never on an ordinary
    /// `timeline` row, which always reports the tick's own fresh LP output. The publisher emits a NEXT
    /// command ONLY when this is `true`, so what a controller applies at the mark always equals what
    /// the brain itself latches at rollover (see `mpc_loop::freeze_committed_next`).
    pub frozen: bool,
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
    /// The fitted per-zone internal-gain profiles (W per daypart) now in use by the plan.
    pub gains_w: HashMap<String, crate::optimize::config::GainProfile>,
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
    // The peak (17–20) / off-peak (1–5) windows are local-time tariff hours, so classify each
    // block by ITS OWN local hour (cf. `tariff_prices`/`hourly_to_blocks`) — deriving hours from
    // the block index assumed an on-the-hour start, shifting the windows by up to 45 min on the
    // 3-of-4 ticks that start mid-hour. Levels approximate the recent CZ spot shape (≈0.10
    // EUR/kWh base) — the old 0.25/0.45 placeholder priced the pre-auction tail ~2× reality and
    // skewed the afternoon look-ahead's arbitrage.
    (0..HORIZON_BLOCKS)
        .map(|b| {
            let at = start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64);
            match at.with_timezone(&local_offset).hour() {
                17..=20 => 0.18,
                1..=5 => 0.04,
                _ => 0.10,
            }
        })
        .collect()
}

/// Fill each block's price: the block's own published value if any, else the DAY-TYPE MEDIAN
/// estimate (`optimize::price_forecast`, Amendment 3 — the backtested-better predictor: median
/// price at the same local clock slot over the most recent same-day-type days), else the real
/// price published for the same clock block one day earlier (persistence), else the fixed
/// placeholder curve. Returns `(spot_price, price_is_placeholder, missing, persisted, estimated)`
/// — the mask is `true` for every block that wasn't itself published, regardless of which fallback
/// filled it (an estimated or day-old price is still not today's real spread, so battery arbitrage
/// against it stays banned); `missing` counts how many blocks fell back at all, `estimated` how
/// many of those used the day-type median, `persisted` how many of the REMAINDER used the day-ago
/// real price rather than the fixed curve.
fn fill_block_prices(
    current: &[Option<f64>],
    estimated: &[Option<f64>],
    day_ago: &[Option<f64>],
    placeholder: &[f64],
) -> (Vec<f64>, Vec<bool>, usize, usize, usize) {
    let mut missing = 0usize;
    let mut persisted = 0usize;
    let mut used_estimate = 0usize;
    let mut price = Vec::with_capacity(current.len());
    let mut is_placeholder = Vec::with_capacity(current.len());
    for (b, &p) in current.iter().enumerate() {
        match p {
            Some(v) => {
                price.push(v);
                is_placeholder.push(false);
            }
            None => {
                missing += 1;
                is_placeholder.push(true);
                match estimated.get(b).copied().flatten() {
                    Some(v) => {
                        used_estimate += 1;
                        price.push(v);
                    }
                    None => match day_ago.get(b).copied().flatten() {
                        Some(v) => {
                            persisted += 1;
                            price.push(v);
                        }
                        None => price.push(placeholder[b]),
                    },
                }
            }
        }
    }
    (price, is_placeholder, missing, persisted, used_estimate)
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
    if let Some(hvac) = &config.hvac {
        for zone in hvac.comfort.keys() {
            anyhow::ensure!(
                known(zone),
                "config hvac.comfort[{zone:?}] does not exist in the model"
            );
        }
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
    pub calibration: PvBandCalibration,
    /// Per-zone internal gains (W) used by the plan. The MPC loop re-fits these from a trailing
    /// window (see [`fit_live_internal_gains`]) on its own slow cadence and writes them here; absent
    /// that, [`build_cache`] seeds them from the calibrated `heating` config values.
    pub internal_gains: HashMap<String, crate::optimize::config::GainProfile>,
    /// Fitted scheduled-load magnitudes (W, ≥ 0), aligned 1:1 to `config.scheduled_loads`. The MPC
    /// loop writes its live re-fit here; [`build_cache`] seeds them to zero (no effect) until the
    /// first fit lands.
    pub scheduled_w: Vec<f64>,
    /// Inputs that fell back while building this cache (neutral PV calibration, flat consumption).
    /// `current_plan` folds these into `placeholder_inputs` so a degraded cache is never presented
    /// as fully-calibrated, and the loop retries a degraded cache on a short back-off.
    pub fallbacks: Vec<String>,
    /// Bounded (≤28 day) day-ahead price history for the day-type-median outlook/unpublished-block
    /// estimator (`optimize::price_forecast`), `(time, price)` in the SAME EUR/kWh spot-price units
    /// `fill_block_prices`'s `current`/`day_ago` use (tariffing, where needed, applies afterward —
    /// see `estimate_outlook_prices`'s and `fill_block_prices`'s own call sites). Refreshed at most
    /// [`PRICE_HISTORY_TTL`] — INDEPENDENT of `PlanCache`'s own (shorter) refresh cadence, since 28
    /// days of settled history changes slowly and a bounded-but-large query has no business running
    /// every few minutes (see `memory/`: an unbounded price query once caused a failsafe).
    pub price_history: Vec<(DateTime<Utc>, f64)>,
    /// When [`Self::price_history`] was last refreshed; `None` means never (forces a refresh on the
    /// next [`build_cache`]).
    pub price_history_fetched_at: Option<DateTime<Utc>>,
}

/// Minimum spacing between `price_history` refreshes — the history changes slowly (settled day-
/// ahead prices), and re-querying it every `PlanCache` cycle (as often as every couple of minutes
/// under [`crate::mpc_loop::DEGRADED_CACHE_RETRY`]) would be needless InfluxDB load for no benefit.
const PRICE_HISTORY_TTL: Duration = Duration::hours(1);
/// How far back `price_history` reads — enough same-day-type history for the `K = 4` median even
/// on a house with only Saturdays/Sundays sparsely represented, bounded so the query can never grow
/// unbounded (see [`PlanCache::price_history`]'s doc).
const PRICE_HISTORY_DAYS: i64 = 28;

/// Whether `price_history` needs a refresh: never fetched (`None`), or [`PRICE_HISTORY_TTL`] has
/// elapsed since the last fetch. Pure — extracted from [`build_cache`] so the "at most hourly" gate
/// is directly unit-testable without a live/mocked `SourceClients`.
fn price_history_is_stale(fetched_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    fetched_at.is_none_or(|t| now - t >= PRICE_HISTORY_TTL)
}

/// Minimum scored (clean daylight) hours before the PV backtest ratio is trusted as a calibration.
/// Below this, one cloudy afternoon could fit a clamped 0.5×/2.0× scale from noise.
const CALIBRATION_MIN_SCORED_HOURS: usize = 24;
/// Minimum clean hours in one local-time band before its own ratio is trusted over the overall.
const CALIBRATION_MIN_BAND_HOURS: usize = 8;

/// Build the cacheable slow inputs — the 7-day PV-calibration backtest and the trailing-window
/// consumption training (the two heaviest reads). Refreshed periodically by the MPC loop. The
/// internal gains start at the config baseline; the loop overwrites them with its live re-fit.
/// Every fallback taken here is recorded in `fallbacks` — the loop path has no other way to know.
/// `previous` is the cache being replaced, when there is one. A component that FAILS to refresh
/// keeps the previous good value instead of collapsing to its fallback: a transient Influx blip at
/// refresh time would otherwise throw away a well-trained consumption model and a real PV
/// calibration, replace them with a flat 0.4 kWh/h curve and a neutral multiplier, and — since the
/// loop stores the refreshed cache unconditionally — actuate a plan built on them for a full TTL.
/// A stale-but-real model beats a fresh-but-fabricated one; the substitution is still recorded in
/// `fallbacks`, so the degradation stays visible.
pub async fn build_cache(
    db: &SourceClients,
    net: &RcNetwork,
    config: &ControlConfig,
    previous: Option<&PlanCache>,
) -> PlanCache {
    let mut fallbacks = Vec::new();
    let calibration = match backtest_pv(db, &config.site, 7).await {
        Ok(bt) if bt.scored_hours >= CALIBRATION_MIN_SCORED_HOURS => {
            // Shape-aware: per-band ratios where a band has enough clean hours, the totals ratio
            // elsewhere — a totals-only scalar corrects energy but not the shoulder-of-day timing
            // the battery's morning/evening decisions ride on.
            PvBandCalibration::from_backtest(
                bt.band_solcast_kwh,
                bt.band_actual_kwh,
                bt.band_clean_hours,
                Calibration::from_totals_default(bt.total_solcast_kwh, bt.total_actual_kwh),
                CALIBRATION_MIN_BAND_HOURS,
            )
        }
        Ok(bt) => {
            fallbacks.push(format!(
                "PV calibration ({} scored hours < {CALIBRATION_MIN_SCORED_HOURS}; neutral)",
                bt.scored_hours
            ));
            PvBandCalibration::neutral()
        }
        Err(_) => match previous.map(|p| p.calibration) {
            // A failed READ is not evidence the calibration changed — keep the last good one.
            Some(prev) => {
                fallbacks
                    .push("PV calibration (backtest failed; kept the previous fit)".to_string());
                prev
            }
            None => {
                fallbacks.push("PV calibration (backtest failed; neutral)".to_string());
                PvBandCalibration::neutral()
            }
        },
    };
    let consumption = match train_consumption(db, net, config).await {
        Ok(Some(m)) => m,
        _ => match previous.map(|p| p.consumption.clone()) {
            Some(prev) => {
                fallbacks
                    .push("consumption (training failed; kept the previous model)".to_string());
                prev
            }
            None => {
                fallbacks.push("consumption (training failed; flat 0.4 kWh/h)".to_string());
                flat_consumption()
            }
        },
    };
    // Bounded (≤28 day), hourly-refreshed price history for the day-type median estimator (see
    // `PlanCache::price_history`'s doc) — refreshed independently of everything else above,
    // because 28 days of settled prices doesn't need re-reading every cache cycle. A failed read
    // (or one still within `PRICE_HISTORY_TTL`) keeps the previous history rather than emptying it
    // (an empty history just means every block falls back to persistence — see
    // `estimate_outlook_prices` / `fill_block_prices` — so keeping a slightly stale one is
    // strictly better than discarding real data over a transient blip).
    let stale = price_history_is_stale(
        previous.and_then(|p| p.price_history_fetched_at),
        Utc::now(),
    );
    let (price_history, price_history_fetched_at) = if stale {
        let now = Utc::now();
        match db
            .read_prices_range(
                &crate::live_inputs::flux_time(now - Duration::days(PRICE_HISTORY_DAYS)),
                &crate::live_inputs::flux_time(now),
            )
            .await
        {
            Ok(samples) => (
                samples
                    .into_iter()
                    .map(|s| (s.time, s.price_eur_mwh))
                    .collect(),
                Some(now),
            ),
            Err(_) => match previous {
                Some(p) => (p.price_history.clone(), p.price_history_fetched_at),
                None => (Vec::new(), None),
            },
        }
    } else {
        let p = previous.expect("stale is false only when previous carries a fetch timestamp");
        (p.price_history.clone(), p.price_history_fetched_at)
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
            .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
            .collect(),
        fallbacks,
        price_history,
        price_history_fetched_at,
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
    let local_offset = config.site.offset_at(Utc::now());
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
}

/// Build the kernel cache for the live serve paths — the expensive, state-independent half of the
/// thermal condensation, computed once at startup (see [`KernelSet`]).
pub fn build_kernel_cache(config: &ControlConfig, net: &RcNetwork, ss: &StateSpace) -> KernelSet {
    let (hvac_zones, load_sources) = kernel_inputs(config);
    build_kernels(
        ss,
        net,
        BLOCK_SECONDS,
        HORIZON_BLOCKS,
        &hvac_zones,
        &load_sources,
    )
}

/// Everything one solver run needs, owned — `spawn_blocking` requires `'static`.
/// Everything one LP solve (or a fix-and-round pipeline of them) needs, bundled so it can be
/// `Arc`-shared across the strict/fallback blocking-thread closures in [`solve_bounded`] — and,
/// via [`fix_and_round`], reused verbatim by `solve_timing`'s acceptance test so it cannot drift
/// from what a live tick actually runs.
pub(crate) struct SolveJob {
    pub(crate) pv: PvArray,
    pub(crate) consumption: ConsumptionModel,
    pub(crate) battery: BatterySpec,
    pub(crate) heating: crate::optimize::config::HeatingConfig,
    pub(crate) hvac: crate::optimize::config::HvacConfig,
    pub(crate) ss: StateSpace,
    pub(crate) net: RcNetwork,
    pub(crate) ctx: ForecastContext,
    pub(crate) x0: DVector<f64>,
    pub(crate) ev_specs: Vec<crate::optimize::unified::EvSpec>,
    pub(crate) ev_monitored: Vec<f64>,
    pub(crate) committed: Option<HashMap<String, f64>>,
    pub(crate) kernels: Option<Arc<KernelSet>>,
}

/// Run one LP solve of `job` — `fixed` pins every binary (fix-and-round's pinned re-solve) or
/// leaves them all free (the relaxed pass and the plain fallback).
pub(crate) fn run_solve(
    job: &SolveJob,
    fixed: Option<&crate::optimize::unified::FixedBinaries>,
    solve_budget: crate::optimize::unified::SolveBudget,
) -> Result<crate::optimize::unified::UnifiedPlan> {
    plan_unified(
        &job.pv,
        &job.consumption,
        &job.battery,
        &job.heating,
        &job.hvac,
        &job.ss,
        &job.net,
        &job.ctx,
        &job.x0,
        &job.ev_specs,
        &job.ev_monitored,
        PlanOptions {
            kernels: job.kernels.as_deref(),
            committed_heat: job.committed.as_ref(),
            fixed_binaries: fixed,
            solve_budget,
        },
    )
}

/// The fix-and-round pipeline (design item F/A): a relaxed LP, deterministic rounding, then a
/// fully-pinned re-solve — the sole integrality mechanism now that HiGHS never runs branch-and-
/// bound, and the ONLY solve path a normal tick takes (see [`solve_bounded`]'s strict closure,
/// which is exactly this function). Extracted so `solve_timing`'s acceptance test runs the REAL
/// production pipeline rather than a hand-rolled copy that could silently drift from it.
///
/// `Ok((plan, Rounded))` on a successful pinned re-solve (or when the relaxed solve was already
/// integral — see below); `Ok((relaxed_plan, Relaxed))` if the re-solve itself fails (the relaxed
/// plan is still returned, advisory); `Err` only if even the first (relaxed) solve fails.
///
/// `salvage` is filled with the relaxed plan as soon as it succeeds, BEFORE the pinned re-solve
/// starts (rework cycle 1, finding 1's salvage): the pinned re-solve is the slower of the two LPs
/// to go wrong (it starts from a harder, pinned-integral feasible region), so if `solve_bounded`'s
/// outer timeout fires while this function is still stuck in it, the relaxed plan already sitting
/// in `salvage` is a real, freshly-computed answer — worth publishing (graded `Relaxed`) instead of
/// starting a brand-new fallback LP from scratch.
pub(crate) fn fix_and_round(
    job: &SolveJob,
    budget: crate::optimize::unified::SolveBudget,
    salvage: &Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>>,
) -> Result<(crate::optimize::unified::UnifiedPlan, SolveGrade)> {
    fix_and_round_inner(job, budget, salvage, false)
}

/// item 11 (rework cycle 2, finding 6): the real body of [`fix_and_round`], with the "already
/// integral" skip check overridable so a timing test can force the pinned re-solve to actually run —
/// otherwise a scenario whose relaxed LP happens to land on integral values (as both of
/// `solve_timing`'s catch-up scenarios now do, since `HEAT_COOL_PIN_BLOCKS` widened to cover blocks 0
/// AND 1 — item 4) skips the second LP entirely and the timed region silently stops exercising it,
/// exactly what the Refuter's finding 6 caught: nothing in the suite timed a two-LP tick any more.
/// `fix_and_round` itself always passes `false` — production behaviour is completely unchanged.
pub(crate) fn fix_and_round_inner(
    job: &SolveJob,
    budget: crate::optimize::unified::SolveBudget,
    salvage: &Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>>,
    force_pinned_resolve: bool,
) -> Result<(crate::optimize::unified::UnifiedPlan, SolveGrade)> {
    let relaxed_plan = run_solve(job, None, budget)?;
    *salvage.lock().unwrap_or_else(|e| e.into_inner()) = Some(relaxed_plan.clone());
    let loads = crate::optimize::coordinator::controllable_load_specs(&job.ctx);
    // Skip the pinned re-solve entirely when the relaxed LP already settled on integral values
    // (see `relaxed_plan_is_already_integral`'s doc) — a second LP that can only reproduce numbers
    // already in hand costs a full solve for nothing, and on the live tick budget every skipped
    // one is roughly half a tick's wall-clock cost.
    if !force_pinned_resolve
        && crate::optimize::unified::relaxed_plan_is_already_integral(
            &relaxed_plan,
            &job.heating,
            &job.hvac,
            &job.ev_specs,
            &loads,
        )
    {
        eprintln!("[solve] relaxed plan already integral; skipping the pinned re-solve");
        return Ok((relaxed_plan, SolveGrade::Rounded));
    }
    let dt = job.ctx.grid.dt_hours_vec();
    let fixed = crate::optimize::unified::round_binaries(
        &relaxed_plan,
        &job.heating,
        &job.hvac,
        &job.ev_specs,
        &loads,
        &dt,
    );
    match run_solve(job, Some(&fixed), budget) {
        Ok(p) => Ok((p, SolveGrade::Rounded)),
        Err(e) => {
            eprintln!("[solve] pinned re-solve failed ({e}); publishing the relaxed plan");
            Ok((relaxed_plan, SolveGrade::Relaxed))
        }
    }
}

/// HiGHS's own wall-clock limit for EACH LP solve (both the strict fix-and-round pipeline's two
/// solves and the fallback's one) — comfortably inside the outer timeouts below, leaving headroom
/// for model build + presolve, which sit outside HiGHS's own time-limit check (see `research.md`'s
/// pitfalls).
const PER_LP_HIGHS_TIME_LIMIT_S: f64 = 14.0;
/// The strict (fix-and-round) pipeline's outer budget: a relaxed LP, a cheap deterministic
/// rounding pass, and a fully-pinned re-solve — each LP capped at `PER_LP_HIGHS_TIME_LIMIT_S`,
/// with headroom for the rounding pass and model build between them.
const STRICT_SOLVE_TIMEOUT: StdDuration = StdDuration::from_secs(32);
/// The fallback's outer budget: ONE plain relaxed LP (no rounding/re-solve) when the strict
/// pipeline itself times out or its permit is busy. strict + fallback + the on-demand path's
/// pre-solve DB reads must fit inside the web layer's `COMPUTE_TIMEOUT` (55 s) with headroom.
const FALLBACK_SOLVE_TIMEOUT: StdDuration = StdDuration::from_secs(15);
/// The fallback's OWN per-LP HiGHS limit: unlike the strict pipeline (two sequential LPs sharing
/// `STRICT_SOLVE_TIMEOUT`), the fallback runs a single LP inside `FALLBACK_SOLVE_TIMEOUT`, so it
/// can use nearly all of it — 1 s of headroom for model build + presolve outside HiGHS's own
/// time-limit check (same reasoning as [`PER_LP_HIGHS_TIME_LIMIT_S`]). Rework cycle 1, finding 1:
/// previously the fallback reused `PER_LP_HIGHS_TIME_LIMIT_S` itself, which left only 1 s of
/// outer-timeout headroom by numeric coincidence (both were 14/15); computed from
/// `FALLBACK_SOLVE_TIMEOUT` so the two can never silently drift apart again.
const FALLBACK_PER_LP_HIGHS_TIME_LIMIT_S: f64 = FALLBACK_SOLVE_TIMEOUT.as_secs_f64() - 1.0;

/// How a plan's solve concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolveGrade {
    /// Fix-and-round: relaxed LP → deterministic rounding → fully-pinned re-solve. INTEGRAL and
    /// self-consistent — the NORMAL result now that HiGHS never runs branch-and-bound; actuated,
    /// latched and snapshotted.
    Rounded,
    /// The plain relaxed LP alone (the strict pipeline timed out, its permit was busy, or its own
    /// pinned re-solve failed) — advisory only; the publisher skips it and the loop neither
    /// latches nor snapshots from it.
    Relaxed,
}

/// Run the bounded FALLBACK: a single plain relaxed LP on one blocking thread. Its own one-permit
/// gate (same detached-supervisor pattern as the strict path) stops abandoned fallback threads
/// piling up if a pathological LP outlives its timeout tick after tick.
async fn run_fallback<T, G>(fallback: G, timeout: StdDuration, loop_caller: bool) -> Result<T>
where
    T: Send + 'static,
    G: FnOnce() -> Result<T> + Send + 'static,
{
    // Split loop/web permits for the same reason `solve_bounded` splits the strict ones: a web
    // caller holding the only fallback permit would make the loop's fallback error out, and a
    // loop tick with no plan at all is worse than the relaxed plan it was trying to produce.
    static LOOP_FALLBACK: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    static WEB_FALLBACK: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let gate: Arc<tokio::sync::Semaphore> = if loop_caller {
        Arc::clone(&LOOP_FALLBACK)
    } else {
        Arc::clone(&WEB_FALLBACK)
    };
    let permit = gate.try_acquire_owned().map_err(|_| {
        anyhow::anyhow!("previous fallback solve still running — keeping the last plan")
    })?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _permit = permit; // released only when the blocking thread truly finishes
        let _ = tx.send(tokio::task::spawn_blocking(fallback).await);
    });
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(joined)) => {
            joined.map_err(|e| anyhow::anyhow!("fallback solver task failed: {e}"))?
        }
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "fallback solver supervisor dropped its channel"
        )),
        Err(_) => Err(anyhow::anyhow!("fallback solve also timed out")),
    }
}

/// Run `strict` (the fix-and-round pipeline) off the async runtime with a timeout; on expiry (or
/// when a previous strict solve still holds the permit) run `fallback` (a single plain relaxed LP)
/// instead. Returns the plan, its grade, and — when the fallback path was used — why.
///
/// A pure LP under HiGHS's own `time_limit_s` either finishes or errors well inside the outer
/// timeouts in practice, but a stuck strict thread still **cannot be killed** (blocking C++ FFI),
/// so the same belt-and-suspenders structure as before stays:
/// - a detached SUPERVISOR task owns the one-permit semaphore's permit for the blocking thread's
///   full lifetime — the caller may itself be cancelled (the web layer's `COMPUTE_TIMEOUT` drops
///   the whole future) without releasing the permit early, so strict solves can never pile up no
///   matter how the caller ends;
/// - while the permit is held by a stuck solve, callers fall back to the bounded relaxed LP
///   (flagged) instead of erroring — fresh advisory plans keep flowing.
async fn solve_bounded<T, F, G>(
    strict: F,
    fallback: G,
    strict_timeout: StdDuration,
    fallback_timeout: StdDuration,
    loop_caller: bool,
    salvage: Arc<Mutex<Option<T>>>,
) -> Result<(T, SolveGrade, Option<String>)>
where
    T: Send + 'static,
    F: FnOnce() -> Result<(T, SolveGrade)> + Send + 'static,
    G: FnOnce() -> Result<T> + Send + 'static,
{
    // Separate permits so an on-demand /api/plan solve can never hold the loop's: a displaced
    // loop tick would fall to the relaxed fallback, which the publisher refuses to actuate — a
    // dashboard viewer would silently pause actuation. Worst case two strict solves overlap
    // (~seconds of CPU on separate blocking threads), which is fine.
    static LOOP_SOLVER: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    static WEB_SOLVER: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let solver: Arc<tokio::sync::Semaphore> = if loop_caller {
        Arc::clone(&LOOP_SOLVER)
    } else {
        Arc::clone(&WEB_SOLVER)
    };
    match solver.try_acquire_owned() {
        Ok(permit) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _permit = permit; // released only when the blocking thread truly finishes
                let _ = tx.send(tokio::task::spawn_blocking(strict).await);
            });
            match tokio::time::timeout(strict_timeout, rx).await {
                // The blocking task finished (no panic) inside the outer timeout — but "finished"
                // still splits into the strict pipeline's own Ok/Err: a strict `Err` (HiGHS
                // `TimeLimit`/`NoSolutionFound`, or anything else `fix_and_round` can return) used
                // to propagate straight out of `solve_bounded` here via `?`, skipping the fallback
                // entirely — the exact "planning failed" mode this whole branch exists to remove,
                // now reachable BELOW every outer timeout (rework cycle 1, finding 1). Route it to
                // the same fallback the outer timeout uses instead.
                Ok(Ok(Ok(Ok((plan, grade))))) => Ok((plan, grade, None)),
                Ok(Ok(Ok(Err(e)))) => {
                    // item 10 (rework cycle 2, finding 10): the outer-timeout arm below already
                    // salvages a relaxed plan that reached `salvage` before the strict pipeline
                    // failed/hung; this arm assumed `salvage` must be empty whenever `fix_and_round`
                    // itself returns `Err` and always paid for a brand-new fallback LP — one that, if
                    // IT then also failed, left nothing published even though a perfectly good relaxed
                    // plan might already be sitting in `salvage`. Check it first, same as the timeout
                    // arm, before falling back to a fresh solve.
                    if let Some(plan) = salvage.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "fix-and-round error: {e}; salvaged the relaxed plan"
                            )),
                        ));
                    }
                    let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                    Ok((
                        plan,
                        SolveGrade::Relaxed,
                        Some(format!("fix-and-round error: {e}")),
                    ))
                }
                // A PANIC inside the strict closure (`fix_and_round`) — a bug, not an ordinary
                // solver failure — used to propagate straight out of `solve_bounded` as a hard
                // `Err`, the same "planning failed, no plan published" mode the `Err` branch above
                // exists to avoid. Route it through the SAME fallback instead, logged loudly (a
                // panic here always deserves investigation, unlike a routine HiGHS TimeLimit).
                Ok(Ok(Err(join_err))) => {
                    eprintln!(
                        "[mpc] PANIC in strict solve: {join_err} — falling back to the relaxed LP"
                    );
                    let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                    Ok((
                        plan,
                        SolveGrade::Relaxed,
                        Some(format!("strict solve panicked: {join_err}")),
                    ))
                }
                Ok(Err(_)) => Err(anyhow::anyhow!("solver supervisor dropped its channel")),
                Err(_) => {
                    // Outer STRICT_SOLVE_TIMEOUT fired with the blocking task still running
                    // (detached; it keeps going and will eventually release the permit). If the
                    // relaxed LP already succeeded and stored itself in `salvage` — the common
                    // shape, since the PINNED re-solve is the slower/harder of the two LPs — publish
                    // that real, freshly-solved plan instead of paying for a brand-new fallback LP
                    // (finding 1's salvage, rework cycle 1).
                    if let Some(plan) = salvage.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "fix-and-round timeout after {}s; salvaged the relaxed plan",
                                strict_timeout.as_secs()
                            )),
                        ));
                    }
                    let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                    Ok((
                        plan,
                        SolveGrade::Relaxed,
                        Some(format!(
                            "fix-and-round timeout after {}s",
                            strict_timeout.as_secs()
                        )),
                    ))
                }
            }
        }
        Err(_) => {
            let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
            Ok((
                plan,
                SolveGrade::Relaxed,
                Some("previous fix-and-round still running".to_string()),
            ))
        }
    }
}

/// The site-local instant the controllable load's window occurrence in progress at `now` opened,
/// or `None` when `now` is outside every window. Pure — the block grid and the local-time rule are
/// the same ones `controllable_load_specs` uses to build the LP's window mask.
fn occurrence_start(
    site: &crate::optimize::config::SiteConfig,
    load: &crate::optimize::config::ScheduledLoad,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let in_window = |t: DateTime<Utc>| {
        let l = t.with_timezone(&site.offset_at(t));
        load.unit_profile(l.month(), l.hour() * 60 + l.minute()) != 0.0
    };
    if !in_window(now) {
        return None;
    }
    // Walk back to where this occurrence opened. Bounded by a day: a window cannot be longer, and an
    // all-day window would otherwise walk for ever.
    let step = Duration::minutes(15);
    let mut start = now;
    while now - start < Duration::hours(24) && in_window(start - step) {
        start -= step;
    }
    Some(start)
}

/// Hours the load actually drew inside `(start, now]`, from stop-stamped 15-minute means.
///
/// Pure, and deliberately strict at both ends:
/// * `s.time > start` — a stop-stamped sample AT `start` covers `[start − 15 min, start)`, i.e. the
///   quarter-hour BEFORE the occurrence opened. Counting it credited the appliance for running
///   under its own native control just before the MPC window began.
/// * `s.time <= now` — `stop: now()` makes Flux clamp and emit a trailing PARTIAL window; a load
///   drawing for one minute of the current block yields a near-rated mean that would otherwise
///   count as a full 15 minutes. The same keep-first/drop-the-partial convention as
///   `estimate::keep_first_by_hour` and `validate::read_heating_kw`.
fn run_hours_from_samples(
    samples: &[crate::influxdb::TimeSample],
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    rated_kw: f64,
) -> f64 {
    samples
        .iter()
        .filter(|s| s.time > start && s.time <= now && s.value / 1000.0 > 0.5 * rated_kw)
        .count() as f64
        * 0.25
}

/// Hours a **controllable** load has actually run inside the window occurrence in progress at `now`,
/// measured from its `sensor`. `None` when the load has no sensor, is outside its window, or the
/// sensor yields NO evidence either way (a read error, or a successful read with no sample in
/// range) — the caller then keeps the MPC loop's planned-actuation tally. `Some(0.0)` means the
/// sensor genuinely reported the load idle, which is evidence; an empty series is not.
///
/// Why this exists: the loop's tally counts what it PLANNED to actuate, and a strict plan is not
/// proof it reached the hardware — the publisher, the broker or the controller can be down, or the
/// controller unarmed, with nothing flowing back to this read-only brain. An over-counted tally
/// zeroes the occurrence's remaining demand, so the load silently stops being scheduled for the rest
/// of its window; an under-counted one re-demands the full target every tick and over-runs it. A
/// sensor turns both guesses into evidence.
pub async fn measured_run_hours(
    db: &SourceClients,
    site: &crate::optimize::config::SiteConfig,
    load: &crate::optimize::config::ScheduledLoad,
    now: DateTime<Utc>,
) -> Option<f64> {
    let sensor = load.sensor.as_ref()?;
    let rated_kw = load.power_w? / 1000.0;
    if rated_kw <= 0.0 {
        return None;
    }
    // Floor `now` to the 15-minute grid FIRST: the query uses an absolute stop and stop-stamped
    // windows, and Flux clamps the trailing window's bounds to the range — so an unaligned `now`
    // made both the leading and trailing windows sub-15-min partials that the `(start, now]` filter
    // still admitted as full blocks (~+0.25 h systematic overcount, which shaves the boiler's
    // remaining demand short). Aligned bounds ⇒ every returned window is complete.
    let now = {
        let secs = now.timestamp();
        DateTime::from_timestamp(secs - secs.rem_euclid(900), 0).unwrap_or(now)
    };
    let Some(start) = occurrence_start(site, load, now) else {
        return Some(0.0); // outside every window: nothing has run in an occurrence that isn't open
    };
    // An EXPLICIT absolute range, not `-Nm` … `now()`: the relative form re-anchors to the server's
    // clock at query time, so the samples could not be aligned against the `start`/`now` this
    // function reasons about.
    let series = db
        .read_locator_series(sensor, &start.to_rfc3339(), &now.to_rfc3339(), "15m")
        .await
        .ok()?;
    if !series.iter().any(|s| s.time > start && s.time <= now) {
        // No evidence at all (locator renamed, field dropped, ingest gap). Returning 0 here would
        // OVERRIDE the loop's tally with "nothing has run", re-demanding the full target every tick
        // until the load over-ran its window — the mirror image of the bug this function fixes.
        eprintln!(
            "[mpc] controllable load {:?}: sensor returned no samples for the occurrence; \
             keeping the planned-actuation tally",
            crate::optimize::coordinator::load_name(load)
        );
        return None;
    }
    Some(run_hours_from_samples(&series, start, now, rated_kw))
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

    // The multi-rate planning grid (item F): fine (15-min) blocks for `config.horizon.fine_hours`,
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
    // `grid.block_end(grid.len() - 1)`, NOT a fixed `start + HORIZON_HOURS` (rework cycle 1,
    // finding 9): on the multi-rate grid the trailing partial hour is dropped, so the grid's true
    // end can be up to ~45 min before `start + HORIZON_HOURS` — `coordinator::outlook_thermal_
    // inputs` already anchors the outlook's KNOWN INPUTS at exactly `ctx.start + step_seconds *
    // n_fine` (the true fine-lattice end) when it continues the free-response simulation past the
    // horizon; the weather FETCH used to anchor at the fixed offset instead, so `Outlook.
    // temperature_c[0]` was read for the wrong instant (up to 45 min of skew) relative to what the
    // simulation actually treated it as covering.
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
        None => match backtest_pv(db, &config.site, 7).await {
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
    let (raw_pv, pv_kw, pv_calibration_scale, pv_p10_kw) = match solcast {
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
            (raw, calibrated, calibration.overall_scale(), p10)
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
            (clear_sky.clone(), clear_sky, 1.0, None)
        }
    };
    // raw_pv / pv_kw are per-block kW; sum × block-hours = kWh over the horizon.
    let pv_raw_kwh: f64 = raw_pv.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);
    let pv_calibrated_kwh: f64 = pv_kw.iter().sum::<f64>() * (BLOCK_SECONDS / 3600.0);

    // Day-ahead spot prices (EUR/kWh) from OTE — fall back to the placeholder curve if not yet
    // published or unreadable (a transient DB error must not fail the whole planning cycle).
    let (spot_price, price_is_placeholder): (Vec<f64>, Vec<bool>) =
        match block_prices(db, start, HORIZON_BLOCKS).await {
            Ok(Some(BlockPrices { current, day_ago })) => {
                // Use real prices where published; fill the unpublished tail (e.g. tomorrow before
                // the ~14:00 auction) FIRST with the day-type median estimate (Amendment 3: the
                // backtested-better predictor — median price at the same local clock slot over the
                // most recent same-day-type days, from the cached ≤28-day history), else the real
                // price of the same clock block a day earlier (persistence), else the fixed
                // placeholder curve. The per-block MASK is unchanged either way (the LP must not
                // commit battery arbitrage against an invented spread, and an estimated or day-old
                // price is exactly that) — flag how much fell back and by which route.
                let placeholder = placeholder_price_curve(start, local_offset);
                let public_holidays: Vec<(u32, u32)> = config
                    .site
                    .public_holidays
                    .iter()
                    .filter_map(|md| crate::optimize::price_forecast::parse_month_day(md))
                    .collect();
                let price_history: &[(DateTime<Utc>, f64)] = cache
                    .map(|c| c.price_history.as_slice())
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
                    let source = if estimated_count > 0 {
                        "day-type median"
                    } else if persisted > 0 {
                        "persistence"
                    } else {
                        "placeholder"
                    };
                    placeholders.push(format!(
                        "day-ahead prices ({missing}/{HORIZON_BLOCKS} blocks unpublished; {source})"
                    ));
                }
                (price, is_placeholder)
            }
            Ok(None) | Err(_) => {
                placeholders.push("day-ahead prices (unavailable; placeholder curve)".to_string());
                (
                    placeholder_price_curve(start, local_offset),
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
    let terminal_value = terminal_soc_value(
        terminal_basis,
        battery_amortisation,
        battery.charge_efficiency * battery.discharge_efficiency,
    );

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
        terminal_value,
        import_price,
        min_final_soc_kwh: Some(battery.min_soc_kwh),
        price_is_placeholder: price_is_placeholder.clone(),
        // Physical grid-connection limits from config (`grid` block); None ⇒ unconstrained.
        max_import_kw: config.grid.max_import_kw,
        max_export_kw: config.grid.max_export_kw,
        pv_kw_override: Some(pv_kw),
        load_scale: 1.0,
        outlook,
        // On-demand (no cache) gets no day-type median history — falls back to plain persistence,
        // same as before this feature existed, rather than a fresh bounded Influx read per call.
        price_history: cache.map(|c| c.price_history.clone()).unwrap_or_default(),
        public_holidays: config
            .site
            .public_holidays
            .iter()
            .filter_map(|md| crate::optimize::price_forecast::parse_month_day(md))
            .collect(),
        easter_holidays: config.site.easter_holidays,
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
    // LP (finding 1, rework cycle 1).
    let salvage: Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>> =
        Arc::new(Mutex::new(None));
    let strict_salvage = Arc::clone(&salvage);
    let (plan, grade, fallback_cause) = solve_bounded(
        // Strict = fix-and-round (see `fix_and_round`'s own doc) — the NORMAL plan path now that
        // HiGHS never runs branch-and-bound.
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
    // (item F) — `plan`'s are per GRID BLOCK. Aggregate them here the SAME way `plan_unified` did
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

    // Per-block duration (item F: no longer uniform) — every kWh total below weights each block's
    // kW by ITS OWN duration, not a flat BLOCK_SECONDS.
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

    // Item G: block 1 with its start instant, computed once so the borrow below completes before
    // `timeline` is moved into the struct literal — see `next_timeline_step`'s doc.
    let next_step = next_timeline_step(&timeline);

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
    fn p10_curtailment_masks_and_caps() {
        // 4 blocks, only the middle two are "tomorrow"; dt = 0.25 h.
        let p10 = [8.0, 8.0, 4.0, 8.0];
        let load = [1.0, 2.0, 6.0, 1.0];
        let tomorrow = [false, true, true, false];
        // Surplus counts only tomorrow's blocks with p10 > load: (8-2)*0.25 = 1.5 kWh.
        let (surplus, risk) = p10_curtailment(&p10, &load, &tomorrow, 1.0, 0.25);
        assert!((surplus - 1.5).abs() < 1e-9);
        assert!((risk - 0.5).abs() < 1e-9); // 1.5 kWh surplus - 1.0 kWh headroom
                                            // Enough headroom ⇒ zero risk, surplus unchanged.
        let (s2, r2) = p10_curtailment(&p10, &load, &tomorrow, 5.0, 0.25);
        assert!((s2 - 1.5).abs() < 1e-9);
        assert_eq!(r2, 0.0);
        // Negative headroom (over-full telemetry) is clamped, not added to the risk.
        let (_, r3) = p10_curtailment(&p10, &load, &tomorrow, -2.0, 0.25);
        assert!((r3 - 1.5).abs() < 1e-9);
    }

    #[test]
    fn next_solar_day_mask_targets_the_coming_daylight() {
        let cest = FixedOffset::east_opt(2 * 3600).unwrap();
        // Overnight plan (02:00 local): the sun that squeezes tonight's pre-charge rises THIS
        // local day — the mask must cover today's daylight, not tomorrow's.
        let start = utc("2026-06-10T00:00:00Z"); // 02:00 local
        let mask = next_solar_day_mask(start, cest, 144, 900.0); // 36 h horizon
                                                                 // Block at local 12:00 today = 10 h after start.
        assert!(mask[10 * 4]);
        // Block at local 12:00 tomorrow = 34 h after start — a different solar day.
        assert!(!mask[34 * 4]);
        // Afternoon plan (14:00 local): tonight's pre-charge is squeezed by TOMORROW's sun.
        let start = utc("2026-06-10T12:00:00Z"); // 14:00 local
        let mask = next_solar_day_mask(start, cest, 144, 900.0);
        // Local 12:00 tomorrow = 22 h after start.
        assert!(mask[22 * 4]);
        // The remaining hours of today's afternoon are not the refill day.
        assert!(!mask[4]); // 15:00 local today
    }

    #[test]
    fn placeholder_curve_classifies_by_local_hour() {
        // 23:00 UTC. In UTC+2 that is 01:00 local → off-peak (0.04); the curve must use the local hour.
        let start = utc("2024-01-01T23:00:00Z");
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert!((placeholder_price_curve(start, plus2)[0] - 0.04).abs() < 1e-9);
        // The same instant is 23:00 in UTC → the regular band (0.10), not off-peak.
        let utc0 = FixedOffset::east_opt(0).unwrap();
        assert!((placeholder_price_curve(start, utc0)[0] - 0.10).abs() < 1e-9);

        // Mid-hour start: each block keys to ITS OWN local hour, not block-index arithmetic.
        // 16:45 local start (UTC+0): block 0 is hour 16 (base), block 1 (17:00) is peak.
        let start = utc("2024-01-01T16:45:00Z");
        let curve = placeholder_price_curve(start, utc0);
        assert!((curve[0] - 0.10).abs() < 1e-9, "16:45 is still base");
        assert!((curve[1] - 0.18).abs() < 1e-9, "17:00 is peak");
    }

    #[test]
    fn fill_block_prices_persists_before_falling_to_placeholder() {
        // 144-block horizon: blocks 100-143 unpublished today; the SAME clock blocks a day
        // earlier are real for 100-119 only, so 120-143 must fall all the way to the fixed curve.
        let mut current = vec![Some(0.10); HORIZON_BLOCKS];
        for p in current.iter_mut().skip(100) {
            *p = None;
        }
        let mut day_ago = vec![None; HORIZON_BLOCKS];
        for (b, p) in day_ago.iter_mut().enumerate().take(120).skip(100) {
            *p = Some(0.08 + b as f64 * 1e-4); // distinct per-block values
        }
        let placeholder: Vec<f64> = (0..HORIZON_BLOCKS).map(|_| 0.5).collect();
        let estimated = vec![None; HORIZON_BLOCKS]; // no day-type estimate in this scenario
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 44);
        assert_eq!(persisted, 20);
        assert_eq!(estimated_count, 0);
        for b in 0..100 {
            assert!(
                (price[b] - 0.10).abs() < 1e-9,
                "block {b} published unchanged"
            );
            assert!(!is_placeholder[b]);
        }
        for b in 100..120 {
            assert!(
                (price[b] - day_ago[b].unwrap()).abs() < 1e-9,
                "block {b} must equal its day-ago real price"
            );
            assert!(
                is_placeholder[b],
                "persisted block still counts as placeholder"
            );
        }
        for b in 120..HORIZON_BLOCKS {
            assert!(
                (price[b] - 0.5).abs() < 1e-9,
                "block {b} with neither source falls to the fixed curve"
            );
            assert!(is_placeholder[b]);
        }
    }

    #[test]
    fn fill_block_prices_reports_placeholder_when_nothing_persisted() {
        let current = vec![None; 4];
        let estimated = vec![None; 4];
        let day_ago = vec![None; 4];
        let placeholder = vec![0.5; 4];
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 4);
        assert_eq!(persisted, 0);
        assert_eq!(estimated_count, 0);
        assert!(price.iter().all(|&p| (p - 0.5).abs() < 1e-9));
        assert!(is_placeholder.iter().all(|&f| f));
    }

    /// Amendment criterion 16: the day-type median estimate is tried BEFORE day-ago persistence
    /// and the fixed placeholder — a block with both an estimate and a day-ago real price must use
    /// the estimate.
    #[test]
    fn fill_block_prices_prefers_estimate_over_persistence_and_placeholder() {
        let current = vec![None; 3]; // all three blocks unpublished
        let estimated = vec![Some(0.07), None, None]; // only block 0 has an estimate
        let day_ago = vec![Some(0.09), Some(0.11), None]; // blocks 0 and 1 have a day-ago price
        let placeholder = vec![0.5; 3];
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 3);
        assert_eq!(estimated_count, 1);
        assert_eq!(persisted, 1); // only block 1 falls through to day-ago
        assert!(
            (price[0] - 0.07).abs() < 1e-9,
            "block 0 must use the estimate (0.07), not day-ago (0.09): got {}",
            price[0]
        );
        assert!((price[1] - 0.11).abs() < 1e-9, "block 1 falls to day-ago");
        assert!(
            (price[2] - 0.5).abs() < 1e-9,
            "block 2 falls to the placeholder"
        );
        assert!(is_placeholder.iter().all(|&f| f));
    }

    /// Amendment criterion 17: `price_history` refreshes at most [`PRICE_HISTORY_TTL`] (hourly) —
    /// never on every `build_cache` cycle, which can run as often as every couple of minutes.
    #[test]
    fn price_history_is_stale_respects_the_hourly_ttl() {
        let now = utc("2024-01-15T12:00:00Z");
        assert!(price_history_is_stale(None, now), "never fetched -> stale");
        assert!(
            !price_history_is_stale(Some(now - Duration::minutes(30)), now),
            "30 min ago, well under the 1 h TTL -> NOT stale"
        );
        assert!(
            !price_history_is_stale(Some(now - Duration::minutes(59)), now),
            "just under the TTL -> NOT stale"
        );
        assert!(
            price_history_is_stale(Some(now - Duration::hours(1)), now),
            "exactly the TTL -> stale (>=), so a refresh always eventually happens"
        );
        assert!(
            price_history_is_stale(Some(now - Duration::hours(2)), now),
            "well past the TTL -> stale"
        );
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
    fn terminal_value_survives_a_negative_price_block() {
        // One negative block must NOT collapse the terminal value to 0 (the LP would then drain
        // the battery at the horizon edge on every such day): the break-even cap is skipped for a
        // negative cheapest price and the median values the leftover SoC.
        let v = terminal_soc_value(&[-0.10, 0.20], 0.0, 0.85);
        assert!((v - 0.05 * 0.99).abs() < 1e-9, "median-valued: {v}");
        // All-negative horizon: median < 0 ⇒ floored at 0 (leftover energy really is worthless).
        assert_eq!(terminal_soc_value(&[-0.10, -0.20], 0.0, 0.85), 0.0);
        assert_eq!(terminal_soc_value(&[], 0.0, 0.85), 0.0);
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

    /// Amendment criterion 9: a stored forecast shorter than the requested outlook must truncate
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

    #[test]
    fn classify_mode_uses_loxone_vocabulary() {
        // Args: charge, discharge, batt_grid_charge, batt_to_grid, grid_import(total),
        // grid_export(total), soc_kwh, inverter; min_soc=2, max_soc=10.
        let m = |c, d, bgc, btg, gi, ge, soc, inv| {
            classify_mode(
                &BlockFlows {
                    charge_kw: c,
                    discharge_kw: d,
                    batt_grid_charge_kw: bgc,
                    batt_to_grid_kw: btg,
                    grid_import_kw: gi,
                    grid_export_kw: ge,
                    soc_kwh: soc,
                    inverter_on: inv,
                },
                2.0,
                10.0,
                0.0,
            )
        };
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, false), "inverter_off"); // paused
                                                                                 // A grid dispatch below the ACTUATOR's floor is demoted to regular: the controller would
                                                                                 // round it up to ~2.45 kW, actuating up to ~8x the planned energy.
        let floor = |bgc, btg| {
            classify_mode(
                &BlockFlows {
                    charge_kw: bgc,
                    discharge_kw: btg,
                    batt_grid_charge_kw: bgc,
                    batt_to_grid_kw: btg,
                    grid_import_kw: bgc,
                    grid_export_kw: btg,
                    soc_kwh: 5.0,
                    inverter_on: true,
                },
                2.0,
                10.0,
                2.45,
            )
        };
        assert_eq!(floor(0.3, 0.0), "regular", "sub-floor grid charge demoted");
        assert_eq!(
            floor(0.0, 0.3),
            "regular",
            "sub-floor grid discharge demoted"
        );
        assert_eq!(
            floor(3.0, 0.0),
            "charge_from_grid",
            "above the floor is untouched"
        );
        assert_eq!(
            m(2.0, 0.0, 2.0, 0.0, 2.0, 0.0, 5.0, true),
            "charge_from_grid"
        ); // AC-charging
        assert_eq!(
            m(0.0, 2.0, 0.0, 2.0, 0.0, 2.0, 5.0, true),
            "discharge_to_grid"
        ); // battery → grid
        assert_eq!(
            m(0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 5.0, true),
            "sell_production"
        ); // export though battery has room
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 10.0, true), "regular"); // full battery — passive export
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 5.0, true), "battery_hold"); // importing, battery saved
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 2.0, true), "regular"); // importing at the SoC floor
        assert_eq!(m(2.0, 0.0, 0.0, 0.0, 0.0, 1.0, 5.0, true), "regular"); // solar charge + tiny spill
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, true), "regular"); // self-consume / idle

        // The regression this split exists for: EV grid draw during a SOLAR battery charge is not
        // charge_from_grid (the inverter must not force AC charge)...
        assert_eq!(m(2.0, 0.0, 0.0, 0.0, 7.0, 0.0, 5.0, true), "regular");
        // ...and battery→EV during solar export is not discharge_to_grid (no battery drain to grid).
        assert_eq!(m(0.0, 2.0, 0.0, 0.0, 0.0, 3.0, 5.0, true), "regular");
    }

    /// A UTC site (fixed offset 0, no IANA zone) so test hour bins map 1:1.
    fn utc_site() -> SiteConfig {
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
    fn tariff_prices_apply_distribution_and_sell_fee() {
        let t = tariff();
        let start = utc("2024-01-15T00:00:00Z");
        // 48 15-min blocks = 12 h. Block 0 = 00:00 (NT/low), block 40 = 10:00 (VT/high).
        let spot = vec![0.10; 48]; // EUR/kWh
        let (import, export) = tariff_prices(&t, &utc_site(), &spot, start);
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
        // Spot below the export floor (0.5/25 = 0.02 EUR): export floored to 0, still ≤ import.
        let (import, export) = tariff_prices(&t, &utc_site(), &[0.005; 24], start);
        for h in 0..24 {
            assert_eq!(export[h], 0.0);
            assert!(export[h] <= import[h] + 1e-12);
        }
    }
    #[tokio::test]
    async fn solve_bounded_falls_back_to_relaxed_on_timeout() {
        // Millisecond-scale stand-ins (never test with the real 30 s under single-threaded CI).
        let strict_fast = || Ok::<_, anyhow::Error>((1, SolveGrade::Rounded));
        let fallback = || Ok::<_, anyhow::Error>(2);
        let (v, grade, cause) = solve_bounded(
            strict_fast,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 1);
        assert_eq!(grade, SolveGrade::Rounded);
        assert!(cause.is_none());

        // A stuck strict solve times out and the relaxed fallback answers instead.
        let strict_stuck = || {
            std::thread::sleep(StdDuration::from_millis(300));
            Ok::<_, anyhow::Error>((1, SolveGrade::Rounded))
        };
        let fallback = || Ok::<_, anyhow::Error>(2);
        let (v, grade, cause) = solve_bounded(
            strict_stuck,
            fallback,
            StdDuration::from_millis(20),
            StdDuration::from_millis(500),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 2);
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("fix-and-round timeout"));

        // While the stuck strict thread still holds the permit, a concurrent caller is served by
        // the relaxed fallback instead of erroring (fresh plans keep flowing).
        let (v, grade, cause) = solve_bounded(
            || Ok::<_, anyhow::Error>((1, SolveGrade::Rounded)),
            || Ok::<_, anyhow::Error>(3),
            StdDuration::from_millis(200),
            StdDuration::from_millis(500),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 3);
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("still running"));
        // Give the detached stuck thread time to release the permit for later tests.
        tokio::time::sleep(StdDuration::from_millis(350)).await;

        // Rework cycle 1, finding 1: a strict closure that returns `Err` (HiGHS
        // `TimeLimit`/`NoSolutionFound`, or anything else) must run the fallback exactly like the
        // outer timeout does, rather than propagating the raw error out of `solve_bounded`.
        let strict_err =
            || Err::<(i32, SolveGrade), anyhow::Error>(anyhow::anyhow!("NoSolutionFound"));
        let fallback = || Ok::<_, anyhow::Error>(9);
        let (v, grade, cause) = solve_bounded(
            strict_err,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 9, "the fallback's answer, not a propagated error");
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("fix-and-round error"));

        // Brief G-brain leftover: a strict closure that PANICS (a JoinError, not a normal `Err`)
        // must also run the fallback instead of propagating a hard error. `spawn_blocking` catches
        // the panic (no `panic = "abort"` profile is set) and reports it as a `JoinError`; the
        // default panic hook still prints the panic message to stderr, which is expected noise for
        // this one test, not a failure.
        let strict_panics = || -> Result<(i32, SolveGrade), anyhow::Error> { panic!("boom") };
        let fallback = || Ok::<_, anyhow::Error>(7);
        let (v, grade, cause) = solve_bounded(
            strict_panics,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 7, "the fallback's answer, not a propagated panic");
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("panicked"));

        // Rework cycle 1, finding 1's salvage: a strict closure that stores a relaxed plan in
        // `salvage` as soon as it has one, then keeps running past the outer timeout, must have
        // that STORED plan returned (graded `Relaxed`) rather than a brand-new fallback LP.
        let salvage: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let salvage_for_strict = Arc::clone(&salvage);
        let strict_salvages_then_hangs = move || {
            *salvage_for_strict.lock().unwrap() = Some(42);
            std::thread::sleep(StdDuration::from_millis(300));
            Ok::<_, anyhow::Error>((1, SolveGrade::Rounded))
        };
        let fallback = || Ok::<_, anyhow::Error>(99);
        let (v, grade, cause) = solve_bounded(
            strict_salvages_then_hangs,
            fallback,
            StdDuration::from_millis(20),
            StdDuration::from_millis(500),
            false,
            salvage,
        )
        .await
        .unwrap();
        assert_eq!(
            v, 42,
            "the salvaged relaxed plan, not the fallback's fresh answer"
        );
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("salvaged"));
        // Give the detached stuck thread time to release the permit for later tests.
        tokio::time::sleep(StdDuration::from_millis(350)).await;

        // item 10 (rework cycle 2, finding 10): the strict-`Err` arm (as opposed to the
        // outer-timeout arm above) must ALSO prefer a salvaged relaxed plan over paying for a fresh
        // fallback LP — and, critically, over losing the plan entirely if that fresh fallback then
        // also errors. Kept in THIS test function (not a separate `#[tokio::test]`) deliberately:
        // `solve_bounded` gates on a module-level `static` semaphore shared by every call in the
        // process, so a standalone test risks racing this file's OTHER `solve_bounded` tests for the
        // same permit under `cargo test`'s default parallel runner (observed: it intermittently took
        // the "previous fix-and-round still running" branch instead of the one under test) — exactly
        // why every other multi-scenario check here already lives in one sequential test.
        let salvage: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let salvage_for_strict = Arc::clone(&salvage);
        let strict_salvages_then_errors = move || {
            *salvage_for_strict.lock().unwrap() = Some(7);
            Err::<(i32, SolveGrade), anyhow::Error>(anyhow::anyhow!("pinned re-solve blew up"))
        };
        // A fallback that would be an obviously WRONG answer if ever reached, so the assertion
        // below proves the salvage path won rather than merely matching by coincidence.
        let fallback = || Ok::<_, anyhow::Error>(99);
        let (v, grade, cause) = solve_bounded(
            strict_salvages_then_errors,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(500),
            false,
            salvage,
        )
        .await
        .unwrap();
        assert_eq!(
            v, 7,
            "the salvaged relaxed plan, not the fallback's fresh answer"
        );
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("salvaged"));
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
        );
        assert_eq!(curve.len(), HORIZON_BLOCKS);
    }
    /// The three failure modes the panel found in `measured_run_hours`, pinned on its pure core.
    #[test]
    fn run_hours_from_samples_is_strict_at_both_ends() {
        use crate::influxdb::TimeSample;
        let t = |h: i64, m: i64| {
            chrono::DateTime::from_timestamp(h * 3600 + m * 60, 0).expect("valid timestamp")
        };
        let (start, now) = (t(0, 0), t(1, 0));
        let s = |at: chrono::DateTime<Utc>, w: f64| TimeSample { time: at, value: w };
        // Rated 2 kW ⇒ the half-rated threshold is 1 kW.
        let rated = 2.0;

        // A stop-stamped sample AT `start` covers the quarter-hour BEFORE the occurrence opened —
        // the appliance running under its own control just before the window. Not ours.
        assert_eq!(
            run_hours_from_samples(&[s(start, 2000.0)], start, now, rated),
            0.0
        );
        // Flux clamps the trailing partial window's stop to the range, so it is stamped exactly AT
        // `now` — with block-aligned bounds (the caller floors `now` to the grid) a sample at `now`
        // is a COMPLETE block and counts; one past `now` (out of range) must not.
        assert_eq!(
            run_hours_from_samples(&[s(now + Duration::minutes(15), 2000.0)], start, now, rated),
            0.0
        );
        // Genuine in-range blocks count; a sub-threshold one (standby draw) does not.
        let samples = [
            s(t(0, 15), 2000.0),
            s(t(0, 30), 500.0),
            s(t(0, 45), 1900.0),
            s(t(1, 0), 2000.0),
        ];
        assert_eq!(run_hours_from_samples(&samples, start, now, rated), 0.75);
    }

    /// A minimal, otherwise-zeroed [`TimelineBlock`] at `t`, for tests that only care about block
    /// identity/ordering (e.g. `next_timeline_step`).
    fn test_block(t: DateTime<Utc>) -> TimelineBlock {
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
            slot: "regular".to_string(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    // Item G acceptance: "next_step is block 1 of the timeline with t = grid.block_start(1)".
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

    // Item G acceptance: "absent when the plan has < 2 blocks".
    #[test]
    fn next_timeline_step_is_absent_with_fewer_than_2_blocks() {
        assert!(next_timeline_step(&[]).is_none());
        assert!(next_timeline_step(&[test_block(utc("2026-01-15T00:15:00Z"))]).is_none());
    }
}
