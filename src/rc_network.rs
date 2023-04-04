use std::collections::HashMap;

use itertools::Itertools;
use petgraph::graph::{NodeIndex, UnGraph};
use uom::si::f64::{Area, HeatCapacity, ThermalConductance};

use crate::model::{BoundaryLayer, BoundaryType, Model};

#[derive(Debug)]
pub struct Node {
    pub zone_name: Option<String>,
    pub heat_capacity: HeatCapacity,
}

#[derive(Debug)]
pub struct Edge {
    pub conductance: ThermalConductance,
}

#[derive(Debug)]
pub struct RcNetwork {
    pub graph: UnGraph<Node, Edge>,
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
                    }),
                )
            })
            .collect();

        for boundary in &model.boundaries {
            let z1 = zone_indices[&boundary.zones[0].name];
            let z2 = zone_indices[&boundary.zones[1].name];
            let convection_conductance: ThermalConductance = boundary.convection_conductance();

            match boundary.boundary_type.as_ref() {
                BoundaryType::Layered { name: _, layers } => {
                    add_layered_boundary_nodes(
                        &mut graph,
                        z1,
                        z2,
                        layers,
                        boundary.area,
                        convection_conductance,
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
) {
    let mut nodes = Vec::with_capacity(layers.len() + 1);
    nodes.push(add_boundary_outer_node(
        graph,
        z1,
        layers.first().unwrap(),
        area,
        convection_conductance,
    ));
    for (layer1, layer2) in layers.iter().tuple_windows() {
        nodes.push(graph.add_node(Node {
            zone_name: None,
            heat_capacity: (layer1.heat_capacity(area) + layer2.heat_capacity(area)) / 2.0,
        }));
    }
    nodes.push(add_boundary_outer_node(
        graph,
        z2,
        layers.last().unwrap(),
        area,
        convection_conductance,
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
) -> NodeIndex {
    let new_node = graph.add_node(Node {
        zone_name: None,
        heat_capacity: boundary_layer.heat_capacity(area) / 2.0,
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
