//! Steady-state Kalman filter / disturbance observer for the thermal state estimate.
//!
//! The classic estimator ([`crate::estimate::estimate_initial_state`]) drives the model open-loop
//! over history and then hard-overwrites each measured zone's air state with its latest reading —
//! measurements correct only the final instant, and only the air nodes. This filter instead folds
//! every hourly zone measurement into the estimate as it happens: predict one hour with the exact
//! same input physics as the open-loop drive (the shared [`crate::estimate::build_input`]), then
//! apply **sequential scalar measurement updates** — one per zone that actually has a sample that
//! hour — using per-zone gain columns derived from the steady-state Riccati solution. Missing or
//! stale samples simply skip that zone's update; no time-varying covariance, no per-pattern gain
//! rebuild, no matrix inversion at runtime.
//!
//! Optionally ([`EstimatorConfig::disturbance`]) the state is augmented with one **constant
//! disturbance flux per measured zone** (a random-walk W state injected at the zone's air node),
//! which makes the estimate offset-free under unmodelled steady gains. The disturbance corrects
//! the STATE only; it is never fed into the forward prediction.
//!
//! **The calibration invariant is untouched**: `validate::fit_gains` keeps consuming the pure
//! open-loop [`crate::estimate::drive`]; this filter is a separate consumer of the same
//! [`DriveData`] + [`crate::estimate::build_input`], so the two can never disagree on the plant.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use nalgebra::{DMatrix, DVector};
use uom::si::f64::Angle;

use crate::estimate::{build_input, hour_key, DriveData};
use crate::influxdb::TimeSample;
use crate::optimize::config::EstimatorConfig;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;

/// Innovations larger than this (K) are treated as sensor glitches and skipped — unless they
/// PERSIST ([`GATE_PERSISTENCE`] consecutive hours), which is a real shift (open window, wrong
/// seed) the filter must correct, not noise.
const INNOVATION_GATE_K: f64 = 5.0;
/// Consecutive over-gate innovations after which the gate yields (a persistent shift is real).
const GATE_PERSISTENCE: usize = 3;
/// Relative convergence tolerance for the steady-state Riccati iteration.
const RICCATI_TOL: f64 = 1e-9;
/// Iteration cap — the thermal system is stable and detectable, so convergence is typically a few
/// hundred iterations; hitting the cap means the noise setup is degenerate (warned, then used).
const RICCATI_MAX_ITERS: usize = 5000;

/// The startup-built filter: the (possibly disturbance-augmented) discrete plant plus one
/// steady-state gain column per measured zone. Build once per process (like the kernel cache) —
/// it depends only on the model, dt and the noise config, never on live data.
pub struct KalmanFilter {
    /// Augmented `Ad` (`n_aug × n_aug`); the top-left `n_x × n_x` block is the physical plant.
    ad: DMatrix<f64>,
    /// Physical `Bd` rows embedded in the augmented state (`n_aug × n_inputs`).
    bd: DMatrix<f64>,
    /// Physical state count (augmented disturbance states follow).
    n_x: usize,
    /// Per measured zone: `(zone, physical air-state row, steady-state gain column over x_aug)`.
    gains: Vec<(String, usize, DVector<f64>)>,
    /// Zones with a disturbance state, in augmented-state order (empty when disturbance is off).
    dist_zones: Vec<String>,
    /// Hard clamp on |disturbance| (W).
    max_disturbance_w: f64,
}

/// The filtered estimate: physical states plus the observer's per-zone disturbance flux.
pub struct KalmanEstimate {
    /// Physical state vector (K), same layout as the open-loop drive's.
    pub x: DVector<f64>,
    /// Physical state after each grid step (post-update), `trajectory[0] = x0` — the same
    /// convention as [`crate::estimate::drive`], so the backtest scorer consumes either.
    pub trajectory: Vec<DVector<f64>>,
    /// Estimated constant disturbance flux per zone (W, + heats); empty when the observer is off.
    pub disturbance_w: HashMap<String, f64>,
    /// Scalar measurement updates applied over the window.
    pub updates_applied: usize,
    /// Updates skipped by the innovation gate (glitch guard).
    pub innovations_gated: usize,
}

impl KalmanFilter {
    /// Build the filter for `measured_zones` (zones with a temperature sensor + a state row).
    /// Iterates the discrete Riccati with the same sequential per-zone update the runtime uses,
    /// so the cached gains are exactly the converged ones.
    pub fn build(
        net: &RcNetwork,
        ss: &StateSpace,
        cfg: &EstimatorConfig,
        measured_zones: &[String],
    ) -> Result<KalmanFilter> {
        cfg.validate()?;
        let disc = ss.discretize(3600.0);
        let n_x = ss.n_states();
        // Measured zones → (zone, state row); silently drop names without a sensor/state row —
        // the caller passes zones that produced data recently.
        let rows: Vec<(String, usize)> = measured_zones
            .iter()
            .filter_map(|z| {
                let node = net.zone_indices.get(z)?;
                Some((z.clone(), ss.state_index(*node)?))
            })
            .collect();
        ensure!(
            !rows.is_empty(),
            "kalman: no measured zone maps to a state row"
        );

        // Disturbance augmentation: one constant-flux state per measured zone, entering the
        // physical plant through the zone air node's Bd flux column.
        let (dist_zones, dist_cols): (Vec<String>, Vec<usize>) = if cfg.disturbance {
            rows.iter()
                .filter_map(|(z, _)| {
                    let node = net.zone_indices.get(z)?;
                    Some((z.clone(), ss.flux_input_column(*node)?))
                })
                .unzip()
        } else {
            (Vec::new(), Vec::new())
        };
        let n_d = dist_zones.len();
        let n_aug = n_x + n_d;

        // Ad_aug = [[Ad, Bd_dist_cols], [0, I]] — the standard discrete constant-disturbance model.
        let mut ad = DMatrix::<f64>::identity(n_aug, n_aug);
        ad.view_mut((0, 0), (n_x, n_x)).copy_from(&disc.ad);
        for (j, &col) in dist_cols.iter().enumerate() {
            ad.view_mut((0, n_x + j), (n_x, 1))
                .copy_from(&disc.bd.column(col).into_owned());
        }
        // Physical inputs drive only the physical rows.
        let mut bd = DMatrix::<f64>::zeros(n_aug, disc.bd.ncols());
        bd.view_mut((0, 0), (n_x, disc.bd.ncols()))
            .copy_from(&disc.bd);

        // Q: diagonal — air states, mass states, disturbance random walk.
        let air_rows: std::collections::HashSet<usize> = net
            .zone_indices
            .values()
            .filter_map(|&node| ss.state_index(node))
            .collect();
        let mut q = DMatrix::<f64>::zeros(n_aug, n_aug);
        for i in 0..n_x {
            let sigma = if air_rows.contains(&i) {
                cfg.sigma_air_k
            } else {
                cfg.sigma_mass_k
            };
            q[(i, i)] = sigma * sigma;
        }
        for j in 0..n_d {
            q[(n_x + j, n_x + j)] = cfg.sigma_disturbance_w * cfg.sigma_disturbance_w;
        }
        let r = cfg.sigma_meas_k * cfg.sigma_meas_k;

        // Steady-state Riccati by fixed-point iteration, using the SAME sequential scalar update
        // the runtime applies: per zone, K = P c / (cᵀP c + R), P ← (I − K cᵀ) P; then
        // P ← Ad P Adᵀ + Q. Symmetrize each round to keep numerical drift out.
        let mut p = q.clone();
        let mut converged = false;
        for _ in 0..RICCATI_MAX_ITERS {
            let mut p_upd = p.clone();
            for &(_, row) in rows.iter().map(|(z, r)| (z, r)).collect::<Vec<_>>().iter() {
                let c_p = p_upd.row(*row).transpose(); // P c (P symmetric)
                let s = p_upd[(*row, *row)] + r;
                let k = &c_p / s;
                // P ← P − k (c' P): rank-1 downdate.
                p_upd -= &k * c_p.transpose();
            }
            let mut p_next = &ad * p_upd * ad.transpose() + &q;
            // Symmetrize.
            p_next = (&p_next + p_next.transpose()) * 0.5;
            let diff = (&p_next - &p).norm();
            let scale = p.norm().max(1e-12);
            p = p_next;
            if diff / scale < RICCATI_TOL {
                converged = true;
                break;
            }
        }
        if !converged {
            eprintln!(
                "[kalman] Riccati did not converge in {RICCATI_MAX_ITERS} iterations — \
                 using the last iterate (check estimator sigmas)"
            );
        }

        // Per-zone gain columns from P∞. The runtime applies these updates SEQUENTIALLY to the same
        // `x` (see `filter`), so each zone's gain must come from the covariance *after* the previous
        // zones were folded in — not from the prior P∞ for all of them. Replay the same rank-1
        // downdate the Riccati iteration uses, in the same `rows` order: taking every gain from the
        // prior would re-count information shared through the strongly-correlated wall/air states,
        // making each gain after the first too large and over-trusting the sensors.
        let mut p_seq = p.clone();
        let gains = rows
            .iter()
            .map(|(zone, row)| {
                let c_p = p_seq.row(*row).transpose();
                let s = p_seq[(*row, *row)] + r;
                let k = &c_p / s;
                p_seq -= &k * c_p.transpose();
                (zone.clone(), *row, k)
            })
            .collect();

        Ok(KalmanFilter {
            ad,
            bd,
            n_x,
            gains,
            dist_zones,
            max_disturbance_w: cfg.max_disturbance_w,
        })
    }

    /// Run the filter over the drive window: predict each hour with the shared input physics,
    /// then apply the per-zone scalar updates where a measured sample exists for that grid hour.
    /// `measured` is each zone's hourly series (the same series `seed_state` returns).
    ///
    /// `updates_until_hour` is a HARD cutoff (exclusive, in `hour_key` units): no measurement
    /// update runs at or after it. The held-out backtest needs this because truncating `measured`
    /// alone does not hold — the last surviving sample has no successor, so the forward-fill below
    /// carries it `MEAS_FFILL_HOURS` past the cut and silently keeps correcting inside the window
    /// that is supposed to be pure open-loop. `None` for the live estimator (no cutoff).
    #[allow(clippy::too_many_arguments)] // the model, site, seed, window and measurements are all distinct
    pub fn filter(
        &self,
        net: &RcNetwork,
        ss: &StateSpace,
        latitude: Angle,
        longitude: Angle,
        x0: &DVector<f64>,
        data: &DriveData,
        measured: &HashMap<String, Vec<TimeSample>>,
        updates_until_hour: Option<i64>,
    ) -> KalmanEstimate {
        // Hour-keyed measurement lookup (measured hourly means are stop-stamped; grid step h
        // is covered by the sample at hours[h+1], the same convention as build_input). The house
        // pipeline stores a point only ON CHANGE (≥ ~0.1 K deadband on a 15-min poll), so a
        // stable room's silence is itself a measurement — "unchanged within the deadband" — and
        // is forward-filled up to [`MEAS_FFILL_HOURS`]; beyond that the sensor may genuinely be
        // dead and the hour is skipped (no update).
        const MEAS_FFILL_HOURS: i64 = 6;
        let by_hour: HashMap<&str, HashMap<i64, f64>> = measured
            .iter()
            .map(|(z, series)| {
                let mut m: HashMap<i64, f64> = HashMap::new();
                let mut sorted: Vec<(i64, f64)> = series
                    .iter()
                    .map(|s| (hour_key(s.time), s.value + 273.15))
                    .collect();
                sorted.sort_by_key(|&(h, _)| h);
                for (i, &(h, v)) in sorted.iter().enumerate() {
                    // Keep-FIRST on an hour collision, like every other reader: the trailing
                    // partial `stop=now()` window shares the completed hour's key, and taking the
                    // later (partial) mean would drive the final update — the one that produces the
                    // seed state — from a fraction of an hour.
                    m.entry(h).or_insert(v);
                    // Fill forward until the next real sample or the freshness bound.
                    let until = sorted
                        .get(i + 1)
                        .map(|&(nh, _)| nh)
                        .unwrap_or(h + MEAS_FFILL_HOURS + 1)
                        .min(h + MEAS_FFILL_HOURS + 1);
                    for hh in (h + 1)..until {
                        m.entry(hh).or_insert(v);
                    }
                }
                (z.as_str(), m)
            })
            .collect();

        let n_aug = self.n_x + self.dist_zones.len();
        let mut x = DVector::<f64>::zeros(n_aug);
        x.rows_mut(0, self.n_x).copy_from(x0);
        let mut trajectory = Vec::with_capacity(data.grid_times.len());
        trajectory.push(x0.clone());
        let mut updates_applied = 0usize;
        let mut innovations_gated = 0usize;
        let mut consecutive_gated: HashMap<&str, usize> = HashMap::new();

        for h in 0..data.grid_times.len().saturating_sub(1) {
            let u = build_input(net, ss, latitude, longitude, data, h);
            x = &self.ad * &x + &self.bd * &u;
            let key = data.hours.get(h + 1).copied().unwrap_or_default();
            // Past the held-out cutoff this is a pure open-loop roll (see `updates_until_hour`).
            if updates_until_hour.is_some_and(|cut| key >= cut) {
                trajectory.push(x.rows(0, self.n_x).into_owned());
                continue;
            }
            for (zone, row, gain) in &self.gains {
                let Some(&y) = by_hour.get(zone.as_str()).and_then(|m| m.get(&key)) else {
                    continue;
                };
                let innovation = y - x[*row];
                if innovation.abs() > INNOVATION_GATE_K {
                    let n = consecutive_gated.entry(zone.as_str()).or_insert(0);
                    *n += 1;
                    if *n < GATE_PERSISTENCE {
                        innovations_gated += 1;
                        continue;
                    }
                    // Persistent — a real shift, not a glitch; let the update through.
                } else {
                    consecutive_gated.insert(zone.as_str(), 0);
                }
                x += gain * innovation;
                updates_applied += 1;
            }
            // Clamp at the END of the step, not just after the prediction: the gain column spans
            // the AUGMENTED state, so a measurement update moves the disturbance rows too — and
            // since each step ends on an update, a post-prediction-only clamp let the harvested
            // `disturbance_w` (and `/api/state`) exceed the configured `max_disturbance_w`.
            for j in 0..self.dist_zones.len() {
                let d = &mut x[self.n_x + j];
                *d = d.clamp(-self.max_disturbance_w, self.max_disturbance_w);
            }
            trajectory.push(x.rows(0, self.n_x).into_owned());
        }

        let disturbance_w = self
            .dist_zones
            .iter()
            .enumerate()
            .map(|(j, z)| (z.clone(), x[self.n_x + j]))
            .collect();
        KalmanEstimate {
            x: x.rows(0, self.n_x).into_owned(),
            trajectory,
            disturbance_w,
            updates_applied,
            innovations_gated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::drive;
    use chrono::{Duration, TimeZone, Utc};
    use uom::si::angle::degree;

    /// A tiny 2-node house: one zone with an interior mass layer, outside boundary.
    fn toy() -> (RcNetwork, StateSpace) {
        let model = crate::model::Model::from_json(
            r#"{
                materials: {
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.1 },
                    ] },
                },
                zones: { room: { volume: 50 } },
                boundaries: [
                    { boundary_type: "wall", zones: ["room", "outside"], area: 30 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    /// Two zones sharing a thin interior wall — their air/mass states are strongly correlated, so
    /// the sequential-vs-prior gain distinction is measurable.
    fn toy_two_zone() -> (RcNetwork, StateSpace) {
        let model = crate::model::Model::from_json(
            r#"{
                materials: {
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.1 },
                    ] },
                    partition: { layers: [ { material: "concrete", thickness: 0.05 } ] },
                },
                zones: { room: { volume: 50 }, room_b: { volume: 50 } },
                boundaries: [
                    { boundary_type: "wall", zones: ["room", "outside"], area: 30 },
                    { boundary_type: "wall", zones: ["room_b", "outside"], area: 30 },
                    { boundary_type: "partition", zones: ["room", "room_b"], area: 12 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    /// REGRESSION: `updates_until_hour` must stop measurement updates DEAD at the cutoff. The
    /// held-out backtest relies on it: truncating the measured map alone leaves the last surviving
    /// sample without a successor, so the forward-fill carries it `MEAS_FFILL_HOURS` past the cut
    /// and keeps correcting inside the window that is supposed to be pure open-loop.
    #[test]
    fn updates_stop_at_the_held_out_cutoff() {
        let (net, ss) = toy();
        let data = drive_data(48, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 20.0);
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &["room".to_string()]).unwrap();
        // One sample early in the window; the forward-fill extends it MEAS_FFILL_HOURS(6) further,
        // so the cutoff must land INSIDE that filled span to prove it truncates the leak.
        let cut_idx = 5usize;
        let cutoff = data.hours[cut_idx];
        let series = vec![TimeSample {
            time: data.grid_times[2],
            value: 30.0, // far from the seed, so any update is obvious
        }];
        let measured = HashMap::from([("room".to_string(), series)]);

        let open = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
            &measured,
            Some(cutoff),
        );
        // Same run with NO cutoff must apply strictly more updates (the forward-filled hours).
        let leaky = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
            &measured,
            None,
        );
        assert!(
            open.updates_applied < leaky.updates_applied,
            "cutoff applied {} updates, uncapped {} — the cutoff is not holding",
            open.updates_applied,
            leaky.updates_applied
        );
    }

    /// REGRESSION: with more than one measured zone the gains are applied sequentially to the same
    /// `x`, so each must come from the covariance AFTER the previous zone's update. Extracting them
    /// all from the prior P∞ (the original bug) re-counts information shared through the coupled
    /// states and leaves the later gains too large. Assert the second zone's self-gain is strictly
    /// below what the prior-based formula would give — the single-zone tests can't see this.
    #[test]
    fn multi_zone_gains_are_sequentially_downdated_not_prior_based() {
        let (net, ss) = toy_two_zone();
        let zones = ["room".to_string(), "room_b".to_string()];
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &zones).unwrap();
        assert_eq!(f.gains.len(), 2);
        // Every gain stays a physically sane blend and finite.
        for (_, row, gain) in &f.gains {
            assert!(gain[*row] > 0.0 && gain[*row] < 1.0, "{}", gain[*row]);
            assert!(gain.iter().all(|g| g.is_finite()));
        }
        // The second zone's update sees a covariance already reduced by the first zone's update, so
        // its self-gain must be strictly smaller than the first zone's (identical geometry here, so
        // a prior-based extraction would make them equal).
        let (_, row0, g0) = &f.gains[0];
        let (_, row1, g1) = &f.gains[1];
        assert!(
            g1[*row1] < g0[*row0] - 1e-9,
            "second gain {} not downdated below the first {}",
            g1[*row1],
            g0[*row0]
        );
    }

    fn drive_data(hours: usize, outside_c: f64) -> DriveData {
        let t0 = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        DriveData {
            grid_times: (0..hours).map(|h| t0 + Duration::hours(h as i64)).collect(),
            hours: (0..hours)
                .map(|h| {
                    (t0 + Duration::hours(h as i64))
                        .timestamp()
                        .div_euclid(3600)
                })
                .collect(),
            outside_c: vec![outside_c; hours],
            ground_c: 10.0,
            cloud: vec![1.0; hours], // overcast: no solar, pure conduction
            solar: Vec::new(),
            heating_kw: HashMap::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            sensor_power_w: Vec::new(),
            local_offset: chrono::FixedOffset::east_opt(0).unwrap(),
        }
    }

    fn cfg(disturbance: bool) -> EstimatorConfig {
        EstimatorConfig {
            disturbance,
            ..Default::default()
        }
    }

    fn measured_from_truth(
        data: &DriveData,
        truth: &[DVector<f64>],
        row: usize,
        noise: &[f64],
    ) -> HashMap<String, Vec<TimeSample>> {
        let series = data
            .grid_times
            .iter()
            .zip(truth)
            .enumerate()
            .map(|(i, (t, x))| TimeSample {
                time: *t,
                value: x[row] - 273.15 + noise.get(i).copied().unwrap_or(0.0),
            })
            .collect();
        HashMap::from([("room".to_string(), series)])
    }

    #[test]
    fn riccati_gain_is_sane_on_the_toy_model() {
        let (net, ss) = toy();
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &["room".to_string()]).unwrap();
        let (_, row, gain) = &f.gains[0];
        // The measured row's own gain must be a real blend (0, 1); other rows small but real.
        assert!(gain[*row] > 0.0 && gain[*row] < 1.0, "{}", gain[*row]);
        assert!(gain.iter().all(|g| g.is_finite()));
    }

    #[test]
    fn filter_beats_open_loop_from_a_wrong_seed() {
        let (net, ss) = toy();
        let data = drive_data(48, 0.0);
        // Truth: start warm at 24 °C everywhere and cool toward 0 °C outside.
        let x_true0 = DVector::from_element(ss.n_states(), 273.15 + 24.0);
        let truth = drive(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x_true0,
            &data,
        );
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        // Wrong seed: 10 K cold everywhere.
        let x_wrong = DVector::from_element(ss.n_states(), 273.15 + 14.0);
        let open_loop = drive(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x_wrong,
            &data,
        );
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &["room".to_string()]).unwrap();
        let est = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x_wrong,
            &data,
            &measured,
            None,
        );
        let err = |x: &DVector<f64>| (x - truth.last().unwrap()).norm();
        assert!(
            err(&est.x) < err(open_loop.last().unwrap()) * 0.5,
            "filter {:.3} K vs open-loop {:.3} K",
            err(&est.x),
            err(open_loop.last().unwrap())
        );
        assert!(est.updates_applied > 0);
    }

    #[test]
    fn missing_hours_are_skipped_not_fatal() {
        let (net, ss) = toy();
        let data = drive_data(24, 5.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 20.0);
        let truth = drive(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
        );
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let mut measured = measured_from_truth(&data, &truth, zone_row, &[]);
        // Blank the middle half of the samples.
        measured.get_mut("room").unwrap().drain(6..18);
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &["room".to_string()]).unwrap();
        let est = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
            &measured,
            None,
        );
        assert!(est.x.iter().all(|v| v.is_finite()));
        assert!(est.updates_applied > 0 && est.updates_applied < 23);
    }

    #[test]
    fn innovation_gate_ignores_a_glitch() {
        let (net, ss) = toy();
        let data = drive_data(24, 5.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 20.0);
        let truth = drive(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
        );
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let mut measured = measured_from_truth(&data, &truth, zone_row, &[]);
        measured.get_mut("room").unwrap()[12].value += 30.0; // a 30 K spike
        let f = KalmanFilter::build(&net, &ss, &cfg(false), &["room".to_string()]).unwrap();
        let est = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
            &measured,
            None,
        );
        assert_eq!(est.innovations_gated, 1);
        // The glitch must not have dragged the estimate: still near the truth.
        assert!((est.x[zone_row] - truth.last().unwrap()[zone_row]).abs() < 0.5);
    }

    #[test]
    fn disturbance_observer_recovers_an_injected_flux() {
        let (net, ss) = toy();
        let data = drive_data(96, 10.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 20.0);
        // Truth: the same drive PLUS a constant +200 W at the zone air node the model doesn't know.
        let zone_node = net.zone_indices["room"];
        let zone_row = ss.state_index(zone_node).unwrap();
        let flux_col = ss.flux_input_column(zone_node).unwrap();
        let disc = ss.discretize(3600.0);
        let mut x = x0.clone();
        let mut truth = vec![x.clone()];
        for h in 0..data.grid_times.len() - 1 {
            let mut u = build_input(
                &net,
                &ss,
                Angle::new::<degree>(49.0),
                Angle::new::<degree>(14.5),
                &data,
                h,
            );
            u[flux_col] += 200.0;
            x = ss.step(&disc, &x, &u);
            truth.push(x.clone());
        }
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &cfg(true), &["room".to_string()]).unwrap();
        let est = f.filter(
            &net,
            &ss,
            Angle::new::<degree>(49.0),
            Angle::new::<degree>(14.5),
            &x0,
            &data,
            &measured,
            None,
        );
        let d = est.disturbance_w["room"];
        assert!(
            (d - 200.0).abs() < 40.0,
            "disturbance estimate {d:.0} W should approach the injected 200 W"
        );
        assert!(d.abs() <= 500.0 + 1e-9, "clamp holds");
        // Offset-free: the air estimate tracks the (disturbed) truth closely.
        assert!((est.x[zone_row] - truth.last().unwrap()[zone_row]).abs() < 0.3);
    }
}
