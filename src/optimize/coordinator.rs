//! Forecast-to-dispatch coordinator.
//!
//! The seam between the forecast layer and the optimizer: it evaluates the PV and consumption
//! forecast models over a horizon to produce per-hour PV and load, then runs the battery
//! dispatch against the electricity prices. Pure and IO-free — the forecasts and prices are
//! supplied by the caller (the data layer reads them from InfluxDB).
//!
//! The coordinator works on a fixed-duration block grid ([`ForecastContext::step_seconds`]; 15-min
//! blocks for the live plan, matching the OTE price grid). The consumption model is hour-binned, so
//! each block takes its hour's average power (kW), which is what the optimizer's power balance
//! wants; PV is sampled at each block's midpoint to share that block-average convention.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use chrono::{DateTime, Datelike, Duration, FixedOffset, Timelike, Utc, Weekday};
use nalgebra::DVector;
use uom::si::{
    f64::{Angle, Power, Ratio, ThermodynamicTemperature},
    power::{kilowatt, watt},
    ratio::ratio,
    thermodynamic_temperature::degree_celsius,
};

use super::battery::{optimize_dispatch, BatterySpec, DispatchInputs, DispatchPlan};
use super::config::{HeatingConfig, HvacConfig, ScheduledLoad};
use super::thermal::build_context;
use super::unified::{optimize_unified, ControllableLoadSpec, EvSpec, FlowParams, UnifiedPlan};

/// Fraction of end-of-horizon banked slab heat that survives to displace future heating (the rest
/// leaks through the envelope before the house needs it). A conservative constant; the value it
/// scales is already the median-import-based `terminal_value`.
const TERMINAL_HEAT_RETENTION: f64 = 0.8;

/// Does the horizon actually need heating? True when any heated zone's free response (all
/// actuators off) dips within `MARGIN_K` of its band floor — the gate for the terminal slab-heat
/// credit, which values banked heat only in seasons where it displaces real future heating.
fn heating_demanded(thermal: &super::thermal::ThermalContext, heating: &HeatingConfig) -> bool {
    const MARGIN_K: f64 = 1.0;
    const KELVIN_OFFSET: f64 = 273.15;
    heating.zones.iter().any(|(zone, z)| {
        thermal.free_response.get(zone).is_some_and(|fr| {
            fr.iter()
                .any(|&t_k| t_k < z.t_min + KELVIN_OFFSET + MARGIN_K)
        })
    })
}

use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::tools::sun::{tilted_irradiance, SolarInput};

/// Per-block forecast context for a dispatch plan. The forecast vectors must all be the same
/// length; that length is the planning horizon in blocks (the block duration is [`Self::step_seconds`]).
#[derive(Debug, Clone)]
pub struct ForecastContext {
    pub latitude: Angle,
    pub longitude: Angle,
    /// Start of the first block (UTC).
    pub start: DateTime<Utc>,
    /// Duration of one block / dispatch step, in seconds (e.g. 900 for 15-minute blocks, the OTE
    /// price granularity; 3600 for hourly). The thermal model runs on the same grid.
    pub step_seconds: f64,
    /// Fixed offset from UTC to the site's local civil time, used **only** for the consumption
    /// model's hour-of-day / weekday lookup (solar position stays in UTC). For central Europe
    /// use +1 in winter, +2 in summer; DST transitions within a horizon are not handled.
    pub local_offset: FixedOffset,
    /// Outside temperature (°C) per hour.
    pub temperature_c: Vec<f64>,
    /// Ground temperature (°C) under the slab — the `ground` boundary condition for the thermal
    /// model. A seasonal constant is a fine first approximation; it varies far slower than air.
    pub ground_temperature_c: f64,
    /// Cloud cover (fraction 0..1) per hour.
    pub cloud_cover: Vec<f64>,
    /// Per-block solar input for the thermal model (measured radiation → GHI split → cloud
    /// model), aligned to the block grid. Empty ⇒ build [`SolarInput::Cloud`] from `cloud_cover`
    /// per block (bit-identical to the legacy path); when non-empty must match the horizon.
    pub solar: Vec<SolarInput>,
    /// Per-zone constant internal heat gain (W) — occupants/appliances/fireplace — injected at each
    /// zone's air node alongside the boundary temperatures and solar. The calibrated term from
    /// [`crate::validate::calibrate_internal_gains`]; empty = none. Keeps the live forecast from
    /// running cold in rooms with unmodelled gains (kitchen cooking, livingroom fireplace).
    pub internal_gain_w: HashMap<String, super::config::GainProfile>,
    /// Scheduled heat fluxes at a zone's air node (e.g. a water heat-pump that cools its room on a
    /// seasonal schedule) — only the direction + schedule; the magnitude is [`Self::scheduled_w`].
    /// Applied at each load's zone air node alongside the internal gain, evaluated at the block's
    /// local time. Empty = none.
    pub scheduled_loads: Vec<ScheduledLoad>,
    /// Fitted magnitude (W, ≥ 0) of each [`Self::scheduled_loads`] entry, aligned 1:1 (the calibration
    /// learns it; see [`crate::validate::fit_gains`]). Empty or shorter than `scheduled_loads` ⇒ the
    /// missing entries contribute nothing.
    pub scheduled_w: Vec<f64>,
    /// Grid import price (price-units per kWh) per hour.
    pub import_price: Vec<f64>,
    /// Grid export / feed-in price (price-units per kWh) per block.
    pub export_price: Vec<f64>,
    /// Per-block: may the inverter export to the grid? (false below the export-floor spot price.)
    pub export_allowed: Vec<bool>,
    /// Per-block: is the inverter powered on? (false in deeply-negative-price blocks.)
    pub inverter_on: Vec<bool>,
    /// Battery wear charged per kWh discharged (price-units), folded into the dispatch objective.
    pub battery_amortisation: f64,
    /// Value of one kWh left in the battery at the horizon end (price-units); stops the optimizer
    /// draining the battery at the edge of the horizon.
    pub terminal_value: f64,
    /// Optional end-of-horizon battery reserve (see [`DispatchInputs::min_final_soc_kwh`]); set
    /// it in a rolling/MPC loop to stop the optimizer draining the battery at the horizon edge.
    pub min_final_soc_kwh: Option<f64>,
    /// Per-block: is this block's price the PLACEHOLDER curve (unpublished day-ahead tail) rather
    /// than a real OTE price? Battery arbitrage (grid-charge, battery→grid) is forbidden in
    /// placeholder blocks — their profitability would rest on an invented spread. Empty = all
    /// real (the `block_local_minutes` convention).
    pub price_is_placeholder: Vec<bool>,
    /// Grid-connection import cap (kW, `config.grid.max_import_kw`); `None` ⇒ unconstrained.
    pub max_import_kw: Option<f64>,
    /// Grid-connection export cap (kW, `config.grid.max_export_kw`); `None` ⇒ unconstrained.
    pub max_export_kw: Option<f64>,
    /// Optional PV forecast (kW per hour) to use instead of the clear-sky [`PvArray`] model — e.g.
    /// the calibrated Solcast curve from InfluxDB. Must match the horizon length when set.
    pub pv_kw_override: Option<Vec<f64>>,
    /// Self-correction applied to the consumption forecast (1.0 = none); see
    /// [`crate::forecast::calibration`].
    pub load_scale: f64,
}

/// The midpoint (UTC) of block `h`, where PV/solar are sampled so they share the block-average
/// convention of the consumption model.
fn block_midpoint(ctx: &ForecastContext, h: usize) -> DateTime<Utc> {
    let step = ctx.step_seconds as i64;
    ctx.start + Duration::seconds(step * h as i64 + step / 2)
}

/// The start instant (UTC) of block `h`.
fn block_start(ctx: &ForecastContext, h: usize) -> DateTime<Utc> {
    ctx.start + Duration::seconds(ctx.step_seconds as i64 * h as i64)
}

/// The horizon-length consistency check the optimizer never sees (it only gets the prices).
fn check_forecast_lengths(ctx: &ForecastContext) -> Result<usize> {
    let n = ctx.import_price.len();
    ensure!(
        ctx.temperature_c.len() == n && ctx.cloud_cover.len() == n,
        "temperature and cloud-cover forecasts must match the price-horizon length"
    );
    ensure!(
        ctx.export_allowed.len() == n && ctx.inverter_on.len() == n,
        "export_allowed/inverter_on gates must match the price-horizon length"
    );
    ensure!(
        ctx.solar.is_empty() || ctx.solar.len() == n,
        "solar inputs must be empty or match the price-horizon length"
    );
    Ok(n)
}

/// Evaluate the PV and consumption forecasts over the horizon into per-block power (kW). PV uses
/// the `pv_kw_override` (e.g. the calibrated Solcast curve) when present, else the clear-sky
/// model; the consumption forecast is scaled by the self-correction `load_scale`.
pub(crate) fn forecast_pv_load(
    pv: &PvArray,
    consumption: &ConsumptionModel,
    ctx: &ForecastContext,
    n: usize,
) -> Result<(Vec<f64>, Vec<f64>)> {
    ensure!(
        ctx.load_scale.is_finite() && ctx.load_scale > 0.0,
        "load_scale must be finite and positive"
    );
    let pv_kw = if let Some(override_kw) = &ctx.pv_kw_override {
        ensure!(
            override_kw.len() == n,
            "pv_kw_override length ({}) must match the horizon ({n})",
            override_kw.len()
        );
        // Clamp at 0: the flow-split equality (solar legs are all >= 0) is hard-infeasible for a
        // negative PV value, so one bad stored Solcast sample would kill the whole plan. NaN is
        // rejected by DispatchInputs::validate downstream.
        override_kw.iter().map(|kw| kw.max(0.0)).collect()
    } else {
        (0..n)
            .map(|h| {
                let cloud = Ratio::new::<ratio>(ctx.cloud_cover[h]);
                pv.predict(ctx.latitude, ctx.longitude, &block_midpoint(ctx, h), cloud)
                    .get::<kilowatt>()
            })
            .collect()
    };
    let load_kw = (0..n)
        .map(|h| {
            // Hour-of-day and weekend are local-clock concepts for the consumption model.
            let local = block_start(ctx, h).with_timezone(&ctx.local_offset);
            let is_weekend = matches!(local.weekday(), Weekday::Sat | Weekday::Sun);
            // Clamped ≥ 0 like PV: the load-balance equality can't absorb a negative demand.
            (consumption.predict(ctx.temperature_c[h], local.hour(), is_weekend) * ctx.load_scale)
                .max(0.0)
        })
        .collect();
    Ok((pv_kw, load_kw))
}

/// Evaluate the forecast models over the horizon into the optimizer's per-hour PV and load.
fn forecast_inputs(
    pv: &PvArray,
    consumption: &ConsumptionModel,
    ctx: &ForecastContext,
) -> Result<DispatchInputs> {
    let n = check_forecast_lengths(ctx)?;
    let (pv_kw, load_kw) = forecast_pv_load(pv, consumption, ctx, n)?;
    Ok(DispatchInputs {
        dt_hours: ctx.step_seconds / 3600.0,
        import_price: ctx.import_price.clone(),
        export_price: ctx.export_price.clone(),
        pv_kw,
        load_kw,
        min_final_soc_kwh: ctx.min_final_soc_kwh,
    })
}

/// Build the optimizer inputs from the forecast models and solve the battery dispatch.
pub fn plan_dispatch(
    pv: &PvArray,
    consumption: &ConsumptionModel,
    battery: &BatterySpec,
    ctx: &ForecastContext,
) -> Result<DispatchPlan> {
    optimize_dispatch(battery, &forecast_inputs(pv, consumption, ctx)?)
}

/// The per-block known thermal inputs: outside/ground boundary temperatures and solar gain on each
/// oriented surface, with heating off. This is everything the thermal free-response needs.
fn known_thermal_inputs(
    ss: &StateSpace,
    net: &RcNetwork,
    ctx: &ForecastContext,
    n: usize,
) -> Vec<DVector<f64>> {
    let outside = net.zone_indices.get("outside").copied();
    let ground = net.zone_indices.get("ground").copied();
    let mut u_known = Vec::with_capacity(n);
    for h in 0..n {
        let mut u = ss.zero_input();
        if let Some(node) = outside {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(ctx.temperature_c[h]),
            );
        }
        if let Some(node) = ground {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(ctx.ground_temperature_c),
            );
        }
        let when = block_midpoint(ctx, h);
        let input = ctx.solar.get(h).copied().unwrap_or(SolarInput::Cloud {
            cloud: ctx.cloud_cover[h],
        });
        for surf in &net.solar_surfaces {
            let irradiance = tilted_irradiance(
                ctx.latitude,
                ctx.longitude,
                &when,
                input,
                surf.tilt,
                surf.azimuth,
            );
            ss.set_flux(&mut u, surf.node, irradiance * surf.area * surf.absorptance);
        }
        // Combined per-zone air-node flux: the constant internal gain plus any scheduled loads active
        // at this block's local time (their fitted magnitude × signed unit profile). Accumulate into
        // one map then write once per zone so a gain and a scheduled load on the same air node combine.
        let local = block_midpoint(ctx, h).with_timezone(&ctx.local_offset);
        let (month, minute) = (local.month(), local.hour() * 60 + local.minute());
        let mut air_flux_w: HashMap<&str, f64> = HashMap::new();
        for (zone, gain) in &ctx.internal_gain_w {
            *air_flux_w.entry(zone.as_str()).or_insert(0.0) += gain.at(minute);
        }
        // Transmitted window solar `g × A × I`, split [`WINDOW_SOLAR_TO_AIR`] to the air node and
        // the rest into the zone's floor slab (its heating-marker nodes, the modelled floor mass)
        // when it has one. Accumulated maps — several windows / gains can share nodes without
        // clobbering each other.
        let mut marker_flux_w: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
        for w in &net.window_surfaces {
            let irradiance =
                tilted_irradiance(ctx.latitude, ctx.longitude, &when, input, w.tilt, w.azimuth);
            let gain_w = (irradiance * w.area * w.g).get::<watt>();
            match net
                .marker_indices
                .get_vec(&(w.zone.clone(), "heating".to_string()))
                .filter(|nodes| !nodes.is_empty())
            {
                Some(nodes) => {
                    *air_flux_w.entry(w.zone.as_str()).or_insert(0.0) +=
                        gain_w * crate::rc_network::WINDOW_SOLAR_TO_AIR;
                    let per_node = gain_w * (1.0 - crate::rc_network::WINDOW_SOLAR_TO_AIR)
                        / nodes.len() as f64;
                    for &node in nodes {
                        *marker_flux_w.entry(node).or_insert(0.0) += per_node;
                    }
                }
                None => *air_flux_w.entry(w.zone.as_str()).or_insert(0.0) += gain_w,
            }
        }
        for (node, flux_w) in marker_flux_w {
            ss.set_flux(&mut u, node, Power::new::<watt>(flux_w));
        }
        for (load, &w) in ctx.scheduled_loads.iter().zip(&ctx.scheduled_w) {
            // A *controllable* load is NOT a passive flux here — the optimizer switches it, and its
            // heat enters via the kernel scaled by the on/off decision. Including it here too would
            // double-count it. (Forecast-only: the calibration drive over real past data still applies
            // its measured/scheduled flux, since the load actually ran then.)
            if load.controllable {
                continue;
            }
            *air_flux_w.entry(load.zone.as_str()).or_insert(0.0) +=
                w * load.unit_profile(month, minute);
        }
        for (zone, flux_w) in air_flux_w {
            if let Some(&node) = net.zone_indices.get(zone) {
                ss.set_flux(&mut u, node, Power::new::<watt>(flux_w));
            }
        }
        u_known.push(u);
    }
    u_known
}

/// The kernel-cache inputs derived from config alone — shared by the startup kernel build and
/// [`plan_unified`]'s per-tick path so the cached [`crate::optimize::thermal::KernelSet`] always
/// matches what the live solve would build fresh (a divergence silently invalidates the cache).
pub fn kernel_inputs(
    config: &crate::optimize::config::ControlConfig,
) -> (Vec<String>, Vec<(String, String)>) {
    let hvac_zones = config
        .hvac
        .as_ref()
        .map(|h| h.served_zones())
        .unwrap_or_default();
    let load_sources = config
        .scheduled_loads
        .iter()
        .filter(|l| l.controllable)
        .map(|l| (load_name(l), l.zone.clone()))
        .collect();
    (hvac_zones, load_sources)
}

/// The stable identifier for a scheduled load — its `label`, or its `zone` when the label is empty.
/// Used as the controllable-load schedule / kernel key (shared by the plan report and the controller).
pub fn load_name(load: &ScheduledLoad) -> String {
    if load.label.is_empty() {
        load.zone.clone()
    } else {
        load.label.clone()
    }
}

/// Build the per-block [`ControllableLoadSpec`]s for the optimizer from the context's controllable
/// scheduled loads. The window for block `i` is `unit_profile != 0` at that block's local time (so the
/// optimizer can switch the load only inside its configured windows). Non-controllable loads are
/// skipped (they enter the thermal free-response as a passive flux instead).
pub(crate) fn controllable_load_specs(
    ctx: &ForecastContext,
    n: usize,
) -> Vec<ControllableLoadSpec> {
    ctx.scheduled_loads
        .iter()
        .filter(|l| l.controllable)
        .map(|l| {
            let window: Vec<bool> = (0..n)
                .map(|h| {
                    let local = block_midpoint(ctx, h).with_timezone(&ctx.local_offset);
                    l.unit_profile(local.month(), local.hour() * 60 + local.minute()) != 0.0
                })
                .collect();
            let sign = match l.kind {
                super::config::LoadKind::Sink => -1.0,
                super::config::LoadKind::Source => 1.0,
            };
            ControllableLoadSpec {
                name: load_name(l),
                zone: l.zone.clone(),
                rated_kw: l.power_w.unwrap_or(0.0) / 1000.0,
                heat_kw: sign * l.controllable_heat_kw(),
                window,
                run_hours: l.run_hours.unwrap_or(0.0),
            }
        })
        .collect()
}

/// Optional cross-cutting knobs for [`plan_unified`], bundled so the signature doesn't grow a
/// parameter per feature. `Default` = the plain behaviour (fresh kernels, no commitment, strict
/// binaries) — every pre-existing caller passes `PlanOptions::default()`.
#[derive(Default, Clone, Copy)]
pub struct PlanOptions<'a> {
    /// Startup-built kernel cache (see [`crate::optimize::thermal::KernelSet`]); `None` builds
    /// fresh, bit-identically.
    pub kernels: Option<&'a crate::optimize::thermal::KernelSet>,
    /// Block-0 heating commitment from the MPC loop's latch: the relays decided at the block start,
    /// held for the block so per-minute re-plans can't sub-cycle them. Fixed INTO the LP (the relay
    /// binary is pinned), so `first_step`, the timeline and both armed controllers agree by
    /// construction. `None` = block 0 optimizes freely (the on-demand/advisory paths).
    pub committed_heat: Option<&'a HashMap<String, f64>>,
    /// Relax every binary to its `[0, 1]` LP interval — the timeout fallback: a plan with
    /// fractional relays beats no plan when the MILP stalls (flagged as a placeholder upstream).
    pub relax_binaries: bool,
    /// Fix-and-round: pin every binary to these pre-rounded values (min = max) — the fallback's
    /// integral re-solve. See [`super::unified::FixedBinaries`].
    pub fixed_binaries: Option<&'a super::unified::FixedBinaries>,
}

/// Plan the whole house: drive the unified battery + heating optimizer from the forecasts.
///
/// Builds the per-hour known thermal inputs (outside/ground temperatures + solar) from the
/// forecast, condenses the thermal model around them, and solves the unified dispatch. `x0` is the
/// initial thermal state (Kelvin); seeding the unmeasured wall/slab masses is the caller's job — a
/// state estimator is a documented follow-up.
#[allow(clippy::too_many_arguments)] // the model, the forecast/spec inputs, and the state are all genuinely distinct
pub fn plan_unified(
    pv: &PvArray,
    consumption: &ConsumptionModel,
    battery: &BatterySpec,
    heating: &HeatingConfig,
    hvac: &HvacConfig,
    ss: &StateSpace,
    net: &RcNetwork,
    ctx: &ForecastContext,
    x0: &DVector<f64>,
    // Controllable EV chargers the optimizer schedules.
    ev: &[EvSpec],
    // Expected exogenous load (kW) from *monitored* (uncontrollable) chargers, added to the house
    // load so the plan reacts around it; empty ⇒ none.
    ev_monitored_kw: &[f64],
    opts: PlanOptions<'_>,
) -> Result<UnifiedPlan> {
    let n = check_forecast_lengths(ctx)?;
    let (pv_kw, mut load_kw) = forecast_pv_load(pv, consumption, ctx, n)?;
    if !ev_monitored_kw.is_empty() {
        ensure!(
            ev_monitored_kw.len() == n,
            "ev_monitored_kw length ({}) must match the horizon ({n})",
            ev_monitored_kw.len()
        );
        for (l, m) in load_kw.iter_mut().zip(ev_monitored_kw) {
            *l += m.max(0.0);
        }
    }
    let u_known = known_thermal_inputs(ss, net, ctx, n);
    // Controllable scheduled loads the optimizer switches (deferrable boiler-style loads); each gets an
    // air-node kernel so its heat-when-on couples into the comfort prediction. Drop any on an
    // unmodelled zone (no thermal state ⇒ no kernel): it would otherwise schedule electricity but
    // couple no heat. Mirrors `build_context`'s kernel filter, kept consistent.
    let controllable: Vec<ControllableLoadSpec> = controllable_load_specs(ctx, n)
        .into_iter()
        .filter(|l| {
            let modelled = net
                .zone_indices
                .get(&l.zone)
                .and_then(|&node| ss.state_index(node))
                .is_some();
            if !modelled {
                eprintln!(
                    "  plan_unified: controllable load {:?} on unmodelled zone {:?} — skipping",
                    l.name, l.zone
                );
            }
            modelled
        })
        .collect();
    let load_sources: Vec<(String, String)> = controllable
        .iter()
        .map(|l| (l.name.clone(), l.zone.clone()))
        .collect();
    // HVAC zones get an air-node actuator/kernel; the outdoor-temp forecast feeds each unit's COP.
    let thermal = build_context(
        ss,
        net,
        x0,
        &u_known,
        ctx.step_seconds,
        &hvac.served_zones(),
        &load_sources,
        opts.kernels,
    )?;
    let inputs = DispatchInputs {
        dt_hours: ctx.step_seconds / 3600.0,
        import_price: ctx.import_price.clone(),
        export_price: ctx.export_price.clone(),
        pv_kw,
        load_kw,
        min_final_soc_kwh: ctx.min_final_soc_kwh,
    };
    let flow = FlowParams {
        export_allowed: ctx.export_allowed.clone(),
        inverter_on: ctx.inverter_on.clone(),
        price_placeholder: ctx.price_is_placeholder.clone(),
        amortisation: ctx.battery_amortisation,
        terminal_value: ctx.terminal_value,
        // The thermal twin: banked slab heat displaces future heating electricity at 1/COP per
        // kWh thermal, discounted for envelope leakage before the banked heat is consumed.
        // Gated on ACTUAL heating demand: in summer/shoulder seasons banked heat displaces
        // nothing, and the credit would otherwise buy tail heat year-round whenever a tail block
        // undercuts the median. Demand = some heated zone's free response dips within 1 K of its
        // band floor inside the horizon (i.e. the horizon itself would need heating).
        terminal_heat_value: if heating_demanded(&thermal, heating) {
            ctx.terminal_value / heating.cop * TERMINAL_HEAT_RETENTION
        } else {
            0.0
        },
        max_import_kw: ctx.max_import_kw,
        max_export_kw: ctx.max_export_kw,
    };
    // Each block's local minute-of-day (at the midpoint, matching the block-average convention) —
    // drives the comfort-band schedule windows (night setback).
    let block_local_minutes: Vec<u32> = (0..n)
        .map(|h| {
            let local = block_midpoint(ctx, h).with_timezone(&ctx.local_offset);
            local.hour() * 60 + local.minute()
        })
        .collect();
    optimize_unified(
        battery,
        heating,
        hvac,
        &thermal,
        &inputs,
        &flow,
        &ctx.temperature_c,
        ev,
        &controllable,
        opts.committed_heat,
        opts.relax_binaries,
        &block_local_minutes,
        opts.fixed_binaries,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uom::si::{angle::degree, ratio::ratio as ratio_unit};

    fn deg(d: f64) -> Angle {
        Angle::new::<degree>(d)
    }

    fn pv_array() -> PvArray {
        PvArray {
            peak_power: uom::si::f64::Power::new::<kilowatt>(5.0),
            tilt: deg(30.0),
            azimuth: deg(180.0),
            system_efficiency: Ratio::new::<ratio_unit>(0.85),
        }
    }

    fn consumption_model() -> ConsumptionModel {
        // Cold evenings are high-load, warm middays low-load (keyed on local hour).
        let mut m = ConsumptionModel::new();
        for _ in 0..4 {
            m.add_sample(-5.0, 18, false, 3.0);
        }
        for _ in 0..4 {
            m.add_sample(20.0, 12, false, 0.5);
        }
        m.build();
        m
    }

    fn battery() -> BatterySpec {
        BatterySpec {
            max_charge_kw: 3.0,
            max_discharge_kw: 3.0,
            charge_efficiency: 0.95,
            discharge_efficiency: 0.95,
            min_soc_kwh: 0.0,
            max_soc_kwh: 10.0,
            initial_soc_kwh: 2.0,
        }
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Context with the local offset set to UTC so hour bins map 1:1 (other tests vary it).
    fn context() -> ForecastContext {
        let temperature_c = (0..24).map(|h| if h == 18 { -5.0 } else { 20.0 }).collect();
        ForecastContext {
            latitude: deg(49.5),
            longitude: deg(17.4),
            start: utc("2023-06-21T00:00:00Z"),
            step_seconds: 3600.0,
            local_offset: FixedOffset::east_opt(0).unwrap(),
            temperature_c,
            ground_temperature_c: 10.0,
            cloud_cover: vec![0.0; 24],
            solar: Vec::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            import_price: vec![0.20; 24],
            export_price: vec![0.05; 24],
            export_allowed: vec![true; 24],
            inverter_on: vec![true; 24],
            battery_amortisation: 0.0,
            terminal_value: 0.0,
            min_final_soc_kwh: None,
            max_import_kw: None,
            max_export_kw: None,
            pv_kw_override: None,
            load_scale: 1.0,
            price_is_placeholder: Vec::new(),
        }
    }

    #[test]
    fn forecast_inputs_reflect_time_and_temperature() {
        let model = consumption_model();
        let inputs = forecast_inputs(&pv_array(), &model, &context()).unwrap();
        assert_eq!(inputs.pv_kw.len(), 24);
        assert_eq!(inputs.pv_kw[0], 0.0, "no PV at midnight");
        assert!(inputs.pv_kw[11] > 0.0, "PV around noon");
        // Exact pass-through of the consumption lookup (UTC == local here).
        assert_eq!(inputs.load_kw[18], model.predict(-5.0, 18, false));
        assert_eq!(inputs.load_kw[12], model.predict(20.0, 12, false));
        assert!(inputs.load_kw[18] > inputs.load_kw[12]);
    }

    #[test]
    fn consumption_uses_local_time() {
        // local_offset +2: UTC 06:00 is local 08:00. A model whose high-load sample is at local
        // hour 8 must be picked up for the first hour when the UTC start is 06:00.
        let mut model = ConsumptionModel::new();
        for _ in 0..4 {
            model.add_sample(10.0, 8, false, 5.0);
        }
        model.build();
        let ctx = ForecastContext {
            latitude: deg(49.5),
            longitude: deg(17.4),
            start: utc("2023-06-21T06:00:00Z"),
            step_seconds: 3600.0,
            local_offset: FixedOffset::east_opt(2 * 3600).unwrap(),
            temperature_c: vec![10.0; 3],
            ground_temperature_c: 10.0,
            cloud_cover: vec![0.0; 3],
            solar: Vec::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            import_price: vec![0.2; 3],
            export_price: vec![0.05; 3],
            export_allowed: vec![true; 3],
            inverter_on: vec![true; 3],
            battery_amortisation: 0.0,
            terminal_value: 0.0,
            min_final_soc_kwh: None,
            max_import_kw: None,
            max_export_kw: None,
            pv_kw_override: None,
            load_scale: 1.0,
            price_is_placeholder: Vec::new(),
        };
        let inputs = forecast_inputs(&pv_array(), &model, &ctx).unwrap();
        assert_eq!(
            inputs.load_kw[0], 5.0,
            "UTC 06:00 should map to local hour 8"
        );
    }

    #[test]
    fn min_final_soc_is_forwarded() {
        let mut ctx = context();
        ctx.min_final_soc_kwh = Some(4.0);
        let inputs = forecast_inputs(&pv_array(), &consumption_model(), &ctx).unwrap();
        assert_eq!(inputs.min_final_soc_kwh, Some(4.0));
    }

    #[test]
    fn plan_dispatch_produces_a_valid_plan() {
        let battery = battery();
        let plan = plan_dispatch(&pv_array(), &consumption_model(), &battery, &context()).unwrap();
        assert_eq!(plan.charge_kw.len(), 24);
        assert!(plan.total_cost.is_finite());
        for t in 0..24 {
            assert!(plan.soc_kwh[t] >= battery.min_soc_kwh - 1e-6);
            assert!(plan.soc_kwh[t] <= battery.max_soc_kwh + 1e-6);
        }
    }

    #[test]
    fn mismatched_lengths_error() {
        let mut ctx = context();
        ctx.cloud_cover.pop();
        assert!(plan_dispatch(&pv_array(), &consumption_model(), &battery(), &ctx).is_err());
    }

    #[test]
    fn pv_override_length_mismatch_errors() {
        let mut ctx = context();
        ctx.pv_kw_override = Some(vec![1.0; 5]); // shorter than the horizon
        assert!(plan_dispatch(&pv_array(), &consumption_model(), &battery(), &ctx).is_err());
    }

    #[test]
    fn non_positive_load_scale_errors() {
        let mut ctx = context();
        ctx.load_scale = 0.0;
        assert!(plan_dispatch(&pv_array(), &consumption_model(), &battery(), &ctx).is_err());
    }

    /// A tiny heated house for the unified-plan test.
    fn heated_house() -> (RcNetwork, StateSpace) {
        let model = crate::model::Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [
                        { material: "concrete", thickness: 0.05 },
                        { marker: "heating" },
                        { material: "concrete", thickness: 0.05 },
                    ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { livingroom: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["livingroom", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["livingroom", "outside"], area: 25 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    fn heating_config() -> HeatingConfig {
        use super::super::config::ZoneComfort;
        HeatingConfig {
            cop: 3.5,
            comfort_penalty: 50.0,
            zones: std::collections::HashMap::from([(
                "livingroom".to_string(),
                ZoneComfort {
                    max_heat_kw: 6.0,
                    t_min: 20.0,
                    t_max: 23.0,
                    internal_gain_w: 0.0,
                    windows: Vec::new(),
                },
            )]),
        }
    }

    #[test]
    fn plan_unified_produces_valid_plan() {
        let (net, ss) = heated_house();
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );
        // Cold winter night with a cheap-overnight / expensive-evening price split.
        let n = 12;
        let ctx = ForecastContext {
            latitude: deg(49.5),
            longitude: deg(17.4),
            start: utc("2024-01-15T00:00:00Z"),
            step_seconds: 3600.0,
            local_offset: FixedOffset::east_opt(3600).unwrap(),
            temperature_c: vec![-3.0; n],
            ground_temperature_c: 8.0,
            cloud_cover: vec![0.8; n],
            solar: Vec::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            import_price: (0..n).map(|h| if h < n / 2 { 0.1 } else { 0.5 }).collect(),
            export_price: vec![0.03; n],
            export_allowed: vec![true; n],
            inverter_on: vec![true; n],
            battery_amortisation: 0.0,
            terminal_value: 0.0,
            min_final_soc_kwh: Some(1.0),
            max_import_kw: None,
            max_export_kw: None,
            pv_kw_override: None,
            load_scale: 1.0,
            price_is_placeholder: Vec::new(),
        };
        let mut consumption = ConsumptionModel::new();
        for h in 0..24u32 {
            consumption.add_sample(-3.0, h, false, 0.4);
        }
        consumption.build();

        let plan = plan_unified(
            &pv_array(),
            &consumption,
            &battery(),
            &heating_config(),
            &HvacConfig::default(),
            &ss,
            &net,
            &ctx,
            &x0,
            &[],
            &[],
            PlanOptions::default(),
        )
        .unwrap();

        assert_eq!(plan.heat_kw["livingroom"].len(), n);
        assert!(plan.total_cost.is_finite());
        assert_eq!(plan.zone_temp_c["livingroom"].len(), n);
        // Heating is front-loaded into the cheap first half (pre-heat the slab, then coast).
        let early: f64 = plan.heat_kw["livingroom"][0..n / 2].iter().sum();
        let late: f64 = plan.heat_kw["livingroom"][n / 2..].iter().sum();
        assert!(
            early > late,
            "expected cheap-hour pre-heating: {early} vs {late}"
        );
    }

    #[test]
    fn plan_unified_rejects_mismatched_lengths() {
        let (net, ss) = heated_house();
        let x0 = DVector::from_element(ss.n_states(), 293.15);
        let mut ctx = context();
        ctx.temperature_c.pop(); // one shorter than the price horizon → length mismatch
        let err = plan_unified(
            &pv_array(),
            &consumption_model(),
            &battery(),
            &heating_config(),
            &HvacConfig::default(),
            &ss,
            &net,
            &ctx,
            &x0,
            &[],
            &[],
            PlanOptions::default(),
        );
        assert!(err.is_err());
    }

    use super::super::config::{LoadKind, LoadWindow, ScheduledLoad};

    fn boiler_load(controllable: bool) -> ScheduledLoad {
        ScheduledLoad {
            zone: "livingroom".to_string(),
            label: "boiler".to_string(),
            kind: LoadKind::Source,
            power_w: Some(2000.0),
            sensor: None,
            // Tiny heat fraction: a hot-water boiler dumps most of its energy into the tank/draw, not
            // the room — so the comfort band doesn't fight the run-hours target in this scheduling test.
            power_factor: Some(0.02),
            controllable,
            run_hours: Some(3.0),
            // Active all day (months empty, 00:00–24:00 wraps to the whole day via 00:00→00:00).
            windows: vec![LoadWindow {
                months: Vec::new(),
                start: "00:00".to_string(),
                end: "23:59".to_string(),
            }],
        }
    }

    /// `controllable_load_specs` selects only controllable loads, with the window evaluated per block.
    #[test]
    fn controllable_specs_select_only_controllable_loads() {
        let mut ctx = context();
        ctx.scheduled_loads = vec![boiler_load(false), boiler_load(true)];
        ctx.scheduled_w = vec![2000.0, 2000.0];
        let specs = controllable_load_specs(&ctx, ctx.import_price.len());
        assert_eq!(specs.len(), 1, "only the controllable load becomes a spec");
        let s = &specs[0];
        assert_eq!(s.name, "boiler");
        assert_eq!(s.zone, "livingroom");
        assert!((s.rated_kw - 2.0).abs() < 1e-9); // 2000 W rated draw
        assert!((s.heat_kw - 0.04).abs() < 1e-9); // source sign, 2 kW × factor 0.02 = 0.04 kW
        assert_eq!(s.run_hours, 3.0);
        assert!(
            s.window.iter().all(|&w| w),
            "all-day window is active every block"
        );
    }

    /// A controllable load is wired end-to-end through `plan_unified`: it is scheduled for its
    /// `run_hours` at the rated power and reported in `controllable_load_kw`. (The pure cheapest-blocks
    /// proof — with no comfort interaction — is the keystone in `unified.rs`; here a real heated zone is
    /// involved, so comfort can also pull the load, which is correct behaviour.)
    #[test]
    fn plan_unified_schedules_controllable_load_cheap_first() {
        let (net, ss) = heated_house();
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(22.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );
        let n = 12;
        let mut ctx = context();
        // Trim the all-24h context vectors to n and set a cheap-first / expensive-second price split.
        ctx.temperature_c.truncate(n);
        ctx.cloud_cover.truncate(n);
        ctx.import_price = (0..n).map(|h| if h < n / 2 { 0.1 } else { 0.5 }).collect();
        ctx.export_price = vec![0.03; n];
        ctx.export_allowed = vec![true; n];
        ctx.inverter_on = vec![true; n];
        ctx.scheduled_loads = vec![boiler_load(true)];
        ctx.scheduled_w = vec![2000.0];

        let plan = plan_unified(
            &pv_array(),
            &consumption_model(),
            &battery(),
            &heating_config(),
            &HvacConfig::default(),
            &ss,
            &net,
            &ctx,
            &x0,
            &[],
            &[],
            PlanOptions::default(),
        )
        .unwrap();
        let draw = &plan.controllable_load_kw["boiler"];
        assert_eq!(draw.len(), n);
        // Reported per-block draw is either off (0) or the rated 2 kW (an on/off relay).
        assert!(
            draw.iter().all(|&d| d < 1e-6 || (d - 2.0).abs() < 1e-6),
            "draw is on/off at the rated power: {draw:?}"
        );
        // Scheduled for ≈ run_hours (3 h) of run-time at 2 kW ⇒ ≈ 6 kWh total over the horizon.
        let total: f64 = draw.iter().sum::<f64>(); // × dt(=1 h) = kWh
        assert!((total - 6.0).abs() < 0.2, "≈3 h × 2 kW scheduled: {draw:?}");
    }
}
