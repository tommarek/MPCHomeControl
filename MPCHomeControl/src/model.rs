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

        loaded.boundaries = loaded.boundaries
            .iter()
            .flat_map(|boundary| boundary.expanded_sub_boundaries())
            .collect();

        Ok(loaded)
    }

    fn verify(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct Material {
    pub thermal_conductivity: f64,
    pub specific_heat_capacity: f64,
    pub density: f64,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct BoundaryLayer {
    pub material: String,
    pub thickness: f64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Zone {
     Inner {
         volume: f64,
         #[serde(default)]
         uncontrollable: bool
     },
     Outer,
}

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
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
