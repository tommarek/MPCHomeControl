//! Condensed thermal prediction for the unified optimizer.
//!
//! The house has ~183 thermal states, far too many to put into the LP. Instead we precompute,
//! from the discretized state-space, an **affine** map from the per-zone heating decisions to
//! each controllable zone's air temperature over the horizon:
//!
//! ```text
//! T_z[k] = free_response[z][k] + Σ_{z'} Σ_{j<k} kernel[z,z'][k-j] · heat[z'][j]
//! ```
//!
//! Subscripts here are 1-based **lags**; the stored vectors are 0-indexed, so `free_response[z][k]`
//! is `free_response[z][k-1]` in code and the lag-`(k-j)` kernel is `kernel[z,z'][k-j-1]` (vector
//! index 0 = lag 1). The code uses those `-1` accesses; the math above is the same map.
//!
//! - `free_response` is the zone-air temperature drift under the *known* inputs (boundary
//!   temperatures + solar), with heating off — obtained from one `simulate`.
//! - `kernel[z,z']` is the air-temperature impulse response of zone `z` to a 1 kW heating pulse
//!   in zone `z'`, obtained from one cheap forward rollout per heated zone (`g[1]=Bd·e`,
//!   `g[lag]=Ad·g[lag-1]`).
//!
//! Cross-zone kernels (`z ≠ z'`) capture heat flowing through shared walls. The 183 states live
//! only in these precomputed `f64` kernels; the LP sees only the small, affine predictions.

use std::collections::HashMap;

use anyhow::Result;
use nalgebra::DVector;
use uom::si::{f64::Power, power::kilowatt};

use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;

const HEATING_MARKER: &str = "heating";

/// The precomputed condensed prediction handed to the unified LP. Pure data.
#[derive(Debug, Clone)]
pub struct ThermalContext {
    pub dt: f64,
    pub horizon: usize,
    /// Underfloor-heated zones (a `"heating"` slab marker), sorted and de-duplicated.
    pub heated_zones: Vec<String>,
    /// Zones served by an HVAC unit (an air-node actuator), sorted and de-duplicated.
    pub hvac_zones: Vec<String>,
    /// Per zone: air temperature (K) under the known inputs with all actuators off, for steps
    /// `1..=horizon` (vector index `0` is step 1). Covers the union of heated and HVAC zones.
    pub free_response: HashMap<String, Vec<f64>>,
    /// Per `(target, source)`: air-temperature response (K) of `target` to a 1 kW heating pulse at
    /// `source`'s **slab** (`"heating"` marker), by lag `1..=horizon` (vector index `0` is lag 1).
    pub kernels: HashMap<(String, String), Vec<f64>>,
    /// Per `(target, source)`: air-temperature response (K) of `target` to a 1 kW pulse at
    /// `source`'s **air node** — the HVAC/AC actuator. Positive (air-heating); cooling applies it
    /// with a negative decision. Faster than the slab kernel (no slab lag).
    pub air_kernels: HashMap<(String, String), Vec<f64>>,
    /// Per `(target, load_name)`: air-temperature response (K) of `target` to a 1 kW pulse at a
    /// **controllable load's** zone air node, by lag `1..=horizon`. Keyed by the *load name* (not the
    /// zone) so several loads can act on the same room independently. The optimizer applies it with the
    /// load's signed per-kW heat when the load is on; it is the same air-node mechanism as
    /// [`Self::air_kernels`], just driven by the on/off load decision rather than the HVAC decision.
    pub load_kernels: HashMap<(String, String), Vec<f64>>,
}

impl ThermalContext {
    /// Predicted air temperature (K) of `zone` at step `k` (`1..=horizon`) for a slab-heating
    /// schedule `heat[source][j]` (kW), a **signed** HVAC air schedule `air[source][j]` (kW;
    /// positive = air-heating, negative = cooling), and a **signed** controllable-load air schedule
    /// `loads[load_name][j]` (kW; the load's per-kW heat × its on/off), `j = 0..horizon`. Affine in
    /// the decisions — the LP builds the same expression symbolically; this evaluates it for
    /// reporting/tests.
    ///
    /// `zone` must be one of [`Self::free_response`]'s keys (a controlled zone with a state row);
    /// callers derive their zone list from there. Pass empty `air` / `loads` maps when there is no
    /// HVAC / no controllable load.
    pub fn predict(
        &self,
        zone: &str,
        k: usize,
        heat: &HashMap<String, Vec<f64>>,
        air: &HashMap<String, Vec<f64>>,
        loads: &HashMap<String, Vec<f64>>,
    ) -> f64 {
        debug_assert!(
            (1..=self.horizon).contains(&k),
            "predict: k={k} out of range 1..={}",
            self.horizon
        );
        let mut t = self.free_response[zone][k - 1];
        for source in &self.heated_zones {
            let (Some(kernel), Some(schedule)) = (
                self.kernels.get(&(zone.to_string(), source.clone())),
                heat.get(source),
            ) else {
                continue; // a heated zone with no scheduled heat contributes nothing
            };
            for j in 0..k {
                t += schedule[j] * kernel[k - j - 1];
            }
        }
        for source in &self.hvac_zones {
            let (Some(kernel), Some(schedule)) = (
                self.air_kernels.get(&(zone.to_string(), source.clone())),
                air.get(source),
            ) else {
                continue; // an HVAC zone with no scheduled air power contributes nothing
            };
            for j in 0..k {
                t += schedule[j] * kernel[k - j - 1];
            }
        }
        // Each controllable load, by name: its signed per-kW heat applied through the same air-node
        // kernel (keyed by the load name, target = this zone).
        for ((target, name), kernel) in &self.load_kernels {
            if target != zone {
                continue;
            }
            let Some(schedule) = loads.get(name) else {
                continue;
            };
            for j in 0..k {
                t += schedule[j] * kernel[k - j - 1];
            }
        }
        t
    }
}

/// Build the condensed prediction from the state-space, the initial state, and the known-input
/// trajectory (boundary temperatures + solar, heating off), at step `dt`.
pub fn build_context(
    ss: &StateSpace,
    net: &RcNetwork,
    x0: &DVector<f64>,
    u_known: &[DVector<f64>],
    dt: f64,
    hvac_zones: &[String],
    // Controllable scheduled loads, as `(load_name, zone)`: each gets a 1 kW air-node kernel keyed by
    // its name (see [`ThermalContext::load_kernels`]). Empty ⇒ none, and the result is unchanged.
    controllable_loads: &[(String, String)],
) -> Result<ThermalContext> {
    let n = u_known.len();

    let zone_row = |zone: &str| -> Option<usize> {
        net.zone_indices
            .get(zone)
            .and_then(|&node| ss.state_index(node))
    };

    // Heated zones with a `"heating"` marker and a state row (skip a marker on a reserved/boundary
    // zone, which has no state to actuate) — symmetric with the HVAC filter below.
    let mut heated_zones: Vec<String> = net
        .marker_indices
        .keys()
        .filter(|(zone, marker)| marker == HEATING_MARKER && zone_row(zone).is_some())
        .map(|(zone, _)| zone.clone())
        .collect();
    heated_zones.sort();
    heated_zones.dedup();

    // HVAC zones with a state row (skip any that aren't real zone-air states).
    let mut hvac_zones: Vec<String> = hvac_zones
        .iter()
        .filter(|z| zone_row(z).is_some())
        .cloned()
        .collect();
    hvac_zones.sort();
    hvac_zones.dedup();

    // Controllable-load source zones with a real state row (skip a load on a reserved/boundary zone).
    let load_sources: Vec<(String, String)> = controllable_loads
        .iter()
        .filter(|(_, zone)| zone_row(zone).is_some())
        .cloned()
        .collect();

    // Controlled = heated ∪ HVAC ∪ controllable-load zones; free response covers all of them (a
    // controllable load's zone gets a comfort band and a temperature prediction even when it has no
    // heating/HVAC actuator of its own — its load is the only thing acting on the air there).
    let mut controlled = heated_zones.clone();
    controlled.extend(hvac_zones.iter().cloned());
    controlled.extend(load_sources.iter().map(|(_, zone)| zone.clone()));
    controlled.sort();
    controlled.dedup();

    // Free response: drift under the known inputs (all actuators off).
    let traj = ss.simulate(x0, u_known, dt)?;
    let mut free_response = HashMap::new();
    for z in &controlled {
        if let Some(row) = zone_row(z) {
            free_response.insert(z.clone(), (1..=n).map(|k| traj[k][row]).collect());
        }
    }

    let disc = ss.discretize(dt);

    // Roll a unit input forward and read each controlled target's air-temperature response:
    // g[0] = Bd·e, g[i] = Ad·g[i-1]; kernel[(target, source)][lag] = g[lag][target_row].
    let kernel_from = |e: &DVector<f64>| -> HashMap<String, Vec<f64>> {
        let mut g = Vec::with_capacity(n);
        let mut cur = &disc.bd * e;
        for _ in 0..n {
            g.push(cur.clone());
            cur = &disc.ad * &cur;
        }
        controlled
            .iter()
            .filter_map(|target| {
                zone_row(target).map(|row| (target.clone(), g.iter().map(|gv| gv[row]).collect()))
            })
            .collect()
    };

    // Slab kernels: a 1 kW pulse at each heated zone's `"heating"` marker node(s).
    let mut kernels = HashMap::new();
    for source in &heated_zones {
        let nodes = net
            .marker_indices
            .get_vec(&(source.clone(), HEATING_MARKER.to_string()))
            .cloned()
            .unwrap_or_default();
        if nodes.is_empty() {
            continue;
        }
        // 1 kW total, split equally across the zone's heating nodes.
        let per_node = Power::new::<kilowatt>(1.0 / nodes.len() as f64);
        let mut e = ss.zero_input();
        for node in &nodes {
            ss.set_flux(&mut e, *node, per_node);
        }
        for (target, response) in kernel_from(&e) {
            kernels.insert((target, source.clone()), response);
        }
    }

    // Air kernels: a 1 kW pulse at each HVAC zone's air node (the room air directly).
    let mut air_kernels = HashMap::new();
    for source in &hvac_zones {
        let Some(&node) = net.zone_indices.get(source) else {
            continue;
        };
        let mut e = ss.zero_input();
        ss.set_flux(&mut e, node, Power::new::<kilowatt>(1.0));
        for (target, response) in kernel_from(&e) {
            air_kernels.insert((target, source.clone()), response);
        }
    }

    // Controllable-load kernels: a 1 kW pulse at each controllable load's zone air node — the same
    // air-node mechanism as the HVAC kernel, but keyed by the *load name* so the LP can scale it by
    // that load's on/off decision and its signed per-kW heat.
    let mut load_kernels = HashMap::new();
    for (name, zone) in &load_sources {
        let Some(&node) = net.zone_indices.get(zone) else {
            continue;
        };
        let mut e = ss.zero_input();
        ss.set_flux(&mut e, node, Power::new::<kilowatt>(1.0));
        for (target, response) in kernel_from(&e) {
            load_kernels.insert((target, name.clone()), response);
        }
    }

    Ok(ThermalContext {
        dt,
        horizon: n,
        heated_zones,
        hvac_zones,
        free_response,
        kernels,
        air_kernels,
        load_kernels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use approx::assert_abs_diff_eq;
    use uom::si::f64::ThermodynamicTemperature;
    use uom::si::thermodynamic_temperature::{degree_celsius, kelvin};

    /// Two zones, each with an underfloor-heating layer; zone `a` has a split floor (two heated
    /// boundaries → two heating nodes) and shares a wall with `b` (cross-coupling).
    fn small_model() -> Model {
        Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                },
                boundary_types: {
                    floor: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { marker: "heating" },
                        { material: "concrete", thickness: 0.1 },
                    ] },
                    wall: { layers: [ { material: "concrete", thickness: 0.2 } ] },
                },
                zones: { a: { volume: 50 }, b: { volume: 50 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["a", "ground"], area: 10 },
                    { boundary_type: "floor", zones: ["a", "ground"], area: 10 },
                    { boundary_type: "floor", zones: ["b", "ground"], area: 20 },
                    { boundary_type: "wall",  zones: ["a", "outside"], area: 10 },
                    { boundary_type: "wall",  zones: ["a", "b"], area: 10 },
                ],
            }"#,
        )
        .unwrap()
    }

    fn fixture() -> (
        RcNetwork,
        StateSpace,
        DVector<f64>,
        Vec<DVector<f64>>,
        f64,
        usize,
    ) {
        let model = small_model();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let dt = 900.0;
        let n = 8;
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(5.0),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(10.0),
        );
        let u_known = vec![u0; n];
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(20.0).get::<kelvin>(),
        );
        (net, ss, x0, u_known, dt, n)
    }

    #[test]
    fn affine_prediction_matches_simulate() {
        let (net, ss, x0, u_known, dt, n) = fixture();
        // Treat zone "a" as also HVAC-served (an air-node actuator) on top of both zones' slabs —
        // the keystone check that the affine map matches a full simulate for slab + air fluxes.
        let ctx = build_context(&ss, &net, &x0, &u_known, dt, &["a".to_string()], &[]).unwrap();
        assert_eq!(ctx.heated_zones, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(ctx.hvac_zones, vec!["a".to_string()]);

        let heat: HashMap<String, Vec<f64>> = HashMap::from([
            (
                "a".to_string(),
                vec![2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (
                "b".to_string(),
                vec![0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
        ]);
        // Signed HVAC air power for "a": air-heating, then cooling.
        let air: HashMap<String, Vec<f64>> = HashMap::from([(
            "a".to_string(),
            vec![0.0, 0.0, 1.5, 0.0, -2.0, -1.0, 0.0, 0.0],
        )]);

        // Reference: simulate with the slab heating fluxes (equal split) AND the air-node flux added.
        let mut u_full = u_known.clone();
        for source in &ctx.heated_zones {
            let nodes = net
                .marker_indices
                .get_vec(&(source.clone(), "heating".to_string()))
                .cloned()
                .unwrap();
            let m = nodes.len() as f64;
            for (j, uj) in u_full.iter_mut().enumerate() {
                let p = Power::new::<kilowatt>(heat[source][j] / m);
                for &node in &nodes {
                    ss.set_flux(uj, node, p);
                }
            }
        }
        let air_node = net.zone_indices["a"];
        for (j, uj) in u_full.iter_mut().enumerate() {
            ss.set_flux(uj, air_node, Power::new::<kilowatt>(air["a"][j]));
        }
        let traj_full = ss.simulate(&x0, &u_full, dt).unwrap();

        let no_loads: HashMap<String, Vec<f64>> = HashMap::new();
        for zone in &ctx.heated_zones {
            let row = ss.state_index(net.zone_indices[zone]).unwrap();
            for (k, state) in traj_full.iter().enumerate().take(n + 1).skip(1) {
                assert_abs_diff_eq!(
                    ctx.predict(zone, k, &heat, &air, &no_loads),
                    state[row],
                    epsilon = 1e-6
                );
            }
        }
    }

    /// A controllable load registered at zone "a"'s air node gets a kernel keyed by its **name**,
    /// matching the HVAC air kernel for the same zone (it's the same air-node pulse) — and `predict`
    /// drives the prediction with that load's signed schedule.
    #[test]
    fn controllable_load_kernel_matches_air_kernel_and_predicts() {
        let (net, ss, x0, u_known, dt, n) = fixture();
        let ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            dt,
            &[],
            &[("boiler".to_string(), "a".to_string())],
        )
        .unwrap();
        // The load kernel onto its own zone equals the air-node kernel for that zone (same 1 kW pulse).
        let load_k = &ctx.load_kernels[&("a".to_string(), "boiler".to_string())];
        let air_ctx = build_context(&ss, &net, &x0, &u_known, dt, &["a".to_string()], &[]).unwrap();
        let air_k = &air_ctx.air_kernels[&("a".to_string(), "a".to_string())];
        for (lk, ak) in load_k.iter().zip(air_k) {
            assert_abs_diff_eq!(lk, ak, epsilon = 1e-12);
        }
        // A positive (source) load raises the zone; a negative (sink) one lowers it below free.
        let no_heat: HashMap<String, Vec<f64>> = HashMap::new();
        let no_air: HashMap<String, Vec<f64>> = HashMap::new();
        let on: HashMap<String, Vec<f64>> = HashMap::from([("boiler".to_string(), vec![2.0; n])]);
        let off: HashMap<String, Vec<f64>> = HashMap::from([("boiler".to_string(), vec![-2.0; n])]);
        assert!(ctx.predict("a", n, &no_heat, &no_air, &on) > ctx.free_response["a"][n - 1]);
        assert!(ctx.predict("a", n, &no_heat, &no_air, &off) < ctx.free_response["a"][n - 1]);
    }

    #[test]
    fn zero_heating_equals_free_response() {
        let (net, ss, x0, u_known, dt, n) = fixture();
        let ctx = build_context(&ss, &net, &x0, &u_known, dt, &[], &[]).unwrap();
        let zero: HashMap<String, Vec<f64>> = ctx
            .heated_zones
            .iter()
            .map(|z| (z.clone(), vec![0.0; n]))
            .collect();
        let no_air: HashMap<String, Vec<f64>> = HashMap::new();
        let no_loads: HashMap<String, Vec<f64>> = HashMap::new();
        for zone in &ctx.heated_zones {
            for k in 1..=n {
                assert_abs_diff_eq!(
                    ctx.predict(zone, k, &zero, &no_air, &no_loads),
                    ctx.free_response[zone][k - 1],
                    epsilon = 1e-12
                );
            }
        }
    }

    #[test]
    fn heating_kernels_are_nonnegative_and_warm_the_zone() {
        let (net, ss, x0, u_known, dt, _n) = fixture();
        let ctx = build_context(&ss, &net, &x0, &u_known, dt, &[], &[]).unwrap();
        for ((_target, _source), kernel) in &ctx.kernels {
            // Heating never cools any zone (within numerical noise).
            assert!(kernel.iter().all(|&v| v >= -1e-9));
        }
        // Heating a zone measurably warms its own air over the horizon.
        let self_kernel = &ctx.kernels[&("a".to_string(), "a".to_string())];
        assert!(self_kernel.iter().sum::<f64>() > 0.0);
    }

    #[test]
    fn air_kernel_is_fast_and_cools_with_negative_power() {
        let (net, ss, x0, u_known, dt, n) = fixture();
        let ctx = build_context(&ss, &net, &x0, &u_known, dt, &["a".to_string()], &[]).unwrap();
        // The HVAC zone gets an air-node kernel; +1 kW warms its own air immediately.
        let air = &ctx.air_kernels[&("a".to_string(), "a".to_string())];
        assert!(air.iter().all(|&v| v >= -1e-9));
        assert!(air[0] > 0.0, "air injection acts on the same node at once");
        // The air-node actuator responds faster than the slab (no slab lag) at the first step.
        let slab = &ctx.kernels[&("a".to_string(), "a".to_string())];
        assert!(
            air[0] > slab[0],
            "air-node actuator is faster than the slab: {} vs {}",
            air[0],
            slab[0]
        );
        // A cooling decision (negative air power) drives the prediction below the free response.
        let no_heat: HashMap<String, Vec<f64>> = HashMap::new();
        let no_loads: HashMap<String, Vec<f64>> = HashMap::new();
        let cool: HashMap<String, Vec<f64>> = HashMap::from([("a".to_string(), vec![-1.0; n])]);
        assert!(ctx.predict("a", n, &no_heat, &cool, &no_loads) < ctx.free_response["a"][n - 1]);
    }
}
