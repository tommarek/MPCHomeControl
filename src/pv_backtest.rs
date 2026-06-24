//! Backtest PV-generation forecasts against the inverter's actual output.
//!
//! The house records its solar forecast (`solar_forecast_history`, an hourly kW curve per
//! `forecast_date`) alongside live Growatt telemetry. This compares that forecast against the
//! **actual** PV power (`InputPower`, the total PV-string DC input) over recent days. The stored
//! forecast blends sources — pure `solcast`, `model+solcast+api`, or `model+api` (no Solcast) —
//! so each day's `source` is reported rather than assumed to be Solcast.
//!
//! **Curtailment.** Solcast forecasts the array's *potential*; the inverter only harvests what it
//! can use. When grid export is disabled and the battery is full, the panels are curtailed and
//! actual drops below potential — not a forecast error. Those hours (`export_enabled == 0` and
//! `battery_soc` ≈ full) are detected and **excluded** from scoring.
//!
//! Caveat: `InputPower` is DC; Solcast is an AC estimate, so actual runs a few percent high from
//! inverter losses. Our own clear-sky model can be added to the comparison once the real array
//! specs (peak power, tilt, azimuth) are configured — a documented follow-up.

use std::collections::{HashMap, HashSet};

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, Timelike, Utc};

use serde::Serialize;

use crate::influxdb::{InfluxDB, TimeSample};
use crate::solar_forecast::forecast_curves;

const SOLAR_BUCKET: &str = "solar";
/// A daylight hour counts toward scoring only if the forecast (or actual) exceeds this, in kW.
const DAYLIGHT_KW: f64 = 0.05;
/// Battery state-of-charge (%) at/above which the battery is treated as full for curtailment.
const SOC_FULL: f64 = 99.0;

/// Per-day comparison of the Solcast forecast against actual generation (over clean hours).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvDayCompare {
    pub date: NaiveDate,
    /// Which forecast source produced this day's curve (`solcast`, `model+solcast+api`, …).
    pub source: String,
    /// Forecast energy (kWh) summed over the scored (clean daylight) hours.
    pub solcast_kwh: f64,
    /// Actual generation (kWh) over the same hours.
    pub actual_kwh: f64,
    pub clean_hours: usize,
    pub curtailed_hours: usize,
    /// RMS error (kW) over the scored hours.
    pub rmse_kw: f64,
    /// Mean signed error, actual − solcast (kW): positive = the house out-generated the forecast.
    pub bias_kw: f64,
}

/// Whole-backtest summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvBacktest {
    pub days: Vec<PvDayCompare>,
    pub overall_rmse_kw: f64,
    pub total_solcast_kwh: f64,
    pub total_actual_kwh: f64,
    pub scored_hours: usize,
    pub curtailed_hours: usize,
}

/// Accumulator for one day's scored hours.
#[derive(Default)]
struct DayScore {
    solcast_kwh: f64,
    actual_kwh: f64,
    clean_hours: usize,
    curtailed_hours: usize,
    sse: f64,
    bias_sum: f64,
}

/// Mean of `sum` over `n` samples (0 when `n == 0`).
fn mean(sum: f64, n: usize) -> f64 {
    if n > 0 {
        sum / n as f64
    } else {
        0.0
    }
}

/// Root-mean-square from a sum-of-squared-errors over `n` samples (0 when `n == 0`).
fn rmse(sse: f64, n: usize) -> f64 {
    mean(sse, n).sqrt()
}

/// Score one day: compare `solcast` vs `actual` over daylight hours, skipping `curtailed` ones.
fn score_day(
    solcast: &HashMap<u32, f64>,
    actual: &HashMap<u32, f64>,
    curtailed: &HashSet<u32>,
) -> DayScore {
    let mut s = DayScore::default();
    for hour in 0..24u32 {
        // A missing forecast hour is a genuine 0 kW prediction; a missing *actual* hour is
        // unscoreable (no ground truth) and is skipped.
        let forecast = solcast.get(&hour).copied().unwrap_or(0.0);
        let Some(&measured) = actual.get(&hour) else {
            continue;
        };
        // Daylight only: skip night hours where both are ~zero.
        if forecast < DAYLIGHT_KW && measured < DAYLIGHT_KW {
            continue;
        }
        if curtailed.contains(&hour) {
            s.curtailed_hours += 1;
            continue;
        }
        s.clean_hours += 1;
        s.solcast_kwh += forecast;
        s.actual_kwh += measured;
        s.sse += (measured - forecast).powi(2);
        s.bias_sum += measured - forecast;
    }
    s
}

/// Actual PV power (kW) per hour: `InputPower` (W) hourly-mean / 1000. An hour-mean kW summed over
/// 1-hour windows equals kWh, so the per-day sums are energy.
async fn read_pv_kw(db: &InfluxDB, start: &str) -> Result<Vec<TimeSample>> {
    let mut series = db
        .read_series(
            SOLAR_BUCKET,
            "solar",
            "InputPower",
            &[],
            start,
            "now()",
            "1h",
        )
        .await?;
    for s in &mut series {
        s.value /= 1000.0;
    }
    // Drop non-finite or negative readings (PV input power is physically >= 0; a sensor glitch
    // must not corrupt the stats or the calibration ratio).
    series.retain(|s| s.value.is_finite() && s.value >= 0.0);
    Ok(series)
}

/// An hourly-mean scalar field from the `solar` measurement. Propagates query failures (so a DB
/// outage on the curtailment fields isn't silently mistaken for "field has no data").
async fn hourly_field(db: &InfluxDB, field: &str, start: &str) -> Result<Vec<TimeSample>> {
    db.read_series(SOLAR_BUCKET, "solar", field, &[], start, "now()", "1h")
        .await
}

/// Backtest the stored PV forecast against actual generation over the last `days` days, excluding
/// curtailed hours. `utc_offset_hours` maps UTC to the local civil time the forecast curve is
/// keyed in; pass the offset for the window, which must not cross a DST boundary (a fixed offset,
/// consistent with `coordinator.rs`). The forecast's local hour-of-day keys align with the
/// stop-stamped hourly-mean actuals at zero shift — verified empirically (a ±1 h shift raises RMSE).
pub async fn backtest_pv(db: &InfluxDB, utc_offset_hours: i32, days: i64) -> Result<PvBacktest> {
    ensure!(days > 0, "backtest window must be positive");
    let offset = FixedOffset::east_opt(utc_offset_hours * 3600).context("invalid UTC offset")?;
    let start = format!("-{days}d");

    let pv = read_pv_kw(db, &start).await?;
    ensure!(
        !pv.is_empty(),
        "no actual PV (InputPower) data in the window"
    );
    let export = hourly_field(db, "export_enabled", &start).await?;
    let soc = hourly_field(db, "battery_soc", &start).await?;
    let forecasts = forecast_curves(db, "solar_forecast_history", &start).await?;
    ensure!(
        !forecasts.is_empty(),
        "no solar forecast history in the window"
    );

    // Index everything by (local date, local hour).
    let key = |t: DateTime<Utc>| {
        let local = t.with_timezone(&offset);
        (local.date_naive(), local.hour())
    };
    let actual: HashMap<(NaiveDate, u32), f64> =
        pv.iter().map(|s| (key(s.time), s.value)).collect();
    let export_on: HashMap<(NaiveDate, u32), f64> =
        export.iter().map(|s| (key(s.time), s.value)).collect();
    let soc_pct: HashMap<(NaiveDate, u32), f64> =
        soc.iter().map(|s| (key(s.time), s.value)).collect();

    let mut dates: Vec<NaiveDate> = forecasts.keys().copied().collect();
    dates.sort();

    let mut days_out = Vec::new();
    let (mut tot_sse, mut tot_n, mut tot_sol, mut tot_act, mut tot_curt) =
        (0.0, 0usize, 0.0, 0.0, 0);
    for date in dates {
        let (forecast, source) = &forecasts[&date];
        let mut act_h: HashMap<u32, f64> = HashMap::new();
        let mut curtailed: HashSet<u32> = HashSet::new();
        for hour in 0..24u32 {
            if let Some(&a) = actual.get(&(date, hour)) {
                act_h.insert(hour, a);
            }
            // Curtailed when export is disabled and the battery is full (nowhere for PV to go).
            // Missing export/soc data defaults to "not curtailed" (score the hour) rather than
            // dropping it — conservative and transparent.
            let exporting = export_on.get(&(date, hour)).copied().unwrap_or(1.0) >= 0.5;
            let battery_full = soc_pct.get(&(date, hour)).copied().unwrap_or(0.0) >= SOC_FULL;
            if !exporting && battery_full {
                curtailed.insert(hour);
            }
        }
        let score = score_day(forecast, &act_h, &curtailed);
        tot_curt += score.curtailed_hours;
        // Skip days with no scoreable hours (e.g. fully curtailed) rather than emit a 0-error row.
        if score.clean_hours == 0 {
            continue;
        }
        tot_sse += score.sse;
        tot_n += score.clean_hours;
        tot_sol += score.solcast_kwh;
        tot_act += score.actual_kwh;
        days_out.push(PvDayCompare {
            date,
            source: source.clone(),
            solcast_kwh: score.solcast_kwh,
            actual_kwh: score.actual_kwh,
            clean_hours: score.clean_hours,
            curtailed_hours: score.curtailed_hours,
            rmse_kw: rmse(score.sse, score.clean_hours),
            bias_kw: mean(score.bias_sum, score.clean_hours),
        });
    }

    Ok(PvBacktest {
        days: days_out,
        overall_rmse_kw: rmse(tot_sse, tot_n),
        total_solcast_kwh: tot_sol,
        total_actual_kwh: tot_act,
        scored_hours: tot_n,
        curtailed_hours: tot_curt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_day_excludes_curtailed_and_night() {
        let solcast = HashMap::from([(2, 0.0), (10, 1.0), (11, 2.0), (12, 3.0)]);
        let actual = HashMap::from([(2, 0.0), (10, 1.5), (11, 2.0), (12, 2.5)]);
        let curtailed = HashSet::from([12]); // hour 12 curtailed -> excluded
        let s = score_day(&solcast, &actual, &curtailed);
        assert_eq!(s.clean_hours, 2); // hours 10, 11 (hour 2 is night, hour 12 curtailed)
        assert_eq!(s.curtailed_hours, 1);
        assert!((s.solcast_kwh - 3.0).abs() < 1e-12); // 1 + 2
        assert!((s.actual_kwh - 3.5).abs() < 1e-12); // 1.5 + 2
        assert!((s.sse - 0.25).abs() < 1e-12); // 0.5^2 + 0^2
        assert!((s.bias_sum - 0.5).abs() < 1e-12); // +0.5 + 0
    }

    #[test]
    fn score_day_skips_hours_without_actual() {
        let solcast = HashMap::from([(10, 1.0), (11, 2.0)]);
        let actual = HashMap::from([(10, 1.0)]); // no actual for hour 11
        let s = score_day(&solcast, &actual, &HashSet::new());
        assert_eq!(s.clean_hours, 1);
    }
}
