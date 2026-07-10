//! `what-if <days>` — battery-economics backtester over **measured** history.
//!
//! Replays the last `<days>` local days with the real measured PV and total house load and the
//! real historical day-ahead prices, and solves the pure battery dispatch LP per day under a grid
//! of scenarios (amortisation sweep, optional flat distribution tariff, no battery), plus a
//! best-effort "as operated" row from the measured grid meters. The house load is deliberately
//! the **total** (`INVPowerToLocalLoad`, heating and EV included) — this tool isolates the
//! *battery* economics, so the flexible loads are taken as they actually ran.
//!
//! **Perfect foresight caveat**: each day is solved with the whole day's real PV/load/prices known
//! in advance, so every scenario is an upper bound on what a live rolling forecast can achieve.
//! Differences BETWEEN scenarios are the meaningful output, not the absolute costs.

use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::app::{battery_spec, tariff_prices, terminal_soc_value};
use crate::influxdb::TimeSample;
use crate::live_inputs::block_prices;
use crate::optimize::battery::DispatchInputs;
use crate::optimize::config::ControlConfig;
use crate::optimize::thermal::ThermalContext;
use crate::optimize::unified::{optimize_unified, FlowParams};
use crate::source::SourceClients;

const BLOCK_SECONDS: i64 = 900;
const BLOCKS_PER_DAY: usize = 96;
/// Minimum fraction of a day's blocks that must have a measured PV *and* load sample; sparser
/// days (telemetry outage) are skipped rather than scored against zero-filled gaps.
const MIN_COVERAGE: f64 = 0.9;

/// One scenario's accumulated results over the replayed days.
#[derive(Debug, Default, Clone)]
pub struct ScenarioTotals {
    pub label: String,
    pub cost_eur: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub discharge_kwh: f64,
    /// End-of-day SoC carried into the next day (the chained state).
    pub soc_kwh: f64,
}

/// Align windowed-mean samples onto the day's 15-min block grid (`None` where no sample landed).
fn align_15min(samples: &[TimeSample], start: DateTime<Utc>, n: usize) -> Vec<Option<f64>> {
    let mut blocks = vec![None; n];
    for s in samples {
        let b = (s.time - start).num_seconds().div_euclid(BLOCK_SECONDS);
        if (0..n as i64).contains(&b) {
            blocks[b as usize] = Some(s.value);
        }
    }
    blocks
}

/// Import/export prices (EUR/kWh) for a **flat** distribution tariff: `import = spot + flat_dist`,
/// export identical to the real tariff (`spot − sell_fee`, floored at 0, capped at import — the
/// LP's convexity precondition).
fn flat_tariff_prices(spot: &[f64], flat_dist_eur: f64, sell_fee_eur: f64) -> (Vec<f64>, Vec<f64>) {
    spot.iter()
        .map(|&s| {
            let import = s + flat_dist_eur;
            (import, (s - sell_fee_eur).max(0.0).min(import))
        })
        .unzip()
}

/// An inert thermal context (no heated/HVAC zones) — the what-if dispatch is battery-only, the
/// house load is measured as-run.
fn empty_thermal(n: usize) -> ThermalContext {
    ThermalContext {
        dt: BLOCK_SECONDS as f64,
        horizon: n,
        heated_zones: Vec::new(),
        hvac_zones: Vec::new(),
        free_response: HashMap::new(),
        kernels: HashMap::new(),
        air_kernels: HashMap::new(),
        load_kernels: HashMap::new(),
    }
}

/// Solve one day's battery dispatch for a scenario and fold it into the totals. `soc0` chains
/// from the previous day. Returns the end-of-day SoC.
#[allow(clippy::too_many_arguments)] // a flat argument list keeps the pure solver testable
fn run_day(
    config: &ControlConfig,
    label: &str,
    import: &[f64],
    export: &[f64],
    spot: &[f64],
    pv_kw: &[f64],
    load_kw: &[f64],
    amort_eur: f64,
    battery_off: bool,
    soc0: f64,
    totals: &mut ScenarioTotals,
) -> Result<()> {
    let n = import.len();
    let mut battery = battery_spec(&config.battery);
    if battery_off {
        battery.max_charge_kw = 0.0;
        battery.max_discharge_kw = 0.0;
    }
    battery.initial_soc_kwh = soc0.clamp(battery.min_soc_kwh, battery.max_soc_kwh);
    let export_floor = config.tariff.czk_to_eur(config.tariff.export_price_min_czk);
    let inverter_off = config
        .tariff
        .czk_to_eur(config.tariff.inverter_off_price_czk);
    let flow = FlowParams {
        export_allowed: spot.iter().map(|&s| s >= export_floor).collect(),
        inverter_on: spot.iter().map(|&s| s >= inverter_off).collect(),
        price_placeholder: Vec::new(),
        amortisation: amort_eur,
        terminal_value: terminal_soc_value(
            import,
            amort_eur,
            battery.charge_efficiency * battery.discharge_efficiency,
        ),
        terminal_heat_value: 0.0,
        max_import_kw: config.grid.max_import_kw,
        max_export_kw: config.grid.max_export_kw,
    };
    let inputs = DispatchInputs {
        dt_hours: BLOCK_SECONDS as f64 / 3600.0,
        import_price: import.to_vec(),
        export_price: export.to_vec(),
        pv_kw: pv_kw.to_vec(),
        load_kw: load_kw.to_vec(),
        min_final_soc_kwh: None,
    };
    let heating = crate::optimize::config::HeatingConfig {
        cop: 1.0,
        comfort_penalty: 1.0,
        zones: HashMap::new(),
        bias_correction: Default::default(),
    };
    let minutes: Vec<u32> = (0..n).map(|b| ((b * 15) % 1440) as u32).collect();
    let outdoor = vec![15.0; n];
    let plan = optimize_unified(
        &battery,
        &heating,
        &crate::optimize::config::HvacConfig::default(),
        &empty_thermal(n),
        &inputs,
        &flow,
        &outdoor,
        &[],
        &[],
        None,
        false,
        &minutes,
        None,
    )
    .with_context(|| format!("what-if solve failed for scenario {label}"))?;
    let dt = BLOCK_SECONDS as f64 / 3600.0;
    totals.label = label.to_string();
    totals.cost_eur += plan.total_cost;
    totals.import_kwh += plan.grid_import_kw.iter().sum::<f64>() * dt;
    totals.export_kwh += plan.grid_export_kw.iter().sum::<f64>() * dt;
    totals.discharge_kwh += plan.discharge_kw.iter().sum::<f64>() * dt;
    totals.soc_kwh = plan.soc_kwh.last().copied().unwrap_or(soc0);
    Ok(())
}

/// Parse the CLI args after `what-if`: `<days> [--amort 0.5,1.0,...] [--flat-dist <czk>]`.
fn parse_args(args: &[String]) -> Result<(i64, Vec<f64>, Option<f64>)> {
    let days: i64 = args
        .first()
        .context("usage: what-if <days> [--amort 0.5,1.0] [--flat-dist <czk>]")?
        .parse()
        .context("what-if: <days> must be an integer")?;
    ensure!((1..=60).contains(&days), "what-if: days must be 1..=60");
    let mut amorts = vec![0.5, 1.0, 1.5, 2.0];
    let mut flat_dist = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--amort" => {
                let list = args.get(i + 1).context("--amort needs a value list")?;
                amorts = list
                    .split(',')
                    .map(|v| v.trim().parse::<f64>())
                    .collect::<Result<_, _>>()
                    .context("--amort: comma-separated numbers (CZK/kWh)")?;
                i += 2;
            }
            "--flat-dist" => {
                flat_dist = Some(
                    args.get(i + 1)
                        .context("--flat-dist needs a value (CZK/kWh)")?
                        .parse::<f64>()
                        .context("--flat-dist: a number (CZK/kWh)")?,
                );
                i += 2;
            }
            other => anyhow::bail!("what-if: unknown argument {other}"),
        }
    }
    Ok((days, amorts, flat_dist))
}

/// Run the backtester: replay the last `<days>` full local days and print the scenario table.
pub async fn run(db: &SourceClients, config: &ControlConfig, args: &[String]) -> Result<()> {
    let (days, amorts, flat_dist) = parse_args(args)?;
    let dt = BLOCK_SECONDS as f64 / 3600.0;

    // Scenario set: the amortisation sweep on the real D57d tariff, the optional flat-distribution
    // sweep, a no-battery baseline, and the measured as-operated row.
    let mut scenarios: Vec<(String, f64, bool, bool)> = Vec::new(); // (label, amort_czk, flat, off)
    for &a in &amorts {
        scenarios.push((format!("D57d amort {a:.2}"), a, false, false));
        if flat_dist.is_some() {
            scenarios.push((format!("flat amort {a:.2}"), a, true, false));
        }
    }
    scenarios.push(("no battery".to_string(), 0.0, false, true));
    let mut totals: Vec<ScenarioTotals> = vec![ScenarioTotals::default(); scenarios.len()];
    let mut operated = ScenarioTotals {
        label: "as operated".to_string(),
        ..Default::default()
    };

    // Day 1 SoC seed: the first SOC sample of the window (falls back to the config floor).
    let now = Utc::now();
    let today_local = now.with_timezone(&config.site.offset_at(now)).date_naive();
    let capacity = config.battery.capacity_kwh;
    let mut socs: Vec<f64> = Vec::new(); // per scenario, chained across days
    let mut seeded = false;
    let mut scored_days = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for d in (1..=days).rev() {
        let date = today_local - chrono::Days::new(d as u64);
        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        // Interpret local midnight through the site's offset AT that instant (approximated from
        // its UTC reading — a DST-transition day is off by the changed hour, acceptable here).
        let approx = Utc.from_utc_datetime(&midnight);
        let offset = config.site.offset_at(approx);
        let start =
            Utc.from_utc_datetime(&(midnight - Duration::seconds(offset.local_minus_utc() as i64)));
        let stop = start + Duration::seconds(BLOCK_SECONDS * BLOCKS_PER_DAY as i64);
        let (start_s, stop_s) = (start.to_rfc3339(), stop.to_rfc3339());

        let pv = db
            .growatt_series("InputPower", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let load = db
            .growatt_series("INVPowerToLocalLoad", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let pv_b = align_15min(&pv, start, BLOCKS_PER_DAY);
        let load_b = align_15min(&load, start, BLOCKS_PER_DAY);
        let covered = pv_b
            .iter()
            .zip(&load_b)
            .filter(|(p, l)| p.is_some() && l.is_some())
            .count();
        let prices = block_prices(db, start, BLOCKS_PER_DAY).await?;
        let spot: Option<Vec<f64>> =
            prices.and_then(|p| p.into_iter().collect::<Option<Vec<f64>>>());
        let Some(spot) = spot else {
            skipped.push(format!("{date} (prices missing)"));
            continue;
        };
        if (covered as f64) < MIN_COVERAGE * BLOCKS_PER_DAY as f64 {
            skipped.push(format!(
                "{date} (telemetry {covered}/{BLOCKS_PER_DAY} blocks)"
            ));
            continue;
        }
        let pv_kw: Vec<f64> = pv_b
            .iter()
            .map(|v| (v.unwrap_or(0.0) / 1000.0).max(0.0))
            .collect();
        let load_kw: Vec<f64> = load_b
            .iter()
            .map(|v| (v.unwrap_or(0.0) / 1000.0).max(0.0))
            .collect();

        if !seeded {
            let soc0 = db
                .growatt_series("SOC", &start_s, &stop_s, "15m")
                .await
                .ok()
                .and_then(|s| s.first().map(|x| x.value / 100.0 * capacity))
                .unwrap_or_else(|| config.battery.min_soc_pct / 100.0 * capacity);
            socs = vec![soc0; scenarios.len()];
            seeded = true;
        }

        let (import_real, export_real) = tariff_prices(&config.tariff, &config.site, &spot, start);
        for (i, (label, amort_czk, flat, off)) in scenarios.iter().enumerate() {
            let (import, export) = if *flat {
                flat_tariff_prices(
                    &spot,
                    config.tariff.czk_to_eur(flat_dist.unwrap_or(0.0)),
                    config.tariff.sell_fee_eur(),
                )
            } else {
                (import_real.clone(), export_real.clone())
            };
            run_day(
                config,
                label,
                &import,
                &export,
                &spot,
                &pv_kw,
                &load_kw,
                config.tariff.czk_to_eur(*amort_czk),
                *off,
                socs[i],
                &mut totals[i],
            )?;
            socs[i] = totals[i].soc_kwh;
        }

        // As operated (best-effort): the measured grid meters priced at the real tariff.
        let imp = db
            .growatt_series("ACPowerToUser", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let exp = db
            .growatt_series("ACPowerToGrid", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let dis = db
            .growatt_series("DischargePower", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        for (b, v) in align_15min(&imp, start, BLOCKS_PER_DAY).iter().enumerate() {
            let kw = (v.unwrap_or(0.0) / 1000.0).max(0.0);
            operated.import_kwh += kw * dt;
            operated.cost_eur += kw * dt * import_real[b];
        }
        for (b, v) in align_15min(&exp, start, BLOCKS_PER_DAY).iter().enumerate() {
            let kw = (v.unwrap_or(0.0) / 1000.0).max(0.0);
            operated.export_kwh += kw * dt;
            operated.cost_eur -= kw * dt * export_real[b];
        }
        for v in align_15min(&dis, start, BLOCKS_PER_DAY).iter() {
            operated.discharge_kwh += (v.unwrap_or(0.0) / 1000.0).max(0.0) * dt;
        }
        scored_days += 1;
    }

    ensure!(scored_days > 0, "what-if: no scoreable day in the window");
    println!(
        "\nWhat-if battery economics — {scored_days} measured day(s), PERFECT FORESIGHT (each \
         day solved with its real PV/load/prices known; compare scenarios, not absolutes):"
    );
    if !skipped.is_empty() {
        println!("  skipped: {}", skipped.join(", "));
    }
    println!(
        "  {:<18}{:>10}{:>10}{:>10}{:>11}{:>10}{:>8}",
        "scenario", "cost CZK", "imp kWh", "exp kWh", "disch kWh", "wear CZK", "cycles"
    );
    let mut rows = totals;
    rows.push(operated);
    for t in &rows {
        let wear_czk = t.discharge_kwh
            * t.label
                .rsplit(' ')
                .next()
                .and_then(|a| a.parse::<f64>().ok())
                .unwrap_or(config.tariff.battery_amortisation_czk);
        println!(
            "  {:<18}{:>10.0}{:>10.1}{:>10.1}{:>11.1}{:>10.0}{:>8.2}",
            t.label,
            config.tariff.eur_to_czk(t.cost_eur),
            t.import_kwh,
            t.export_kwh,
            t.discharge_kwh,
            wear_czk,
            if capacity > 0.0 {
                t.discharge_kwh / capacity
            } else {
                0.0
            },
        );
    }
    println!(
        "  (cost = grid cash only; wear = discharge × amortisation; 'as operated' from the \
         measured meters at the D57d tariff)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ControlConfig {
        // A minimal config: real-tariff/battery/grid defaults, no zones/site specifics needed.
        json5::from_str(
            r#"{
                site: { latitude: 49.0, longitude: 14.5, utc_offset_hours: 1, timezone: "Europe/Prague" },
                heating: { cop: 1.0, comfort_penalty: 5.0, zones: {} },
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn align_15min_places_samples_and_leaves_gaps() {
        let start = DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let samples = vec![
            TimeSample {
                time: start + Duration::minutes(0),
                value: 1.0,
            },
            TimeSample {
                time: start + Duration::minutes(30),
                value: 3.0,
            },
            TimeSample {
                time: start - Duration::minutes(15), // before the window: dropped
                value: 9.0,
            },
        ];
        let b = align_15min(&samples, start, 4);
        assert_eq!(b, vec![Some(1.0), None, Some(3.0), None]);
    }

    #[test]
    fn flat_tariff_export_never_exceeds_import() {
        let spot = vec![0.10, -0.05, 0.30];
        let (import, export) = flat_tariff_prices(&spot, 0.05, 0.02);
        assert!((import[0] - 0.15).abs() < 1e-12);
        assert!((export[0] - 0.08).abs() < 1e-12);
        // Deeply negative spot: import goes to 0.0; export floors at 0 and stays ≤ import.
        assert!((import[1] - 0.0).abs() < 1e-12);
        assert_eq!(export[1], 0.0);
        for (i, e) in import.iter().zip(&export) {
            assert!(e <= i);
        }
    }

    #[test]
    fn higher_amortisation_discharges_less() {
        // A synthetic arbitrage day: cheap night, expensive evening, no PV — profitable to cycle
        // at zero wear, not at a wear that exceeds the price spread.
        let cfg = cfg();
        let n = 96;
        let spot: Vec<f64> = (0..n).map(|b| if b < 48 { 0.02 } else { 0.20 }).collect();
        let (import, export) = tariff_prices(&cfg.tariff, &cfg.site, &spot, Utc::now());
        let pv = vec![0.0; n];
        let load = vec![0.5; n];
        let mut cheap = ScenarioTotals::default();
        let mut dear = ScenarioTotals::default();
        run_day(
            &cfg, "cheap", &import, &export, &spot, &pv, &load, 0.0, false, 2.0, &mut cheap,
        )
        .unwrap();
        run_day(
            &cfg, "dear", &import, &export, &spot, &pv, &load, 10.0, false, 2.0, &mut dear,
        )
        .unwrap();
        assert!(
            cheap.discharge_kwh > dear.discharge_kwh + 1.0,
            "zero-wear cycles ({:.1} kWh) must exceed high-wear ({:.1} kWh)",
            cheap.discharge_kwh,
            dear.discharge_kwh
        );
    }

    #[test]
    fn parse_args_defaults_and_flags() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (days, amorts, flat) = parse_args(&a(&["7"])).unwrap();
        assert_eq!(days, 7);
        assert_eq!(amorts, vec![0.5, 1.0, 1.5, 2.0]);
        assert!(flat.is_none());
        let (_, amorts, flat) =
            parse_args(&a(&["3", "--amort", "0.8,1.2", "--flat-dist", "0.6"])).unwrap();
        assert_eq!(amorts, vec![0.8, 1.2]);
        assert_eq!(flat, Some(0.6));
        assert!(parse_args(&a(&["0"])).is_err());
        assert!(parse_args(&a(&["7", "--bogus"])).is_err());
    }
}
