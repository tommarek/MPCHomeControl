mod app;
mod estimate;
mod ev;
mod export_audit;
mod forecast;
mod forecast_validation;
mod heating_backtest;
mod influxdb;
mod kalman;
mod ledger;
mod live;
mod live_inputs;
mod model;
mod mpc_loop;
mod optimize;
mod pv_backtest;
mod rc_network;
mod solar_forecast;
mod solar_scale_backtest;
mod source;
mod state_space;
mod terminal_backtest;
mod tools;
mod topology;
mod validate;
mod web;
mod what_if;

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
    // `... backtest-heating --start <rfc3339, hour-aligned> --days <1..=7> [--warmup-h 48]
    // [--model <path>] [--config <path>] [--out <json>] [--dump <fixture>] [--from <fixture>]
    // [--legacy-duty] [--on-duty 0.7] [--min-on-h 2] [--off-h 4] [--off-duty 0.1]` — winter heating
    // kernel validation on a bounded window. Parsed BEFORE the default `Model::load`/`ControlConfig::
    // load` below so `--model`/`--config` can point at a candidate pair instead.
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "backtest-heating") {
            return heating_backtest::run(&args[i + 1..]).await;
        }
    }
    let model = Model::load("model.json5")?;
    let rcnet: RcNetwork = (&model).into();
    let ss: StateSpace = (&rcnet).into();
    // Plain (Rc-free) envelope snapshot for the read-only `/api/model/topology` endpoint, taken while
    // `model` is still alive (it is dropped once `rcnet`/`ss` are built).
    let topology = crate::topology::ModelTopology::from(&model);

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "serve") {
        return run_server(rcnet, ss, topology).await;
    }
    // `... what-if <days> [--amort 0.5,1.0] [--flat-dist <czk>]` — battery-economics backtest
    // over measured history (tariff & wear scenario table).
    if let Some(i) = args.iter().position(|a| a == "what-if") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        return what_if::run(&db, &config, &args[i + 1..]).await;
    }
    // `... audit-export [--log <file|->] [--plan <file.json>]...` — read-only proof tool for the
    // pv-gated-export item (fiction-vs-reality historical audit + live OLD/NEW comparison).
    if let Some(i) = args.iter().position(|a| a == "audit-export") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        let latitude = Angle::new::<degree>(config.site.latitude);
        let longitude = Angle::new::<degree>(config.site.longitude);
        return export_audit::run(
            &db,
            &config,
            &rcnet,
            &ss,
            latitude,
            longitude,
            &args[i + 1..],
        )
        .await;
    }
    // `... audit-dispatch-floor` — read-only proof tool for the demoted-discharge-floor item:
    // live OLD (floor=0) / NEW (as configured) comparison, both replayed under the real Growatt
    // dispatch floor.
    if args.iter().any(|a| a == "audit-dispatch-floor") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        let latitude = Angle::new::<degree>(config.site.latitude);
        let longitude = Angle::new::<degree>(config.site.longitude);
        return export_audit::run_dispatch_floor(&db, &config, &rcnet, &ss, latitude, longitude)
            .await;
    }
    // `... backtest-terminal <days> [--publish-hour H] [--live]` — rolling-horizon backtest: OLD
    // (in-horizon median) vs NEW (post-horizon outlook) terminal SoC valuation, on real history.
    if let Some(i) = args.iter().position(|a| a == "backtest-terminal") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        return terminal_backtest::run(&db, &config, &rcnet, &ss, &args[i + 1..]).await;
    }
    // `... backtest-dispatch-floor <days> [--publish-hour H]` — rolling-horizon backtest: OLD
    // (floor 0) vs NEW (`battery.min_dispatch_kw`, fix-and-round) under the actuator demotion rule.
    if let Some(i) = args.iter().position(|a| a == "backtest-dispatch-floor") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        return terminal_backtest::run_floor(&db, &config, &args[i + 1..]).await;
    }
    // `... backtest-kalman-solar <days> [--from <file>] [--dump <file>] [--sigma-dist <W>]
    // [--json <out>]` — real-data replay proof for the Kalman per-zone solar-gain scale: old (flag
    // off) vs new (flag on) scored per zone x lead bin, plus the night-bias-after-sunny-afternoon
    // metric. `--from` skips InfluxDB entirely, so the SourceClients connection is built lazily.
    if let Some(i) = args.iter().position(|a| a == "backtest-kalman-solar") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let rest = &args[i + 1..];
        let db = if rest.iter().any(|a| a == "--from") {
            None
        } else {
            Some(SourceClients::with_signals(
                InfluxDB::from_config("config.json5")?,
                config.data_sources.clone(),
            ))
        };
        return solar_scale_backtest::run(db.as_ref(), &config, &rcnet, &ss, rest).await;
    }
    // `... backtest-pv-nowcast <days> [--efold H,H,...] [--max-hours H,H,...] [--clamp-hi X,X,...]
    // [--min-forecast KW,KW,...]` — the pv-nowcast proof: one InfluxDB read, re-scored under a
    // parameter sweep (`clamp` lo fixed at 0.3) plus the config-default combination.
    if let Some(i) = args.iter().position(|a| a == "backtest-pv-nowcast") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        let db = SourceClients::with_signals(
            InfluxDB::from_config("config.json5")?,
            config.data_sources.clone(),
        );
        return run_backtest_pv_nowcast(&db, &config, &args[i + 1..]).await;
    }
    // `... ledger <import|score|show> ...` — the decision ledger's read-only CLI (writes only its
    // own `MPC_LEDGER_STORE`): backfill rows from a saved plan/log, run one scoring pass, or print
    // the aggregated report the `/api/ledger` endpoint serves. The DB connection is built lazily
    // (only `score` needs it) so `import`/`show` work without `INFLUX_TOKEN` set.
    if let Some(i) = args.iter().position(|a| a == "ledger") {
        let config = optimize::config::ControlConfig::load("config.json5")?;
        return ledger::run(
            || {
                Ok(SourceClients::with_signals(
                    InfluxDB::from_config("config.json5")?,
                    config.data_sources.clone(),
                ))
            },
            &config,
            &rcnet,
            &args[i + 1..],
        )
        .await;
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
    match app::current_plan(
        &db,
        rcnet,
        ss,
        &config,
        lat,
        lon,
        app::PlanExtras::default(),
    )
    .await
    {
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
    let Ok(config) = optimize::config::ControlConfig::load("config.json5") else {
        println!("PV backtest: config.json5 unavailable — skipping");
        return;
    };
    match pv_backtest::backtest_pv(&db, &config.site, 7, &config.pv.nowcast).await {
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

/// `--flag v1,v2,...` -> parsed `f64`s, or `None` when the flag is absent (the caller's own
/// default sweep applies then). A present flag with no values that parse is also `None` — the
/// caller's default stands rather than silently sweeping zero combinations.
fn parse_flag_list(args: &[String], flag: &str) -> Option<Vec<f64>> {
    let i = args.iter().position(|a| a == flag)?;
    let values: Vec<f64> = args
        .get(i + 1)?
        .split(',')
        .filter_map(|s| s.parse().ok())
        .collect();
    (!values.is_empty()).then_some(values)
}

/// One nowcast-replay parameter combination's console row: the params, per-k bin stats (n, plain
/// vs nowcast rmse/bias, Δrmse) and the gate/clamp counters.
fn print_nowcast_replay_row(r: &pv_backtest::PvNowcastReplay, label: &str) {
    println!(
        "{label}efold={:<4} max_h={:<4} clamp=[{:.2},{:.2}] min_fc={:<4} window={}m",
        r.params.efold_hours,
        r.params.max_hours,
        r.params.clamp[0],
        r.params.clamp[1],
        r.params.min_forecast_kw,
        r.params.window_minutes
    );
    for (all, applied) in r.all.iter().zip(&r.applied) {
        println!(
            "    [{:>3.0}-{:<3.0}h) all n={:<5} plain {:.3}/{:+.3} nowcast {:.3}/{:+.3} Δ{:+.3} | applied n={:<5} plain {:.3}/{:+.3} nowcast {:.3}/{:+.3} Δ{:+.3}",
            all.lead_from_h,
            all.lead_to_h,
            all.n,
            all.plain.rmse_kw,
            all.plain.bias_kw,
            all.nowcast.rmse_kw,
            all.nowcast.bias_kw,
            all.nowcast.rmse_kw - all.plain.rmse_kw,
            applied.n,
            applied.plain.rmse_kw,
            applied.plain.bias_kw,
            applied.nowcast.rmse_kw,
            applied.nowcast.bias_kw,
            applied.nowcast.rmse_kw - applied.plain.rmse_kw,
        );
    }
    println!(
        "    ref: n={} applied={} gated={} curtailed={} refreshed={} missing={}  clamp_lo={} clamp_hi={}  neutral_calibration_days={} mean_scale={:.3}",
        r.n_ref,
        r.n_ref_applied,
        r.n_ref_gated,
        r.n_ref_curtailed,
        r.n_ref_refreshed,
        r.n_ref_missing,
        r.n_clamp_lo,
        r.n_clamp_hi,
        r.n_neutral_calibration_days,
        r.mean_scale,
    );
}

/// `backtest-pv-nowcast <days> [--efold H,H,...] [--max-hours H,H,...] [--clamp-hi X,X,...]
/// [--min-forecast KW,KW,...]` — the pv-nowcast accuracy proof: ONE bounded InfluxDB read
/// ([`pv_backtest::fetch_pv_backtest_data`]), re-scored ([`pv_backtest::score_pv_backtest_data`],
/// pure) under the config-default nowcast params plus a parameter sweep (`clamp` lo fixed at 0.3
/// — see spec Decision 3). `days` clamps to 1..=21 (the 14-day lead window + 7 days of trailing
/// band-calibration history the replay's per-date calibration needs).
async fn run_backtest_pv_nowcast(
    db: &SourceClients,
    config: &optimize::config::ControlConfig,
    args: &[String],
) -> anyhow::Result<()> {
    const CLAMP_LO: f64 = 0.3;
    let days: i64 = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(21)
        .clamp(1, 21);
    let efold = parse_flag_list(args, "--efold").unwrap_or_else(|| vec![0.5, 1.0, 1.5, 2.0, 3.0]);
    let max_hours = parse_flag_list(args, "--max-hours").unwrap_or_else(|| vec![2.0, 3.0]);
    let clamp_hi = parse_flag_list(args, "--clamp-hi").unwrap_or_else(|| vec![1.3, 1.5, 2.0]);
    let min_forecast =
        parse_flag_list(args, "--min-forecast").unwrap_or_else(|| vec![0.3, 0.5, 1.0]);

    println!("backtest-pv-nowcast: reading {days}d of PV/forecast/curtailment history...");
    let data = pv_backtest::fetch_pv_backtest_data(db, &config.site, days).await?;

    let default_cfg = &config.pv.nowcast;
    let default_bt = pv_backtest::score_pv_backtest_data(&data, &config.site, default_cfg);
    println!("\n=== config-default (pv.nowcast in config.json5) ===");
    print_nowcast_replay_row(&default_bt.nowcast, "* ");

    println!("\n=== sweep (clamp lo fixed at {CLAMP_LO}) ===");
    for &e in &efold {
        for &m in &max_hours {
            for &c in &clamp_hi {
                for &mf in &min_forecast {
                    let cfg = optimize::config::NowcastConfig {
                        enabled: true,
                        window_minutes: default_cfg.window_minutes,
                        min_forecast_kw: mf,
                        efold_hours: e,
                        max_hours: m,
                        clamp: [CLAMP_LO, c],
                    };
                    if let Err(err) = cfg.validate() {
                        println!("skipped efold={e} max_h={m} clamp_hi={c} min_fc={mf}: {err}");
                        continue;
                    }
                    let bt = pv_backtest::score_pv_backtest_data(&data, &config.site, &cfg);
                    print_nowcast_replay_row(&bt.nowcast, "");
                }
            }
        }
    }

    println!(
        "\n=== lead bins (0-1/1-2/2-3/3-6/6-12/12-24/24-48 h, daylight-only, remnants excluded) ==="
    );
    println!(
        "  {:<10}{:>7}{:>9}{:>9}{:>12}",
        "lead", "n", "rmse_kw", "bias_kw", "mean_act_kw"
    );
    for bin in &default_bt.leads {
        let mean_actual = if bin.all.n > 0 {
            bin.all.actual_kwh / bin.all.n as f64
        } else {
            0.0
        };
        println!(
            "  [{:>4}-{:<4}h){:>6}{:>9.3}{:>+9.3}{:>12.3}",
            bin.lead_from_h,
            bin.lead_to_h,
            bin.all.n,
            bin.all.rmse_kw,
            bin.all.bias_kw,
            mean_actual
        );
    }
    Ok(())
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
    let Ok(config) = crate::optimize::config::ControlConfig::load("config.json5") else {
        println!("State estimate: config.json5 unavailable — skipping");
        return;
    };
    match estimate::estimate_initial_state(&db, rcnet, ss, lat, lon, 72, &config, None, None).await
    {
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
                        tools::k_to_c(x0.x0[s])
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
    match validate::backtest_passive(&db, rcnet, ss, lat, lon, &cfg, None).await {
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

/// Start the read-only monitoring HTTP API.
async fn run_server(
    rcnet: RcNetwork,
    ss: StateSpace,
    topology: crate::topology::ModelTopology,
) -> anyhow::Result<()> {
    let config = optimize::config::ControlConfig::load("config.json5")?;
    // Cross-check the config's zone references against the model before anything runs: a typo'd
    // zone name silently drops that room from heating/HVAC/gain control (the optimizer intersects,
    // it doesn't error), which on the live house means a room that never heats.
    app::validate_config_zones(&config, &rcnet)?;
    let db = SourceClients::with_signals(
        InfluxDB::from_config("config.json5")?,
        config.data_sources.clone(),
    );
    // The deployed server takes its site coordinates from config.json5 (the demos use `site()`).
    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);
    let tick = std::time::Duration::from_secs(config.mpc_tick_minutes.max(1) * 60);
    web::serve(
        web::AppState::new(rcnet, ss, topology, config, db, latitude, longitude),
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
        grid: optimize::grid::BlockGrid::uniform(parse_utc("2023-06-21T00:00:00Z"), 24, 3600.0),
        local_offset: chrono::FixedOffset::east_opt(2 * 3600).unwrap(),
        temperature_c: vec![18.0; 24],
        ground_temperature_c: 12.0,
        cloud_cover: vec![0.2; 24],
        solar: Vec::new(),
        internal_gain_w: Default::default(), // battery-only demo: thermal side unused
        solar_scale: Default::default(),
        scheduled_loads: Vec::new(),
        load_run_hours: Default::default(),
        scheduled_w: Vec::new(),
        export_price: import_price.iter().map(|p| p * 0.3).collect(),
        export_allowed: vec![true; 24],
        inverter_on: vec![true; 24],
        battery_amortisation: 0.0,
        export_needs_pv: false,
        min_dispatch_kw: 0.0,
        terminal_value: 0.0,
        terminal_heat_basis: 0.0,
        import_price,
        min_final_soc_kwh: Some(2.0),
        max_import_kw: None,
        max_export_kw: None,
        pv_kw_override: None,
        load_scale: 1.0,
        price_is_placeholder: Vec::new(),
        outlook: None,
        price_history: Vec::new(),
        public_holidays: Vec::new(),
        easter_holidays: false,
        distribution_eur_by_local_hour: [0.0; 24],
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
        grid: optimize::grid::BlockGrid::uniform(
            parse_utc("2024-01-15T00:00:00Z"),
            horizon,
            3600.0,
        ),
        local_offset: FixedOffset::east_opt(config.site.utc_offset_hours * 3600).unwrap(),
        temperature_c: vec![-2.0; horizon],
        ground_temperature_c: 8.0,
        cloud_cover: vec![0.8; horizon],
        solar: Vec::new(),
        internal_gain_w: config.heating.internal_gains(),
        solar_scale: Default::default(),
        scheduled_loads: config.scheduled_loads.clone(),
        load_run_hours: Default::default(),
        scheduled_w: vec![0.0; config.scheduled_loads.len()],
        export_price: import_price.iter().map(|p| p * 0.3).collect(),
        export_allowed: vec![true; 24],
        inverter_on: vec![true; 24],
        battery_amortisation: 0.0,
        export_needs_pv: false,
        min_dispatch_kw: 0.0,
        terminal_value: 0.0,
        terminal_heat_basis: 0.0,
        import_price,
        min_final_soc_kwh: Some(2.0),
        max_import_kw: None,
        max_export_kw: None,
        pv_kw_override: None,
        load_scale: 1.0,
        price_is_placeholder: Vec::new(),
        outlook: None,
        price_history: Vec::new(),
        public_holidays: Vec::new(),
        easter_holidays: false,
        distribution_eur_by_local_hour: [0.0; 24],
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
        optimize::coordinator::PlanOptions::default(),
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
