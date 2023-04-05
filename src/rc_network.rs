use std::collections::HashMap;
use std::fmt;

use itertools::Itertools;
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

#[derive(Debug)]
pub struct Node {
    pub zone_name: Option<String>,
    pub heat_capacity: HeatCapacity,
    pub boundary_group_index: Option<usize>, // Groups edges belonging to the same boundary, only for display
}

#[derive(Debug)]
pub struct Edge {
    pub conductance: ThermalConductance,
}

#[derive(Debug)]
pub struct RcNetwork {
    pub graph: UnGraph<Node, Edge>,
}

#[derive(Debug)]
pub struct DotDisplayer<'a> {
    rc_network: &'a RcNetwork,
}

impl<'a> RcNetwork {
    pub fn to_dot(&'a self) -> DotDisplayer<'a> {
        DotDisplayer { rc_network: self }
    }
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
        write!(f, "{} J/K", self.heat_capacity.get::<joule_per_kelvin>())
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} W/K", self.conductance.get::<watt_per_kelvin>())
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
                    name,
                    graph.add_node(Node {
                        zone_name: Some(name.clone()),
                        heat_capacity: zone.heat_capacity(model),
                        boundary_group_index: None,
                    }),
                )
            })
            .collect();

        for (i, boundary) in model.boundaries.iter().enumerate() {
            let z1 = zone_indices[&boundary.zones[0].name];
            let z2 = zone_indices[&boundary.zones[1].name];
            let convection_conductance =
                air_convection_conductance(Velocity::new::<meter_per_second>(0.0)) * boundary.area;

            match boundary.boundary_type.as_ref() {
                BoundaryType::Layered { name: _, layers } => {
                    add_layered_boundary_nodes(
                        &mut graph,
                        z1,
                        z2,
                        layers,
                        boundary.area,
                        convection_conductance,
                        i,
                    );
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

        RcNetwork { graph }
    }
}

/// Add nodes corresponding to the boundary layers to the graph, including connections.
fn add_layered_boundary_nodes(
    graph: &mut UnGraph<Node, Edge>,
    z1: NodeIndex,
    z2: NodeIndex,
    layers: &Vec<BoundaryLayer>,
    area: Area,
    convection_conductance: ThermalConductance,
    boundary_group_index: usize,
) {
    let mut nodes = Vec::with_capacity(layers.len() + 1);
    nodes.push(add_boundary_outer_node(
        graph,
        z1,
        layers.first().unwrap(),
        area,
        convection_conductance,
        boundary_group_index,
    ));
    for (layer1, layer2) in layers.iter().tuple_windows() {
        nodes.push(graph.add_node(Node {
            zone_name: None,
            heat_capacity: (layer1.heat_capacity(area) + layer2.heat_capacity(area)) / 2.0,
            boundary_group_index: Some(boundary_group_index),
        }));
    }
    nodes.push(add_boundary_outer_node(
        graph,
        z2,
        layers.last().unwrap(),
        area,
        convection_conductance,
        boundary_group_index,
    ));

    for (layer, (node1, node2)) in layers.iter().zip(nodes.iter().tuple_windows()) {
        graph.add_edge(
            *node1,
            *node2,
            Edge {
                conductance: layer.conductance(area),
            },
        );
    }
}

/// Create a node in which a layered boundary meets with a zone, connect it to the
/// zone node and return it.
fn add_boundary_outer_node(
    graph: &mut UnGraph<Node, Edge>,
    zone_node: NodeIndex,
    boundary_layer: &BoundaryLayer,
    area: Area,
    convection_conductance: ThermalConductance,
    boundary_group_index: usize,
) -> NodeIndex {
    let new_node = graph.add_node(Node {
        zone_name: None,
        heat_capacity: boundary_layer.heat_capacity(area) / 2.0,
        boundary_group_index: Some(boundary_group_index),
    });
    graph.add_edge(
        zone_node,
        new_node,
        Edge {
            conductance: convection_conductance,
        },
    );
    new_node
}

/// Return thermal conductance of a surface in air.
/// Based on https://www.engineeringtoolbox.com/convective-heat-transfer-d_430.html
pub fn air_convection_conductance(wind_speed: Velocity) -> HeatTransfer {
    let wind_speed = wind_speed.get::<meter_per_second>();
    HeatTransfer::new::<watt_per_square_meter_kelvin>(
        12.12 - 1.16 * wind_speed + 11.6 * wind_speed.sqrt(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra::{assert_approx_eq_eps, ApproxEq};

    #[test]
    fn air_convection_conductance_example() {
        // Just a single point picked from graph in
        // https://www.engineeringtoolbox.com/convective-heat-transfer-d_430.html
        let conductance = air_convection_conductance(Velocity::new::<meter_per_second>(14.5));
        assert_approx_eq_eps!(conductance.get::<watt_per_square_meter_kelvin>(), 40.0, 1.0);
    }
}
