use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use uom::si::f64::{
    Area, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity, ThermalConductivity,
    Volume,
};

#[derive(Clone, Debug)]
pub struct Model {
    pub zones: HashMap<String, Rc<Zone>>,
    pub boundaries: Vec<Boundary>,
}

impl Model {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        let loaded: as_loaded::Model = json5::from_str(&string)?;
        let converted = loaded.try_into()?;
        Ok(converted)
    }
}

impl TryFrom<as_loaded::Model> for Model {
    type Error = anyhow::Error;
    fn try_from(value: as_loaded::Model) -> Result<Self, Self::Error> {
        let converted_materials: HashMap<_, _> = value
            .materials
            .into_iter()
            .map(|(name, material)| (name.clone(), Rc::new(material.convert(name))))
            .collect();
        let converted_boundary_types = value
            .boundary_types
            .into_iter()
            .map(|(name, boundary_type)| {
                Ok((
                    name.clone(),
                    Rc::new(boundary_type.convert(name, &converted_materials)?),
                ))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let mut converted_zones = HashMap::new();
        let mut converted_boundaries = Vec::new();

        for (zone_name, zone) in value.zones.into_iter() {
            let (zone_rc, adjacent_zones) = match zone {
                as_loaded::Zone::Inner {
                    volume,
                    adjacent_zones,
                } => (Rc::new(Zone::Inner { volume }), adjacent_zones),
                as_loaded::Zone::Outer => (Rc::new(Zone::Outer), Vec::new()),
            };

            for adjacent_zone in adjacent_zones {
                let adj_zone_rc = Rc::new(Zone::Inner {
                    volume: Default::default(),
                });
                converted_zones.insert(
                    format!("{}/{}", zone_name, adjacent_zone.suffix),
                    Rc::clone(&adj_zone_rc),
                );
                converted_boundaries.push(Boundary {
                    boundary_type: Rc::clone(
                        &converted_boundary_types[&adjacent_zone.boundary_type],
                    ),
                    zones: [Rc::clone(&zone_rc), adj_zone_rc],
                    area: adjacent_zone.area,
                })
            }

            converted_zones.insert(zone_name, zone_rc);
        }

        for boundary in value.boundaries.into_iter() {
            let mut remaining_area = boundary.area;
            let zone_pair = [
                get(&converted_zones, &boundary.zones[0], "zone")?,
                get(&converted_zones, &boundary.zones[1], "zone")?,
            ];
            for sub_boundary in boundary.sub_boundaries {
                if sub_boundary.area > remaining_area {
                    anyhow::bail!(
                        "Boundary {:?} has less area than the sum of its sub-boundaries",
                        boundary.zones
                    )
                }
                remaining_area -= sub_boundary.area;

                converted_boundaries.push(Boundary {
                    boundary_type: get(
                        &converted_boundary_types,
                        &sub_boundary.boundary_type,
                        "boundary type",
                    )?,
                    zones: zone_pair.clone(),
                    area: sub_boundary.area,
                })
            }

            converted_boundaries.push(Boundary {
                boundary_type: get(
                    &converted_boundary_types,
                    &boundary.boundary_type,
                    "boundary type",
                )?,
                zones: zone_pair,
                area: remaining_area,
            })
        }

        Ok(Model {
            zones: converted_zones,
            boundaries: converted_boundaries,
        })
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

#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub name: String,
    pub thermal_conductivity: ThermalConductivity,
    pub specific_heat_capacity: SpecificHeatCapacity,
    pub density: MassDensity,
}

fn get<'a, K, V, Q>(h: &'a HashMap<K, Rc<V>>, key: &Q, label: &str) -> anyhow::Result<Rc<V>>
where
    K: std::borrow::Borrow<Q>,
    K: std::hash::Hash + std::cmp::Eq,
    Q: std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
{
    Ok(Rc::clone(h.get(key).ok_or_else(|| {
        anyhow::anyhow!("Could not find {} {:?}", label, key)
    })?))
}

mod as_loaded {
    use std::collections::HashMap;
    use std::rc::Rc;

    use serde::Deserialize;
    use uom::si::f64::{
        Area, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity, ThermalConductivity,
        Volume,
    };

    use super::get;

    #[derive(Clone, Debug, Deserialize)]
    pub struct Model {
        pub zones: HashMap<String, Zone>,
        pub boundaries: Vec<Boundary>,
        pub materials: HashMap<String, Material>,
        pub boundary_types: HashMap<String, BoundaryType>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    #[serde(untagged)]
    pub enum Zone {
        Inner {
            volume: Volume,
            #[serde(default)]
            adjacent_zones: Vec<AdjacentZone>,
        },
        Outer,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct AdjacentZone {
        pub suffix: String,
        pub boundary_type: String,
        pub area: Area,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct Boundary {
        pub boundary_type: String,
        pub zones: [String; 2],
        pub area: Area,
        #[serde(default)]
        pub sub_boundaries: Vec<SubBoundary>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct SubBoundary {
        pub boundary_type: String,
        pub area: Area,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    #[serde(untagged)]
    pub enum BoundaryType {
        Layered {
            layers: Vec<BoundaryLayer>,
        },
        /// Simple boundaries don't have any mass!
        Simple {
            u: HeatTransfer,
            g: Ratio,
        },
    }

    impl BoundaryType {
        pub fn convert(
            self,
            name: String,
            materials: &HashMap<String, Rc<super::Material>>,
        ) -> anyhow::Result<super::BoundaryType> {
            Ok(match self {
                BoundaryType::Layered { layers } => super::BoundaryType::Layered {
                    name,
                    layers: layers
                        .into_iter()
                        .map(|layer| layer.convert(materials))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                },
                BoundaryType::Simple { u, g } => super::BoundaryType::Simple { name, u, g },
            })
        }
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct BoundaryLayer {
        pub material: String,
        pub thickness: Length,
    }

    impl BoundaryLayer {
        pub fn convert(
            self,
            materials: &HashMap<String, Rc<super::Material>>,
        ) -> anyhow::Result<super::BoundaryLayer> {
            Ok(super::BoundaryLayer {
                material: get(materials, &self.material, "material")?,
                thickness: self.thickness,
            })
        }
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct Material {
        pub thermal_conductivity: ThermalConductivity,
        pub specific_heat_capacity: SpecificHeatCapacity,
        pub density: MassDensity,
    }

    impl Material {
        pub fn convert(self, name: String) -> super::Material {
            super::Material {
                name,
                thermal_conductivity: self.thermal_conductivity,
                specific_heat_capacity: self.specific_heat_capacity,
                density: self.density,
            }
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
