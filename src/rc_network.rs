use std::collections::HashMap;
use std::fmt;

use itertools::Itertools;
use multimap::MultiMap;
use petgraph::{
    graph::{NodeIndex, UnGraph},
    visit::{EdgeRef, IntoNodeReferences, NodeIndexable},
};
use uom::si::{
    f64::{Area, HeatCapacity, HeatTransfer, ThermalConductance, Velocity},
    heat_capacity::joule_per_kelvin,
    heat_transfer::watt_per_square_meter_kelvin,
    thermal_conductance::watt_per_kelvin,
    velocity::meter_per_second,
};

use crate::model::{BoundaryLayer, BoundaryType, Model};

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub zone_name: Option<String>,
    pub marker: Option<(String, String)>,
    pub heat_capacity: HeatCapacity,
    pub boundary_group_index: Option<usize>, // Groups edges belonging to the same boundary, only for display
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Edge {
    pub conductance: ThermalConductance,
}

#[derive(Clone, Debug)]
pub struct RcNetwork {
    pub graph: UnGraph<Node, Edge>,

    /// Mapping of zone names to node indices.
    /// Used to reference named nodes in the graph
    pub zone_indices: HashMap<String, NodeIndex>,

    /// Mapping of (zone name, marker) pairs to node indices
    pub marker_indices: MultiMap<(String, String), NodeIndex>,
}

#[derive(Copy, Clone, Debug)]
pub struct DotDisplayer<'a> {
    rc_network: &'a RcNetwork,
}

impl<'a> fmt::Display for DotDisplayer<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let g = &self.rc_network.graph;

        let mut ungrouped_nodes: Vec<_> = Vec::new();
        let mut grouped_nodes: Vec<Vec<_>> = Vec::new();

        for (index, node) in g.node_references() {
            let index = g.to_index(index);
            if let Some(boundary_group_index) = node.boundary_group_index {
                if grouped_nodes.len() <= boundary_group_index {
                    grouped_nodes.resize_with(boundary_group_index + 1, Default::default);
                }
                grouped_nodes[boundary_group_index].push((index, node));
            } else {
                ungrouped_nodes.push((index, node));
            }
        }

        writeln!(f, "graph {{")?;
        for (index, node) in ungrouped_nodes {
            writeln!(f, "    node_{} [ label = \"{}\" ]", index, node)?;
        }

        for (index, group) in grouped_nodes.iter().enumerate() {
            writeln!(f, "    subgraph cluster_{} {{", index)?;
            for (index, node) in group {
                writeln!(f, "        node_{} [ label = \"{}\" ]", index, node)?;
            }
            writeln!(f, "    }}")?;
        }

        for edge in g.edge_references() {
            writeln!(
                f,
                "    node_{} -- node_{} [ label = \"{}\" ]",
                g.to_index(edge.source()),
                g.to_index(edge.target()),
                edge.weight()
            )?
        }

        writeln!(f, "}}")
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.zone_name {
            write!(f, "{name}\\n")?;
        }
        if let Some((zone, marker)) = &self.marker {
            write!(f, "{zone}/{marker}\\n")?;
        }
        write!(f, "{} J/K", self.heat_capacity.get::<joule_per_kelvin>())
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} W/K", self.conductance.get::<watt_per_kelvin>())
    }
}

impl<'a> RcNetwork {
    pub fn to_dot(&'a self) -> DotDisplayer<'a> {
        DotDisplayer { rc_network: self }
    }
}

impl From<&Model> for RcNetwork {
    fn from(model: &Model) -> Self {
        let mut graph = UnGraph::default();
        let zone_indices: HashMap<_, _> = model
            .zones
            .iter()
            .map(|(name, zone)| {
                (
                    name.clone(),
                    graph.add_node(Node {
                        zone_name: Some(name.clone()),
                        marker: None,
                        heat_capacity: zone.heat_capacity(&model.air),
                        boundary_group_index: None,
                    }),
                )
            })
            .collect();
        let mut marker_indices: MultiMap<_, _> = MultiMap::new();

        let mut boundary_group_index = 0;
        for boundary in model.boundaries.iter() {
            let z1 = zone_indices[&boundary.zones[0].name];
            let z2 = zone_indices[&boundary.zones[1].name];
            let convection_conductance =
                air_convection_conductance(Velocity::new::<meter_per_second>(0.0)) * boundary.area;

            match boundary.boundary_type.as_ref() {
                BoundaryType::Layered {
                    name: _,
                    layers,
                    initial_marker,
                } => {
                    add_layered_boundary_nodes(
                        &mut graph,
                        z1,
                        z2,
                        layers,
                        initial_marker,
                        boundary.area,
                        convection_conductance,
                        boundary_group_index,
                        &boundary.zones[0].name,
                        &mut marker_indices,
                    );
                    boundary_group_index += 1;
                }
                BoundaryType::Simple { name: _, u, g: _ } => {
                    graph.add_edge(
                        z1,
                        z2,
                        Edge {
                            conductance: 1f64
                                / (1f64 / convection_conductance
                                    + 1f64 / (*u * boundary.area)
                                    + 1f64 / convection_conductance),
                        },
                    );
                }
            }
        }

        RcNetwork {
            graph,
            zone_indices,
            marker_indices,
        }
    }
}

/// Add nodes corresponding to the boundary layers to the graph, including connections.
fn add_layered_boundary_nodes(
    graph: &mut UnGraph<Node, Edge>,
    z1: NodeIndex,
    z2: NodeIndex,
    layers: &[BoundaryLayer],
    initial_marker: &Option<String>,
    area: Area,
    convection_conductance: ThermalConductance,
    boundary_group_index: usize,
    zone_name: &str,
    marker_indices: &mut MultiMap<(String, String), NodeIndex>,
) {
    let mut current_node = add_boundary_node(
        graph,
        layers.first().unwrap().heat_capacity(area) / 2.0,
        z1,
        convection_conductance,
        boundary_group_index,
        zone_name,
        initial_marker,
        marker_indices,
    );

    for (layer1, layer2) in layers.iter().tuple_windows() {
        current_node = add_boundary_node(
            graph,
            (layer1.heat_capacity(area) + layer2.heat_capacity(area)) / 2.0,
            current_node,
            layer1.conductance(area),
            boundary_group_index,
            zone_name,
            &layer1.following_marker,
            marker_indices,
        );
    }

    let last_layer = layers.last().unwrap();

    current_node = add_boundary_node(
        graph,
        last_layer.heat_capacity(area) / 2.0,
        current_node,
        last_layer.conductance(area),
        boundary_group_index,
        zone_name,
        &last_layer.following_marker,
        marker_indices,
    );

    graph.add_edge(
        current_node,
        z2,
        Edge {
            conductance: convection_conductance,
        },
    );
}

/// Add a new node on a boundary between two nodes, process its markers and connect
/// it to the graph.
/// This is used both for the nodes within the boundary and for nodes between the
/// boundary and the zone.
fn add_boundary_node(
    graph: &mut UnGraph<Node, Edge>,
    heat_capacity: HeatCapacity,
    prev_node: NodeIndex,
    thermal_conductance: ThermalConductance,
    boundary_group_index: usize,
    zone_name: &str,
    marker: &Option<String>,
    marker_indices: &mut MultiMap<(String, String), NodeIndex>,
) -> NodeIndex {
    let marker = marker
        .as_ref()
        .map(|marker| (zone_name.into(), marker.clone()));

    let node = graph.add_node(Node {
        zone_name: None,
        marker: marker.clone(),
        heat_capacity,
        boundary_group_index: Some(boundary_group_index),
    });

    if let Some(marker) = marker {
        marker_indices.insert(marker, node);
    }

    graph.add_edge(
        prev_node,
        node,
        Edge {
            conductance: thermal_conductance,
        },
    );

    node
}

/// Return thermal conductance of a surface in air.
/// Based on https://www.engineeringtoolbox.com/convective-heat-transfer-d_430.html
pub fn air_convection_conductance(wind_speed: Velocity) -> HeatTransfer {
    // The calculation is done outside of UOM, because the coefficient units would be awkward
    let wind_speed = wind_speed.get::<meter_per_second>();
    HeatTransfer::new::<watt_per_square_meter_kelvin>(
        12.12 - 1.16 * wind_speed + 11.6 * wind_speed.sqrt(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra::{assert_approx_eq_eps, ApproxEq};
    use test_case::test_case;
    use test_strategy::proptest;

    // The test values are taken from the illustration graph in the source articles,
    // converted to pairs using web plot digitizer. The plot appears to be very imprecise,
    // forcing this test to have very error high tolerance.
    #[test_case( 3.0, 27.4; "example1")]
    #[test_case( 8.0, 35.2; "example2")]
    #[test_case(13.0, 39.3; "example3")]
    #[test_case(18.0, 41.6; "example4")]
    fn air_convection_conductance_example(air_velocity: f64, expected_heat_transfer: f64) {
        let conductance =
            air_convection_conductance(Velocity::new::<meter_per_second>(air_velocity));
        assert_approx_eq_eps!(
            conductance.get::<watt_per_square_meter_kelvin>(),
            expected_heat_transfer,
            1.5
        );
    }

    #[proptest]
    fn graph_node_count(model: Model) {
        let mut expected_node_count = model.zones.len();
        let mut expected_edge_count = 0;
        for boundary in model.boundaries.iter() {
            match boundary.boundary_type.as_ref() {
                BoundaryType::Simple {
                    name: _,
                    u: _,
                    g: _,
                } => expected_edge_count += 1,
                BoundaryType::Layered {
                    name: _,
                    layers,
                    initial_marker: _,
                } => {
                    expected_node_count += layers.len() + 1;
                    expected_edge_count += layers.len() + 2;
                }
            }
        }

        let net: RcNetwork = (&model).into();

        assert_eq!(net.graph.node_count(), expected_node_count);
        assert_eq!(net.graph.edge_count(), expected_edge_count);
    }

    /*
    #[test]
    fn two_zones() {
        let model = Model::from_json(
            r#"{
            materials: {
                air: {
                    thermal_conductivity: 1,
                    specific_heat_capacity: 1,
                    density: 1,
                },
                m1: {
                    thermal_conductivity: 1,
                    specific_heat_capacity: 2,
                    density: 3,
                },
                m2: {
                    thermal_conductivity: 4,
                    specific_heat_capacity: 5,
                    density: 6,
                }
            },
            boundary_types: {
                bt: {
                    layers: [
                        {
                            material: "m1",
                            thickness: 1,
                        },
                        {
                            material: "m2",
                            thickness: 1,
                        }
                    ]
                },
                window: {
                    u: 1,
                    g: 2,
                }
            },
            zones: {
                a: { volume: 123 },
                b: null,
            },
            boundaries: [
                {
                    boundary_type: "bt",
                    zones: ["a", "b"],
                    area: 10,
                },
                {
                    boundary_type: "window",
                    zones: ["a", "b"],
                    area: 10,
                }
            ],
        }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();

        let a = *net.zone_indices.get("a").unwrap();
        let b = *net.zone_indices.get("b").unwrap();

        assert_eq!(net.graph.node_weight(a).unwrap(),
            &Node {
                zone_name: Some("a".into()),
                heat_capacity: HeatCapacity::new::<joule_per_kelvin>(123.0),
                boundary_group_index: None
            }
        );

        use std::io::Write;
        let mut file = std::fs::File::create("/tmp/graph.dot").unwrap();
        write!(file, "{}", net.to_dot()).unwrap();
    }
    */
}
