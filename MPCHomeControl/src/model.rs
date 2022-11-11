use std::collections::HashMap;
use std::fs;
use std::iter::once;
use std::path::Path;

use itertools::chain;
use serde::Deserialize;
use uom::si::f64::{
    Area, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity, ThermalConductivity,
    Volume,
};

#[derive(Debug)]
pub struct Model {
    pub materials: HashMap<String, Material>,
    pub boundary_types: HashMap<String, BoundaryType>,
    pub zones: HashMap<String, Zone>,
    pub boundaries: Vec<Boundary>,
}

impl Model {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        let loaded: ModelTmp = json5::from_str(&string)?;
        let converted: Model = loaded.into();
        converted.verify()?;
        Ok(converted)
    }

    fn verify(&self) -> anyhow::Result<()> {
        for (boundary_name, boundary_type) in &self.boundary_types {
            boundary_type.verify_references(boundary_name, self)?;
        }

        for boundary in &self.boundaries {
            boundary.verify_references(self)?;
        }

        Ok(())
    }
}

impl From<ModelTmp> for Model {
    fn from(value: ModelTmp) -> Self {
        Model {
            materials: value.materials,
            boundary_types: value.boundary_types,
            zones: value
                .zones
                .into_iter()
                .flat_map(|(zone_name, zone)| zone.expanded_adjanced_zones(zone_name))
                .collect(),
            boundaries: value
                .boundaries
                .into_iter()
                .flat_map(|boundary| boundary.expanded_sub_boundaries())
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Material {
    pub thermal_conductivity: ThermalConductivity,
    pub specific_heat_capacity: SpecificHeatCapacity,
    pub density: MassDensity,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum BoundaryType {
    Layered { layers: Vec<BoundaryLayer> },
    Simple { u: HeatTransfer, g: Ratio },
}

impl BoundaryType {
    fn verify_references(&self, self_name: &str, model: &Model) -> anyhow::Result<()> {
        match self {
            BoundaryType::Layered { layers } => {
                for (i, layer) in layers.iter().enumerate() {
                    if model.materials.get(&layer.material).is_none() {
                        anyhow::bail!(
                            "Material {:?} (referenced from boundary type {:?}, layer {}) not found",
                            layer.material, self_name, i
                        );
                    }
                }
                Ok(())
            }
            BoundaryType::Simple { u: _, g: _ } => Ok(()),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct BoundaryLayer {
    pub material: String,
    pub thickness: Length,
}

#[derive(Debug, PartialEq)]
pub struct Boundary {
    pub boundary_type: String,
    pub zones: [String; 2],
    pub area: Area,
}

impl Boundary {
    fn verify_references(&self, model: &Model) -> anyhow::Result<()> {
        if model.boundary_types.get(&self.boundary_type).is_none() {
            anyhow::bail!(
                "Boundary type {:?} (referenced from {:?}-{:?} boundary) not found",
                self.boundary_type,
                self.zones[0],
                self.zones[1]
            );
        }
        for zone in &self.zones {
            if model.zones.get(zone).is_none() {
                anyhow::bail!(
                    "Zone {:?} (referenced from {:?}-{:?} boundary) not found",
                    zone,
                    self.zones[0],
                    self.zones[1]
                );
            }
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub enum Zone {
    Inner { volume: Volume },
    Outer,
}

#[derive(Debug, Deserialize)]
struct ModelTmp {
    materials: HashMap<String, Material>,
    boundary_types: HashMap<String, BoundaryType>,
    zones: HashMap<String, ZoneTmp>,
    boundaries: Vec<BoundaryTmp>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum ZoneTmp {
    Inner {
        volume: Volume,
        #[serde(default)]
        adjacent_zones: Vec<AdjacentZone>,
    },
    Outer,
}

impl ZoneTmp {
    fn expanded_adjanced_zones(self, name: String) -> impl Iterator<Item = (String, Zone)> {
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
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct AdjacentZone {
    suffix: String,
    boundary_type: String,
    area: Area,
}

#[derive(Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(Clone))]
struct BoundaryTmp {
    boundary_type: String,
    zones: [String; 2],
    area: Area,
    #[serde(default)]
    sub_boundaries: Vec<SubBoundary>,
}

impl BoundaryTmp {
    /// Convert a boundary to an iterator of boundaries with expanded sub boundaries.
    fn expanded_sub_boundaries(self) -> impl Iterator<Item = Boundary> {
        let remaining_area = self.area - self.sub_boundaries.iter().map(|x| x.area).sum::<Area>();
        let cloned_zones = self.zones.clone();
        chain!(
            self.sub_boundaries
                .into_iter()
                .map(move |sub_boundary| Boundary {
                    boundary_type: sub_boundary.boundary_type,
                    zones: cloned_zones.clone(),
                    area: sub_boundary.area,
                }),
            once(Boundary {
                boundary_type: self.boundary_type,
                zones: self.zones,
                area: remaining_area,
            })
        )
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(Clone))]
struct SubBoundary {
    boundary_type: String,
    area: Area,
}

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
