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

use anyhow::{ensure, Result};
use nalgebra::DVector;
use uom::si::{f64::Power, power::kilowatt};

use super::grid::BlockGrid;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;

const HEATING_MARKER: &str = "heating";

/// The precomputed condensed prediction handed to the unified LP. Pure data.
#[derive(Debug, Clone)]
pub struct ThermalContext {
    /// The multi-rate planning grid this context was built on. Every `Self` vector indexed
    /// `1..=horizon` (or `0..horizon` internally) is indexed by GRID BLOCK, not fine step; the
    /// kernels stay on the fine (15-minute) lattice — see [`Self::predict`].
    pub grid: BlockGrid,
    /// Number of BLOCKS (== `grid.len()`) — kept alongside `grid` since it's what callers compare
    /// their own per-block vectors' lengths against.
    pub horizon: usize,
    /// Underfloor-heated zones (a `"heating"` slab marker), sorted and de-duplicated.
    pub heated_zones: Vec<String>,
    /// Zones served by an HVAC unit (an air-node actuator), sorted and de-duplicated.
    pub hvac_zones: Vec<String>,
    /// Per zone: air temperature (K) under the known inputs with all actuators off, sampled at the
    /// END of each grid block (index `0` is block 1's end) — see [`BlockGrid::sample_end`]. On a
    /// uniform grid this is identical to sampling every fine step. Covers the union of heated and
    /// HVAC zones.
    pub free_response: HashMap<String, Vec<f64>>,
    /// Per zone: air temperature (K), all actuators off, CONTINUING past the horizon over the
    /// outlook window (see `build_context`'s `outlook_u`) — from the horizon-end state, not from
    /// `x0`. Empty when no outlook was supplied (today's behaviour); otherwise same indexing
    /// convention as [`Self::free_response`] but relative to the horizon end (index `0` is the
    /// first outlook step). Used only by the terminal heat-credit's `heating_demanded` gate and
    /// its per-zone energy-budget cap — never fed into the LP.
    pub outlook_free_response: HashMap<String, Vec<f64>>,
    /// Per `(target, source)`: air-temperature response (K) of `target` to a 1 kW heating pulse held
    /// for one FINE (15-minute) step at `source`'s **slab** (`"heating"` marker), by fine-step lag
    /// `1..=grid.n_fine()` (vector index `0` is lag 1) — the physics stays exact on the fine
    /// lattice regardless of the grid's block structure; [`Self::predict`] aggregates it onto
    /// blocks on the fly (an hourly decision is constant power over its 4 fine steps).
    pub kernels: HashMap<(String, String), Vec<f64>>,
    /// Per `(target, source)`: fine-lattice air-temperature response (K) of `target` to a 1 kW pulse
    /// at `source`'s **air node** — the HVAC/AC actuator. Positive (air-heating); cooling applies it
    /// with a negative decision. Faster than the slab kernel (no slab lag). Same fine-lattice
    /// convention as [`Self::kernels`].
    pub air_kernels: HashMap<(String, String), Vec<f64>>,
    /// Per `(target, load_name)`: fine-lattice air-temperature response (K) of `target` to a 1 kW
    /// pulse at a **controllable load's** zone air node. Keyed by the *load name* (not the zone) so
    /// several loads can act on the same room independently. The optimizer applies it with the
    /// load's signed per-kW heat when the load is on; it is the same air-node mechanism as
    /// [`Self::air_kernels`], just driven by the on/off load decision rather than the HVAC decision.
    /// Same fine-lattice convention as [`Self::kernels`].
    pub load_kernels: HashMap<(String, String), Vec<f64>>,
}

impl ThermalContext {
    /// Predicted air temperature (K) of `zone` at the END of grid block `k - 1` (`k` in
    /// `1..=horizon`, matching [`Self::free_response`]'s indexing) for a **per-block** slab-heating
    /// schedule `heat[source][j]` (kW), a **signed** per-block HVAC air schedule `air[source][j]`
    /// (kW; positive = air-heating, negative = cooling), and a **signed** per-block controllable-load
    /// air schedule `loads[load_name][j]` (kW; the load's per-kW heat × its on/off), `j = 0..horizon`
    /// — one entry per GRID BLOCK, not per fine step. Affine in the decisions — the LP builds the
    /// same expression symbolically; this evaluates it for reporting/tests.
    ///
    /// Aggregates the fine-lattice kernels onto the block grid on the fly: block `j`'s decision is
    /// constant power held over every fine step `f` in `grid.fine_range(j)`, so its contribution to
    /// the state at block `k-1`'s end (fine index `e_k`) is `Σ_f kernel[e_k - f]` — on a uniform
    /// grid (`fine_range(j) == {j}`) this reduces to the single-lag lookup this used to be,
    /// bit-identically.
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
        let bk = k - 1;
        let e_k = self.grid.fine_range(bk).end - 1;
        let mut t = self.free_response[zone][bk];
        let mut accumulate = |kernel: &[f64], schedule: &[f64]| {
            for (j, &s) in schedule.iter().enumerate().take(bk + 1) {
                if s == 0.0 {
                    continue;
                }
                for f in self.grid.fine_range(j) {
                    t += s * kernel[e_k - f];
                }
            }
        };
        for source in &self.heated_zones {
            let (Some(kernel), Some(schedule)) = (
                self.kernels.get(&(zone.to_string(), source.clone())),
                heat.get(source),
            ) else {
                continue; // a heated zone with no scheduled heat contributes nothing
            };
            accumulate(kernel, schedule);
        }
        for source in &self.hvac_zones {
            let (Some(kernel), Some(schedule)) = (
                self.air_kernels.get(&(zone.to_string(), source.clone())),
                air.get(source),
            ) else {
                continue; // an HVAC zone with no scheduled air power contributes nothing
            };
            accumulate(kernel, schedule);
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
            accumulate(kernel, schedule);
        }
        t
    }
}

/// The **x0-independent** part of the condensed prediction: the ZOH discretization and every unit
/// impulse kernel, on the FINE (15-minute) lattice regardless of the planning grid's block
/// structure — see `ThermalContext::predict`'s on-the-fly block aggregation. These depend only on
/// the model, `dt`, the fine-step count, and which zones/loads are actuated — never on the state or
/// the forecast — so the live loop builds this **once at startup** and re-runs only the cheap
/// free-response simulation per tick. Building the kernels is the pipeline's dense-linear-algebra
/// hot spot (a ~(states+inputs)² matrix exponential plus a matrix power chain per source);
/// recomputing it every tick dominated the live solve time.
pub struct KernelSet {
    /// The fine-lattice step (seconds) — `BlockGrid::fine_seconds`, 900 live.
    pub dt: f64,
    /// The fine-lattice step count each kernel covers — `BlockGrid::n_fine()`.
    pub horizon: usize,
    /// The ZOH discretization at `dt` — reused for the per-tick free-response simulate too.
    pub disc: crate::state_space::Discretized,
    pub heated_zones: Vec<String>,
    pub hvac_zones: Vec<String>,
    /// Heated ∪ HVAC ∪ controllable-load zones (the comfort-controlled set).
    pub controlled: Vec<String>,
    /// The `(load_name, zone)` pairs that got kernels (filtered to modelled zones).
    pub load_sources: Vec<(String, String)>,
    pub kernels: HashMap<(String, String), Vec<f64>>,
    pub air_kernels: HashMap<(String, String), Vec<f64>>,
    pub load_kernels: HashMap<(String, String), Vec<f64>>,
}

#[cfg(test)]
thread_local! {
    /// Test-only instrumentation: counts calls to [`build_kernels`] ON THIS THREAD — the expensive,
    /// dense matrix-exponential build `KernelSet` exists to avoid paying per tick. Lets a test
    /// assert a cache HIT actually avoided a rebuild (rework cycle 1, finding 2), rather than
    /// merely checking the two results agree (which is true either way). THREAD-LOCAL, not a
    /// process-global atomic: the default test harness runs each `#[test]` on its own thread, and a
    /// global counter raced with unrelated tests' own kernel builds running concurrently —
    /// spuriously failing on builds this test's own call never made.
    pub(crate) static KERNEL_BUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Build the [`KernelSet`] — see there. `n` is the fine-lattice step count (`grid.n_fine()`);
/// `hvac_zones` and `controllable_loads` (as `(load_name, zone)`) are filtered to zones with a real
/// state row.
pub fn build_kernels(
    ss: &StateSpace,
    net: &RcNetwork,
    dt: f64,
    n: usize,
    hvac_zones: &[String],
    controllable_loads: &[(String, String)],
) -> KernelSet {
    #[cfg(test)]
    KERNEL_BUILD_COUNT.with(|c| c.set(c.get() + 1));
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

    KernelSet {
        dt,
        horizon: n,
        disc,
        heated_zones,
        hvac_zones,
        controlled,
        load_sources,
        kernels,
        air_kernels,
        load_kernels,
    }
}

/// Whether a cached [`KernelSet`] can serve this build's inputs (same dt AND the same actuated
/// sets — a config/model change between startup and now must fall back to a fresh build, never
/// silently use stale kernels) at a fine-step count of AT MOST the cache's own horizon.
///
/// `ks.horizon >= n` (not `==`): a kernel's value at lag `L` depends only on `L` and the model/dt
/// — never on how many lags the cache happened to compute — so a cache built at the live
/// `HORIZON_BLOCKS` (144) already contains, as an exact PREFIX, every shorter kernel a smaller `n`
/// needs (see [`build_context`]'s truncation). Requiring exact equality (rework cycle 1, finding 2)
/// meant the single 144-step startup cache matched only a `:00`-aligned grid; a `:15`/`:30`/`:45`
/// start's multi-rate `n_fine` (143/142/141 live) missed it every time, paying two full kernel
/// rebuilds (a dense matrix-exponential + matrix-power chain) inside every live tick.
fn kernel_set_matches(
    ks: &KernelSet,
    dt: f64,
    n: usize,
    hvac_zones: &[String],
    controllable_loads: &[(String, String)],
) -> bool {
    let mut hv: Vec<&String> = hvac_zones.iter().collect();
    hv.sort();
    hv.dedup();
    ks.dt == dt
        && ks.horizon >= n
        && ks.hvac_zones.iter().collect::<Vec<_>>() == hv
        && ks.load_sources == controllable_loads
}

/// Build the condensed prediction from the state-space, the initial state, the known-input
/// trajectory (boundary temperatures + solar, heating off, on the FINE lattice), and the planning
/// `grid` (whose `n_fine()` must match `u_known.len()`). `cached` supplies a startup-built
/// [`KernelSet`] (the expensive, x0-independent part); when it matches, only the free-response
/// simulation runs — zero matrix exponentials per call. `None` (tests, one-shot paths) builds
/// everything fresh, bit-identically.
#[allow(clippy::too_many_arguments)] // the model, state, grid, actuated sets and cache are all distinct
pub fn build_context(
    ss: &StateSpace,
    net: &RcNetwork,
    x0: &DVector<f64>,
    u_known: &[DVector<f64>],
    grid: &BlockGrid,
    hvac_zones: &[String],
    // Controllable scheduled loads, as `(load_name, zone)`: each gets a 1 kW air-node kernel keyed by
    // its name (see [`ThermalContext::load_kernels`]). Empty ⇒ none, and the result is unchanged.
    controllable_loads: &[(String, String)],
    // Known inputs for the POST-horizon outlook window (same construction as `u_known`, on the same
    // fine `dt`), used ONLY to continue the free-response simulation past the horizon end into
    // [`ThermalContext::outlook_free_response`]. Empty ⇒ no outlook (today's behaviour).
    outlook_u: &[DVector<f64>],
    cached: Option<&KernelSet>,
) -> Result<ThermalContext> {
    let n_fine = u_known.len();
    ensure!(
        n_fine == grid.n_fine(),
        "u_known length ({n_fine}) must match the grid's fine-step count ({})",
        grid.n_fine()
    );

    // Filter the load list the same way build_kernels does, so the cache-match compare is apples
    // to apples (the cached set stores the filtered list).
    let zone_row = |zone: &str| -> Option<usize> {
        net.zone_indices
            .get(zone)
            .and_then(|&node| ss.state_index(node))
    };
    let filtered_loads: Vec<(String, String)> = controllable_loads
        .iter()
        .filter(|(_, zone)| zone_row(zone).is_some())
        .cloned()
        .collect();
    // Filter the requested HVAC zones the same way build_kernels stores them (state rows only):
    // comparing the raw list against the stored filtered one would spuriously reject the cache
    // every tick whenever a served zone lacks a state row — correct results, wasted rebuilds.
    let filtered_hvac: Vec<String> = hvac_zones
        .iter()
        .filter(|z| zone_row(z).is_some())
        .cloned()
        .collect();

    let fresh;
    let ks = match cached {
        Some(ks)
            if kernel_set_matches(
                ks,
                grid.fine_seconds,
                n_fine,
                &filtered_hvac,
                &filtered_loads,
            ) =>
        {
            ks
        }
        _ => {
            fresh = build_kernels(
                ss,
                net,
                grid.fine_seconds,
                n_fine,
                hvac_zones,
                controllable_loads,
            );
            &fresh
        }
    };

    // Free response: drift under the known inputs (all actuators off) — reusing the cached
    // discretization (the matrix exponential this refactor exists to avoid). Sampled at each grid
    // block's END (identity on a uniform grid, since `fine_range(j) == {j}` there).
    let traj = ss.simulate_with(&ks.disc, x0, u_known)?;
    let mut free_response = HashMap::new();
    for z in &ks.controlled {
        if let Some(row) = zone_row(z) {
            let fine_series: Vec<f64> = (1..=n_fine).map(|k| traj[k][row]).collect();
            free_response.insert(z.clone(), grid.sample_end(&fine_series));
        }
    }

    // Outlook: continue from the horizon-END fine state (traj[n_fine]), not x0, over the outlook's
    // own known inputs — reusing `ks.disc` (dt-only, so valid regardless of `ks.horizon`'s
    // kernel-lag count). The outlook is never fed to the LP, so it stays on its own (fine) lattice,
    // unaffected by the grid's block structure.
    let mut outlook_free_response = HashMap::new();
    if !outlook_u.is_empty() {
        let outlook_traj = ss.simulate_with(&ks.disc, &traj[n_fine], outlook_u)?;
        let m = outlook_u.len();
        for z in &ks.controlled {
            if let Some(row) = zone_row(z) {
                outlook_free_response
                    .insert(z.clone(), (1..=m).map(|k| outlook_traj[k][row]).collect());
            }
        }
    }

    // Each kernel is a per-(target, source) Vec indexed by LAG (index 0 = lag 1). A cache whose own
    // `ks.horizon` is LONGER than this build's `n_fine` (rework cycle 1, finding 2: `kernel_set_
    // matches` now accepts `ks.horizon >= n_fine`) still has every lag `1..=n_fine` right — a
    // kernel's value at a given lag depends only on the model/dt, never on how many lags were
    // computed — so taking the PREFIX is bit-identical to a fresh build at `n_fine`, not merely
    // large-enough-to-index-safely. Keeps every downstream reader's kernel length equal to
    // `n_fine` regardless of which cache horizon served it (`prune_negligible_pairs`'
    // whole-horizon influence sum, in particular, would otherwise sum an extra tail lag).
    let truncate =
        |m: &HashMap<(String, String), Vec<f64>>| -> HashMap<(String, String), Vec<f64>> {
            m.iter()
                .map(|(k, v)| (k.clone(), v[..n_fine].to_vec()))
                .collect()
        };

    Ok(ThermalContext {
        grid: grid.clone(),
        horizon: grid.len(),
        heated_zones: ks.heated_zones.clone(),
        hvac_zones: ks.hvac_zones.clone(),
        free_response,
        outlook_free_response,
        kernels: truncate(&ks.kernels),
        air_kernels: truncate(&ks.air_kernels),
        load_kernels: truncate(&ks.load_kernels),
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

    fn utc(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[allow(clippy::type_complexity)]
    fn fixture() -> (
        RcNetwork,
        StateSpace,
        DVector<f64>,
        Vec<DVector<f64>>,
        f64,
        usize,
        BlockGrid,
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
        let grid = BlockGrid::uniform(utc("2026-01-15T00:00:00Z"), n, dt);
        (net, ss, x0, u_known, dt, n, grid)
    }

    #[test]
    fn affine_prediction_matches_simulate() {
        let (net, ss, x0, u_known, dt, n, grid) = fixture();
        // Treat zone "a" as also HVAC-served (an air-node actuator) on top of both zones' slabs —
        // the keystone check that the affine map matches a full simulate for slab + air fluxes.
        let ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &["a".to_string()],
            &[],
            &[],
            None,
        )
        .unwrap();
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

    /// Keystone (b): on a MULTI-RATE grid (1h fine + 1h hourly = 5 blocks over the same 8 fine
    /// steps `fixture()` already builds `u_known` for), the affine `predict` at every block's end
    /// must equal a full fine-lattice `simulate` in which the hourly block's decision is held
    /// CONSTANT over its four fine steps — the aggregation `ThermalContext::predict` does on the
    /// fly. Mirrors `affine_prediction_matches_simulate`, generalized to per-block (not per-fine-
    /// step) schedules.
    #[test]
    fn multi_rate_affine_prediction_matches_fine_simulate() {
        let (net, ss, x0, u_known, dt, _n, _uniform_grid) = fixture();
        let grid = BlockGrid::multi_rate(utc("2026-01-15T00:00:00Z"), 2, 1, dt);
        assert_eq!(grid.len(), 5, "4 fine blocks + 1 hourly block");
        assert_eq!(grid.n_fine(), 8, "matches fixture()'s u_known length");

        let ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &["a".to_string()],
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(ctx.horizon, 5);

        // Per-BLOCK schedules (5 entries, not 8): block 4 (the hourly one) is ONE decision applied
        // over its whole hour.
        let heat: HashMap<String, Vec<f64>> = HashMap::from([
            ("a".to_string(), vec![2.0, 1.0, 0.0, 0.0, 1.5]),
            ("b".to_string(), vec![0.0, 0.0, 3.0, 0.0, 0.5]),
        ]);
        let air: HashMap<String, Vec<f64>> =
            HashMap::from([("a".to_string(), vec![0.0, 0.0, 1.5, 0.0, -1.0])]);

        // Reference: expand each block's decision to constant power over its fine_range, run a full
        // fine-lattice simulate, and compare against `predict` at every block's end.
        let mut u_full = u_known.clone();
        for source in &ctx.heated_zones {
            let nodes = net
                .marker_indices
                .get_vec(&(source.clone(), "heating".to_string()))
                .cloned()
                .unwrap();
            let m = nodes.len() as f64;
            for (b, &kw) in heat[source].iter().enumerate().take(grid.len()) {
                let p = Power::new::<kilowatt>(kw / m);
                for f in grid.fine_range(b) {
                    for &node in &nodes {
                        ss.set_flux(&mut u_full[f], node, p);
                    }
                }
            }
        }
        let air_node = net.zone_indices["a"];
        for (b, &kw) in air["a"].iter().enumerate().take(grid.len()) {
            let p = Power::new::<kilowatt>(kw);
            for f in grid.fine_range(b) {
                ss.set_flux(&mut u_full[f], air_node, p);
            }
        }
        let traj_full = ss.simulate(&x0, &u_full, dt).unwrap();

        let no_loads: HashMap<String, Vec<f64>> = HashMap::new();
        for zone in &ctx.heated_zones {
            let row = ss.state_index(net.zone_indices[zone]).unwrap();
            for b in 0..grid.len() {
                let traj_idx = grid.fine_range(b).end; // traj[0]=x0, traj[i]=state after i fine steps
                assert_abs_diff_eq!(
                    ctx.predict(zone, b + 1, &heat, &air, &no_loads),
                    traj_full[traj_idx][row],
                    epsilon = 1e-6
                );
            }
        }
    }

    /// Rework cycle 1, finding 2: a cache built at a LONGER horizon (here 8, standing in for the
    /// live 144-step startup cache) must serve a SHORTER multi-rate request without rebuilding —
    /// `kernel_set_matches`' `ks.horizon >= n_fine` — and the truncated-prefix kernels it returns
    /// must be bit-identical to a fresh build at that shorter `n_fine`, not merely long enough to
    /// index safely. Mirrors the live shape: a startup cache at `HORIZON_BLOCKS` (144) vs. a
    /// `:15`/`:30`/`:45` start's multi-rate `n_fine` (143/142/141) — every quarter-hour but `:00`
    /// used to miss the exact-equality match and pay two full kernel rebuilds per tick.
    #[test]
    fn kernel_cache_reused_for_a_shorter_multi_rate_grid() {
        let (net, ss, x0, u_known, dt, n, _uniform_grid) = fixture();
        let ks = build_kernels(&ss, &net, dt, n, &[], &[]);

        // :15 past the hour: fine_hours=1 rounds the fine section up to the next hour boundary, so
        // n_fine comes out SHORTER (7) than the cache's own horizon (8, from `fixture()`'s n).
        let start = utc("2026-01-15T00:15:00Z");
        let grid = BlockGrid::multi_rate(start, 2, 1, dt);
        let n_fine = grid.n_fine();
        assert!(
            n_fine < n,
            "the whole point of this test: a shorter request than the cache's horizon"
        );

        let before = KERNEL_BUILD_COUNT.with(|c| c.get());
        let cached_ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known[..n_fine],
            &grid,
            &[],
            &[],
            &[],
            Some(&ks),
        )
        .unwrap();
        let after = KERNEL_BUILD_COUNT.with(|c| c.get());
        assert_eq!(
            after, before,
            "a matching (longer) cache must not trigger a rebuild"
        );

        let fresh_ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known[..n_fine],
            &grid,
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(cached_ctx.kernels.len(), fresh_ctx.kernels.len());
        for (key, fresh_k) in &fresh_ctx.kernels {
            assert_eq!(
                &cached_ctx.kernels[key], fresh_k,
                "{key:?}: cached-prefix kernel must match a fresh build bit-for-bit"
            );
        }
    }

    /// A controllable load registered at zone "a"'s air node gets a kernel keyed by its **name**,
    /// matching the HVAC air kernel for the same zone (it's the same air-node pulse) — and `predict`
    /// drives the prediction with that load's signed schedule.
    #[test]
    fn controllable_load_kernel_matches_air_kernel_and_predicts() {
        let (net, ss, x0, u_known, _dt, n, grid) = fixture();
        let ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &[],
            &[("boiler".to_string(), "a".to_string())],
            &[],
            None,
        )
        .unwrap();
        // The load kernel onto its own zone equals the air-node kernel for that zone (same 1 kW pulse).
        let load_k = &ctx.load_kernels[&("a".to_string(), "boiler".to_string())];
        let air_ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &["a".to_string()],
            &[],
            &[],
            None,
        )
        .unwrap();
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
        let (net, ss, x0, u_known, _dt, n, grid) = fixture();
        let ctx = build_context(&ss, &net, &x0, &u_known, &grid, &[], &[], &[], None).unwrap();
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
        let (net, ss, x0, u_known, _dt, _n, grid) = fixture();
        let ctx = build_context(&ss, &net, &x0, &u_known, &grid, &[], &[], &[], None).unwrap();
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
        let (net, ss, x0, u_known, _dt, n, grid) = fixture();
        let ctx = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &["a".to_string()],
            &[],
            &[],
            None,
        )
        .unwrap();
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
    #[test]
    fn cached_kernels_build_identical_context() {
        let (net, ss, x0, u_known, dt, n, grid) = fixture();
        let hvac = vec!["a".to_string()];
        let loads = vec![("boiler".to_string(), "a".to_string())];
        let ks = build_kernels(&ss, &net, dt, n, &hvac, &loads);
        let fresh =
            build_context(&ss, &net, &x0, &u_known, &grid, &hvac, &loads, &[], None).unwrap();
        let cached = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &hvac,
            &loads,
            &[],
            Some(&ks),
        )
        .unwrap();
        // Bit-identical: the cache is the same math, just precomputed.
        assert_eq!(fresh.heated_zones, cached.heated_zones);
        assert_eq!(fresh.hvac_zones, cached.hvac_zones);
        assert_eq!(fresh.free_response, cached.free_response);
        assert_eq!(fresh.kernels, cached.kernels);
        assert_eq!(fresh.air_kernels, cached.air_kernels);
        assert_eq!(fresh.load_kernels, cached.load_kernels);

        // A mismatched cache (different horizon / actuated sets) falls back to a fresh build
        // rather than silently serving stale kernels.
        let stale = build_kernels(&ss, &net, dt, n + 4, &hvac, &loads);
        let rebuilt = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &hvac,
            &loads,
            &[],
            Some(&stale),
        )
        .unwrap();
        assert_eq!(rebuilt.kernels, fresh.kernels);
        let other_hvac = build_kernels(&ss, &net, dt, n, &[], &loads);
        let rebuilt = build_context(
            &ss,
            &net,
            &x0,
            &u_known,
            &grid,
            &hvac,
            &loads,
            &[],
            Some(&other_hvac),
        )
        .unwrap();
        assert_eq!(rebuilt.air_kernels, fresh.air_kernels);
    }
}
