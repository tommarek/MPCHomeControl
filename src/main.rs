mod app;
mod estimate;
mod ev;
mod forecast;
mod forecast_validation;
mod influxdb;
mod live;
mod live_inputs;
mod model;
mod mpc_loop;
mod optimize;
mod pv_backtest;
mod rc_network;
mod solar_forecast;
mod source;
mod state_space;
mod tools;
mod validate;
mod web;

use chrono::prelude::*;
use nalgebra::DVector;
use uom::si::{
    angle::degree,
    area::square_meter,
    energy::kilowatt_hour,
    f64::{Angle, Power, Ratio, ThermodynamicTemperature},
    heat_flux_density::watt_per_square_meter,
    power::{kilowatt, watt},
    ratio::ratio,
    thermodynamic_temperature::degree_celsius,
};

use influxdb::{price_range, InfluxDB};
use model::Model;
use rc_network::RcNetwork;
use source::SourceClients;
use state_space::StateSpace;
use tools::sun::calculate_tilted_irradiance;

/// Scratch entrypoint: load the model, build the network and state-space, and run each subsystem's
/// demo — or, with the `serve` argument, start the read-only monitoring API and MPC loop.
/// Not a finished control loop.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model = Model::load("model.json5")?;
    let rcnet: RcNetwork = (&model).into();
    let ss: StateSpace = (&rcnet).into();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "serve") {
        return run_server(rcnet, ss).await;
    }
    // `... backtest-heating <start-rfc3339> <stop-rfc3339>` validates the heat model under active
    // heating, driving it with the recorded per-zone relays over an explicit (e.g. winter) window.
    if let Some(i) = args.iter().position(|a| a == "backtest-heating") {
        let start = args.get(i + 1).cloned().unwrap_or_else(|| "-9d".into());
        let stop = args.get(i + 2).cloned().unwrap_or_else(|| "-2d".into());
        return run_backtest_heating(rcnet, ss, &start, &stop).await;
    }

    demo_database().await;
    println!("{}", rcnet.to_dot());
    demo_free_response(&rcnet, &ss)?;
    demo_solar_gain(&rcnet, &ss)?;
    demo_consumption();
    demo_pv();
    demo_battery();
    demo_plan();
    demo_heating(&rcnet, &ss)?;
    demo_validation(&rcnet, &ss).await;
    demo_estimate(&rcnet, &ss).await;
    demo_pv_backtest().await;
    demo_solcast_mpc(&rcnet, &ss).await;

    Ok(())
}

/// Capstone: feed the live Solcast PV forecast into the MPC, **self-corrected** by a calibration
/// fit from the last week's forecast-vs-actual. The calibration is recomputed from recent realized
/// data every run, so the forecast stays reliable as the days go by. Skips if the DB is unreachable.
async fn demo_solcast_mpc(rcnet: &RcNetwork, ss: &StateSpace) {
    use optimize::config::ControlConfig;

    if ss.n_states() == 0 {
        return;
    }
    let (config, db) = match (
        ControlConfig::load("config.json5"),
        InfluxDB::from_config("config.json5"),
    ) {
        (Ok(c), Ok(d)) => {
            let db = SourceClients::with_signals(d, c.data_sources.clone());
            (c, db)
        }
        _ => {
            println!("\nSolcast MPC: config or InfluxDB unavailable.");
            return;
        }
    };
    let (lat, lon) = site();
    match app::current_plan(&db, rcnet, ss, &config, lat, lon, None).await {
        Ok(r) => {
            println!(
                "\nSolcast-driven MPC — self-corrected PV forecast (next {} h):",
                r.horizon_hours
            );
            println!(
                "  Solcast {:.1} kWh raw → {:.1} kWh calibrated (×{:.2}, fit from 7-day backtest)",
                r.pv_raw_kwh, r.pv_calibrated_kwh, r.pv_calibration_scale,
            );
            println!(
                "  plan: cost {:.2} EUR, grid import {:.1} / export {:.1} kWh, heating {:.1} kWh \
                 (PV from Solcast, not the clear-sky model)",
                r.total_cost_eur, r.grid_import_kwh, r.grid_export_kwh, r.heating_kwh,
            );
        }
        Err(e) => println!("\nSolcast MPC: {e}"),
    }
}

/// Open InfluxDB for a demo, or print why it's unavailable and skip (the demos run on synthetic data
/// when the DB/token is missing, so this never aborts the program).
fn demo_db(demo: &str) -> Option<SourceClients> {
    match InfluxDB::from_config("config.json5") {
        // Best-effort signal map: use the config's `data_sources` if it loads, else the defaults.
        Ok(db) => {
            let signals = optimize::config::ControlConfig::load("config.json5")
                .map(|c| c.data_sources)
                .unwrap_or_default();
            Some(SourceClients::with_signals(db, signals))
        }
        Err(e) => {
            println!("\n{demo}: InfluxDB unavailable: {e}");
            None
        }
    }
}

/// Backtest the Solcast PV forecast against actual Growatt generation over the last week, with
/// curtailed hours excluded. Skips cleanly if the DB is unreachable.
async fn demo_pv_backtest() {
    let Some(db) = demo_db("PV backtest") else {
        return;
    };
    let offset = optimize::config::ControlConfig::load("config.json5")
        .map(|c| c.site.utc_offset_hours)
        .unwrap_or(2);
    match pv_backtest::backtest_pv(&db, offset, 7).await {
        Ok(bt) => {
            println!(
                "\nPV forecast backtest — house solar forecast vs actual generation, last 7 days (curtailed hours excluded):"
            );
            println!(
                "  {:<12}{:>9}{:>9}{:>8}{:>8}{:>7}  source",
                "date", "fcast", "actual", "RMSE", "bias", "curt"
            );
            for d in &bt.days {
                println!(
                    "  {:<12}{:>6.1}kWh{:>6.1}kWh{:>8.2}{:>+8.2}{:>6}h  {}",
                    d.date,
                    d.solcast_kwh,
                    d.actual_kwh,
                    d.rmse_kw,
                    d.bias_kw,
                    d.curtailed_hours,
                    d.source
                );
            }
            println!(
                "  total: forecast {:.0} kWh vs actual {:.0} kWh over {} scored hours; overall RMSE {:.2} kW ({} curtailed excluded)",
                bt.total_solcast_kwh, bt.total_actual_kwh, bt.scored_hours, bt.overall_rmse_kw, bt.curtailed_hours,
            );
            println!(
                "  (forecast blends Solcast + local model; InputPower is DC vs forecast AC ~+3%; latest day partial)"
            );
        }
        Err(e) => println!("\nPV backtest: {e}"),
    }
}

/// State estimator: drive the model over the last 72 h of measured outside temperature + solar to
/// recover a real initial state (the slow wall/slab masses, which we never measure), instead of a
/// flat guess. This `x0` is what the live MPC should start from. Skips if the DB is unreachable.
async fn demo_estimate(rcnet: &RcNetwork, ss: &StateSpace) {
    if ss.n_states() == 0 {
        return;
    }
    let Some(db) = demo_db("State estimate") else {
        return;
    };
    let (lat, lon) = site();
    match estimate::estimate_initial_state(&db, rcnet, ss, lat, lon, 72, 14.0).await {
        Ok(x0) => {
            println!(
                "\nThermal state estimate (model x0 from 72 h of measured history, vs a flat seed):"
            );
            for zone in ["livingroom", "bedroom", "kitchen", "office", "ground_hall"] {
                if let Some(s) = rcnet
                    .zone_indices
                    .get(zone)
                    .and_then(|&n| ss.state_index(n))
                {
                    println!(
                        "  {zone:<14} {:5.1} °C (estimated current air)",
                        tools::k_to_c(x0[s])
                    );
                }
            }
        }
        Err(e) => println!("\nState estimate failed: {e}"),
    }
}

/// Backtest the thermal model against measured house data: in summer the heating is off, so the
/// house drifts passively — drive the model with the measured outside temperature over the last
/// day and compare predicted vs measured zone temperatures. Skips cleanly if the DB is unreachable.
async fn demo_validation(rcnet: &RcNetwork, ss: &StateSpace) {
    let Some(db) = demo_db("Model validation") else {
        return;
    };
    let (lat, lon) = site();
    let cfg = validate::BacktestConfig {
        warmup_hours: 48,
        window_hours: 24,
        ground_temperature_c: 14.0,
        cloud_cover: 0.5, // fallback only; real per-hour open-meteo cloud is used when available
    };
    match validate::backtest_passive(&db, rcnet, ss, lat, lon, &cfg).await {
        Ok(results) => {
            println!(
                "\nThermal model backtest — passive drift vs measured, last {} h (after {} h warm-up, real hourly cloud):",
                cfg.window_hours, cfg.warmup_hours,
            );
            println!(
                "  {:<18}{:>9}{:>9}{:>8}{:>8}{:>8}",
                "zone", "model°C", "meas°C", "RMSE", "bias", "maxErr"
            );
            for r in &results {
                println!(
                    "  {:<18}{:>9.1}{:>9.1}{:>8.2}{:>+8.2}{:>8.2}",
                    r.zone,
                    r.predicted_final_c,
                    r.measured_final_c,
                    r.rmse_k,
                    r.mean_bias_k,
                    r.max_abs_error_k,
                );
            }
            if !results.is_empty() {
                let mean = results.iter().map(|r| r.rmse_k).sum::<f64>() / results.len() as f64;
                println!(
                    "  mean RMSE across {} zones: {mean:.2} K  (unmodeled: internal gains, forecast-vs-measured outside temp)",
                    results.len(),
                );
            }
        }
        Err(e) => println!("\nModel validation: {e}"),
    }
}

/// Validate the heat model under **active** heating: drive it with the recorded per-zone heating
/// relays (plus measured outside temperature + solar) over an explicit `[start, stop]` window and
/// score predicted vs measured zone temperatures. Use for a winter week when heating was on.
async fn run_backtest_heating(
    rcnet: RcNetwork,
    ss: StateSpace,
    start: &str,
    stop: &str,
) -> anyhow::Result<()> {
    let config = optimize::config::ControlConfig::load("config.json5")?;
    let db = SourceClients::with_signals(
        InfluxDB::from_config("config.json5")?,
        config.data_sources.clone(),
    );
    let (lat, lon) = (
        Angle::new::<degree>(config.site.latitude),
        Angle::new::<degree>(config.site.longitude),
    );
    // Score everything after a 48 h warm-up (the front of the window relaxes the unknown slab seed).
    let warmup_hours = 48;
    let total_hours = match (
        DateTime::parse_from_rfc3339(start),
        DateTime::parse_from_rfc3339(stop),
    ) {
        (Ok(a), Ok(b)) => (b - a).num_hours(),
        _ => warmup_hours + 120, // relative ranges: default to scoring ~5 days
    };
    let cfg = validate::BacktestConfig {
        warmup_hours,
        window_hours: (total_hours - warmup_hours).max(1),
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    let local_offset = chrono::FixedOffset::east_opt(config.site.utc_offset_hours * 3600)
        .expect("site.utc_offset_hours validated at config load");
    let (before, after, fit) = validate::calibrate_internal_gains(
        &db,
        &rcnet,
        &ss,
        &config.heating,
        &config.scheduled_loads,
        local_offset,
        lat,
        lon,
        &cfg,
        start,
        stop,
    )
    .await?;
    let gains = fit.gains;
    println!(
        "\nActive heating backtest {start} .. {stop}  (scored last {} h after {warmup_hours} h warm-up,\nmodel driven by the recorded per-zone heating relays + measured outside temp + solar):",
        cfg.window_hours,
    );
    println!(
        "  {:<18}{:>9}{:>9}{:>9}{:>10}",
        "zone", "RMSE pre", "RMSE cal", "bias pre", "gain (W)"
    );
    // After is sorted worst-first; show the same zones, joining the pre-calibration RMSE/bias.
    let pre: std::collections::HashMap<&str, &validate::ZoneBacktest> =
        before.iter().map(|z| (z.zone.as_str(), z)).collect();
    for a in &after {
        let b = pre.get(a.zone.as_str());
        println!(
            "  {:<18}{:>9.2}{:>9.2}{:>+9.2}{:>10.0}",
            a.zone,
            b.map(|z| z.rmse_k).unwrap_or(f64::NAN),
            a.rmse_k,
            b.map(|z| z.mean_bias_k).unwrap_or(f64::NAN),
            gains.get(&a.zone).copied().unwrap_or(0.0),
        );
    }
    let mean = |v: &[validate::ZoneBacktest]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().map(|r| r.rmse_k).sum::<f64>() / v.len() as f64
        }
    };
    println!(
        "  mean RMSE across {} zones: {:.2} K -> {:.2} K (with fitted internal gains)",
        after.len(),
        mean(&before),
        mean(&after),
    );
    // Fitted scheduled-load magnitudes (e.g. the water heat-pump): only the schedule + direction are
    // configured; the magnitude is learnt here.
    for (load, &w) in config.scheduled_loads.iter().zip(&fit.scheduled_w) {
        let label = if load.label.is_empty() {
            load.zone.as_str()
        } else {
            load.label.as_str()
        };
        println!(
            "  scheduled load '{label}' ({:?}, zone {}): fitted magnitude {w:.0} W",
            load.kind, load.zone,
        );
    }
    Ok(())
}

/// Start the read-only monitoring HTTP API.
async fn run_server(rcnet: RcNetwork, ss: StateSpace) -> anyhow::Result<()> {
    let config = optimize::config::ControlConfig::load("config.json5")?;
    let db = SourceClients::with_signals(
        InfluxDB::from_config("config.json5")?,
        config.data_sources.clone(),
    );
    // The deployed server takes its site coordinates from config.json5 (the demos use `site()`).
    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);
    let tick = std::time::Duration::from_secs(config.mpc_tick_minutes.max(1) * 60);
    web::serve(
        web::AppState::new(rcnet, ss, config, db, latitude, longitude),
        3000,
        tick,
    )
    .await
}

/// Project site (central Europe). Kept in sync with `config.json5`'s `site` block, which the
/// deployed server reads directly; the offline demos use this constant.
fn site() -> (Angle, Angle) {
    (
        Angle::new::<degree>(49.494934),
        Angle::new::<degree>(17.390341),
    )
}

fn parse_utc(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// Set a zone's boundary-temperature input, if that zone is a boundary node.
fn set_boundary(
    rcnet: &RcNetwork,
    ss: &StateSpace,
    u: &mut DVector<f64>,
    zone: &str,
    celsius: f64,
) {
    if let Some(&node) = rcnet.zone_indices.get(zone) {
        ss.set_boundary_temp(
            u,
            node,
            ThermodynamicTemperature::new::<degree_celsius>(celsius),
        );
    }
}

/// Read live zone temperatures and electricity prices (prints a message if the DB is unreachable).
async fn demo_database() {
    let Some(db) = demo_db("Database") else {
        return;
    };
    match db.read_zone("livingroom").await {
        Ok(zone) => println!("livingroom: {zone:?}"),
        Err(e) => println!("livingroom read failed: {e}"),
    }
    match db.read_prices("-1d").await {
        Ok(prices) => match price_range(&prices) {
            Some((min, max)) => {
                let latest = prices.last().unwrap();
                println!(
                    "electricity prices: {} samples over 24 h, {min:.1}..{max:.1} EUR/MWh \
                     (latest {:.1} at {})",
                    prices.len(),
                    latest.price_eur_mwh,
                    latest.time,
                );
            }
            None => println!("electricity prices: no samples in range"),
        },
        Err(e) => println!("electricity prices: {e}"),
    }
}

/// Free thermal response: hold the outside/ground boundaries fixed and let the house drift.
fn demo_free_response(rcnet: &RcNetwork, ss: &StateSpace) -> anyhow::Result<()> {
    println!(
        "\nState-space model: {} states, {} inputs ({} boundary temps + {} flux columns)",
        ss.n_states(),
        ss.n_inputs(),
        ss.n_boundary(),
        ss.n_states()
    );
    if ss.n_states() == 0 {
        return Ok(());
    }

    let boundary_zones: Vec<_> = ss
        .labels()
        .iter()
        .filter_map(|label| match label {
            state_space::InputLabel::BoundaryTemp { zone_name, .. } => zone_name.clone(),
            state_space::InputLabel::Flux { .. } => None,
        })
        .collect();
    println!("  boundary inputs: {boundary_zones:?}");

    let initial = DVector::from_element(ss.n_states(), tools::c_to_k(20.0));
    let mut u = ss.zero_input();
    set_boundary(rcnet, ss, &mut u, "outside", 5.0);
    set_boundary(rcnet, ss, &mut u, "ground", 10.0);

    let dt = 15.0 * 60.0; // 15-minute steps
    let steps = 24 * 4; // 24 hours
    let trajectory = ss.simulate(&initial, &vec![u; steps], dt)?;
    let last = trajectory.last().unwrap();

    let mut zone_names: Vec<_> = rcnet.zone_indices.keys().cloned().collect();
    zone_names.sort();
    println!(
        "Zone air temperatures after {:.0} h free response (outside 5 °C, ground 10 °C):",
        steps as f64 * dt / 3600.0
    );
    for name in zone_names {
        if let Some(s) = ss.state_index(rcnet.zone_indices[&name]) {
            println!(
                "  {name:<16} {:6.2} °C -> {:6.2} °C",
                tools::k_to_c(initial[s]),
                tools::k_to_c(last[s])
            );
        }
    }
    Ok(())
}

/// Solar gain by surface orientation, then a short simulation with that gain applied.
fn demo_solar_gain(rcnet: &RcNetwork, ss: &StateSpace) -> anyhow::Result<()> {
    if ss.n_states() == 0 {
        return Ok(());
    }
    let (lat, lon) = site();
    let noon = parse_utc("2023-06-21T11:00:00Z");
    let clear = Ratio::new::<ratio>(0.0);

    println!(
        "\nSolar irradiance on the {} oriented exterior surfaces (clear-sky summer noon):",
        rcnet.solar_surfaces.len()
    );
    let mut u = ss.zero_input();
    set_boundary(rcnet, ss, &mut u, "outside", 15.0);
    set_boundary(rcnet, ss, &mut u, "ground", 12.0);

    let mut total = Power::default();
    for surf in &rcnet.solar_surfaces {
        let irradiance =
            calculate_tilted_irradiance(lat, lon, &noon, clear, surf.tilt, surf.azimuth);
        let flux = irradiance * surf.area * surf.absorptance;
        total += flux;
        ss.set_flux(&mut u, surf.node, flux);
        println!(
            "  azimuth {:3.0}° tilt {:2.0}°  area {:5.1} m²  ->  {:4.0} W/m²  ({:5.0} W)",
            surf.azimuth.get::<degree>(),
            surf.tilt.get::<degree>(),
            surf.area.get::<square_meter>(),
            irradiance.get::<watt_per_square_meter>(),
            flux.get::<watt>(),
        );
    }
    println!("  total incident solar: {:.0} W", total.get::<watt>());

    let initial = DVector::from_element(ss.n_states(), tools::c_to_k(20.0));
    let trajectory = ss.simulate(&initial, &vec![u; 24], 15.0 * 60.0)?;
    if let Some(s) = rcnet
        .zone_indices
        .get("livingroom")
        .and_then(|&n| ss.state_index(n))
    {
        println!(
            "  livingroom over 6 h (outside 15 °C, ground 12 °C, with solar): {:.2} °C -> {:.2} °C",
            tools::k_to_c(initial[s]),
            tools::k_to_c(trajectory.last().unwrap()[s])
        );
    }
    Ok(())
}

/// Temperature-aware consumption forecast, built from a few illustrative samples (wiring real
/// history from InfluxDB is a follow-up).
fn demo_consumption() {
    let mut model = forecast::consumption::ConsumptionModel::new();
    for (temp, kwh) in [
        (-5.0, 3.0),
        (-5.0, 3.2),
        (-5.0, 3.4),
        (-5.0, 3.6),
        (15.0, 0.7),
        (15.0, 0.8),
        (15.0, 0.9),
        (15.0, 1.0),
    ] {
        model.add_sample(temp, 7, false, kwh);
    }
    model.build();
    println!(
        "\nConsumption forecast ({} samples): 07:00 weekday @ -5 °C -> {:.2} kWh, @ +15 °C -> {:.2} kWh",
        model.data_points(),
        model.predict(-5.0, 7, false),
        model.predict(15.0, 7, false),
    );
}

/// A 10 kWp south-facing array, shared by the PV and planning demos.
fn demo_pv_array() -> forecast::solar::PvArray {
    app::default_pv_array()
}

/// A small home battery, shared by the dispatch demos.
fn demo_battery_spec() -> optimize::battery::BatterySpec {
    app::default_battery_spec()
}

/// PV production forecast for a clear summer day (clear-sky physical baseline).
fn demo_pv() {
    let (lat, lon) = site();
    let pv = demo_pv_array();
    let series = pv.predict_series(
        lat,
        lon,
        &parse_utc("2023-06-21T00:00:00Z"),
        24,
        Ratio::new::<ratio>(0.0),
    );
    println!(
        "PV forecast: 10 kWp south array, clear-sky summer day -> peak {:.2} kW, {:.1} kWh",
        forecast::solar::peak_power(&series).get::<kilowatt>(),
        forecast::solar::hourly_energy(&series).get::<kilowatt_hour>(),
    );
}

/// Battery economic-dispatch demo: two cheap hours followed by two expensive ones.
fn demo_battery() {
    use optimize::battery::{optimize_dispatch, DispatchInputs};
    let spec = demo_battery_spec();
    let inputs = DispatchInputs {
        dt_hours: 1.0,
        import_price: vec![0.10, 0.10, 0.40, 0.40],
        export_price: vec![0.05; 4],
        pv_kw: vec![0.0; 4],
        load_kw: vec![1.0; 4],
        min_final_soc_kwh: None,
    };
    match optimize_dispatch(&spec, &inputs) {
        Ok(plan) => {
            let imported: f64 = plan.grid_import_kw.iter().sum::<f64>() * inputs.dt_hours;
            println!(
                "\nBattery dispatch (4 h, prices 0.10 -> 0.40 EUR/kWh): imported {imported:.1} kWh, cost {:.2} EUR",
                plan.total_cost,
            );
        }
        Err(e) => println!("battery dispatch failed: {e}"),
    }
}

/// End-to-end demo: drive the battery dispatch from the PV and consumption forecasts and a
/// day-ahead price curve over 24 hours.
fn demo_plan() {
    use optimize::coordinator::{plan_dispatch, ForecastContext};

    let (lat, lon) = site();
    let pv = demo_pv_array();

    // A rough daily load profile (kWh/h): higher mornings and evenings, lower midday/overnight.
    let mut consumption = forecast::consumption::ConsumptionModel::new();
    for h in 0..24u32 {
        let kwh = match h {
            6..=9 | 17..=21 => 1.5,
            10..=16 => 0.8,
            _ => 0.5,
        };
        for _ in 0..4 {
            consumption.add_sample(18.0, h, false, kwh);
        }
    }
    consumption.build();

    let battery = demo_battery_spec();

    // Cheap overnight, expensive evening peak; feed-in at 30% of the import price.
    let import_price: Vec<f64> = (0..24)
        .map(|h| match h {
            17..=20 => 0.45,
            1..=5 => 0.10,
            _ => 0.25,
        })
        .collect();
    let ctx = ForecastContext {
        latitude: lat,
        longitude: lon,
        start: parse_utc("2023-06-21T00:00:00Z"),
        step_seconds: 3600.0,
        local_offset: chrono::FixedOffset::east_opt(2 * 3600).unwrap(),
        temperature_c: vec![18.0; 24],
        ground_temperature_c: 12.0,
        cloud_cover: vec![0.2; 24],
        internal_gain_w: Default::default(), // battery-only demo: thermal side unused
        scheduled_loads: Vec::new(),
        scheduled_w: Vec::new(),
        export_price: import_price.iter().map(|p| p * 0.3).collect(),
        export_allowed: vec![true; 24],
        inverter_on: vec![true; 24],
        battery_amortisation: 0.0,
        terminal_value: 0.0,
        import_price,
        min_final_soc_kwh: Some(2.0),
        pv_kw_override: None,
        load_scale: 1.0,
    };

    match plan_dispatch(&pv, &consumption, &battery, &ctx) {
        Ok(plan) => {
            let charged: f64 = plan.charge_kw.iter().sum();
            let discharged: f64 = plan.discharge_kw.iter().sum();
            println!(
                "\nDay-ahead plan (24 h forecast PV + load + prices): cost {:.2} EUR, battery charged {charged:.1} kWh / discharged {discharged:.1} kWh",
                plan.total_cost,
            );
        }
        Err(e) => println!("dispatch planning failed: {e}"),
    }
}

/// Capstone demo: the unified optimizer schedules underfloor heating across the whole house as a
/// price-responsive flexible load alongside the battery, holding each heated zone's comfort band
/// while shifting heating toward the cheap overnight hours. Runs on the real model with the
/// `config.json5` heating settings and a synthetic cold-winter day.
fn demo_heating(rcnet: &RcNetwork, ss: &StateSpace) -> anyhow::Result<()> {
    use optimize::config::ControlConfig;
    use optimize::coordinator::{plan_unified, ForecastContext};

    if ss.n_states() == 0 {
        return Ok(());
    }

    // Site + heat-pump + per-zone comfort settings come from config.json5.
    let config = match ControlConfig::load("config.json5") {
        Ok(c) => c,
        Err(e) => {
            println!("\nUnified heating plan: control config unavailable: {e}");
            return Ok(());
        }
    };
    println!(
        "\nUnified heating plan — site {:.3}°N {:.3}°E (UTC{:+}), heat-pump COP {:.1}:",
        config.site.latitude,
        config.site.longitude,
        config.site.utc_offset_hours,
        config.heating.cop,
    );

    // A cold, overcast winter day: cheap overnight, expensive evening peak.
    const DT_HOURS: f64 = 1.0;
    let horizon = 24;
    let import_price: Vec<f64> = (0..horizon)
        .map(|h| match h {
            17..=20 => 0.45,
            1..=5 => 0.10,
            _ => 0.25,
        })
        .collect();
    let ctx = ForecastContext {
        latitude: Angle::new::<degree>(config.site.latitude),
        longitude: Angle::new::<degree>(config.site.longitude),
        start: parse_utc("2024-01-15T00:00:00Z"),
        step_seconds: 3600.0,
        local_offset: FixedOffset::east_opt(config.site.utc_offset_hours * 3600).unwrap(),
        temperature_c: vec![-2.0; horizon],
        ground_temperature_c: 8.0,
        cloud_cover: vec![0.8; horizon],
        internal_gain_w: config.heating.internal_gains(),
        scheduled_loads: config.scheduled_loads.clone(),
        scheduled_w: vec![0.0; config.scheduled_loads.len()],
        export_price: import_price.iter().map(|p| p * 0.3).collect(),
        export_allowed: vec![true; 24],
        inverter_on: vec![true; 24],
        battery_amortisation: 0.0,
        terminal_value: 0.0,
        import_price,
        min_final_soc_kwh: Some(2.0),
        pv_kw_override: None,
        load_scale: 1.0,
    };

    // Flat base load; underfloor heating is the flexible part the optimizer schedules.
    let mut consumption = forecast::consumption::ConsumptionModel::new();
    for h in 0..24u32 {
        consumption.add_sample(-2.0, h, false, 0.4);
    }
    consumption.build();

    // Slab + air seeded at 20 °C (a thermal-state estimator is a documented follow-up).
    let x0 = DVector::from_element(ss.n_states(), tools::c_to_k(20.0));

    match plan_unified(
        &demo_pv_array(),
        &consumption,
        &demo_battery_spec(),
        &config.heating,
        &config.hvac.clone().unwrap_or_default(),
        ss,
        rcnet,
        &ctx,
        &x0,
        &[],
        &[],
    ) {
        Ok(plan) if plan.heat_kw.is_empty() => println!("  no heated zones in the model."),
        Ok(plan) => {
            let heat_kwh: f64 = plan.heat_kw.values().flatten().sum::<f64>() * DT_HOURS;
            let elec_kwh = heat_kwh / config.heating.cop;
            let imported: f64 = plan.grid_import_kw.iter().sum::<f64>() * DT_HOURS;
            let exported: f64 = plan.grid_export_kw.iter().sum::<f64>() * DT_HOURS;
            let charged: f64 = plan.charge_kw.iter().sum::<f64>() * DT_HOURS;
            let discharged: f64 = plan.discharge_kw.iter().sum::<f64>() * DT_HOURS;
            println!(
                "  24 h horizon, {} heated zones: {heat_kwh:.1} kWh heat / {elec_kwh:.1} kWh electricity, total cost {:.2} EUR",
                plan.heat_kw.len(),
                plan.total_cost,
            );
            println!(
                "  grid imported {imported:.1} kWh / exported {exported:.1} kWh; battery charged {charged:.1} / discharged {discharged:.1} kWh, final SoC {:.1} kWh",
                plan.soc_kwh.last().copied().unwrap_or_default(),
            );
            if let (Some(temps), Some(heat)) = (
                plan.zone_temp_c.get("livingroom"),
                plan.heat_kw.get("livingroom"),
            ) {
                let lo = temps.iter().cloned().fold(f64::INFINITY, f64::min);
                let hi = temps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let night: f64 = heat[1..6].iter().sum::<f64>() * DT_HOURS;
                let evening: f64 = heat[17..21].iter().sum::<f64>() * DT_HOURS;
                println!(
                    "  livingroom: temp held in {lo:.1}..{hi:.1} °C; heating {night:.1} kWh overnight (cheap) vs {evening:.1} kWh evening (peak)",
                );
            }
        }
        Err(e) => println!("  unified heating plan failed: {e}"),
    }
    Ok(())
}
