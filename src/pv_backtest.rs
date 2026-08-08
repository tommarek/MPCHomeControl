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
//! `SOC` ≈ full) are detected and **excluded** from scoring.
//!
//! Caveat: `InputPower` is DC; Solcast is an AC estimate, so actual runs a few percent high from
//! inverter losses. Our own clear-sky model can be added to the comparison once the real array
//! specs (peak power, tilt, azimuth) are configured — a documented follow-up.

use std::collections::{HashMap, HashSet};

use anyhow::{ensure, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Timelike, Utc};

use serde::Serialize;

use crate::influxdb::TimeSample;
use crate::solar_forecast::{fold_snapshots, forecast_snapshots, SnapshotCurve, SnapshotPick};
use crate::source::SourceClients;
use crate::tools::{mean, rmse};

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
    /// Clean-hour forecast/actual sums split by local-hour band (morning/midday/evening) —
    /// feeds the shape-aware calibration ([`crate::forecast::calibration::PvBandCalibration`]).
    pub band_solcast_kwh: [f64; 3],
    pub band_actual_kwh: [f64; 3],
    pub band_clean_hours: [usize; 3],
    pub scored_hours: usize,
    pub curtailed_hours: usize,
    /// Days excluded from scoring because the stored forecast was too incomplete to compare fairly
    /// (a forecast-snapshotting gap left only an end-of-day remnant, or nothing). Surfaced so a data
    /// gap reads as a gap rather than silently dragging the calibration — never as a forecast error.
    pub incomplete_forecast_days: Vec<NaiveDate>,
    /// Lead-time-resolved accuracy over every retained snapshot of the last [`LEAD_WINDOW_DAYS`]
    /// days (see [`PvLeadBin`]) — how forecast skill degrades with how far ahead it was made.
    pub leads: Vec<PvLeadBin>,
}

/// Accumulator for one day's scored hours.
#[derive(Default)]
struct DayScore {
    solcast_kwh: f64,
    actual_kwh: f64,
    /// Clean-hour sums split by the local-hour band (morning/midday/evening) — the raw material
    /// for the shape-aware PV calibration.
    band_solcast_kwh: [f64; 3],
    band_actual_kwh: [f64; 3],
    band_clean_hours: [usize; 3],
    clean_hours: usize,
    /// Scored hours where the forecast itself predicted daylight (≥ [`DAYLIGHT_KW`]). Far below
    /// `clean_hours` means the stored curve was a truncated remnant (a snapshotting gap), not a
    /// genuine zero forecast — used to drop the day from the calibration.
    forecast_hours: usize,
    curtailed_hours: usize,
    sse: f64,
    bias_sum: f64,
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
        if forecast >= DAYLIGHT_KW {
            s.forecast_hours += 1;
        }
        s.solcast_kwh += forecast;
        s.actual_kwh += measured;
        let band = crate::forecast::calibration::PvBandCalibration::band_of_hour(hour);
        s.band_solcast_kwh[band] += forecast;
        s.band_actual_kwh[band] += measured;
        s.band_clean_hours[band] += 1;
        s.sse += (measured - forecast).powi(2);
        s.bias_sum += measured - forecast;
    }
    s
}

/// PV lead-time bins (hours ahead the snapshot was recorded, half-open `[from, to)`).
pub const PV_LEAD_BINS_H: [(f64, f64); 4] = [(0.0, 6.0), (6.0, 12.0), (12.0, 24.0), (24.0, 48.0)];

/// Only snapshots for dates within this many days feed the lead bins — bounds the cost of
/// retaining every snapshot when the backtest window is long.
const LEAD_WINDOW_DAYS: i64 = 14;

/// Accuracy accumulated over one lead bin for one source class.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PvLeadScore {
    pub n: usize,
    pub rmse_kw: f64,
    pub bias_kw: f64,
    pub forecast_kwh: f64,
    pub actual_kwh: f64,
}

/// One PV lead-time bin: overall plus the solcast / non-solcast source split (directly useful
/// while two forecast writers coexist during the scraper cut-over). OBSERVABILITY ONLY — nothing
/// feeds back into the calibration yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvLeadBin {
    pub lead_from_h: f64,
    pub lead_to_h: f64,
    pub all: PvLeadScore,
    pub solcast: PvLeadScore,
    pub other: PvLeadScore,
}

/// Raw accumulator behind [`PvLeadScore`].
#[derive(Default, Clone, Copy)]
struct LeadAcc {
    sse: f64,
    bias_sum: f64,
    n: usize,
    forecast_kwh: f64,
    actual_kwh: f64,
}

impl LeadAcc {
    fn add(&mut self, forecast: f64, measured: f64) {
        self.sse += (measured - forecast).powi(2);
        self.bias_sum += measured - forecast;
        self.n += 1;
        self.forecast_kwh += forecast;
        self.actual_kwh += measured;
    }
    fn score(&self) -> PvLeadScore {
        PvLeadScore {
            n: self.n,
            rmse_kw: rmse(self.sse, self.n),
            bias_kw: mean(self.bias_sum, self.n),
            forecast_kwh: self.forecast_kwh,
            actual_kwh: self.actual_kwh,
        }
    }
}

/// Fold one date's snapshots into the lead accumulators: every snapshot's curve is scored against
/// the same actuals/curtailment sets the day scoring used, per hour, into the bin of its lead
/// (`hour-ending instant − snapshot time`; negative leads — remnant snapshots recorded after the
/// hour — contribute nothing). The same hour filter as [`score_day`]. Pure.
#[allow(clippy::too_many_arguments)] // the date context is a flat set of parallel lookups
fn score_leads(
    snaps: &[SnapshotCurve],
    act_h: &HashMap<u32, f64>,
    curtailed: &HashSet<u32>,
    hour_end_utc: impl Fn(u32) -> DateTime<Utc>,
    acc: &mut [(LeadAcc, LeadAcc, LeadAcc)],
) {
    for snap in snaps {
        let is_solcast = snap.source.contains("solcast");
        for hour in 0..24u32 {
            let forecast = snap.curve.get(&hour).copied().unwrap_or(0.0);
            let Some(&measured) = act_h.get(&hour) else {
                continue;
            };
            if forecast < DAYLIGHT_KW && measured < DAYLIGHT_KW {
                continue;
            }
            if curtailed.contains(&hour) {
                continue;
            }
            let lead_h = (hour_end_utc(hour) - snap.when).num_minutes() as f64 / 60.0;
            let Some(bin) = PV_LEAD_BINS_H
                .iter()
                .position(|&(from, to)| lead_h >= from && lead_h < to)
            else {
                continue; // negative lead (remnant) or beyond the last bin
            };
            let (all, solcast, other) = &mut acc[bin];
            all.add(forecast, measured);
            if is_solcast {
                solcast.add(forecast, measured);
            } else {
                other.add(forecast, measured);
            }
        }
    }
}

/// Actual PV power (kW) per hour, from the `InputPower` (W) hourly mean.
async fn read_pv_kw(db: &SourceClients, start: &str) -> Result<Vec<TimeSample>> {
    // Through the SIGNAL MAP, not a hardcoded bucket/measurement/field. `growatt_series` is what
    // `/api/live`, the dashboard overlay and `what_if` all use, and its whole purpose is that
    // "history and live views can never disagree on where a metric lives". Reading raw here meant a
    // house that remapped `data_sources.growatt.InputPower` got an empty series → this returns an
    // error → `/api/pv/backtest` 500s and PV calibration falls back to neutral FOREVER, planning on
    // an uncalibrated forecast. It also ignored the locator's `scale`. The default locator is
    // `solar`/`solar`/`InputPower` with scale 1.0, so this is behaviour-preserving for this house.
    let mut series = db
        .growatt_series("InputPower", start, "now()", "1h")
        .await?;
    for s in &mut series {
        s.value /= 1000.0;
    }
    // Drop non-finite or negative readings (PV input power is physically >= 0; a sensor glitch
    // must not corrupt the stats or the calibration ratio).
    series.retain(|s| s.value.is_finite() && s.value >= 0.0);
    Ok(series)
}

/// Backtest the stored PV forecast against actual generation over the last `days` days, excluding
/// curtailed hours. The forecast curve is keyed in the site's local civil time; the offset derives
/// per sample ([`SiteConfig::offset_at`]), so a window crossing a DST changeover keys both sides
/// correctly. The forecast's local hour-of-day keys align with the stop-stamped hourly-mean actuals
/// at zero shift — verified empirically (a ±1 h shift raises RMSE).
pub async fn backtest_pv(
    db: &SourceClients,
    site: &crate::optimize::config::SiteConfig,
    days: i64,
) -> Result<PvBacktest> {
    ensure!(days > 0, "backtest window must be positive");
    let start = format!("-{days}d");

    let pv = read_pv_kw(db, &start).await?;
    ensure!(
        !pv.is_empty(),
        "no actual PV (InputPower) data in the window"
    );
    // The curtailment flags resolve through the pluggable signal map (default: the `solar`-bucket
    // `export_enabled` / `SOC`); a different inverter remaps them without code.
    let export = db.curtailment_export_series(&start, "now()", "1h").await?;
    let soc = db.curtailment_soc_series(&start, "now()", "1h").await?;
    // Score against the FULLEST snapshot per day, not the latest (which for a finished day is only the
    // end-of-day remnant — see `SnapshotPick`). Look back a couple of days further than the actuals
    // window: a day's full-day forecast is often snapshotted the evening before it begins.
    let fc_start = format!("-{}d", days + 2);
    let snapshots = forecast_snapshots(db, "solar_forecast_history", &fc_start).await?;
    // Lead scoring keeps EVERY snapshot, but only for recent dates (a long backtest window would
    // otherwise multiply hours × snapshots); the per-day pick below still uses the full window.
    let lead_cutoff = Utc::now().date_naive() - chrono::Days::new(LEAD_WINDOW_DAYS as u64);
    let lead_snapshots: HashMap<NaiveDate, Vec<SnapshotCurve>> = snapshots
        .iter()
        .filter(|(d, _)| **d >= lead_cutoff)
        .map(|(d, v)| {
            (
                *d,
                v.iter()
                    .map(|s| SnapshotCurve {
                        when: s.when,
                        source: s.source.clone(),
                        curve: s.curve.clone(),
                        p10: s.p10.clone(),
                    })
                    .collect(),
            )
        })
        .collect();
    let forecasts = fold_snapshots(snapshots, SnapshotPick::Fullest);
    ensure!(
        !forecasts.is_empty(),
        "no solar forecast history in the window"
    );

    // Index everything by (local date, local hour) — the offset derives per sample, so a window
    // crossing a DST changeover keys each side correctly.
    let key = |t: DateTime<Utc>| {
        let local = t.with_timezone(&site.offset_at(t));
        (local.date_naive(), local.hour())
    };
    // Keep-FIRST on an hour collision, the convention every reader here follows (see
    // `estimate::keep_first_by_hour`): these series are stop-stamped over `stop: now()`, so Flux
    // clamps the final window and emits a trailing PARTIAL sample sharing the completed hour's key.
    // Keeping the LAST let a fraction-of-an-hour mean stand in for the full hour — scored against
    // the previous full hour's forecast, and able to mis-mark that hour (and, via the `h-1` rule,
    // the one before) as curtailed. This feeds `PvBandCalibration`, i.e. the multiplier applied to
    // the planning PV forecast every cycle.
    let keep_first = |samples: &[TimeSample]| -> HashMap<(NaiveDate, u32), f64> {
        let mut m: HashMap<(NaiveDate, u32), f64> = HashMap::new();
        for s in samples {
            m.entry(key(s.time)).or_insert(s.value);
        }
        m
    };
    let actual = keep_first(&pv);
    let export_on = keep_first(&export);
    let soc_pct = keep_first(&soc);

    let mut dates: Vec<NaiveDate> = forecasts.keys().copied().collect();
    dates.sort();

    let mut days_out = Vec::new();
    let mut incomplete: Vec<NaiveDate> = Vec::new();
    let (mut tot_sse, mut tot_n, mut tot_sol, mut tot_act, mut tot_curt) =
        (0.0, 0usize, 0.0, 0.0, 0);
    let (mut band_sol, mut band_act, mut band_hours) = ([0.0_f64; 3], [0.0_f64; 3], [0_usize; 3]);
    let mut lead_acc =
        vec![(LeadAcc::default(), LeadAcc::default(), LeadAcc::default()); PV_LEAD_BINS_H.len()];
    for date in dates {
        let day = &forecasts[&date];
        let (forecast, source) = (&day.curve, &day.source);
        let mut act_h: HashMap<u32, f64> = HashMap::new();
        let mut curtailed: HashSet<u32> = HashSet::new();
        for hour in 0..24u32 {
            if let Some(&a) = actual.get(&(date, hour)) {
                act_h.insert(hour, a);
            }
            // Curtailed when export is disabled and the battery is full (nowhere for PV to go).
            // The export flag is min-aggregated and SoC max-aggregated (see the series wrappers),
            // so an hour where the inverter throttled for only part of the hour still flags.
            // Missing export/soc data defaults to "not curtailed" (score the hour) rather than
            // dropping it — conservative and transparent.
            let exporting = export_on.get(&(date, hour)).copied().unwrap_or(1.0) >= 0.5;
            let battery_full = soc_pct.get(&(date, hour)).copied().unwrap_or(0.0) >= SOC_FULL;
            if !exporting && battery_full {
                curtailed.insert(hour);
            }
        }
        // Also exclude the hour immediately preceding each curtailed hour: the battery typically
        // fills partway through it, so its actuals are already throttled while the hourly flags
        // still read clean — scoring it would bias the calibration low. score_day's
        // ratio-of-totals design tolerates dropping the extra hours cheaply.
        let with_preceding: HashSet<u32> = curtailed
            .iter()
            .flat_map(|&h| if h > 0 { vec![h, h - 1] } else { vec![h] })
            .collect();
        let curtailed = with_preceding;
        // Lead-time scoring: every retained snapshot for this date, against the same actuals and
        // curtailment exclusions. The hour-ending UTC instant reconstructs from the local key
        // (hour 0 = the hour ending at this date's local midnight).
        if let Some(snaps) = lead_snapshots.get(&date) {
            let hour_end = |hour: u32| {
                let naive = date.and_hms_opt(hour, 0, 0).unwrap();
                let approx = chrono::Utc.from_utc_datetime(&naive);
                let off = site.offset_at(approx);
                chrono::Utc.from_utc_datetime(
                    &(naive - chrono::Duration::seconds(off.local_minus_utc() as i64)),
                )
            };
            score_leads(snaps, &act_h, &curtailed, hour_end, &mut lead_acc);
        }
        let score = score_day(forecast, &act_h, &curtailed);
        tot_curt += score.curtailed_hours;
        // Skip days with no scoreable hours (e.g. fully curtailed) rather than emit a 0-error row.
        if score.clean_hours == 0 {
            continue;
        }
        // Skip days whose stored forecast covers under half the generating daylight hours: a
        // snapshotting gap left only an end-of-day remnant (or nothing), so the forecast reads
        // near-zero. Scoring it would crater the Solcast total and spuriously inflate the PV
        // calibration — exclude it and surface it as a data gap instead of a forecast error.
        if score.forecast_hours * 2 < score.clean_hours {
            incomplete.push(date);
            continue;
        }
        tot_sse += score.sse;
        tot_n += score.clean_hours;
        tot_sol += score.solcast_kwh;
        tot_act += score.actual_kwh;
        for b in 0..3 {
            band_sol[b] += score.band_solcast_kwh[b];
            band_act[b] += score.band_actual_kwh[b];
            band_hours[b] += score.band_clean_hours[b];
        }
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
        band_solcast_kwh: band_sol,
        band_actual_kwh: band_act,
        band_clean_hours: band_hours,
        scored_hours: tot_n,
        curtailed_hours: tot_curt,
        incomplete_forecast_days: incomplete,
        leads: PV_LEAD_BINS_H
            .iter()
            .zip(&lead_acc)
            .map(|(&(from, to), (all, solcast, other))| PvLeadBin {
                lead_from_h: from,
                lead_to_h: to,
                all: all.score(),
                solcast: solcast.score(),
                other: other.score(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_leads_bins_by_snapshot_age_and_skips_remnants() {
        let noon_utc = |h: u32| {
            DateTime::parse_from_rfc3339(&format!("2026-07-01T{h:02}:00:00Z"))
                .unwrap()
                .with_timezone(&Utc)
        };
        // Two snapshots for the day: one recorded 20 h before local noon (lead ~12-24 bin for
        // midday hours), one recorded AFTER noon (a remnant — negative lead for midday).
        let curve: HashMap<u32, f64> = [(11, 3.0), (12, 4.0)].into_iter().collect();
        let snaps = vec![
            SnapshotCurve {
                when: noon_utc(12) - chrono::Duration::hours(20),
                source: "solcast".to_string(),
                curve: curve.clone(),
                p10: None,
            },
            SnapshotCurve {
                when: noon_utc(12) + chrono::Duration::hours(2),
                source: "model+api".to_string(),
                curve,
                p10: None,
            },
        ];
        let act_h: HashMap<u32, f64> = [(11, 3.5), (12, 4.5)].into_iter().collect();
        let mut acc = vec![
            (LeadAcc::default(), LeadAcc::default(), LeadAcc::default());
            PV_LEAD_BINS_H.len()
        ];
        score_leads(&snaps, &act_h, &HashSet::new(), noon_utc, &mut acc);
        // The early snapshot's two hours land in the 12-24 h bin (leads 19 h and 20 h)...
        assert_eq!(acc[2].0.n, 2);
        // ...credited to the solcast split; the remnant contributes nothing anywhere.
        assert_eq!(acc[2].1.n, 2);
        assert_eq!(acc[2].2.n, 0);
        assert_eq!(acc[0].0.n + acc[1].0.n + acc[3].0.n, 0);
        let s = acc[2].0.score();
        assert!((s.bias_kw - 0.5).abs() < 1e-9);
        assert!((s.forecast_kwh - 7.0).abs() < 1e-9);
    }

    #[test]
    fn score_leads_respects_curtailment_and_daylight() {
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64);
        let curve: HashMap<u32, f64> = [(10, 2.0), (11, 3.0), (2, 0.01)].into_iter().collect();
        let snaps = vec![SnapshotCurve {
            when: t0 - chrono::Duration::hours(1),
            source: "solcast".to_string(),
            curve,
            p10: None,
        }];
        let act_h: HashMap<u32, f64> = [(10, 2.0), (11, 3.0), (2, 0.02)].into_iter().collect();
        let curtailed: HashSet<u32> = [11].into_iter().collect();
        let mut acc = vec![
            (LeadAcc::default(), LeadAcc::default(), LeadAcc::default());
            PV_LEAD_BINS_H.len()
        ];
        score_leads(&snaps, &act_h, &curtailed, hour_end, &mut acc);
        // Hour 11 curtailed, hour 2 below daylight — only hour 10 scores (lead 11 h → bin 1).
        let total: usize = acc.iter().map(|(a, _, _)| a.n).sum();
        assert_eq!(total, 1);
        assert_eq!(acc[1].0.n, 1);
    }

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
    fn score_day_counts_forecast_coverage() {
        // The data-gap pattern: an end-of-day remnant forecast (only hour 19) vs a full afternoon of
        // actual generation. `forecast_hours` must register the gap so the backtest can drop the day.
        let solcast = HashMap::from([(19, 1.5)]);
        let actual = HashMap::from([(10, 4.0), (11, 5.0), (12, 6.0), (19, 1.0)]);
        let s = score_day(&solcast, &actual, &HashSet::new());
        assert_eq!(s.clean_hours, 4); // four daylight hours have actual generation
        assert_eq!(s.forecast_hours, 1); // the forecast predicted daylight in only one of them
        assert!(s.forecast_hours * 2 < s.clean_hours); // the completeness guard would skip this day
    }

    #[test]
    fn score_day_skips_hours_without_actual() {
        let solcast = HashMap::from([(10, 1.0), (11, 2.0)]);
        let actual = HashMap::from([(10, 1.0)]); // no actual for hour 11
        let s = score_day(&solcast, &actual, &HashSet::new());
        assert_eq!(s.clean_hours, 1);
    }
}
