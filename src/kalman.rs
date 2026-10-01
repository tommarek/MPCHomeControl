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

use crate::estimate::{build_input, build_input_parts, hour_key, DriveData};
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
#[derive(Clone)]
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
    /// Zones with BOTH a measured air-state row and a static solar path (any window/opaque
    /// surface attributed to them): `(zone, physical air-state row, sequential s_m)`. These are
    /// the columns of the runtime δ/p/S arrays in [`Self::filter`]. Empty unless
    /// [`EstimatorConfig::solar_scale`].
    solar_zones: Vec<(String, usize, f64)>,
    solar_scale: bool,
    solar_scale_prior_sigma: f64,
    sigma_solar_scale: f64,
    solar_scale_min: f64,
    solar_scale_max: f64,
    solar_scale_min_wm2: f64,
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
    /// Per-zone solar-gain scale `s_z = 1 + δ_z`; empty when the observer is off or no zone has
    /// both a sensor and a solar path.
    pub solar_scale: HashMap<String, f64>,
    /// Scalar δ updates actually applied (gated + past warm-up), across all zones/hours.
    pub solar_scale_updates: usize,
    /// `δ` per solar zone after each grid step, `solar_scale_trace[0]` = the flat seed (zeros) —
    /// parallel to [`Self::trajectory`], zones in [`Self::solar_scale_zones`] order. Empty when
    /// the scale is off. The replay's "does δ move on cloudy days" check reads this.
    pub solar_scale_trace: Vec<Vec<f64>>,
    pub solar_scale_zones: Vec<String>,
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
        let gains_with_s: Vec<(String, usize, DVector<f64>, f64)> = rows
            .iter()
            .map(|(zone, row)| {
                let c_p = p_seq.row(*row).transpose();
                let s = p_seq[(*row, *row)] + r;
                let k = &c_p / s;
                p_seq -= &k * c_p.transpose();
                (zone.clone(), *row, k, s)
            })
            .collect();
        let gains = gains_with_s
            .iter()
            .map(|(z, row, k, _)| (z.clone(), *row, k.clone()))
            .collect();

        // Zones with a sensor row AND any STATIC solar path (a window or opaque exterior surface
        // attributed to them) — the columns of the runtime solar-scale δ/p/S arrays. Computed
        // regardless of `cfg.solar_scale` (cheap); `filter` only ever reads it when the flag is on.
        let solar_zones: Vec<(String, usize, f64)> = if cfg.solar_scale {
            gains_with_s
                .iter()
                .filter(|(zone, ..)| {
                    net.window_surfaces.iter().any(|w| &w.zone == zone)
                        || net.solar_surfaces.iter().any(|s| &s.zone == zone)
                })
                .map(|(zone, row, _, s)| (zone.clone(), *row, *s))
                .collect()
        } else {
            Vec::new()
        };

        Ok(KalmanFilter {
            ad,
            bd,
            n_x,
            gains,
            dist_zones,
            max_disturbance_w: cfg.max_disturbance_w,
            solar_zones,
            solar_scale: cfg.solar_scale,
            solar_scale_prior_sigma: cfg.solar_scale_prior_sigma,
            sigma_solar_scale: cfg.sigma_solar_scale,
            solar_scale_min: cfg.solar_scale_min,
            solar_scale_max: cfg.solar_scale_max,
            solar_scale_min_wm2: cfg.solar_scale_min_wm2,
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

        // Solar-gain-scale runtime state (Friedland two-stage bias filter, kept entirely OUTSIDE
        // the Riccati-built `ad`/`gains` — see the module doc / `EstimatorConfig::solar_scale`): a
        // flat seed every call (the filter keeps no state between ticks), one column per zone in
        // `self.solar_zones`. Zero-sized — and every loop below a no-op — when the flag is off, so
        // the predict/update math literally does not run in that case.
        const SOLAR_SCALE_WARMUP_HOURS: usize = 24;
        let n_solar = self.solar_zones.len();
        let mut delta = vec![0.0_f64; n_solar];
        let mut p_solar =
            vec![self.solar_scale_prior_sigma * self.solar_scale_prior_sigma; n_solar];
        let mut s_mat = DMatrix::<f64>::zeros(n_aug, n_solar);
        let mut solar_scale_updates = 0usize;
        let mut solar_scale_trace = Vec::with_capacity(if n_solar > 0 {
            data.grid_times.len()
        } else {
            0
        });
        if n_solar > 0 {
            solar_scale_trace.push(delta.clone());
        }
        let solar_col: HashMap<&str, usize> = self
            .solar_zones
            .iter()
            .enumerate()
            .map(|(i, (zone, ..))| (zone.as_str(), i))
            .collect();
        let delta_bounds = (self.solar_scale_min - 1.0, self.solar_scale_max - 1.0);

        for h in 0..data.grid_times.len().saturating_sub(1) {
            let (u, solar_entries, gate_wm2) = if self.solar_scale {
                let (u, entries, gate) = build_input_parts(net, ss, latitude, longitude, data, h);
                (u, Some(entries), Some(gate))
            } else {
                (
                    build_input(net, ss, latitude, longitude, data, h),
                    None,
                    None,
                )
            };
            x = &self.ad * &x + &self.bd * &u;

            if self.solar_scale {
                let entries = solar_entries.as_ref().expect("set above when solar_scale");
                let gate = gate_wm2.as_ref().expect("set above when solar_scale");
                // G = Bd · (per-zone solar input vectors), one column per solar zone; the
                // sensitivities advance as ONE product `S ← Ad S + G` (the dominant cost of the
                // scale — per-column matvecs were 2× slower on the real ~540-state model).
                let mut g_mat = DMatrix::<f64>::zeros(n_aug, n_solar);
                for (i, (zone, _, _)) in self.solar_zones.iter().enumerate() {
                    if let Some(zone_entries) = entries.get(zone.as_str()) {
                        let mut g_z = g_mat.column_mut(i);
                        for &(col, watts) in zone_entries {
                            g_z.axpy(watts, &self.bd.column(col), 1.0);
                        }
                    }
                    let gated =
                        gate.get(zone.as_str()).copied().unwrap_or(0.0) >= self.solar_scale_min_wm2;
                    if gated {
                        p_solar[i] += self.sigma_solar_scale * self.sigma_solar_scale;
                    }
                }
                let delta_vec = DVector::from_column_slice(&delta);
                x += &g_mat * &delta_vec;
                s_mat = &self.ad * &s_mat + &g_mat;
            }

            let key = data.hours.get(h + 1).copied().unwrap_or_default();
            // Past the held-out cutoff this is a pure open-loop roll (see `updates_until_hour`) —
            // the δ-scaled prediction above keeps applying (that IS the forward prediction); only
            // the measurement updates below stop.
            if updates_until_hour.is_some_and(|cut| key >= cut) {
                trajectory.push(x.rows(0, self.n_x).into_owned());
                if n_solar > 0 {
                    solar_scale_trace.push(delta.clone());
                }
                continue;
            }
            for (zone, row, gain) in &self.gains {
                let Some(&y) = by_hour.get(zone.as_str()).and_then(|m| m.get(&key)) else {
                    continue;
                };
                let mut innovation = y - x[*row];
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

                // Solar-scale update: zone m's OWN δ, from the SAME (gated, post-warm-up)
                // innovation the physical update below uses — see `KalmanFilter::filter`'s doc /
                // research.md's two-stage derivation. Skipped whenever the physical update above
                // was (the `continue`s above already left this code), when the zone has no solar
                // path, when its irradiance is below the gate, or during the first
                // `SOLAR_SCALE_WARMUP_HOURS` of the run (the seed transient).
                if self.solar_scale {
                    if let Some(&col) = solar_col.get(zone.as_str()) {
                        let gated = gate_wm2
                            .as_ref()
                            .and_then(|g| g.get(zone.as_str()))
                            .copied()
                            .unwrap_or(0.0)
                            >= self.solar_scale_min_wm2;
                        if gated && h >= SOLAR_SCALE_WARMUP_HOURS {
                            let h_hat = s_mat[(*row, col)];
                            let p = p_solar[col];
                            let s_m = self.solar_zones[col].2;
                            let denom = h_hat * h_hat * p + s_m;
                            if denom.abs() > 1e-12 {
                                let k = p * h_hat / denom;
                                let (min_delta, max_delta) = delta_bounds;
                                let delta_prime =
                                    (delta[col] + k * innovation).clamp(min_delta, max_delta);
                                let delta_change = delta_prime - delta[col];
                                delta[col] = delta_prime;
                                p_solar[col] = (1.0 - k * h_hat) * p;
                                for r in 0..n_aug {
                                    x[r] += s_mat[(r, col)] * delta_change;
                                }
                                innovation -= h_hat * delta_change;
                                solar_scale_updates += 1;
                            }
                        }
                    }
                }

                x += gain * innovation;
                updates_applied += 1;

                if self.solar_scale {
                    // The physical update also corrects the δ-sensitivity directions: downdate
                    // EVERY zone's S column by this zone's physical gain (Friedland's
                    // `S -= K_m · S[r_m, :]`), not just the zone just updated.
                    for i in 0..n_solar {
                        let factor = s_mat[(*row, i)];
                        if factor != 0.0 {
                            for r in 0..n_aug {
                                s_mat[(r, i)] -= gain[r] * factor;
                            }
                        }
                    }
                }
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
            if n_solar > 0 {
                solar_scale_trace.push(delta.clone());
            }
        }

        let solar_scale = self
            .solar_zones
            .iter()
            .enumerate()
            .map(|(i, (zone, ..))| (zone.clone(), 1.0 + delta[i]))
            .collect();

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
            solar_scale,
            solar_scale_updates,
            solar_scale_trace,
            solar_scale_zones: self.solar_zones.iter().map(|(z, ..)| z.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::drive;
    use chrono::{DateTime, Duration, TimeZone, Utc};
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

    /// Acceptance 6 (offset-free MPC): a constant unmodelled loss on one zone must not bias the
    /// FORWARD 24 h forecast once the observer's recovered disturbance is folded back in as an
    /// extra constant flux (exactly what `app::current_plan` now does via
    /// `ForecastContext.internal_gain_w`, after the live gain re-fit). Error must stay small and
    /// flat with lead — not grow — which is the whole point of carrying the disturbance forward
    /// instead of dropping it after estimation.
    #[test]
    fn disturbance_correction_keeps_the_24h_forecast_on_the_true_trajectory() {
        let (net, ss) = toy();
        let data = drive_data(96, 10.0); // flat outside temp/cloud, as the recovery test
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 20.0);
        let zone_node = net.zone_indices["room"];
        let zone_row = ss.state_index(zone_node).unwrap();
        let flux_col = ss.flux_input_column(zone_node).unwrap();
        let disc = ss.discretize(3600.0);
        let angle = |d: f64| Angle::new::<degree>(d);

        // TRUE trajectory: 96 h of history PLUS a forward 24 h horizon, with a constant -200 W
        // unmodelled LOSS (an open window / draught the physics model has no source for) applied
        // the entire way — exactly the "constant unmodelled loss on a zone" the criterion asks for.
        const LOSS_W: f64 = -200.0;
        let last_data_h = data.grid_times.len() - 2; // build_input needs data[h+1]; hold flat past this
        let total_steps = data.grid_times.len() + 24 - 1;
        let mut x = x0.clone();
        let mut truth = vec![x.clone()];
        for h in 0..total_steps {
            let mut u = build_input(
                &net,
                &ss,
                angle(49.0),
                angle(14.5),
                &data,
                h.min(last_data_h),
            );
            u[flux_col] += LOSS_W;
            x = ss.step(&disc, &x, &u);
            truth.push(x.clone());
        }

        // Estimate x0 + the disturbance from the HISTORY portion only (the estimator never sees
        // the future) — same shape as `disturbance_observer_recovers_an_injected_flux`.
        let history_truth = &truth[..data.grid_times.len()];
        let measured = measured_from_truth(&data, history_truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &cfg(true), &["room".to_string()]).unwrap();
        let est = f.filter(
            &net,
            &ss,
            angle(49.0),
            angle(14.5),
            &x0,
            &data,
            &measured,
            None,
        );
        let recovered_w = est.disturbance_w["room"];

        // Forward 24 h from the estimated "now" state: UNCORRECTED (today's dropped-on-the-floor
        // behaviour — the forecast has no idea about the loss) vs CORRECTED (the estimated
        // disturbance folded back in as a constant extra flux every step).
        let horizon_start = data.grid_times.len() - 1;
        let mut uncorrected = est.x.clone();
        let mut corrected = est.x.clone();
        let mut corrected_err_k = Vec::with_capacity(24);
        let mut uncorrected_err_k = Vec::with_capacity(24);
        for h in 0..24 {
            let u_plain = build_input(
                &net,
                &ss,
                angle(49.0),
                angle(14.5),
                &data,
                (horizon_start + h).min(last_data_h),
            );
            uncorrected = ss.step(&disc, &uncorrected, &u_plain);
            let mut u_corr = u_plain.clone();
            u_corr[flux_col] += recovered_w;
            corrected = ss.step(&disc, &corrected, &u_corr);

            let true_t = truth[horizon_start + 1 + h][zone_row];
            corrected_err_k.push((corrected[zone_row] - true_t).abs());
            uncorrected_err_k.push((uncorrected[zone_row] - true_t).abs());
        }

        assert!(
            corrected_err_k.iter().all(|&e| e < 0.3),
            "corrected 24h forecast must stay within 0.3 K of truth at every lead: {corrected_err_k:?}"
        );
        // No growth with lead: the error late in the horizon is no worse than early on.
        assert!(
            corrected_err_k[23] < corrected_err_k[0] + 0.1,
            "corrected error grew with lead: first {:.3} K, last {:.3} K",
            corrected_err_k[0],
            corrected_err_k[23]
        );
        // The correction must matter: the uncorrected forecast drifts measurably worse by 24h.
        assert!(
            uncorrected_err_k[23] > corrected_err_k[23] + 0.1,
            "uncorrected {:.3} K should be well behind corrected {:.3} K by 24h",
            uncorrected_err_k[23],
            corrected_err_k[23]
        );
    }

    // --- Acceptance 1: the per-zone solar-gain scale -----------------------------------------

    /// A one-zone house with a south window (`Simple` boundary with `g`/`azimuth`/`angle`) — the
    /// only solar path, so its flux entries are unambiguous for the truth-scaling tests below.
    fn toy_with_window() -> (RcNetwork, StateSpace) {
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
                    window: { u: 1.2, g: 0.6 },
                },
                zones: { room: { volume: 50 } },
                boundaries: [
                    { boundary_type: "wall", zones: ["room", "outside"], area: 25 },
                    { boundary_type: "window", zones: ["room", "outside"], area: 5, azimuth: 180, angle: 90 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    /// Like [`drive_data`] but starting at an arbitrary UTC instant (mid-June for a real sun
    /// path) with a configurable cloud fraction.
    fn drive_data_dated(
        start: DateTime<Utc>,
        hours: usize,
        outside_c: f64,
        cloud: f64,
    ) -> DriveData {
        DriveData {
            grid_times: (0..hours)
                .map(|h| start + Duration::hours(h as i64))
                .collect(),
            hours: (0..hours)
                .map(|h| {
                    (start + Duration::hours(h as i64))
                        .timestamp()
                        .div_euclid(3600)
                })
                .collect(),
            outside_c: vec![outside_c; hours],
            ground_c: 10.0,
            cloud: vec![cloud; hours],
            solar: Vec::new(),
            heating_kw: HashMap::new(),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            sensor_power_w: Vec::new(),
            local_offset: chrono::FixedOffset::east_opt(0).unwrap(),
        }
    }

    fn solar_cfg(min_wm2: f64) -> EstimatorConfig {
        EstimatorConfig {
            solar_scale: true,
            solar_scale_min_wm2: min_wm2,
            ..Default::default()
        }
    }

    /// Roll `data` forward with the window's solar flux scaled by `truth_scale` relative to the
    /// MODELLED (nominal `g`) physics everything else uses — the "true window g is `truth_scale`×
    /// the model's" fixture the acceptance tests need. Only "room"'s solar entries are touched
    /// (this toy house's only solar path), via [`build_input_parts`]'s UNscaled entries.
    fn drive_with_scaled_window(
        net: &RcNetwork,
        ss: &StateSpace,
        latitude: Angle,
        longitude: Angle,
        x0: &DVector<f64>,
        data: &DriveData,
        truth_scale: f64,
    ) -> Vec<DVector<f64>> {
        drive_with_scaled_window_and_night_flux(
            net,
            ss,
            latitude,
            longitude,
            x0,
            data,
            truth_scale,
            0.0,
        )
    }

    /// [`drive_with_scaled_window`] plus an unmodelled `night_w` at the room's air node over
    /// 15:00–02:00 UTC (an evening fireplace the model has no source for) — the evening/night
    /// error the solar scale must NOT learn from.
    #[allow(clippy::too_many_arguments)]
    fn drive_with_scaled_window_and_night_flux(
        net: &RcNetwork,
        ss: &StateSpace,
        latitude: Angle,
        longitude: Angle,
        x0: &DVector<f64>,
        data: &DriveData,
        truth_scale: f64,
        night_w: f64,
    ) -> Vec<DVector<f64>> {
        use chrono::Timelike;
        let disc = ss.discretize(3600.0);
        let air_col = ss.flux_input_column(net.zone_indices["room"]).unwrap();
        let mut x = x0.clone();
        let mut truth = vec![x.clone()];
        for h in 0..data.grid_times.len().saturating_sub(1) {
            let (u, entries, _gate) = build_input_parts(net, ss, latitude, longitude, data, h);
            let mut u_true = u;
            if let Some(room) = entries.get("room") {
                for &(col, watts) in room {
                    u_true[col] += (truth_scale - 1.0) * watts;
                }
            }
            let hour = data.grid_times[h].hour();
            if !(2..15).contains(&hour) {
                u_true[air_col] += night_w;
            }
            x = ss.step(&disc, &x, &u_true);
            truth.push(x.clone());
        }
        truth
    }

    #[test]
    fn solar_scale_recovers_a_known_window_g_error_over_two_sunny_days() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 96, 15.0, 0.0); // 4 clear mid-June days, flat 15 °C outside
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive_with_scaled_window(&net, &ss, lat, lon, &x0, &data, 0.7);

        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let est = f.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);

        let s = est.solar_scale["room"];
        assert!(
            (s - 0.7).abs() < 0.1,
            "recovered solar scale {s:.3} should be within ±0.1 of the true 0.7"
        );
    }

    /// The gate is what keeps the scale honest after dusk: ≈70 % of window solar sits in the
    /// slab, so the update's sensitivity stays nonzero into the evening, and an unmodelled
    /// EVENING flux (a fireplace) would otherwise be charged to the window. Two cutoffs on
    /// evening 2 (steps 15:00–18:00 UTC — the sun is in the west/north-west, so the south window
    /// sees only diffuse light: 75 → 15 W/m², below the 100 W/m² gate but not zero) with the
    /// fireplace on between them: with the gate the scale does not move; with the gate
    /// effectively open it does.
    #[test]
    fn solar_scale_frozen_after_dusk_only_because_of_the_gate() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 72, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth =
            drive_with_scaled_window_and_night_flux(&net, &ss, lat, lon, &x0, &data, 0.7, 400.0);
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        // Cutoff keys 2026-06-16T16:00Z and 19:00Z: the steps whose updates fall between them
        // are h = 39..41 (a step's update uses the sample stamped at hours[h + 1]).
        let (idx_a, idx_b) = (40usize, 43usize);
        let (cutoff_a, cutoff_b) = (data.hours[idx_a], data.hours[idx_b]);
        // The fixture must put those steps in the dusk band the test is about: dim but lit.
        for h in (idx_a - 1)..(idx_b - 1) {
            let (_, _, gate) = build_input_parts(&net, &ss, lat, lon, &data, h);
            let wm2 = gate["room"];
            assert!(
                wm2 > 1e-6 && wm2 < 100.0,
                "hour {h} should be dusk (0 < {wm2:.1} W/m² < 100)"
            );
        }

        let gated =
            KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let est_a = gated.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff_a));
        let est_b = gated.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff_b));
        assert!(
            est_a.solar_scale_updates > 0,
            "the scale must have learned during the daylight before the cutoff"
        );
        assert!(
            (est_a.solar_scale["room"] - 0.7).abs() < 0.15,
            "learned {:.3}, expected near the true 0.7",
            est_a.solar_scale["room"]
        );
        assert_eq!(
            est_a.solar_scale["room"], est_b.solar_scale["room"],
            "no gated hour lies between the two dusk cutoffs — δ must not have moved"
        );

        let open = KalmanFilter::build(&net, &ss, &solar_cfg(1e-6), &["room".to_string()]).unwrap();
        let open_a = open.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff_a));
        let open_b = open.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff_b));
        let moved = (open_b.solar_scale["room"] - open_a.solar_scale["room"]).abs();
        assert!(
            moved > 0.05,
            "with the gate effectively open the 400 W fireplace must be charged to the window \
             (moved {moved:.4}) — the gate is load-bearing"
        );
    }

    /// Two zones, two windows, two different true scales: each zone's δ must recover ITS OWN
    /// error (0.6 and 1.3), which a column/zone mix-up in the sensitivity bookkeeping or a
    /// cross-zone leak in the per-measurement downdate would break — the one-zone tests above
    /// cannot tell the columns apart.
    #[test]
    fn solar_scale_two_zones_recover_their_own_distinct_scales() {
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
                    window: { u: 1.2, g: 0.6 },
                },
                zones: { room: { volume: 50 }, room_b: { volume: 50 } },
                boundaries: [
                    { boundary_type: "wall", zones: ["room", "outside"], area: 25 },
                    { boundary_type: "wall", zones: ["room_b", "outside"], area: 25 },
                    { boundary_type: "partition", zones: ["room", "room_b"], area: 12 },
                    { boundary_type: "window", zones: ["room", "outside"], area: 5, azimuth: 180, angle: 90 },
                    { boundary_type: "window", zones: ["room_b", "outside"], area: 5, azimuth: 180, angle: 90 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 96, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth_scale = [("room", 0.6), ("room_b", 1.3)];
        let disc = ss.discretize(3600.0);
        let mut x = x0.clone();
        let mut truth = vec![x.clone()];
        for h in 0..data.grid_times.len() - 1 {
            let (mut u, entries, _) = build_input_parts(&net, &ss, lat, lon, &data, h);
            for (zone, scale) in truth_scale {
                for &(col, watts) in &entries[zone] {
                    u[col] += (scale - 1.0) * watts;
                }
            }
            x = ss.step(&disc, &x, &u);
            truth.push(x.clone());
        }
        let mut measured = HashMap::new();
        for (zone, _) in truth_scale {
            let row = ss.state_index(net.zone_indices[zone]).unwrap();
            measured.insert(
                zone.to_string(),
                data.grid_times
                    .iter()
                    .zip(&truth)
                    .map(|(t, x)| TimeSample {
                        time: *t,
                        value: x[row] - 273.15,
                    })
                    .collect(),
            );
        }
        let zones = ["room".to_string(), "room_b".to_string()];
        let f = KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &zones).unwrap();
        let est = f.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);
        for (zone, scale) in truth_scale {
            let s = est.solar_scale[zone];
            assert!(
                (s - scale).abs() < 0.1,
                "{zone}: recovered {s:.3}, true {scale}"
            );
        }
        // The trace is per step and starts from the flat seed.
        assert_eq!(est.solar_scale_trace.len(), est.trajectory.len());
        assert!(est.solar_scale_trace[0].iter().all(|&d| d == 0.0));
        assert_eq!(est.solar_scale_zones, zones);
    }

    /// No δ update inside the warm-up: a run shorter than the warm-up learns nothing however
    /// sunny it is (the seed transient would otherwise be charged to the window).
    #[test]
    fn solar_scale_does_not_update_inside_the_warm_up() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 25, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive_with_scaled_window(&net, &ss, lat, lon, &x0, &data, 0.7);
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let est = f.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);
        assert_eq!(est.solar_scale_updates, 0);
        assert_eq!(est.solar_scale["room"], 1.0);
    }

    #[test]
    fn solar_scale_forward_prediction_beats_flag_off_at_12_to_24h_lead() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let history_hours = 96;
        // +1: `build_input`/`build_input_parts` index `data[h+1]`, so the grid needs one more
        // point than the last forecast step (`history_hours + 23`) requires.
        let data = drive_data_dated(t0, history_hours + 24 + 1, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive_with_scaled_window(&net, &ss, lat, lon, &x0, &data, 0.7);
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let history_truth = &truth[..history_hours];
        let measured = measured_from_truth(&data, history_truth, zone_row, &[]);

        let f_on =
            KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let f_off = KalmanFilter::build(
            &net,
            &ss,
            &EstimatorConfig::default(),
            &["room".to_string()],
        )
        .unwrap();
        let cutoff = data.hours[history_hours];
        let est_on = f_on.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff));
        let est_off = f_off.filter(&net, &ss, lat, lon, &x0, &data, &measured, Some(cutoff));
        let s_room = est_on.solar_scale["room"];

        let disc = ss.discretize(3600.0);
        let mut x_new = est_on.trajectory[history_hours].clone();
        let mut x_old = est_off.trajectory[history_hours].clone();
        let mut err_new = Vec::with_capacity(24);
        let mut err_old = Vec::with_capacity(24);
        for h in 0..24 {
            let step = history_hours + h;
            let (u, entries, _gate) = build_input_parts(&net, &ss, lat, lon, &data, step);
            let mut u_new = u.clone();
            if let Some(room) = entries.get("room") {
                for &(col, watts) in room {
                    u_new[col] += (s_room - 1.0) * watts;
                }
            }
            x_new = ss.step(&disc, &x_new, &u_new);
            x_old = ss.step(&disc, &x_old, &u); // flag-off: nominal (unscaled) solar projects flat
            let true_t = truth[step + 1][zone_row];
            err_new.push((x_new[zone_row] - true_t).abs());
            err_old.push((x_old[zone_row] - true_t).abs());
        }
        let mean = |v: &[f64], a: usize, b: usize| v[a..b].iter().sum::<f64>() / (b - a) as f64;
        let (new_12_24, old_12_24) = (mean(&err_new, 12, 24), mean(&err_old, 12, 24));
        assert!(
            new_12_24 < old_12_24,
            "solar-scaled forecast {new_12_24:.3} K should beat flag-off {old_12_24:.3} K at 12-24h lead"
        );
    }

    #[test]
    fn solar_scale_stays_at_one_under_full_overcast() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 96, 15.0, 1.0); // fully overcast throughout
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive(&net, &ss, lat, lon, &x0, &data); // nominal (g=1.0×) physics — overcast
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let est = f.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);
        assert_eq!(
            est.solar_scale["room"], 1.0,
            "overcast irradiance must stay below the gate the whole run — s must not move"
        );
    }

    #[test]
    fn solar_scale_clamps_under_an_extreme_0_1x_truth() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 96, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive_with_scaled_window(&net, &ss, lat, lon, &x0, &data, 0.1);
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);
        let f = KalmanFilter::build(&net, &ss, &solar_cfg(100.0), &["room".to_string()]).unwrap();
        let est = f.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);
        let s = est.solar_scale["room"];
        assert!(
            s >= 0.3 - 1e-9,
            "the lower clamp (0.3) must hold even under an extreme 0.1x truth: got {s}"
        );
        assert!(
            s < 0.5,
            "an extreme 0.1x truth should push the scale firmly toward the floor: got {s}"
        );
    }

    /// Acceptance 1's bit-identity requirement: flag off vs flag on with EVERY zone gated out
    /// (`solar_scale_min_wm2: 1e9`) must produce the SAME `x`/`trajectory`, element by element.
    #[test]
    fn flag_off_matches_flag_on_fully_gated_out_bit_for_bit() {
        let (net, ss) = toy_with_window();
        let (lat, lon) = (Angle::new::<degree>(49.0), Angle::new::<degree>(14.5));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let data = drive_data_dated(t0, 96, 15.0, 0.0);
        let x0 = DVector::from_element(ss.n_states(), 273.15 + 15.0);
        let truth = drive(&net, &ss, lat, lon, &x0, &data);
        let zone_row = ss.state_index(net.zone_indices["room"]).unwrap();
        let measured = measured_from_truth(&data, &truth, zone_row, &[]);

        let f_off = KalmanFilter::build(
            &net,
            &ss,
            &EstimatorConfig::default(),
            &["room".to_string()],
        )
        .unwrap();
        let f_on_gated =
            KalmanFilter::build(&net, &ss, &solar_cfg(1e9), &["room".to_string()]).unwrap();

        let est_off = f_off.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);
        let est_on = f_on_gated.filter(&net, &ss, lat, lon, &x0, &data, &measured, None);

        assert!(est_off.solar_scale.is_empty());
        assert_eq!(est_on.solar_scale["room"], 1.0);
        assert_eq!(est_off.x, est_on.x);
        assert_eq!(est_off.trajectory.len(), est_on.trajectory.len());
        for (a, b) in est_off.trajectory.iter().zip(&est_on.trajectory) {
            assert_eq!(
                a, b,
                "flag-off and fully-gated-out flag-on trajectories must match exactly"
            );
        }
    }
}
