use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::iter::once;

use json5;
use serde::Deserialize;
use itertools::chain;

#[derive(Debug, Deserialize)]
pub struct Model {
    pub materials: HashMap<String, Material>,
    pub boundary_types: HashMap<String, BoundaryType>,
    pub zones: HashMap<String, Zone>,
    pub boundaries: Vec<Boundary>,
}

impl Model {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        let mut loaded = json5::from_str::<Self>(&string)?;

        loaded.verify()?;
        loaded.expand_sub_boundaries();

        Ok(loaded)
    }

    fn verify(&self) -> anyhow::Result<()> {
        for (boundary_name, boundary_type) in &self.boundary_types {
            boundary_type.verify_references(&boundary_name, self)?;
        }

        for boundary in &self.boundaries {
            boundary.verify_references(self)?;
        }

        Ok(())
    }

    fn expand_sub_boundaries(&mut self) {
        self.boundaries = self.boundaries
            .iter()
            .flat_map(|boundary| boundary.expanded_sub_boundaries())
            .collect();
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Material {
    pub thermal_conductivity: f64,
    pub specific_heat_capacity: f64,
    pub density: f64,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum BoundaryType {
    Layered {
        layers: Vec<BoundaryLayer>
    },
    Simple {
        u: f64,
        g: f64
    }
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
            BoundaryType::Simple { u: _, g: _ } => Ok(())
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct BoundaryLayer {
    pub material: String,
    pub thickness: f64,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Zone {
     Inner {
         volume: f64,
         #[serde(default)]
         uncontrollable: bool
     },
     Outer,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Boundary {
    boundary_type: String,
    zones: [String; 2],
    area: f64,
    #[serde(default)]
    sub_boundaries: Vec<SubBoundary>,
}

impl Boundary {
    /// Convert a boundary to an iterator of boundaries with expanded sub boundaries.
    fn expanded_sub_boundaries<'a>(&'a self) -> impl Iterator<Item = Self> + 'a {
        let remaining_area = self.area - self.sub_boundaries.iter().map(|x| x.area).sum::<f64>();
        chain!(
            self.sub_boundaries
                .iter()
                .map(|sub_boundary| Boundary {
                    boundary_type: sub_boundary.boundary_type.clone(),
                    zones: self.zones.clone(),
                    area: sub_boundary.area,
                    sub_boundaries: Vec::new()
                }),
            once(Boundary {
                boundary_type: self.boundary_type.clone(),
                zones: self.zones.clone(),
                area: remaining_area,
                sub_boundaries: Vec::new()
            })
        )
    }

    fn verify_references(&self, model: &Model) -> anyhow::Result<()> {
        if model.boundary_types.get(&self.boundary_type).is_none() {
            anyhow::bail!(
                "Boundary type {:?} (referenced from {:?}-{:?} boundary) not found",
                self.boundary_type, self.zones[0], self.zones[1]
            );
        }
        for zone in &self.zones {
            if model.zones.get(zone).is_none() {
                anyhow::bail!(
                    "Zone {:?} (referenced from {:?}-{:?} boundary) not found",
                    zone, self.zones[0], self.zones[1]
                );
            }
        }

        Ok(())
    }
}


#[derive(Debug, Deserialize, PartialEq)]
pub struct SubBoundary {
    boundary_type: String,
    area: f64,
}

/*
#[derive(Debug)]
struct DoorWindow { // PKS window Uw = 0.72-0.96
    surface_area: f64,
    glass_surface_area: f64,
    u: f64, // heat transfer coef [W/m^2.K]
    g: f64, // solar g-factor - energy transmittance
}

#[derive(Debug)]
struct OpenSpace {
    u: f64, // heat transfer coef [W/m^2.K]
    surface_area: f64,
}

#[derive(Debug)]
struct WallLayer<T> {
    material: T,
    thickness: f64, // [m]
}
#[derive(Debug)]
struct WallSpec {
    layers: Vec<WallLayer>
}
#[derive(Debug)]
struct Wall {
    wall_type: WallType,
    surface_area: f64, // [m^2]
    t1: f64, // [°C]
    t2: f64, // [°C]
    q1: f64, // [W]
    q2: f64, // [W]
    u1: f64, // heat transfer coef [W/m^2.K]
    u2: f64, // heat transfer coef [W/m^2.K]
}

#[derive(Debug)]
struct HorizontalWall { // floor/ceiling
    outer_zone: Zone,
    outer_layers: Vec<WallLayer>,
    inner_zone: Zone,
    inner_layers: Vec<WallLayer>,
    surface_area: f64, // [m^2]
    t1: f64, // [°C]
    t2: f64, // [°C]
    t3: f64, // [°C]
    q1: f64, // [W] floor heating
}

enum WallTypes {
    HorizontalWall,
    Wall,
    DoorWindow,
    OpenSpace,
}

// doors
let entrance_door = DoorWindow {
    u: 0.83, // heat transfer coef [W/m^2.K]
    g: 0.52, // solar g-factor - energy transmittance
    surface_area: 2.42, // [m^2]
}

// windows
let entrance_window = DoorWindow {
    u: 0.74, // heat transfer coef [W/m^2.K]
    g: 0.5, // solar g-factor - energy transmittance
    surface_area: 2.42, // [m^2]
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use test_strategy::proptest;
    //use more_asserts::*;

    /// Test Boundary::expanded_sub_boundaries when no actual expansion is necessary
    #[proptest]
    fn expanded_sub_boundaries_no_expansion(
        boundary_type: String,
        zones: [String; 2],
        area: f64,
    ) {
        let b = Boundary { boundary_type, zones, area, sub_boundaries: Vec::new() };
        let expanded: Vec<_> = b.expanded_sub_boundaries().collect();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], b);
    }

    /// Test Boundary::expanded_sub_boundaries on a hand crafted example
    #[test]
    fn expanded_sub_boundaries_example() {
        let b = Boundary {
            boundary_type: "t1".to_string(),
            zones: ["z1".to_string(), "z2".to_string()],
            area: 100.0,
            sub_boundaries: vec![
                SubBoundary { boundary_type: "t2".to_string(), area: 3.0 },
                SubBoundary { boundary_type: "t3".to_string(), area: 1.0 },
                SubBoundary { boundary_type: "t4".to_string(), area: 4.0 },
            ]
        };
        let expanded: Vec<_> = b.expanded_sub_boundaries().collect();
        // TODO: This test is fragile w.r.t. output boundary order.
        assert_eq!(expanded, vec![
            Boundary {
                boundary_type: "t2".to_string(),
                zones: ["z1".to_string(), "z2".to_string()],
                area: 3.0,
                sub_boundaries: vec![]
            },
            Boundary {
                boundary_type: "t3".to_string(),
                zones: ["z1".to_string(), "z2".to_string()],
                area: 1.0,
                sub_boundaries: vec![]
            },
            Boundary {
                boundary_type: "t4".to_string(),
                zones: ["z1".to_string(), "z2".to_string()],
                area: 4.0,
                sub_boundaries: vec![]
            },
            Boundary {
                boundary_type: "t1".to_string(),
                zones: ["z1".to_string(), "z2".to_string()],
                area: 92.0,
                sub_boundaries: vec![]
            },
        ]);
    }

    #[test]
    fn test_model_expand_sub_boundaries() {
        let mut model = Model {
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
            zones: HashMap::new(),
            boundaries: vec![
                Boundary {
                    boundary_type: "t1".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: 100.0,
                    sub_boundaries: vec![
                        SubBoundary { boundary_type: "t2".to_string(), area: 3.0 },
                        SubBoundary { boundary_type: "t3".to_string(), area: 1.0 },
                        SubBoundary { boundary_type: "t4".to_string(), area: 4.0 },
                    ]
                },
                Boundary {
                    boundary_type: "t5".to_string(),
                    zones: ["z3".to_string(), "z4".to_string()],
                    area: 1.0,
                    sub_boundaries: vec![]
                },
            ]
        };

        model.expand_sub_boundaries();

        // TODO: This test is fragile w.r.t. output boundary order.
        assert_eq!(
            model.boundaries,
            vec![
                Boundary {
                    boundary_type: "t2".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: 3.0,
                    sub_boundaries: vec![]
                },
                Boundary {
                    boundary_type: "t3".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: 1.0,
                    sub_boundaries: vec![]
                },
                Boundary {
                    boundary_type: "t4".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: 4.0,
                    sub_boundaries: vec![]
                },
                Boundary {
                    boundary_type: "t1".to_string(),
                    zones: ["z1".to_string(), "z2".to_string()],
                    area: 92.0,
                    sub_boundaries: vec![]
                },
                Boundary {
                    boundary_type: "t5".to_string(),
                    zones: ["z3".to_string(), "z4".to_string()],
                    area: 1.0,
                    sub_boundaries: vec![]
                },
            ]
        );

    }
}
