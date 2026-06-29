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
use chrono::FixedOffset;
use uom::si::f64::Angle;

use nalgebra::DVector;

use crate::estimate::{drive, hour_key, read_drive_data, resample_ffill, seed_state, DriveData};
use crate::influxdb::TimeSample;
use crate::optimize::config::{HeatingConfig, ScheduledLoad};
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::{k_to_c, mean, rmse, sort_desc_by_key};

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
    let (last_pred, last_meas) = *pairs.last()?;
    let n = pairs.len();
    let sum_sq: f64 = pairs.iter().map(|(p, m)| (p - m).powi(2)).sum();
    let sum_err: f64 = pairs.iter().map(|(p, m)| p - m).sum();
    let max_abs = pairs.iter().map(|(p, m)| (p - m).abs()).fold(0.0, f64::max);
    Some((
        n,
        last_meas,
        last_pred,
        rmse(sum_sq, n),
        mean(sum_err, n),
        max_abs,
    ))
}

/// Run a passive backtest: drive the model with measured data and score each zone.
pub async fn backtest_passive(
    db: &SourceClients,
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
    sort_desc_by_key(&mut results, |r| r.rmse_k);
    results
}

/// Per-zone underfloor-heating power (kW) per grid hour, from the recorded relays
/// (`measurement=relay`, `tag1=heating`, tagged by the zone's room). The hourly mean of the 0/1
/// relay is the fraction of the hour it was on; × the zone's `max_heat_kw` gives the average power.
async fn read_heating_kw(
    db: &SourceClients,
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
        // The relay location resolves through the pluggable signal map (default `loxone`/`relay`,
        // `tag1=heating`); the per-zone room is the field.
        let relay = db
            .heating_relay_series(room, start, stop, "1h")
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
/// descent. `a` is `rows × cols` row-major; the problem is convex so the coordinate sweep converges
/// (the small, well-scaled fit stops well inside the sweep budget via the early-exit below).
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

/// Per scheduled load with a `sensor`, its **measured** electrical-power series (W) forward-filled
/// onto the hourly grid (`None` for a load with no sensor), aligned 1:1 to `scheduled_loads`. The drive
/// derives a sensor-driven load's flux from this (× `power_factor`, gated by the schedule) instead of a
/// fitted/configured magnitude. Mirrors [`read_heating_kw`]'s read → `resample_ffill` alignment; a
/// sensor whose read fails or returns nothing degrades to `None` (the magnitude path applies).
async fn read_sensor_power_w(
    db: &SourceClients,
    scheduled_loads: &[ScheduledLoad],
    hours: &[i64],
    start: &str,
    stop: &str,
) -> Vec<Option<Vec<f64>>> {
    let mut out = Vec::with_capacity(scheduled_loads.len());
    for load in scheduled_loads {
        let series = match &load.sensor {
            Some(loc) => match db.read_locator_series(loc, start, stop, "1h").await {
                Ok(s) if !s.is_empty() => Some(resample_ffill(hours, &s)),
                Ok(_) => None,
                Err(e) => {
                    eprintln!(
                        "  load_active: scheduled load '{}' sensor read failed ({e}), falling back to magnitude",
                        load.label
                    );
                    None
                }
            },
            None => None,
        };
        out.push(series);
    }
    out
}

/// Read everything an active (heating-on) fit/backtest needs over `[start, stop]`: the driving data
/// (outside temp + cloud), the **recorded per-zone heating relays**, the measured seed state, and the
/// measured per-zone temperature series. The returned `DriveData` carries the scheduled loads with
/// their **fixed** (`power_w`) magnitudes applied and the fitted ones at 0 (the probe driver in
/// [`fit_gains`] sets those), each sensor-driven load's **measured** power series, plus the site
/// `local_offset`, but **no** internal gains.
#[allow(clippy::too_many_arguments)] // db, model, heating, loads, offset, cfg and time bounds are all distinct
async fn load_active(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    scheduled_loads: &[ScheduledLoad],
    local_offset: FixedOffset,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<(DriveData, DVector<f64>, HashMap<String, Vec<TimeSample>>)> {
    ensure!(ss.n_states() > 0, "the model has no thermal states");
    let mut data =
        read_drive_data(db, start, stop, cfg.ground_temperature_c, cfg.cloud_cover).await?;
    data.heating_kw = read_heating_kw(db, net, heating, &data.hours, start, stop).await;
    data.scheduled_loads = scheduled_loads.to_vec();
    // Fixed loads (`power_w`) applied at their configured magnitude; fitted loads start at 0 (the fit
    // probe sets those). So the `before` score and the fit baseline both include the known draws.
    data.scheduled_w = scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0))
        .collect();
    // Sensor-driven loads derive their flux from the measured draw, read here onto the same hourly grid.
    data.sensor_power_w = read_sensor_power_w(db, scheduled_loads, &data.hours, start, stop).await;
    data.local_offset = local_offset;
    let (x0, zone_series) = seed_state(db, net, ss, start, stop).await?;
    Ok((data, x0, zone_series))
}

/// The result of the coupled fit: per-zone constant internal gains (W) and the fitted magnitude of
/// each scheduled load (W, ≥ 0), aligned 1:1 to the loads passed into [`fit_gains`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GainFit {
    /// Per-zone constant internal gain (W) — only the zones the fit kept (≥ `MIN_GAIN_W`).
    pub gains: HashMap<String, f64>,
    /// Fitted magnitude (W, ≥ 0) of each scheduled load, aligned to the input `scheduled_loads`
    /// (`0.0` for a load that was dropped as too weakly coupled or fit to nothing).
    pub scheduled_w: Vec<f64>,
}

/// The coupled NNLS fit of per-zone internal gains (W) **and** scheduled-load magnitudes (W), pure
/// (no IO). The model is LTI, so each zone's per-hour residual trajectory is jointly *affine* in the
/// gain/magnitude vector. We fit that **trajectory** (one row per (zone, hour) with measured data),
/// not just each zone's window mean: against a single mean a flat internal gain and a time-localized
/// scheduled load are collinear, but against the trajectory the load's distinctive on/off shape
/// separates it from the always-on gain.
///
/// Drive with no fitted gains and the **fixed** scheduled loads at their configured magnitude for the
/// baseline; probe each *cold* zone (negative mean residual) with a unit internal gain and each
/// **fitted** scheduled load with a unit magnitude to measure their full (coupled) response (one
/// column each); then solve `column·c = target` with non-negative least squares. A **fixed** load
/// (`power_w` set) or a **sensor**-driven load (flux from the measured draw) is a known input, not a
/// candidate: it's applied at its configured/measured magnitude in the baseline drive *and* every
/// probe (the latter via `data`'s `sensor_power_w`), so the fit explains only what's left over it.
/// Fitting zones
/// independently would overheat small rooms via their big neighbours' leakage through shared walls;
/// the joint fit doesn't. `data` must carry the recorded heating, **no** internal gains, and the
/// loads' `zone`/`local_offset`; its `scheduled_w` is ignored — the baseline and every probe rebuild
/// the fixed magnitudes from each load's `power_w`. Scores over the last `window` h.
#[allow(clippy::too_many_arguments)] // model, site, state, data, series, loads, offset and window are all distinct
fn fit_gains(
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    x0: &DVector<f64>,
    data: &DriveData,
    zone_series: &HashMap<String, Vec<TimeSample>>,
    scheduled_loads: &[ScheduledLoad],
    window: usize,
    local_offset: FixedOffset,
) -> GainFit {
    const PROBE_W: f64 = 500.0;
    // A candidate is kept only if it moves some row by at least this (K per W). A zone barely coupled
    // to its gain (a sealed, well-insulated room) — or a scheduled load whose window doesn't overlap
    // the fit window — is *unidentifiable*: a tiny response would let the solver assign an absurd
    // coefficient to explain a small residual. 1e-3 K/W ⇒ ≥0.5 K per 500 W probe.
    const MIN_SELF_RESPONSE: f64 = 1e-3;
    const MIN_GAIN_W: f64 = 1.0;

    // A **fixed** load (`power_w` set) is a known input applied at its configured magnitude in the
    // baseline drive and every probe; a **fitted** load (`power_w` None) is 0 here and becomes a
    // candidate below. The baseline and probes all start from this vector, so the fit explains only the
    // residual the fixed loads leave behind.
    let fixed_w: Vec<f64> = scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0))
        .collect();

    // Fixed row layout: every (zone, hour) over the last `window` hours where a measured value exists.
    // `predicted_rows` reads a trajectory at exactly those (state_row, hour) cells, so every column
    // and the target share one index space.
    let mut zones: Vec<String> = zone_series.keys().cloned().collect();
    zones.sort();
    struct Row {
        zone: usize, // index into `zones`
        state_row: usize,
        hour: usize, // index into `data.hours`
        measured: f64,
    }
    let n_hours = data.hours.len();
    let win_start = n_hours.saturating_sub(window);
    let mut rows: Vec<Row> = Vec::new();
    for (zi, zone) in zones.iter().enumerate() {
        let Some(state_row) = net.zone_indices.get(zone).and_then(|&n| ss.state_index(n)) else {
            continue;
        };
        let measured = align(&data.hours, &zone_series[zone]);
        for hour in win_start..n_hours {
            if let Some(m) = measured.get(hour).copied().flatten() {
                rows.push(Row {
                    zone: zi,
                    state_row,
                    hour,
                    measured: m,
                });
            }
        }
    }
    // Read a driven trajectory at each row's (state_row, hour) cell, in °C.
    let predicted_rows = |traj: &[DVector<f64>]| -> Vec<f64> {
        rows.iter()
            .map(|r| {
                traj.get(r.hour)
                    .map(|x| k_to_c(x[r.state_row]))
                    .unwrap_or(0.0)
            })
            .collect::<Vec<f64>>()
    };

    // Baseline: no fitted gains, fixed loads applied at their configured magnitude. Build the drive
    // explicitly (with the loads + fixed magnitudes + offset) rather than trusting `data` to carry
    // them, so the baseline matches every probe's known-input state.
    let mut baseline = data.clone();
    baseline.internal_gain_w = HashMap::new();
    baseline.scheduled_loads = scheduled_loads.to_vec();
    baseline.scheduled_w = fixed_w.clone();
    baseline.local_offset = local_offset;
    let baseline_pred = predicted_rows(&drive(net, ss, latitude, longitude, x0, &baseline));
    let target: Vec<f64> = rows
        .iter()
        .zip(&baseline_pred)
        .map(|(r, &p)| r.measured - p)
        .collect();
    // Per-zone mean baseline residual (predicted − measured): a cold zone (negative) gets an
    // internal-gain candidate; warm zones constrain the fit through their rows only.
    let mut zone_resid_sum = vec![0.0_f64; zones.len()];
    let mut zone_resid_n = vec![0_usize; zones.len()];
    for (r, &p) in rows.iter().zip(&baseline_pred) {
        zone_resid_sum[r.zone] += p - r.measured;
        zone_resid_n[r.zone] += 1;
    }
    let column_of = |probe: &DriveData| -> Vec<f64> {
        let pred = predicted_rows(&drive(net, ss, latitude, longitude, x0, probe));
        pred.iter()
            .zip(&baseline_pred)
            .map(|(p, b)| (p - b) / PROBE_W)
            .collect::<Vec<f64>>()
    };

    // Candidate columns, in a fixed order: constant gains (cold zones) first, then scheduled loads.
    let mut columns: Vec<Vec<f64>> = Vec::new();
    let mut gain_zones: Vec<String> = Vec::new();
    for (zi, zone) in zones.iter().enumerate() {
        let mean_resid = if zone_resid_n[zi] > 0 {
            zone_resid_sum[zi] / zone_resid_n[zi] as f64
        } else {
            0.0
        };
        if mean_resid >= 0.0 {
            continue; // already warm — can't remove internal heat
        }
        let mut probe = data.clone();
        probe.internal_gain_w = HashMap::from([(zone.clone(), PROBE_W)]);
        probe.scheduled_loads = scheduled_loads.to_vec();
        probe.scheduled_w = fixed_w.clone(); // keep the fixed loads applied
        probe.local_offset = local_offset;
        let column = column_of(&probe);
        if column.iter().fold(0.0_f64, |m, &c| m.max(c.abs())) < MIN_SELF_RESPONSE {
            eprintln!("  fit_gains: zone '{zone}' too weakly coupled to fit a gain, skipping");
            continue;
        }
        gain_zones.push(zone.clone());
        columns.push(column);
    }
    let n_gain_cands = gain_zones.len();

    // One candidate per **fitted** scheduled load (`power_w` None and no `sensor`) whose zone is in the
    // model. A **fixed** load (`power_w`) is a known input already applied in `fixed_w`; a
    // **sensor-driven** load is also known — its measured flux is already in the baseline/probes via
    // `data.sensor_power_w` in the drive — so neither is a candidate. Probe a fitted load by adding the
    // unit magnitude to its slot (on top of the fixed magnitudes) and reading its coupled response.
    let mut load_cand_idx: Vec<usize> = Vec::new(); // index into `scheduled_loads`
    for (li, load) in scheduled_loads.iter().enumerate() {
        if load.power_w.is_some() || load.sensor.is_some() {
            continue; // known magnitude (configured) or measured (sensor) — applied, not fitted
        }
        if !net.zone_indices.contains_key(&load.zone) {
            continue;
        }
        let mut probe = data.clone();
        probe.internal_gain_w = HashMap::new();
        probe.scheduled_loads = scheduled_loads.to_vec();
        let mut sched = fixed_w.clone();
        sched[li] = PROBE_W;
        probe.scheduled_w = sched;
        probe.local_offset = local_offset;
        let column = column_of(&probe);
        if column.iter().fold(0.0_f64, |m, &c| m.max(c.abs())) < MIN_SELF_RESPONSE {
            eprintln!(
                "  fit_gains: scheduled load '{}' too weakly coupled, skipping",
                load.label
            );
            continue;
        }
        load_cand_idx.push(li);
        columns.push(column);
    }

    // Solve min‖column·c − target‖² s.t. c ≥ 0 (NNLS). The matrix is rows × candidates.
    let n_cands = columns.len();
    let matrix: Vec<Vec<f64>> = (0..rows.len())
        .map(|i| columns.iter().map(|col| col[i]).collect())
        .collect();
    let coeffs = nnls(&matrix, &target, n_cands);

    // Constant-gain coeffs (≥ MIN_GAIN_W) → gains; the unit_profile already carries the sink/source
    // sign, so a scheduled load's coeff is the (non-negative) watts it moves.
    let gains: HashMap<String, f64> = gain_zones
        .into_iter()
        .zip(coeffs.iter().take(n_gain_cands).copied())
        .filter(|(_, w)| *w >= MIN_GAIN_W)
        .collect();
    // Start from the configured magnitudes (a fixed load keeps its `power_w`) and overwrite only the
    // fitted slots with their solved coeff.
    let mut scheduled_w = fixed_w.clone();
    for (slot, &w) in load_cand_idx.iter().zip(coeffs.iter().skip(n_gain_cands)) {
        // Drop a sub-watt magnitude as fit noise — the same `MIN_GAIN_W` floor the constant gains use,
        // so a load the data doesn't actually support reports 0 rather than a spurious fraction of a W.
        // (Fitted slots only; a fixed slot is never visited by this loop.)
        scheduled_w[*slot] = if w >= MIN_GAIN_W { w } else { 0.0 };
    }
    GainFit { gains, scheduled_w }
}

/// Fit the per-zone internal gains (W) and scheduled-load magnitudes over a trailing window — the
/// **live self-correction**. Same coupled fit as [`calibrate_internal_gains`] but returning only the
/// fit, so the MPC loop can re-fit periodically and track changes in occupant behaviour (more/fewer
/// people, appliance use) without any config or model edit. An empty result means the model needs no
/// extra gain anywhere.
#[allow(clippy::too_many_arguments)] // the model, heating, loads, site, window and time bounds are all distinct
pub async fn fit_internal_gains(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    scheduled_loads: &[ScheduledLoad],
    local_offset: FixedOffset,
    latitude: Angle,
    longitude: Angle,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<GainFit> {
    let (data, x0, zone_series) = load_active(
        db,
        net,
        ss,
        heating,
        scheduled_loads,
        local_offset,
        cfg,
        start,
        stop,
    )
    .await?;
    Ok(fit_gains(
        net,
        ss,
        latitude,
        longitude,
        &x0,
        &data,
        &zone_series,
        scheduled_loads,
        cfg.window_hours as usize,
        local_offset,
    ))
}

/// Calibrate the per-zone internal gains (W) and scheduled-load magnitudes **and** report the heat
/// model's accuracy before and after — the active backtest. Drives the model with the recorded
/// per-zone heating relays plus the measured outside temperature and solar, scores each zone with no
/// gains (`before`), fits the gains + loads (see [`fit_gains`]), and re-scores with them (`after`).
/// The first `start..` hours act as warm-up; the last `cfg.window_hours` are scored. Returns
/// `(before, after, fit)`.
#[allow(clippy::too_many_arguments)] // the model, heating, loads, site, window and time bounds are all distinct
pub async fn calibrate_internal_gains(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    heating: &HeatingConfig,
    scheduled_loads: &[ScheduledLoad],
    local_offset: FixedOffset,
    latitude: Angle,
    longitude: Angle,
    cfg: &BacktestConfig,
    start: &str,
    stop: &str,
) -> Result<(Vec<ZoneBacktest>, Vec<ZoneBacktest>, GainFit)> {
    let (mut data, x0, zone_series) = load_active(
        db,
        net,
        ss,
        heating,
        scheduled_loads,
        local_offset,
        cfg,
        start,
        stop,
    )
    .await?;
    let window = cfg.window_hours as usize;
    let before = score_zones(
        net,
        ss,
        &drive(net, ss, latitude, longitude, &x0, &data),
        &data.hours,
        &zone_series,
        window,
    );
    let fit = fit_gains(
        net,
        ss,
        latitude,
        longitude,
        &x0,
        &data,
        &zone_series,
        scheduled_loads,
        window,
        local_offset,
    );
    data.internal_gain_w = fit.gains.clone();
    data.scheduled_w = fit.scheduled_w.clone();
    let after = score_zones(
        net,
        ss,
        &drive(net, ss, latitude, longitude, &x0, &data),
        &data.hours,
        &zone_series,
        window,
    );
    Ok((before, after, fit))
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

    use crate::model::Model;
    use crate::optimize::config::{LoadKind, LoadWindow};
    use uom::si::angle::degree;
    use uom::si::f64::ThermodynamicTemperature;
    use uom::si::thermodynamic_temperature::degree_celsius;

    /// A tiny single-zone house: a floor to ground and a wall to outside. Enough thermal mass that the
    /// air node responds to a sustained air-node flux but doesn't track it instantly.
    fn one_zone() -> (RcNetwork, StateSpace) {
        let model = Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [ { material: "concrete", thickness: 0.1 } ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { lr: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["lr", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["lr", "outside"], area: 30 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    /// A hand-built `DriveData` over `n_hours` from `start_hour` (unix-hour), constant outside temp,
    /// no cloud, no heating, carrying the given scheduled loads at zero magnitude and the offset.
    fn synthetic_drive(
        start_hour: i64,
        n_hours: usize,
        outside_c: f64,
        loads: &[ScheduledLoad],
        local_offset: FixedOffset,
    ) -> DriveData {
        let hours: Vec<i64> = (start_hour..start_hour + n_hours as i64).collect();
        let grid_times = hours
            .iter()
            .map(|h| Utc.timestamp_opt(h * 3600, 0).single().unwrap())
            .collect();
        DriveData {
            grid_times,
            hours,
            outside_c: vec![outside_c; n_hours],
            cloud: vec![1.0; n_hours], // fully overcast → solar gain negligible, keeps the test simple
            ground_c: 12.0,
            heating_kw: HashMap::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: loads.to_vec(),
            scheduled_w: vec![0.0; loads.len()],
            sensor_power_w: vec![None; loads.len()],
            local_offset,
        }
    }

    /// KEYSTONE: the fit recovers a known scheduled-sink magnitude from a self-generated "measured"
    /// trajectory, and attributes the windowed sink to the scheduled load — not to a constant gain.
    /// This is the correctness gate: a flat gain and a time-localized load are collinear against a
    /// single window mean; the trajectory fit separates them.
    #[test]
    fn fit_recovers_scheduled_sink_magnitude() {
        let (net, ss) = one_zone();
        let local_offset = FixedOffset::east_opt(0).unwrap(); // UTC == local: window hours are unix hours
        let (lat, lon) = (Angle::new::<degree>(50.0), Angle::new::<degree>(14.0));

        // A daytime sink in `lr`, active 10:00–14:00 every day. Start the grid at unix-hour 0 (a
        // midnight) and run five days so the on/off shape repeats and the slab seed washes out.
        let load = ScheduledLoad {
            zone: "lr".to_string(),
            label: "water heat-pump".to_string(),
            kind: LoadKind::Sink,
            power_w: None,
            sensor: None,
            power_factor: None,
            controllable: false,
            run_hours: None,
            windows: vec![LoadWindow {
                months: Vec::new(),
                start: "10:00".to_string(),
                end: "14:00".to_string(),
            }],
        };
        let loads = [load];
        let n_hours = 24 * 5;
        let mut data = synthetic_drive(0, n_hours, 8.0, &loads, local_offset);

        // Seed the state at a uniform 20 °C.
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );

        // Generate the "measured" series by driving WITH the true sink magnitude.
        const TRUE_W: f64 = 800.0;
        let mut truth = data.clone();
        truth.scheduled_w = vec![TRUE_W];
        let truth_traj = drive(&net, &ss, lat, lon, &x0, &truth);
        let state_row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let zone_series: HashMap<String, Vec<TimeSample>> = HashMap::from([(
            "lr".to_string(),
            data.hours
                .iter()
                .zip(&truth_traj)
                .map(|(&h, x)| TimeSample {
                    time: Utc.timestamp_opt(h * 3600, 0).single().unwrap(),
                    value: k_to_c(x[state_row]),
                })
                .collect(),
        )]);

        // `data` carries the loads at zero magnitude (as load_active would); fit over the full window.
        data.scheduled_w = vec![0.0];
        let fit = fit_gains(
            &net,
            &ss,
            lat,
            lon,
            &x0,
            &data,
            &zone_series,
            &loads,
            n_hours,
            local_offset,
        );

        // (a) the sink magnitude is recovered within a few percent.
        let recovered = fit.scheduled_w[0];
        let rel_err = (recovered - TRUE_W).abs() / TRUE_W;
        assert!(
            rel_err < 0.05,
            "recovered sink {recovered:.1} W vs true {TRUE_W} W (rel err {:.3})",
            rel_err
        );
        // (b) the constant gain for the zone stays near 0 — the windowed sink is attributed to the
        // scheduled load, not absorbed by an always-on gain. (Cooling can't be a gain at all — gains
        // are ≥ 0 — so the real risk is the fit assigning a spurious *positive* gain; assert it's tiny.)
        let gain = fit.gains.get("lr").copied().unwrap_or(0.0);
        assert!(
            gain < 0.05 * TRUE_W,
            "constant gain {gain:.1} W should be ~0"
        );
    }

    /// The central claim, the hard case: a zone with BOTH an always-on internal gain AND a windowed
    /// sink. A single window mean couldn't separate them (collinear); the trajectory fit must recover
    /// *both*. Generate "measured" with both, fit with both unknown, assert each is recovered.
    #[test]
    fn fit_separates_constant_gain_from_scheduled_sink() {
        let (net, ss) = one_zone();
        let local_offset = FixedOffset::east_opt(0).unwrap();
        let (lat, lon) = (Angle::new::<degree>(50.0), Angle::new::<degree>(14.0));
        let loads = [ScheduledLoad {
            zone: "lr".to_string(),
            label: "water heat-pump".to_string(),
            kind: LoadKind::Sink,
            power_w: None,
            sensor: None,
            power_factor: None,
            controllable: false,
            run_hours: None,
            windows: vec![LoadWindow {
                months: Vec::new(),
                start: "10:00".to_string(),
                end: "14:00".to_string(),
            }],
        }];
        let n_hours = 24 * 5;
        let mut data = synthetic_drive(0, n_hours, 8.0, &loads, local_offset);
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );

        // Truth: a 200 W always-on gain AND an 800 W daytime sink, both in `lr` (net warming, so the
        // zone still reads "cold" against the no-gain baseline and gets an internal-gain candidate).
        const TRUE_GAIN: f64 = 200.0;
        const TRUE_SINK: f64 = 800.0;
        let mut truth = data.clone();
        truth.internal_gain_w = HashMap::from([("lr".to_string(), TRUE_GAIN)]);
        truth.scheduled_w = vec![TRUE_SINK];
        let truth_traj = drive(&net, &ss, lat, lon, &x0, &truth);
        let state_row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let zone_series: HashMap<String, Vec<TimeSample>> = HashMap::from([(
            "lr".to_string(),
            data.hours
                .iter()
                .zip(&truth_traj)
                .map(|(&h, x)| TimeSample {
                    time: Utc.timestamp_opt(h * 3600, 0).single().unwrap(),
                    value: k_to_c(x[state_row]),
                })
                .collect(),
        )]);

        data.scheduled_w = vec![0.0];
        let fit = fit_gains(
            &net,
            &ss,
            lat,
            lon,
            &x0,
            &data,
            &zone_series,
            &loads,
            n_hours,
            local_offset,
        );

        let gain = fit.gains.get("lr").copied().unwrap_or(0.0);
        let sink = fit.scheduled_w[0];
        let gain_err = (gain - TRUE_GAIN).abs() / TRUE_GAIN;
        let sink_err = (sink - TRUE_SINK).abs() / TRUE_SINK;
        assert!(
            gain_err < 0.1 && sink_err < 0.1,
            "recovered gain {gain:.1} W (true {TRUE_GAIN}), sink {sink:.1} W (true {TRUE_SINK})"
        );
    }

    /// A **fixed-magnitude** load (`power_w` set) is a known input: it's applied as-is and the fit must
    /// not change it. The harder claim: in a zone with a fixed sink AND an unknown constant gain, the
    /// gain is recovered while the fixed sink stays at exactly its configured value.
    #[test]
    fn fit_keeps_fixed_load_and_recovers_gain_alongside() {
        let (net, ss) = one_zone();
        let local_offset = FixedOffset::east_opt(0).unwrap();
        let (lat, lon) = (Angle::new::<degree>(50.0), Angle::new::<degree>(14.0));

        const FIXED_SINK: f64 = 800.0;
        const TRUE_GAIN: f64 = 200.0;

        // The same windowed sink as the other tests, but with its magnitude CONFIGURED (`power_w`).
        let loads = [ScheduledLoad {
            zone: "lr".to_string(),
            label: "water heat-pump".to_string(),
            kind: LoadKind::Sink,
            power_w: Some(FIXED_SINK),
            sensor: None,
            power_factor: None,
            controllable: false,
            run_hours: None,
            windows: vec![LoadWindow {
                months: Vec::new(),
                start: "10:00".to_string(),
                end: "14:00".to_string(),
            }],
        }];
        let n_hours = 24 * 5;
        let data = synthetic_drive(0, n_hours, 8.0, &loads, local_offset);
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );

        // Truth: the fixed 800 W daytime sink AND a 200 W always-on gain, both in `lr`.
        let mut truth = data.clone();
        truth.internal_gain_w = HashMap::from([("lr".to_string(), TRUE_GAIN)]);
        truth.scheduled_w = vec![FIXED_SINK];
        let truth_traj = drive(&net, &ss, lat, lon, &x0, &truth);
        let state_row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let zone_series: HashMap<String, Vec<TimeSample>> = HashMap::from([(
            "lr".to_string(),
            data.hours
                .iter()
                .zip(&truth_traj)
                .map(|(&h, x)| TimeSample {
                    time: Utc.timestamp_opt(h * 3600, 0).single().unwrap(),
                    value: k_to_c(x[state_row]),
                })
                .collect(),
        )]);

        // The fit is handed the loads with the fixed magnitude; `data.scheduled_w` is irrelevant for a
        // fixed slot (the fit reads `power_w` and applies it itself).
        let fit = fit_gains(
            &net,
            &ss,
            lat,
            lon,
            &x0,
            &data,
            &zone_series,
            &loads,
            n_hours,
            local_offset,
        );

        // (a) the fixed sink is applied as-is and the fit does NOT change it — exactly the config value.
        assert_eq!(
            fit.scheduled_w[0], FIXED_SINK,
            "a fixed load must keep its configured magnitude"
        );
        // (b) the constant gain is recovered alongside it.
        let gain = fit.gains.get("lr").copied().unwrap_or(0.0);
        let gain_err = (gain - TRUE_GAIN).abs() / TRUE_GAIN;
        assert!(
            gain_err < 0.1,
            "recovered gain {gain:.1} W (true {TRUE_GAIN}) with the fixed sink held"
        );
    }

    /// A locator literal for a sensor-driven scheduled load (the actual backend is never read in these
    /// pure-drive tests — the measured series is injected into `DriveData.sensor_power_w` directly).
    fn sensor_locator() -> crate::source::SourceLocator {
        json5::from_str(
            r#"{ type: "influx", bucket: "loxone", measurement: "power", field: "hp_w" }"#,
        )
        .unwrap()
    }

    /// `drive` derives a sensor-driven load's flux from the **measured** power series:
    /// `kind_sign × P_elec[h] × power_factor`, still gated by the window/months — never from
    /// `scheduled_w`. The check: a sensor sink with constant draw `P` and factor `k` produces the same
    /// trajectory as a plain fitted sink at magnitude `P·k`; and outside its months the flux is 0
    /// (identical to a free-response drive with the load off).
    #[test]
    fn drive_uses_measured_sensor_power_times_factor() {
        let (net, ss) = one_zone();
        let local_offset = FixedOffset::east_opt(0).unwrap();
        let (lat, lon) = (Angle::new::<degree>(50.0), Angle::new::<degree>(14.0));
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );
        let state_row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let n_hours = 24 * 3;

        // A daytime sink in `lr`, active 10:00–14:00 only in summer months (June–August).
        let sensor_load = ScheduledLoad {
            zone: "lr".to_string(),
            label: "water heat-pump".to_string(),
            kind: LoadKind::Sink,
            power_w: None,
            sensor: Some(sensor_locator()),
            power_factor: Some(2.0),
            controllable: false,
            run_hours: None,
            windows: vec![LoadWindow {
                months: vec![6, 7, 8],
                start: "10:00".to_string(),
                end: "14:00".to_string(),
            }],
        };

        // Drive A: the sensor load with a constant measured draw of 400 W (× factor 2.0 ⇒ 800 W flux).
        const P_ELEC: f64 = 400.0;
        const FACTOR: f64 = 2.0;
        let mut data_sensor = synthetic_drive(
            0,
            n_hours,
            8.0,
            std::slice::from_ref(&sensor_load),
            local_offset,
        );
        data_sensor.sensor_power_w = vec![Some(vec![P_ELEC; n_hours])];
        let traj_sensor = drive(&net, &ss, lat, lon, &x0, &data_sensor);

        // Drive B: the SAME load as a plain fitted sink (no sensor) at magnitude P·factor = 800 W.
        let plain_load = ScheduledLoad {
            sensor: None,
            power_factor: None,
            ..sensor_load.clone()
        };
        let mut data_plain = synthetic_drive(0, n_hours, 8.0, &[plain_load], local_offset);
        data_plain.scheduled_w = vec![P_ELEC * FACTOR];
        let traj_plain = drive(&net, &ss, lat, lon, &x0, &data_plain);

        // The two trajectories must coincide: the measured-power path equals the magnitude path.
        for (a, b) in traj_sensor.iter().zip(&traj_plain) {
            assert!(
                (a[state_row] - b[state_row]).abs() < 1e-9,
                "sensor flux (P·factor) must equal the magnitude path"
            );
        }

        // grid hour 0 is unix-epoch (1970-01-01, a January = out of the June–August months), so the load
        // is inactive: the sensor trajectory must equal the free response (the load contributes nothing).
        let mut data_off = synthetic_drive(0, n_hours, 8.0, &[sensor_load], local_offset);
        data_off.sensor_power_w = vec![Some(vec![P_ELEC; n_hours])];
        let traj_off = drive(&net, &ss, lat, lon, &x0, &data_off);
        let mut data_free = synthetic_drive(0, n_hours, 8.0, &[], local_offset);
        data_free.sensor_power_w = Vec::new();
        let traj_free = drive(&net, &ss, lat, lon, &x0, &data_free);
        for (a, b) in traj_off.iter().zip(&traj_free) {
            assert!(
                (a[state_row] - b[state_row]).abs() < 1e-9,
                "outside its months a sensor load must contribute zero flux"
            );
        }
    }

    /// A sensor-driven load is a KNOWN input, not a fit candidate: `fit_gains` must not invent a
    /// magnitude for it (its `scheduled_w` slot stays at the seed). The measured flux is already in the
    /// baseline via the drive, so the fit explains only what's left — here, an unknown constant gain.
    #[test]
    fn fit_does_not_fit_a_sensor_driven_load() {
        let (net, ss) = one_zone();
        let local_offset = FixedOffset::east_opt(0).unwrap();
        let (lat, lon) = (Angle::new::<degree>(50.0), Angle::new::<degree>(14.0));
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0)
                .get::<uom::si::thermodynamic_temperature::kelvin>(),
        );
        let state_row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let n_hours = 24 * 5;

        // A daytime sensor-driven sink, active every day so the on/off shape is identifiable in-window.
        let loads = [ScheduledLoad {
            zone: "lr".to_string(),
            label: "water heat-pump".to_string(),
            kind: LoadKind::Sink,
            power_w: None,
            sensor: Some(sensor_locator()),
            power_factor: Some(1.0),
            controllable: false,
            run_hours: None,
            windows: vec![LoadWindow {
                months: Vec::new(),
                start: "10:00".to_string(),
                end: "14:00".to_string(),
            }],
        }];

        // Truth: a 250 W always-on gain AND the measured sink (600 W draw × factor 1.0), both in `lr`.
        const TRUE_GAIN: f64 = 250.0;
        const SENSOR_W: f64 = 600.0;
        let mut truth = synthetic_drive(0, n_hours, 8.0, &loads, local_offset);
        truth.sensor_power_w = vec![Some(vec![SENSOR_W; n_hours])];
        truth.internal_gain_w = HashMap::from([("lr".to_string(), TRUE_GAIN)]);
        let truth_traj = drive(&net, &ss, lat, lon, &x0, &truth);
        let zone_series: HashMap<String, Vec<TimeSample>> = HashMap::from([(
            "lr".to_string(),
            truth
                .hours
                .iter()
                .zip(&truth_traj)
                .map(|(&h, x)| TimeSample {
                    time: Utc.timestamp_opt(h * 3600, 0).single().unwrap(),
                    value: k_to_c(x[state_row]),
                })
                .collect(),
        )]);

        // The fit's `data` carries the same measured sensor series (as `load_active` would), the loads,
        // and no fitted gains; `scheduled_w` for the sensor slot is irrelevant (the drive uses the
        // measured series). The fit must recover the gain and leave the sensor slot untouched (0).
        let mut data = synthetic_drive(0, n_hours, 8.0, &loads, local_offset);
        data.sensor_power_w = vec![Some(vec![SENSOR_W; n_hours])];
        let fit = fit_gains(
            &net,
            &ss,
            lat,
            lon,
            &x0,
            &data,
            &zone_series,
            &loads,
            n_hours,
            local_offset,
        );

        // (a) the sensor-driven load is not a candidate — its magnitude slot stays at the 0 seed.
        assert_eq!(
            fit.scheduled_w[0], 0.0,
            "a sensor-driven load must not be assigned a fitted magnitude"
        );
        // (b) the always-on gain is still recovered (the measured sink is already in the baseline).
        let gain = fit.gains.get("lr").copied().unwrap_or(0.0);
        let gain_err = (gain - TRUE_GAIN).abs() / TRUE_GAIN;
        assert!(
            gain_err < 0.1,
            "recovered gain {gain:.1} W (true {TRUE_GAIN}) with the measured sink held"
        );
    }
}
