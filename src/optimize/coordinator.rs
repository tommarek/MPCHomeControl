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
use super::unified::{optimize_unified, EvSpec, FlowParams, UnifiedPlan};
use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::tools::sun::calculate_tilted_irradiance;

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
    /// Per-zone constant internal heat gain (W) — occupants/appliances/fireplace — injected at each
    /// zone's air node alongside the boundary temperatures and solar. The calibrated term from
    /// [`crate::validate::calibrate_internal_gains`]; empty = none. Keeps the live forecast from
    /// running cold in rooms with unmodelled gains (kitchen cooking, livingroom fireplace).
    pub internal_gain_w: HashMap<String, f64>,
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
    Ok(n)
}

/// Evaluate the PV and consumption forecasts over the horizon into per-block power (kW). PV uses
/// the `pv_kw_override` (e.g. the calibrated Solcast curve) when present, else the clear-sky
/// model; the consumption forecast is scaled by the self-correction `load_scale`.
fn forecast_pv_load(
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
        override_kw.clone()
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
            consumption.predict(ctx.temperature_c[h], local.hour(), is_weekend) * ctx.load_scale
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
        let cloud = Ratio::new::<ratio>(ctx.cloud_cover[h]);
        for surf in &net.solar_surfaces {
            let irradiance = calculate_tilted_irradiance(
                ctx.latitude,
                ctx.longitude,
                &when,
                cloud,
                surf.tilt,
                surf.azimuth,
            );
            ss.set_flux(&mut u, surf.node, irradiance * surf.area);
        }
        // Combined per-zone air-node flux: the constant internal gain plus any scheduled loads active
        // at this block's local time (their fitted magnitude × signed unit profile). Accumulate into
        // one map then write once per zone so a gain and a scheduled load on the same air node combine.
        let local = block_midpoint(ctx, h).with_timezone(&ctx.local_offset);
        let (month, minute) = (local.month(), local.hour() * 60 + local.minute());
        let mut air_flux_w: HashMap<&str, f64> = HashMap::new();
        for (zone, &gain_w) in &ctx.internal_gain_w {
            *air_flux_w.entry(zone.as_str()).or_insert(0.0) += gain_w;
        }
        for (load, &w) in ctx.scheduled_loads.iter().zip(&ctx.scheduled_w) {
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
    // HVAC zones get an air-node actuator/kernel; the outdoor-temp forecast feeds each unit's COP.
    let thermal = build_context(
        ss,
        net,
        x0,
        &u_known,
        ctx.step_seconds,
        &hvac.served_zones(),
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
        amortisation: ctx.battery_amortisation,
        terminal_value: ctx.terminal_value,
    };
    optimize_unified(
        battery,
        heating,
        hvac,
        &thermal,
        &inputs,
        &flow,
        &ctx.temperature_c,
        ev,
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
            pv_kw_override: None,
            load_scale: 1.0,
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
            pv_kw_override: None,
            load_scale: 1.0,
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
            pv_kw_override: None,
            load_scale: 1.0,
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
        );
        assert!(err.is_err());
    }
}
