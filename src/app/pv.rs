//! The PV inputs of a plan: the configured arrays, the clear-sky fallback, the intraday nowcast
//! (trailing measured vs forecast PV) and the Solcast p10 curtailment-risk read-off.

use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, TimeZone, Timelike, Utc};
use serde::Serialize;
use uom::si::{
    angle::degree,
    f64::{Angle, Power, Ratio},
    power::kilowatt,
    ratio::ratio,
};

use super::{BLOCK_SECONDS, HORIZON_BLOCKS};
use crate::forecast::calibration::PvBandCalibration;
use crate::forecast::solar::PvArray;
use crate::influxdb::TimeSample;
use crate::optimize::config::{PvConfig, SiteConfig};
use crate::source::SourceClients;

/// Mask of the horizon blocks belonging to the NEXT solar day — the daylight that will refill the
/// battery after tonight's candidate pre-charge. Before local noon that is *today* (an 02:00 plan
/// is squeezed by the sun rising a few hours later); from noon on it is tomorrow. Masking
/// "start's date + 1" unconditionally excluded today's daylight during exactly the overnight
/// cheap hours the pre-charge guard exists for, collapsing the p10 surplus toward zero there.
pub(super) fn next_solar_day_mask(
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
pub(super) fn p10_curtailment(
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
pub(super) fn clearsky_pv_kw(
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

/// The PV intraday nowcast's outcome this cycle — always present in [`super::PlanReport`], `applied:
/// false` with a `reason` when it was skipped (see [`apply_pv_nowcast`]).
#[derive(Debug, Clone, Serialize)]
pub struct PvNowcast {
    pub applied: bool,
    pub reason: Option<String>,
    pub ratio: Option<f64>,
    pub window_minutes: u32,
    /// `max_hours` when applied, `0.0` when skipped.
    pub applied_hours: f64,
    /// Count of fine blocks the blend touched (nonzero weight).
    pub blocks: usize,
    /// `now − newest measured sample` (seconds); `None` when no measured sample was read.
    pub source_age_s: Option<i64>,
    pub measured_kwh: Option<f64>,
    pub forecast_kwh: Option<f64>,
    /// Σ(nowcast − calibrated)·block_h over the touched blocks (kWh); `0.0` when skipped.
    pub delta_kwh: f64,
}

impl PvNowcast {
    pub(super) fn skip(window_minutes: u32, reason: impl Into<String>) -> Self {
        Self {
            applied: false,
            reason: Some(reason.into()),
            ratio: None,
            window_minutes,
            applied_hours: 0.0,
            blocks: 0,
            source_age_s: None,
            measured_kwh: None,
            forecast_kwh: None,
            delta_kwh: 0.0,
        }
    }
}

/// Floor `t` to the preceding 5-minute mark — so the nowcast read's Flux `stop` is never a partial
/// window.
fn floor_to_5min(t: DateTime<Utc>) -> DateTime<Utc> {
    let secs = t.timestamp();
    Utc.timestamp_opt(secs - secs.rem_euclid(300), 0)
        .single()
        .expect("a 5-minute mark is always a valid timestamp")
}

/// Why the trailing measured-PV window can't feed a ratio — the pure data-health gate of
/// [`apply_pv_nowcast`]. `Stale` and `NoSamples` are feed faults (reported as placeholders); a
/// `Partial` window is a transient.
#[derive(Debug, PartialEq)]
enum WindowSkip {
    NoSamples,
    Stale {
        age_s: i64,
    },
    Partial {
        n: usize,
        expected: usize,
        age_s: i64,
    },
}

/// Ignore a trailing-window feed whose newest sample is older than this (the Growatt feed writes
/// every few seconds; anything older is a stalled feed, not the current hour's weather).
const NOWCAST_STALE_MIN: i64 = 10;

/// Check the trailing measured-PV samples: non-empty, newest sample within [`NOWCAST_STALE_MIN`],
/// and at least 75 % of the 5-min windows present. Returns the newest sample's age (s).
fn check_window(
    samples: &[TimeSample],
    now: DateTime<Utc>,
    window_minutes: u32,
) -> Result<i64, WindowSkip> {
    let newest = samples
        .iter()
        .map(|s| s.time)
        .max()
        .ok_or(WindowSkip::NoSamples)?;
    let age_s = (now - newest).num_seconds();
    if age_s > NOWCAST_STALE_MIN * 60 {
        return Err(WindowSkip::Stale { age_s });
    }
    let expected = (window_minutes / 5).max(1) as usize;
    if samples.len() * 4 < expected * 3 {
        return Err(WindowSkip::Partial {
            n: samples.len(),
            expected,
            age_s,
        });
    }
    Ok(age_s)
}

/// Forecast-covered minutes a window needs before its ratio is trusted: half the window, but never
/// under 10 min (two 5-min samples).
fn min_covered_minutes(window_minutes: u32) -> u32 {
    (window_minutes / 2).max(10)
}

/// The intraday PV nowcast (spec `pv-nowcast`): read the last `window_minutes` of measured PV,
/// compare it against the SAME calibrated Solcast curve the forward blocks use, and blend the
/// resulting ratio into the next `max_hours` of `pv_kw` in place. Thin IO — all the math lives in
/// [`crate::forecast::nowcast`]. Returns the outcome for [`super::PlanReport::pv_nowcast`]; pushes a
/// `placeholders` entry only for the fault-like skip reasons (data read failure/timeout/staleness)
/// — a gated/curtailed/night/partial-coverage skip is normal operation, not a degraded plan.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_pv_nowcast(
    db: &SourceClients,
    cfg: &crate::optimize::config::NowcastConfig,
    site: &SiteConfig,
    forecast: &crate::solar_forecast::PvForecast,
    calibration: &PvBandCalibration,
    pv_kw: &mut [f64],
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    dc_peak_kw: Option<f64>,
    placeholders: &mut Vec<String>,
) -> PvNowcast {
    const NOWCAST_QUERY_TIMEOUT: StdDuration = StdDuration::from_secs(5);

    if !cfg.enabled {
        return PvNowcast::skip(cfg.window_minutes, "disabled");
    }

    let window_end = floor_to_5min(now);
    let window_start = window_end - Duration::minutes(cfg.window_minutes as i64);
    let (ws, we) = (window_start.to_rfc3339(), window_end.to_rfc3339());
    let read = tokio::time::timeout(NOWCAST_QUERY_TIMEOUT, async {
        tokio::join!(
            db.growatt_series("InputPower", &ws, &we, "5m"),
            db.curtailment_export_series(&ws, &we, "5m"),
            db.curtailment_soc_series(&ws, &we, "5m"),
        )
    })
    .await;
    let Ok((pv_read, export_read, soc_read)) = read else {
        placeholders.push("PV nowcast (measured PV unavailable: timed out)".to_string());
        return PvNowcast::skip(cfg.window_minutes, "measured PV unavailable: timed out");
    };
    let samples: Vec<TimeSample> = match pv_read {
        Ok(mut s) => {
            for x in &mut s {
                x.value /= 1000.0; // W -> kW
            }
            s.retain(|x| x.value.is_finite() && x.value >= 0.0);
            s
        }
        Err(e) => {
            placeholders.push(format!("PV nowcast (measured PV unavailable: {e})"));
            return PvNowcast::skip(cfg.window_minutes, format!("measured PV unavailable: {e}"));
        }
    };
    let source_age_s = match check_window(&samples, now, cfg.window_minutes) {
        Ok(age) => age,
        Err(WindowSkip::NoSamples) => {
            placeholders.push("PV nowcast (measured PV unavailable: no samples)".to_string());
            return PvNowcast::skip(cfg.window_minutes, "measured PV unavailable: no samples");
        }
        Err(WindowSkip::Stale { age_s }) => {
            placeholders.push(format!("PV nowcast (stale: newest sample {age_s} s old)"));
            return PvNowcast {
                source_age_s: Some(age_s),
                ..PvNowcast::skip(cfg.window_minutes, format!("stale (age {age_s} s)"))
            };
        }
        Err(WindowSkip::Partial { n, expected, age_s }) => {
            return PvNowcast {
                source_age_s: Some(age_s),
                ..PvNowcast::skip(
                    cfg.window_minutes,
                    format!("partial coverage ({n}/{expected} samples)"),
                )
            };
        }
    };
    // Curtailment: min(export_enabled) < 0.5 AND max(SOC) >= 99 over the window — same rule
    // `pv_backtest` uses. Missing export/SOC data defaults to "not curtailed" (score/blend the
    // window) rather than silently dropping it.
    let exporting = export_read
        .ok()
        .map(|s| s.iter().map(|x| x.value).fold(f64::INFINITY, f64::min))
        .filter(|v| v.is_finite())
        .unwrap_or(1.0)
        >= 0.5;
    let battery_full = soc_read
        .ok()
        .map(|s| s.iter().map(|x| x.value).fold(f64::NEG_INFINITY, f64::max))
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
        >= 99.0;
    if !exporting && battery_full {
        return PvNowcast {
            source_age_s: Some(source_age_s),
            ..PvNowcast::skip(cfg.window_minutes, "curtailed window")
        };
    }

    // Forecast each measured sample at its calendar-hour-ending key, keyed off the SAME chosen
    // snapshot the forward curve reads — a sample whose key that snapshot lacks (the hour just
    // ended, right after a refresh) is "not covered" and excluded from the ratio.
    let today = now.with_timezone(&site.offset_at(now)).date_naive();
    if !forecast.curves.contains_key(&today) {
        return PvNowcast {
            source_age_s: Some(source_age_s),
            ..PvNowcast::skip(cfg.window_minutes, "no forecast curve for today")
        };
    }
    let mut pairs: Vec<(f64, f64)> = Vec::with_capacity(samples.len());
    let mut covered_minutes = 0u32;
    for s in &samples {
        let midpoint = s.time - Duration::seconds(150);
        // Floor in SITE-LOCAL time, not UTC: correct for any offset, not just whole-hour ones.
        let local_mid = midpoint.with_timezone(&site.offset_at(midpoint));
        let hour_end = (local_mid
            - Duration::minutes(local_mid.minute() as i64)
            - Duration::seconds(local_mid.second() as i64)
            + Duration::hours(1))
        .with_timezone(&Utc);
        let Some(raw_fc) = forecast.kw_at(hour_end, site) else {
            continue;
        };
        covered_minutes += 5;
        let local_hour = hour_end.with_timezone(&site.offset_at(hour_end)).hour();
        pairs.push((s.value, calibration.apply_at(raw_fc, local_hour)));
    }
    if covered_minutes < min_covered_minutes(cfg.window_minutes) {
        return PvNowcast {
            source_age_s: Some(source_age_s),
            ..PvNowcast::skip(cfg.window_minutes, "forecast refreshed")
        };
    }

    let clamp = (cfg.clamp[0], cfg.clamp[1]);
    let r = match crate::forecast::nowcast::nowcast_ratio(&pairs, cfg.min_forecast_kw, clamp) {
        Ok(r) => r,
        Err(crate::forecast::nowcast::NowcastSkip::LowForecast { mean_kw }) => {
            return PvNowcast {
                source_age_s: Some(source_age_s),
                ..PvNowcast::skip(
                    cfg.window_minutes,
                    format!(
                        "low forecast ({mean_kw:.2} kW < {:.2} kW)",
                        cfg.min_forecast_kw
                    ),
                )
            };
        }
        Err(crate::forecast::nowcast::NowcastSkip::NoSamples) => {
            return PvNowcast {
                source_age_s: Some(source_age_s),
                ..PvNowcast::skip(cfg.window_minutes, "no covered samples")
            };
        }
    };

    let block_h = BLOCK_SECONDS / 3600.0;
    let measured_kwh: f64 = pairs.iter().map(|&(m, _)| m).sum::<f64>() * (5.0 / 60.0);
    let forecast_kwh: f64 = pairs.iter().map(|&(_, f)| f).sum::<f64>() * (5.0 / 60.0);
    let before = pv_kw.to_vec();
    let tau_hours = |b: usize| {
        ((start + Duration::seconds((BLOCK_SECONDS * b as f64) as i64) - now)
            .num_seconds()
            .max(0) as f64)
            / 3600.0
    };
    let touched =
        crate::forecast::nowcast::blend(pv_kw, tau_hours, r, cfg.efold_hours, cfg.max_hours);
    if let Some(peak) = dc_peak_kw.filter(|&p| p > 0.0) {
        for v in &mut pv_kw[..touched] {
            if *v > peak {
                *v = peak;
            }
        }
    }
    let delta_kwh: f64 = (0..touched).map(|b| (pv_kw[b] - before[b]) * block_h).sum();

    PvNowcast {
        applied: true,
        reason: None,
        ratio: Some(r),
        window_minutes: cfg.window_minutes,
        applied_hours: cfg.max_hours,
        blocks: touched,
        source_age_s: Some(source_age_s),
        measured_kwh: Some(measured_kwh),
        forecast_kwh: Some(forecast_kwh),
        delta_kwh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::*;

    #[test]
    fn nowcast_window_gate_rejects_empty_stale_and_partial() {
        use super::{check_window, floor_to_5min, WindowSkip};
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 7, 30, 7).unwrap();
        let at = |min_ago: i64| crate::influxdb::TimeSample {
            time: now - Duration::minutes(min_ago),
            value: 1.0,
        };
        assert_eq!(check_window(&[], now, 60), Err(WindowSkip::NoSamples));
        // Newest sample 11 min old: the feed has stalled.
        assert_eq!(
            check_window(&[at(11), at(16)], now, 60),
            Err(WindowSkip::Stale { age_s: 660 })
        );
        // Fresh but only 8 of the 12 expected 5-min windows (< 75 %).
        let eight: Vec<_> = (0..8).map(|i| at(1 + 5 * i)).collect();
        assert_eq!(
            check_window(&eight, now, 60),
            Err(WindowSkip::Partial {
                n: 8,
                expected: 12,
                age_s: 60
            })
        );
        // 9 of 12 clears the gate and reports the newest age.
        let nine: Vec<_> = (0..9).map(|i| at(1 + 5 * i)).collect();
        assert_eq!(check_window(&nine, now, 60), Ok(60));
        // The window end sits on a 5-minute mark with no sub-second remainder.
        let end = floor_to_5min(now + Duration::milliseconds(734));
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 10, 1, 7, 30, 0).unwrap());
        assert_eq!(end.timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn nowcast_coverage_threshold_scales_with_the_window() {
        use super::min_covered_minutes;
        assert_eq!(min_covered_minutes(60), 30);
        assert_eq!(min_covered_minutes(180), 90);
        // The shortest valid window (15 min = 3 samples) can still pass.
        assert_eq!(min_covered_minutes(15), 10);
        assert_eq!(min_covered_minutes(25), 12);
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
}
