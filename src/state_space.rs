//! Linear state-space model derived from an [`RcNetwork`].
//!
//! The thermal RC network (nodes = heat capacities, edges = thermal conductances) is
//! turned into a continuous-time linear time-invariant system `x' = A x + B u` and then
//! discretized to `x_{k+1} = Ad x_k + Bd u_k` for simulation (and, later, MPC). See
//! `theory.md` for the physics.
//!
//! - **States** are the finite-heat-capacity nodes (zone air + wall/layer masses).
//! - **Inputs** are, in order: the temperatures of the infinite-capacity *boundary* nodes
//!   (`outside`, `ground`), followed by one heat-flux column per state node (solar,
//!   heating, internal gains). A node with no physical injection simply receives `0`.
//!
//! For each finite node `i`: `C_i dT_i/dt = Σ_j G_ij (T_j − T_i) + Q_i`, which gives
//! `A[i,i] = −ΣG_ij/C_i`, off-diagonal state coupling `A[i,j] = G_ij/C_i`, boundary
//! coupling into `B`, and flux injection `B[i, flux_i] = 1/C_i`.
//!
//! The linear-algebra core works in raw SI `f64` (Kelvin, Watt, J/K, W/K); `uom`
//! conversion is confined to the builder and the typed input setters.

use std::collections::HashMap;

use nalgebra::{DMatrix, DVector};
use petgraph::graph::NodeIndex;
use petgraph::visit::{EdgeRef, IntoNodeReferences, NodeIndexable};
use uom::si::{
    f64::{Power, ThermodynamicTemperature},
    heat_capacity::joule_per_kelvin,
    power::watt,
    thermal_conductance::watt_per_kelvin,
    thermodynamic_temperature::kelvin,
};

use crate::rc_network::RcNetwork;

/// Smallest heat capacity (J/K) used for a state node. Guards against numerical instability
/// from a near-zero computed capacity (e.g. a very thin boundary layer); 1 J/K is a physically
/// negligible thermal mass, so a floored node behaves as a fast-equilibrating massless node
/// rather than producing NaNs.
const MIN_HEAT_CAPACITY: f64 = 1.0;

/// Describes what a single column of the input vector `u` represents.
#[derive(Clone, Debug, PartialEq)]
pub enum InputLabel {
    /// Temperature (Kelvin) of an infinite-capacity boundary node (e.g. `outside`, `ground`).
    BoundaryTemp {
        node: NodeIndex,
        zone_name: Option<String>,
    },
    /// Heat flux (Watt) injected into a state node (solar / heating / internal gains).
    Flux {
        node: NodeIndex,
        zone_name: Option<String>,
        marker: Option<(String, String)>,
    },
}

/// Continuous-time linear state-space model `x' = A x + B u` built from an [`RcNetwork`].
#[derive(Clone, Debug)]
pub struct StateSpace {
    a: DMatrix<f64>,
    b: DMatrix<f64>,
    /// state row index -> graph node
    node_of_state: Vec<NodeIndex>,
    /// graph node -> state row index
    state_of_node: HashMap<NodeIndex, usize>,
    /// boundary node -> temperature input column
    boundary_input: HashMap<NodeIndex, usize>,
    n_boundary: usize,
    labels: Vec<InputLabel>,
}

/// Zero-order-hold discretization of a [`StateSpace`] at a fixed time step `dt` (seconds).
#[derive(Clone, Debug)]
pub struct Discretized {
    pub ad: DMatrix<f64>,
    pub bd: DMatrix<f64>,
}

impl From<&RcNetwork> for StateSpace {
    fn from(net: &RcNetwork) -> Self {
        let graph = &net.graph;

        // Deterministic ordering: sort by petgraph's stable usize node id so matrix
        // structure never depends on HashMap/graph iteration order.
        let mut nodes: Vec<_> = graph
            .node_references()
            .map(|(idx, node)| (graph.to_index(idx), idx, node))
            .collect();
        nodes.sort_by_key(|(id, _, _)| *id);

        let mut node_of_state: Vec<NodeIndex> = Vec::new();
        let mut state_of_node: HashMap<NodeIndex, usize> = HashMap::new();
        let mut boundary_input: HashMap<NodeIndex, usize> = HashMap::new();
        let mut labels: Vec<InputLabel> = Vec::new();

        // Partition nodes: finite capacity -> state, infinite capacity -> boundary input.
        for (_, idx, node) in &nodes {
            if node.heat_capacity.is_finite() {
                state_of_node.insert(*idx, node_of_state.len());
                node_of_state.push(*idx);
            } else {
                boundary_input.insert(*idx, labels.len());
                labels.push(InputLabel::BoundaryTemp {
                    node: *idx,
                    zone_name: node.zone_name.clone(),
                });
            }
        }

        let n_states = node_of_state.len();
        let n_boundary = labels.len();
        let n_inputs = n_boundary + n_states;

        let mut a = DMatrix::<f64>::zeros(n_states, n_states);
        let mut b = DMatrix::<f64>::zeros(n_states, n_inputs);

        let capacity = |idx: NodeIndex| -> f64 {
            graph[idx]
                .heat_capacity
                .get::<joule_per_kelvin>()
                .max(MIN_HEAT_CAPACITY)
        };

        // Each undirected conductance is applied in both directions of the dense matrix.
        for edge in graph.edge_references() {
            let g = edge.weight().conductance.get::<watt_per_kelvin>();
            for (i, j) in [
                (edge.source(), edge.target()),
                (edge.target(), edge.source()),
            ] {
                let Some(&si) = state_of_node.get(&i) else {
                    continue; // boundary nodes own no row
                };
                let ci = capacity(i);
                a[(si, si)] -= g / ci;
                if let Some(&sj) = state_of_node.get(&j) {
                    a[(si, sj)] += g / ci;
                } else if let Some(&bj) = boundary_input.get(&j) {
                    b[(si, bj)] += g / ci;
                }
            }
        }

        // One heat-flux input column per state node.
        for (s, &idx) in node_of_state.iter().enumerate() {
            b[(s, n_boundary + s)] += 1.0 / capacity(idx);
            let node = &graph[idx];
            labels.push(InputLabel::Flux {
                node: idx,
                zone_name: node.zone_name.clone(),
                marker: node.marker.clone(),
            });
        }

        StateSpace {
            a,
            b,
            node_of_state,
            state_of_node,
            boundary_input,
            n_boundary,
            labels,
        }
    }
}

impl StateSpace {
    pub fn n_states(&self) -> usize {
        self.node_of_state.len()
    }

    pub fn n_boundary(&self) -> usize {
        self.n_boundary
    }

    pub fn n_inputs(&self) -> usize {
        self.b.ncols()
    }

    pub fn labels(&self) -> &[InputLabel] {
        &self.labels
    }

    /// State row index for a graph node, if it is a (finite-capacity) state.
    pub fn state_index(&self, node: NodeIndex) -> Option<usize> {
        self.state_of_node.get(&node).copied()
    }

    /// Input column carrying the temperature of a boundary (infinite-capacity) node.
    pub fn boundary_input_column(&self, node: NodeIndex) -> Option<usize> {
        self.boundary_input.get(&node).copied()
    }

    /// Input column carrying the heat flux injected into a state node.
    pub fn flux_input_column(&self, node: NodeIndex) -> Option<usize> {
        self.state_of_node.get(&node).map(|s| self.n_boundary + s)
    }

    /// A zero input vector of the correct length.
    pub fn zero_input(&self) -> DVector<f64> {
        DVector::zeros(self.n_inputs())
    }

    /// Set a boundary-temperature input from a typed temperature (no-op if `node` is not a boundary).
    pub fn set_boundary_temp(
        &self,
        u: &mut DVector<f64>,
        node: NodeIndex,
        temp: ThermodynamicTemperature,
    ) {
        if let Some(col) = self.boundary_input_column(node) {
            u[col] = temp.get::<kelvin>();
        }
    }

    /// Set a heat-flux input from a typed power (no-op if `node` is not a state).
    pub fn set_flux(&self, u: &mut DVector<f64>, node: NodeIndex, power: Power) {
        if let Some(col) = self.flux_input_column(node) {
            u[col] = power.get::<watt>();
        }
    }

    /// Exact zero-order-hold discretization at step `dt` (seconds) via the van Loan
    /// block-matrix exponential `exp([[A, B], [0, 0]] · dt)`. This computes `Bd` without
    /// inverting `A`, so it is correct even when `A` is singular (e.g. an isolated node).
    pub fn discretize(&self, dt: f64) -> Discretized {
        let n = self.n_states();
        let m = self.n_inputs();
        let mut block = DMatrix::<f64>::zeros(n + m, n + m);
        if n > 0 {
            block.view_mut((0, 0), (n, n)).copy_from(&self.a);
            block.view_mut((0, n), (n, m)).copy_from(&self.b);
        }
        let exp = (block * dt).exp();
        let ad = exp.view((0, 0), (n, n)).into_owned();
        let bd = exp.view((0, n), (n, m)).into_owned();
        Discretized { ad, bd }
    }

    /// Advance one step: `x_{k+1} = Ad x_k + Bd u_k` (input held constant over `[t, t+dt]`).
    pub fn step(&self, disc: &Discretized, x: &DVector<f64>, u: &DVector<f64>) -> DVector<f64> {
        &disc.ad * x + &disc.bd * u
    }

    /// Roll the model forward over an input trajectory at fixed `dt`. Returns the state at
    /// `t0, t0+dt, … t0+N·dt` (length `inputs.len() + 1`).
    pub fn simulate(
        &self,
        initial: &DVector<f64>,
        inputs: &[DVector<f64>],
        dt: f64,
    ) -> anyhow::Result<Vec<DVector<f64>>> {
        if initial.len() != self.n_states() {
            anyhow::bail!(
                "initial state has length {}, expected {}",
                initial.len(),
                self.n_states()
            );
        }
        let disc = self.discretize(dt);
        let mut states = Vec::with_capacity(inputs.len() + 1);
        states.push(initial.clone());
        let mut x = initial.clone();
        for (k, u) in inputs.iter().enumerate() {
            if u.len() != self.n_inputs() {
                anyhow::bail!(
                    "input {} has length {}, expected {}",
                    k,
                    u.len(),
                    self.n_inputs()
                );
            }
            x = self.step(&disc, &x, u);
            states.push(x.clone());
        }
        Ok(states)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use approx::assert_relative_eq;
    use test_strategy::proptest;

    /// One finite zone `a` connected to `outside` by a single `Simple` conductance.
    /// Validates A, B, the discretization, and the integrator against the analytic
    /// first-order decay `T(t) = T_out + (T0 − T_out)·exp(−t/τ)`, τ = C/G.
    #[test]
    fn two_node_rc_analytic() {
        let model = Model::from_json(
            r#"{
                materials: { air: { thermal_conductivity: 1, specific_heat_capacity: 1000, density: 1.2 } },
                boundary_types: { win: { u: 2, g: 0 } },
                zones: { a: { volume: 50 } },
                boundaries: [ { boundary_type: "win", zones: ["a", "outside"], area: 3 } ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();

        let c = 50.0 * 1.2 * 1000.0; // V·ρ·c_p = 60_000 J/K
        let g = 2.0 * 3.0; // u·area = 6 W/K (Simple boundary = single resistor)

        assert_eq!(ss.n_states(), 1);
        assert_relative_eq!(ss.a[(0, 0)], -g / c, max_relative = 1e-12);

        let a_node = net.zone_indices["a"];
        let outside = net.zone_indices["outside"];
        assert_eq!(ss.state_index(a_node), Some(0));

        let out_col = ss.boundary_input_column(outside).unwrap();
        assert_relative_eq!(ss.b[(0, out_col)], g / c, max_relative = 1e-12);
        let flux_col = ss.flux_input_column(a_node).unwrap();
        assert_relative_eq!(ss.b[(0, flux_col)], 1.0 / c, max_relative = 1e-12);

        // Analytic decay from 20 °C toward an outside boundary held at 5 °C.
        let tau = c / g;
        let t0 = 293.15;
        let t_out = 278.15;
        let dt = 600.0;
        let steps = 50;
        let mut u = ss.zero_input();
        u[out_col] = t_out;
        let traj = ss
            .simulate(&DVector::from_element(1, t0), &vec![u; steps], dt)
            .unwrap();

        for (k, state) in traj.iter().enumerate() {
            let t = k as f64 * dt;
            let expected = t_out + (t0 - t_out) * (-t / tau).exp();
            assert_relative_eq!(state[0], expected, max_relative = 1e-9);
        }
    }

    /// Steady state under a constant heat flux Q with the boundary at T_out is T_out + Q/G.
    #[test]
    fn steady_state_under_constant_flux() {
        let model = Model::from_json(
            r#"{
                materials: { air: { thermal_conductivity: 1, specific_heat_capacity: 1000, density: 1.2 } },
                boundary_types: { win: { u: 2, g: 0 } },
                zones: { a: { volume: 50 } },
                boundaries: [ { boundary_type: "win", zones: ["a", "outside"], area: 3 } ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();

        let g = 2.0 * 3.0;
        let t_out = 280.0;
        let q = 600.0; // W

        let a_node = net.zone_indices["a"];
        let outside = net.zone_indices["outside"];
        let mut u = ss.zero_input();
        u[ss.boundary_input_column(outside).unwrap()] = t_out;
        ss.set_flux(&mut u, a_node, Power::new::<watt>(q));

        // Long run reaches steady state.
        let traj = ss
            .simulate(&DVector::from_element(1, t_out), &vec![u; 2000], 600.0)
            .unwrap();
        assert_relative_eq!(traj.last().unwrap()[0], t_out + q / g, max_relative = 1e-6);
    }

    /// Every state row of `[A | B_boundary]` must sum to zero — multiplying by C_i recovers
    /// `−ΣG + ΣG_state + ΣG_boundary = 0`. Proves no conductance is dropped or double-counted
    /// and is robust to iteration order. (Flux columns are the only non-balancing term.)
    #[proptest]
    fn row_balance(model: Model) {
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let (n, nb) = (ss.n_states(), ss.n_boundary());
        for i in 0..n {
            let mut sum = 0.0;
            let mut scale = 0.0;
            for j in 0..n {
                sum += ss.a[(i, j)];
                scale += ss.a[(i, j)].abs();
            }
            for b in 0..nb {
                sum += ss.b[(i, b)];
                scale += ss.b[(i, b)].abs();
            }
            assert!(sum.abs() <= 1e-9 * scale.max(1.0));
        }
    }

    /// A uniform temperature with matching boundary temperatures and zero flux is an exact
    /// fixed point of the discretized dynamics (end-to-end check of A, B, and discretization).
    #[proptest]
    fn uniform_temperature_is_fixed_point(model: Model) {
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        if ss.n_states() > 0 {
            let t = 290.0;
            let x0 = DVector::from_element(ss.n_states(), t);
            let mut u = ss.zero_input();
            for b in 0..ss.n_boundary() {
                u[b] = t;
            }
            let disc = ss.discretize(900.0);
            let x1 = ss.step(&disc, &x0, &u);
            for i in 0..ss.n_states() {
                // Exact fixed point in theory; tolerance covers matrix-exponential roundoff,
                // which can be sizeable for stiff random models (tiny floored capacities and
                // huge conductances produce very large A entries). 1e-6 of 290 K is ~0.3 mK.
                assert_relative_eq!(x1[i], t, max_relative = 1e-6);
            }
        }
    }
}
