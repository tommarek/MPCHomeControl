use std::collections::HashMap;
use std::fs;
use std::iter::once;
use std::path::Path;
use std::rc::Rc;

use itertools::chain;
use serde::Deserialize;
use uom::si::f64::{
    Area, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity, ThermalConductivity,
    Volume,
};

// TODO: All convert() method should be fallible!

#[derive(Clone, Debug)]
pub struct Model {
    pub zones: HashMap<String, Rc<Zone>>,
    pub boundaries: Vec<Boundary>,
}

impl Model {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        let loaded: ModelTmp = json5::from_str(&string)?;
        let converted: Model = loaded.into();
        Ok(converted)
    }
}

impl From<ModelTmp> for Model {
    fn from(value: ModelTmp) -> Self {
        let converted_materials: HashMap<_, _> = value
            .materials
            .into_iter()
            .map(|(name, material)| (name.clone(), Rc::new(material.convert(name))))
            .collect();
        let converted_boundary_types: HashMap<_, _> = value
            .boundary_types
            .into_iter()
            .map(|(name, boundary_type)| {
                (
                    name.clone(),
                    Rc::new(boundary_type.convert(name, &converted_materials)),
                )
            })
            .collect();
        let converted_zones: HashMap<String, Rc<Zone>> = value
            .zones
            .into_iter()
            .flat_map(|(zone_name, zone)| zone.convert(zone_name))
            .map(|(zone_name, zone)| (zone_name, Rc::new(zone)))
            .collect();
        let converted_boundaries = value
            .boundaries
            .into_iter()
            .flat_map(|boundary| boundary.convert(&converted_zones, &converted_boundary_types))
            .collect();
        Model {
            zones: converted_zones,
            boundaries: converted_boundaries,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Zone {
    Inner { volume: Volume },
    Outer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Boundary {
    pub boundary_type: Rc<BoundaryType>,
    pub zones: [Rc<Zone>; 2],
    pub area: Area,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BoundaryType {
    Layered {
        name: String,
        layers: Vec<BoundaryLayer>,
    },
    Simple {
        name: String,
        u: HeatTransfer,
        g: Ratio,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundaryLayer {
    pub material: Rc<Material>,
    pub thickness: Length,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Material {
    pub name: String,
    pub thermal_conductivity: ThermalConductivity,
    pub specific_heat_capacity: SpecificHeatCapacity,
    pub density: MassDensity,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelTmp {
    zones: HashMap<String, ZoneTmp>,
    boundaries: Vec<BoundaryTmp>,
    materials: HashMap<String, MaterialTmp>,
    boundary_types: HashMap<String, BoundaryTypeTmp>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum ZoneTmp {
    Inner {
        volume: Volume,
        #[serde(default)]
        adjacent_zones: Vec<AdjacentZone>,
    },
    Outer,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct AdjacentZone {
    suffix: String,
    boundary_type: String,
    area: Area,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct BoundaryTmp {
    boundary_type: String,
    zones: [String; 2],
    area: Area,
    #[serde(default)]
    sub_boundaries: Vec<SubBoundary>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct SubBoundary {
    boundary_type: String,
    area: Area,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum BoundaryTypeTmp {
    Layered {
        layers: Vec<BoundaryLayerTmp>,
    },
    /// Simple boundaries don't have any mass!
    Simple {
        u: HeatTransfer,
        g: Ratio,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct MaterialTmp {
    thermal_conductivity: ThermalConductivity,
    specific_heat_capacity: SpecificHeatCapacity,
    density: MassDensity,
}

impl BoundaryTypeTmp {
    fn convert(self, name: String, materials: &HashMap<String, Rc<Material>>) -> BoundaryType {
        match self {
            BoundaryTypeTmp::Layered { layers } => BoundaryType::Layered {
                name,
                layers: layers
                    .into_iter()
                    .map(|layer| layer.convert(materials))
                    .collect(),
            },
            BoundaryTypeTmp::Simple { u, g } => BoundaryType::Simple { name, u, g },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct BoundaryLayerTmp {
    pub material: String,
    pub thickness: Length,
}

impl BoundaryLayerTmp {
    fn convert(self, materials: &HashMap<String, Rc<Material>>) -> BoundaryLayer {
        BoundaryLayer {
            material: Rc::clone(&materials[&self.material]),
            thickness: self.thickness,
        }
    }
}

impl ZoneTmp {
    /// Convert the as-loaded zone definition into an iterator of "proper" zones.
    /// Expands the adjanced zones into separate zone.
    fn convert(self, name: String) -> impl Iterator<Item = (String, Zone)> {
        let cloned_name = name.clone();
        let (adjanced, ret) = match self {
            Self::Inner {
                volume,
                adjacent_zones,
            } => (adjacent_zones, Zone::Inner { volume }),
            Self::Outer => (Vec::new(), Zone::Outer),
        };

        chain!(
            adjanced.into_iter().map(move |adj_zone| (
                format!("{}/{}", cloned_name, adj_zone.suffix),
                Zone::Inner {
                    volume: Default::default()
                }
            )),
            once((name, ret))
        )

        // TODO: Each adjanced zone also needs a bounary connection! The model will be broken until this is implemented!
    }
}

impl BoundaryTmp {
    /// Convert a boundary to an iterator of boundaries with expanded sub boundaries.
    fn convert<'a>(
        self,
        zones: &'a HashMap<String, Rc<Zone>>,
        boundary_types: &'a HashMap<String, Rc<BoundaryType>>,
    ) -> impl Iterator<Item = Boundary> + 'a {
        let remaining_area = self.area - self.sub_boundaries.iter().map(|x| x.area).sum::<Area>();
        let zone_pair1 = [
            Rc::clone(&zones[&self.zones[0]]),
            Rc::clone(&zones[&self.zones[1]]),
        ];
        let zone_pair2 = zone_pair1.clone(); // Just a trick, to be allow to move those into the
                                             // map functions
        chain!(
            self.sub_boundaries
                .into_iter()
                .map(move |sub_boundary| Boundary {
                    boundary_type: Rc::clone(&boundary_types[&sub_boundary.boundary_type]),
                    zones: zone_pair1.clone(),
                    area: sub_boundary.area,
                }),
            once(Boundary {
                boundary_type: Rc::clone(&boundary_types[&self.boundary_type]),
                zones: zone_pair2,
                area: remaining_area,
            })
        )
    }
}

impl MaterialTmp {
    fn convert(self, name: String) -> Material {
        Material {
            name,
            thermal_conductivity: self.thermal_conductivity,
            specific_heat_capacity: self.specific_heat_capacity,
            density: self.density,
        }
    }
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use test_strategy::proptest;
    use uom::si::area::square_meter;
    //use more_asserts::*;

    /// Test Boundary::expanded_sub_boundaries when no actual expansion is necessary
    #[proptest]
    fn expanded_sub_boundaries_no_expansion(boundary_type: String, zones: [String; 2], area: f64) {
        let area = Area::new::<square_meter>(area);
        let b = BoundaryTmp {
            boundary_type,
            zones,
            area,
            sub_boundaries: Vec::new(),
        };
        let expanded: Vec<_> = b.clone().expanded_sub_boundaries().collect();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].boundary_type, b.boundary_type);
        assert_eq!(expanded[0].zones, b.zones);
        assert_eq!(expanded[0].area, b.area);
    }

    /// Test Boundary::expanded_sub_boundaries on a hand crafted example
    #[test]
    fn expanded_sub_boundaries_example() {
        let b = BoundaryTmp {
            boundary_type: "t1".to_string(),
            zones: ["z1".to_string(), "z2".to_string()],
            area: Area::new::<square_meter>(100.0),
            sub_boundaries: vec![
                SubBoundary {
                    boundary_type: "t2".to_string(),
                    area: Area::new::<square_meter>(3.0),
                },
                SubBoundary {
                    boundary_type: "t3".to_string(),
                    area: Area::new::<square_meter>(1.0),
                },
                SubBoundary {
                    boundary_type: "t4".to_string(),
                    area: Area::new::<square_meter>(4.0),
                },
            ],
        };
        let expanded: Vec<_> = b.expanded_sub_boundaries().collect();
        // TODO: This test is fragile w.r.t. output boundary order.
        assert_eq!(
            expanded,
            vec![
                Boundary {
                    boundary_type: "t2".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: Area::new::<square_meter>(3.0),
                },
                Boundary {
                    boundary_type: "t3".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: Area::new::<square_meter>(1.0),
                },
                Boundary {
                    boundary_type: "t4".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: Area::new::<square_meter>(4.0),
                },
                Boundary {
                    boundary_type: "t1".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: Area::new::<square_meter>(92.0),
                },
            ]
        );
    }
}
*/
