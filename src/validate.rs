//! Backtest the thermal model against measured house data.
//!
//! In summer the underfloor heating is off, so the house drifts **passively** under the outside
//! temperature and solar gain — exactly the model's free response. This module drives the model
//! with the *measured* outside temperature and the open-meteo cloud cover (via [`crate::estimate`])
//! over a recent window and compares the predicted zone-air temperatures against what the house
//! actually did, scoring RMSE / bias / max error per zone.
//!
//! The only thing we can't measure is the initial temperature of the wall/slab masses, so the
//! simulation starts a configurable **warm-up** period before the comparison window: the slow
//! masses relax toward the driven solution and the seed guess washes out before we start scoring.
//! (With `warmup_hours = 0` the scores reflect that arbitrary seed, not the model — keep it large
//! relative to the slab time constant; the demo uses 48 h.)
//!
//! Internal gains (people, appliances, cooking, a fireplace) are heat the physics model has no
//! source for; this module also **calibrates** them, fitting a constant per-zone gain by coupled
//! non-negative least squares ([`fit_gains`]) so the active backtest (and the live forecast it feeds)
//! stops running cold in those rooms.
//!
//! **Time alignment.** Measured points are hourly means stamped at the window *stop* (see
//! [`crate::influxdb::InfluxQuery::aggregate_window`]); `predicted[k]` is the instantaneous
//! simulated state at grid hour `k`. So `predicted[k]` (state at the end of hour `k`) is compared
//! against `measured[k]` (the mean over the hour ending at `k`) — the same hour interval, an
//! endpoint-vs-mean comparison whose only residual is sub-hourly.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use uom::si::{
    f64::{Angle, ThermodynamicTemperature},
    thermodynamic_temperature::{degree_celsius, kelvin},
};

use nalgebra::DVector;

use crate::estimate::{drive, hour_key, read_drive_data, resample_ffill, seed_state, DriveData};
use crate::influxdb::{InfluxDB, TimeSample};
use crate::optimize::config::HeatingConfig;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;

/// Knobs for a passive backtest.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Hours of simulation before the comparison window, to relax the unknown wall/slab seed.
    pub warmup_hours: i64,
    /// Hours of comparison window (the most recent `window_hours`).
    pub window_hours: i64,
    /// Ground temperature (°C) under the slab — the `ground` boundary condition.
    pub ground_temperature_c: f64,
    /// Fallback cloud cover (0 clear .. 1 overcast) used only if the open-meteo cloud series is
    /// unavailable; otherwise the real per-hour cloud drives the solar gain.
    pub cloud_cover: f64,
}

/// Per-zone backtest result over the comparison window.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ZoneBacktest {
    pub zone: String,
    /// Number of compared (model, measured) point pairs.
    pub n: usize,
    pub measured_final_c: f64,
    pub predicted_final_c: f64,
    /// Root-mean-square error (K) over the window.
    pub rmse_k: f64,
    /// Mean signed error, predicted − measured (K): positive = model runs warm.
    pub mean_bias_k: f64,
    pub max_abs_error_k: f64,
}

/// Values aligned to `hours`, `None` where that hour has no sample.
fn align(hours: &[i64], samples: &[TimeSample]) -> Vec<Option<f64>> {
    let by_hour: HashMap<i64, f64> = samples
        .iter()
        .map(|s| (hour_key(s.time), s.value))
        .collect();
    hours.iter().map(|h| by_hour.get(h).copied()).collect()
}

fn k_to_c(kelvin_value: f64) -> f64 {
    ThermodynamicTemperature::new::<kelvin>(kelvin_value).get::<degree_celsius>()
}

/// Error stats over the last `window` points where both predicted and measured exist.
/// Returns `(n, measured_final, predicted_final, rmse, bias, max_abs)`, or `None` if no overlap.
fn error_stats(
    predicted: &[f64],
    measured: &[Option<f64>],
    window: usize,
) -> Option<(usize, f64, f64, f64, f64, f64)> {
    let n_pts = predicted.len().min(measured.len());
    let start = n_pts.saturating_sub(window);
    let mut pairs: Vec<(f64, f64)> = Vec::new();
    for i in start..n_pts {
        if let Some(m) = measured[i] {
            pairs.push((predicted[i], m));
        }
    }
    let (last_pred, last_meas) = *pairs.last()?; // None if no overlapping points
    let n = pairs.len();
    let sum_sq: f64 = pairs.iter().map(|(p, m)| (p - m).powi(2)).sum();
    let sum_err: f64 = pairs.iter().map(|(p, m)| p - m).sum();
    let max_abs = pairs.iter().map(|(p, m)| (p - m).abs()).fold(0.0, f64::max);
    Some((
        n,
        last_meas,
        last_pred,
        (sum_sq / n as f64).sqrt(),
        sum_err / n as f64,
        max_abs,
    ))
}

/// Run a passive backtest: drive the model with measured data and score each zone.
pub async fn backtest_passive(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    cfg: &BacktestConfig,
) -> Result<Vec<ZoneBacktest>> {
    ensure!(ss.n_states() > 0, "the model has no thermal states");
    ensure!(
        cfg.window_hours > 0 && cfg.warmup_hours >= 0,
        "window must be positive and warm-up non-negative"
    );
    let start = format!("-{}h", cfg.warmup_hours + cfg.window_hours);

    let data = read_drive_data(
        db,
        &start,
        "now()",
        cfg.ground_temperature_c,
        cfg.cloud_cover,
    )
    .await?;
    let (x0, zone_series) = seed_state(db, net, ss, &start, "now()").await?;
    let trajectory = drive(net, ss, latitude, longitude, &x0, &data);
    Ok(score_zones(
        net,
        ss,
        &trajectory,
        &data.hours,
        &zone_series,
        cfg.window_hours as usize,
    ))
}

/// Score each zone over the last `window` points: predicted[k] (state at grid hour k) vs the
/// measured hourly mean for that same hour. Sorted worst-RMSE first.
fn score_zones(
    net: &RcNetwork,
    ss: &StateSpace,
    trajectory: &[DVector<f64>],
    hours: &[i64],
    zone_series: &HashMap<String, Vec<TimeSample>>,
    window: usize,
) -> Vec<ZoneBacktest> {
    let mut results: Vec<ZoneBacktest> = Vec::new();
    for (zone, series) in zone_series {
        let Some(state_row) = net.zone_indices.get(zone).and_then(|&n| ss.state_index(n)) else {
            continue;
        };
        let predicted: Vec<f64> = trajectory.iter().map(|x| k_to_c(x[state_row])).collect();
        let measured = align(hours, series);
        if let Some((n, m_final, p_final, rmse, bias, max_abs)) =
            error_stats(&predicted, &measured, window)
        {
            results.push(ZoneBacktest {
                zone: zone.clone(),
                n,
                measured_final_c: m_final,
                predicted_final_c: p_final,
                rmse_k: rmse,
                mean_bias_k: bias,
                max_abs_error_k: max_abs,
            });
        }
    }
    results.sort_by(|a, b| {
        b.rmse_k
            .partial_cmp(&a.rmse_k)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Per-zone underfloor-heating power (kW) per grid hour, from the recorded relays
/// (`measurement=relay`, `tag1=heating`, tagged by the zone's room). The hourly mean of the 0/1
/// relay is the fraction of the hour it was on; × the zone's `max_heat_kw` gives the average power.
async fn read_heating_kw(
    db: &InfluxDB,
    net: &RcNetwork,
    heating: &HeatingConfig,
    hours: &[i64],
    start: &str,
    stop: &str,
) -> HashMap<String, Vec<f64>> {
    let mut out = HashMap::new();
    for (zone, spec) in &heating.zones {
        if !net
            .marker_indices
            .contains_key(&(zone.clone(), "heating".to_string()))
        {
            continue;
        }
        let Some(room) = db.zone_room(zone) else {
            continue;
        };
        let relay = db
            .read_series(
                "loxone",
                "relay",
                room,
                &[("tag1", "heating")],
                start,
                stop,
                "1h",
            )
            .await
            .unwrap_or_default();
        if relay.is_empty() {
            continue;
        }
        let powers: Vec<f64> = resample_ffill(hours, &relay)
            .iter()
            .map(|frac| frac.clamp(0.0, 1.0) * spec.max_heat_kw)
            .collect();
        out.insert(zone.clone(), powers);
    }
    out
}

/// Non-negative least squares: minimise ‖`a`·x − `b`‖² subject to x ≥ 0, by projected coordinate
/// descent. `a` is `rows × cols` row-major; the problem is convex so the coordinate sweep converges.
/// The fit is small (one column per cold zone, ≤ ~16) and well-scaled, so the 1000-sweep budget is
/// far more than it needs — it converges in well under a hundred sweeps in practice.
fn nnls(a: &[Vec<f64>], b: &[f64], cols: usize) -> Vec<f64> {
    let rows = a.len();
    let col_sq: Vec<f64> = (0..cols)
        .map(|j| a.iter().map(|row| row[j] * row[j]).sum::<f64>())
        .collect();
    let mut x = vec![0.0; cols];
    let mut resid: Vec<f64> = b.iter().map(|v| -v).collect(); // residual = a·x − b, with x = 0
    for _ in 0..1000 {
        let mut max_step = 0.0_f64;
        for j in 0..cols {
            if col_sq[j] <= 1e-12 {
                continue;
            }
            let grad: f64 = (0..rows).map(|i| a[i][j] * resid[i]).sum();
            let next = (x[j] - grad / col_sq[j]).max(0.0);
            let delta = next - x[j];
            if delta != 0.0 {
                for (i, ri) in resid.iter_mut().enumerate() {
                    *ri += delta * a[i][j];
                }
                x[j] = next;
                max_step = max_step.max(delta.abs());
            }
        }
        if max_step < 1e-6 {
            break;
        }
    }
    x
}

/// Read everything an active (heating-on) fit/backtest needs over `[start, stop]`: the driving data
/// (outside temp + cloud), the **recorded per-zone heating relays**, the measured seed state, and the
/// measured per-zone temperature series. The returned `DriveData` has **no** internal gains set.
async fn load_active(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<(DriveData, DVector<f64>, HashMap<String, Vec<TimeSample>>)> {
    ensure!(ss.n_states() > 0, "the model has no thermal states");
    let mut data =
        read_drive_data(db, start, stop, cfg.ground_temperature_c, cfg.cloud_cover).await?;
    data.heating_kw = read_heating_kw(db, net, heating, &data.hours, start, stop).await;
    let (x0, zone_series) = seed_state(db, net, ss, start, stop).await?;
    Ok((data, x0, zone_series))
}

/// The coupled NNLS fit of per-zone internal gains (W), pure (no IO). The model is LTI, so the zones'
/// window-mean temperatures are jointly *affine* in the gain vector: drive with no gains to get each
/// zone's bias, probe each *cold* zone with a unit gain to measure the full (coupled) response, and
/// solve `response·g = −bias` with non-negative least squares. Fitting zones independently would
/// overheat small rooms via their big neighbours' leakage through shared walls; the joint fit doesn't.
/// `data` must carry the recorded heating and **no** internal gains. Scores over the last `window` h.
#[allow(clippy::too_many_arguments)] // model, site, state, data, series and window are all distinct
fn fit_gains(
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    x0: &DVector<f64>,
    data: &DriveData,
    zone_series: &HashMap<String, Vec<TimeSample>>,
    window: usize,
) -> HashMap<String, f64> {
    let bias = |traj: &[DVector<f64>]| -> HashMap<String, f64> {
        score_zones(net, ss, traj, &data.hours, zone_series, window)
            .into_iter()
            .map(|z| (z.zone, z.mean_bias_k))
            .collect()
    };
    let bias0 = bias(&drive(net, ss, latitude, longitude, x0, data));

    const PROBE_W: f64 = 500.0;
    // A candidate is kept only if its own gain moves its own window-mean by at least this (K per W).
    // A zone barely coupled to its gain (a sealed, well-insulated room) is *unidentifiable*: a tiny
    // self-response would let the solver assign an absurd gain to explain a small bias (≈ bias /
    // self-response). 1e-3 K/W ⇒ ≥0.5 K per 500 W probe — far below any real slab-heated zone (~10×).
    const MIN_SELF_RESPONSE: f64 = 1e-3;

    let mut zones: Vec<String> = zone_series.keys().cloned().collect();
    zones.sort();
    let bias0_vec: Vec<f64> = zones
        .iter()
        .map(|z| bias0.get(z).copied().unwrap_or(0.0))
        .collect();
    let index_of: HashMap<&str, usize> = zones
        .iter()
        .enumerate()
        .map(|(i, z)| (z.as_str(), i))
        .collect();

    // Candidate gains are the cold zones (negative bias); already-warm zones get none (you can't
    // remove internal heat) but still constrain the fit through their rows in the response matrix.
    // Probe each cold zone with a unit gain to measure the full coupled response (one column), then
    // drop the unidentifiable ones (negligible self-response) before solving.
    let mut cand: Vec<String> = Vec::new();
    let mut columns: Vec<Vec<f64>> = Vec::new(); // each column has one entry per zone (length zones.len())
    for (zone, &b0) in zones.iter().zip(&bias0_vec) {
        if b0 >= 0.0 {
            continue;
        }
        let mut probe = data.clone();
        probe.internal_gain_w = HashMap::from([(zone.clone(), PROBE_W)]);
        let probed = bias(&drive(net, ss, latitude, longitude, x0, &probe));
        // column[i] = °C window-mean shift in zone i per watt of internal gain in this zone.
        let column: Vec<f64> = zones
            .iter()
            .enumerate()
            .map(|(i, zi)| (probed.get(zi).copied().unwrap_or(0.0) - bias0_vec[i]) / PROBE_W)
            .collect();
        if column[index_of[zone.as_str()]] < MIN_SELF_RESPONSE {
            eprintln!("  fit_gains: zone '{zone}' too weakly coupled to fit a gain, skipping");
            continue;
        }
        cand.push(zone.clone());
        columns.push(column);
    }

    // Assemble response[i][j] (zones × candidates) from the per-candidate columns.
    let response: Vec<Vec<f64>> = (0..zones.len())
        .map(|i| columns.iter().map(|col| col[i]).collect())
        .collect();
    // Solve min‖response·g − (−bias0)‖² s.t. g ≥ 0: drive every zone's bias toward zero.
    let target: Vec<f64> = bias0_vec.iter().map(|b| -b).collect();
    let g = nnls(&response, &target, cand.len());
    // Drop thermally-negligible (sub-watt) gains: they're fit noise, not a real internal source.
    const MIN_GAIN_W: f64 = 1.0;
    cand.into_iter()
        .zip(g)
        .filter(|(_, w)| *w >= MIN_GAIN_W)
        .collect()
}

/// Fit the per-zone internal gains (W) over a trailing window — the **live self-correction**. Same
/// coupled fit as [`calibrate_internal_gains`] but returning only the gains, so the shadow loop can
/// re-fit periodically and track changes in occupant behaviour (more/fewer people, appliance use)
/// without any config or model edit. An empty result means the model needs no extra gain anywhere.
#[allow(clippy::too_many_arguments)] // the model, heating, site, window and time bounds are all distinct
pub async fn fit_internal_gains(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    latitude: Angle,
    longitude: Angle,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<HashMap<String, f64>> {
    let (data, x0, zone_series) = load_active(db, net, ss, heating, cfg, start, stop).await?;
    Ok(fit_gains(
        net,
        ss,
        latitude,
        longitude,
        &x0,
        &data,
        &zone_series,
        cfg.window_hours as usize,
    ))
}

/// Calibrate the per-zone internal gains (W) **and** report the heat model's accuracy before and
/// after — the active backtest. Drives the model with the recorded per-zone heating relays plus the
/// measured outside temperature and solar, scores each zone with no gains (`before`), fits the gains
/// (see [`fit_gains`]), and re-scores with them (`after`). The first `start..` hours act as warm-up;
/// the last `cfg.window_hours` are scored. Returns `(before, after, gains_w)`.
#[allow(clippy::too_many_arguments)] // the model, heating, site, window and time bounds are all distinct
pub async fn calibrate_internal_gains(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    latitude: Angle,
    longitude: Angle,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<(Vec<ZoneBacktest>, Vec<ZoneBacktest>, HashMap<String, f64>)> {
    let (mut data, x0, zone_series) = load_active(db, net, ss, heating, cfg, start, stop).await?;
    let window = cfg.window_hours as usize;
    let before = score_zones(
        net,
        ss,
        &drive(net, ss, latitude, longitude, &x0, &data),
        &data.hours,
        &zone_series,
        window,
    );
    let gains = fit_gains(
        net,
        ss,
        latitude,
        longitude,
        &x0,
        &data,
        &zone_series,
        window,
    );
    data.internal_gain_w = gains.clone();
    let after = score_zones(
        net,
        ss,
        &drive(net, ss, latitude, longitude, &x0, &data),
        &data.hours,
        &zone_series,
        window,
    );
    Ok((before, after, gains))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample(hour: i64, value: f64) -> TimeSample {
        TimeSample {
            time: Utc.timestamp_opt(hour * 3600, 0).single().unwrap(),
            value,
        }
    }

    #[test]
    fn align_marks_missing_hours_none() {
        let samples = vec![sample(0, 10.0), sample(2, 12.0)];
        let hours = vec![0, 1, 2];
        assert_eq!(align(&hours, &samples), vec![Some(10.0), None, Some(12.0)]);
    }

    #[test]
    fn error_stats_over_window() {
        // predicted vs measured; window = last 3 points.
        let predicted = vec![20.0, 21.0, 22.0, 23.0];
        let measured = vec![Some(20.0), Some(20.0), Some(20.0), Some(20.0)];
        let (n, m_final, p_final, rmse, bias, max_abs) =
            error_stats(&predicted, &measured, 3).unwrap();
        assert_eq!(n, 3);
        assert_eq!(m_final, 20.0);
        assert_eq!(p_final, 23.0);
        assert_eq!(bias, (1.0 + 2.0 + 3.0) / 3.0); // predicted runs warm
        assert_eq!(max_abs, 3.0);
        assert!((rmse - ((1.0 + 4.0 + 9.0) / 3.0_f64).sqrt()).abs() < 1e-12);
    }

    #[test]
    fn error_stats_skips_missing_measurements() {
        let predicted = vec![20.0, 21.0, 22.0];
        let measured = vec![Some(20.0), None, Some(20.0)];
        let (n, _, _, _, _, max_abs) = error_stats(&predicted, &measured, 3).unwrap();
        assert_eq!(n, 2); // the None point is skipped
        assert_eq!(max_abs, 2.0);
    }

    #[test]
    fn error_stats_no_overlap_is_none() {
        assert!(error_stats(&[20.0], &[None], 1).is_none());
    }

    #[test]
    fn nnls_recovers_unconstrained_solution() {
        // Diagonal-dominant system with a positive solution: x = [2, 3].
        let a = vec![vec![2.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]];
        let b = vec![4.0, 3.0, 5.0]; // a·[2,3] = [4, 3, 5] exactly
        let x = nnls(&a, &b, 2);
        assert!((x[0] - 2.0).abs() < 1e-6, "x0 = {}", x[0]);
        assert!((x[1] - 3.0).abs() < 1e-6, "x1 = {}", x[1]);
    }

    #[test]
    fn nnls_recovers_coupled_solution() {
        // Off-diagonal coupling (the real use case: a gain in one zone warms its neighbours). The
        // exact non-negative solution is x = [1, 2, 3]; recover it through the cross terms.
        let a = vec![
            vec![1.0, 0.3, 0.0],
            vec![0.2, 1.0, 0.1],
            vec![0.0, 0.4, 1.0],
        ];
        // b = a·[1,2,3]
        let b = vec![1.0 + 0.6 + 0.0, 0.2 + 2.0 + 0.3, 0.0 + 0.8 + 3.0];
        let x = nnls(&a, &b, 3);
        for (got, want) in x.iter().zip([1.0, 2.0, 3.0]) {
            assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
        }
    }

    #[test]
    fn nnls_clamps_negative_to_zero() {
        // The least-squares optimum wants x = −1; the non-negativity constraint pins it at 0.
        let a = vec![vec![1.0]];
        let b = vec![-1.0];
        let x = nnls(&a, &b, 1);
        assert_eq!(x[0], 0.0);
    }

    #[test]
    fn nnls_with_no_columns_is_empty() {
        // The "all zones already warm" case: no candidate gains → a zero-width system → empty result.
        let a = vec![vec![], vec![], vec![]];
        let b = vec![1.0, 2.0, 3.0];
        assert!(nnls(&a, &b, 0).is_empty());
    }
}
