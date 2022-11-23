use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use typenum::{N1, N3, P1, P2, Z0};
use uom::si::{
    f64::{
        Area, HeatCapacity, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity,
        ThermalConductivity, Volume,
    },
    volume::cubic_meter,
    Quantity, ISQ, SI,
};

/// Thermal conductance, base unit is Watt/Kelvin
pub type ThermalConductance = Quantity<
    ISQ<
        P2, // length
        P1, // mass
        N3, // time
        Z0, // electric current
        N1, // thermodynamic temperature
        Z0, // amount of substance
        Z0,
    >,
    SI<f64>,
    f64,
>;

#[derive(Clone, Debug)]
pub struct Model {
    pub zones: HashMap<String, Rc<Zone>>,
    pub boundaries: Vec<Boundary>,
    pub air: Rc<Material>,
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
            let (zone_volume, adjacent_zones) = match zone {
                as_loaded::Zone::Inner {
                    volume,
                    adjacent_zones,
                } => (Some(volume), adjacent_zones),
                as_loaded::Zone::Outer => (None, Vec::new()),
            };
            let zone_rc = Rc::new(Zone {
                name: zone_name.clone(),
                volume: zone_volume,
            });

            for adjacent_zone in adjacent_zones {
                let adj_zone_name = format!("{}/{}", zone_name, adjacent_zone.suffix);
                let adj_zone_rc = Rc::new(Zone {
                    name: adj_zone_name.clone(),
                    volume: Some(Volume::new::<cubic_meter>(0.0)),
                });
                converted_zones.insert(adj_zone_name, Rc::clone(&adj_zone_rc));
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

        let air = get(&converted_materials, "air", "material")?;

        Ok(Model {
            zones: converted_zones,
            boundaries: converted_boundaries,
            air,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub name: String,
    pub volume: Option<Volume>,
}

impl Zone {
    pub fn heat_capacity(&self, model: &Model) -> HeatCapacity {
        self.volume
            .unwrap_or_else(|| Volume::new::<cubic_meter>(f64::INFINITY))
            * model.air.density
            * model.air.specific_heat_capacity
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Boundary {
    pub boundary_type: Rc<BoundaryType>,
    pub zones: [Rc<Zone>; 2],
    pub area: Area,
}

impl Boundary {
    pub fn convection_conductance(&self) -> ThermalConductance {
        todo!("Figure out how to model this!")
    }
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

impl BoundaryLayer {
    pub fn heat_capacity(&self, area: Area) -> HeatCapacity {
        let volume = area * self.thickness;
        let material_mass = volume * self.material.density;
        material_mass * self.material.specific_heat_capacity
    }

    pub fn conductance(&self, area: Area) -> ThermalConductance {
        self.material.thermal_conductivity * area / self.thickness
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub name: String,
    pub thermal_conductivity: ThermalConductivity,
    pub specific_heat_capacity: SpecificHeatCapacity,
    pub density: MassDensity,
}

fn get<K, V, Q>(h: &HashMap<K, Rc<V>>, key: &Q, label: &str) -> anyhow::Result<Rc<V>>
where
    K: std::borrow::Borrow<Q>,
    K: std::hash::Hash + std::cmp::Eq,
    Q: std::hash::Hash + std::cmp::Eq + std::fmt::Debug + ?Sized,
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
                BoundaryType::Layered { layers } if layers.is_empty() => {
                    anyhow::bail!("Boundary type {:?} has empty layer list", name)
                }
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

    #[derive(Clone, Debug, Deserialize, PartialEq, Default)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use uom::si::{
        area::square_meter, heat_transfer::watt_per_square_meter_kelvin, length::meter,
        mass_density::kilogram_per_cubic_meter, ratio::percent,
        specific_heat_capacity::joule_per_kilogram_kelvin,
        thermal_conductivity::watt_per_meter_kelvin,
    };

    #[test]
    fn convert_material() {
        let input = as_loaded::Material {
            thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(123.0),
            specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(456.0),
            density: MassDensity::new::<kilogram_per_cubic_meter>(789.0),
        };

        let output = input.convert("qwertyuiop".into());

        assert_eq!(output.name, "qwertyuiop");
        assert_eq!(
            output.thermal_conductivity,
            ThermalConductivity::new::<watt_per_meter_kelvin>(123.0)
        );
        assert_eq!(
            output.specific_heat_capacity,
            SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(456.0)
        );
        assert_eq!(
            output.density,
            MassDensity::new::<kilogram_per_cubic_meter>(789.0)
        );
    }

    #[test]
    fn convert_boundary_layer() {
        let input = as_loaded::BoundaryLayer {
            material: "mat1".into(),
            thickness: Length::new::<meter>(0.2),
        };
        let materials = converted_materials_hashmap();

        let output = input.convert(&materials).unwrap();

        assert_eq!(output.thickness, Length::new::<meter>(0.2));
        assert!(Rc::ptr_eq(&output.material, &materials["mat1"]));
    }

    #[test]
    fn convert_boundary_layer_missing_material() {
        let input = as_loaded::BoundaryLayer {
            material: "mat1".into(),
            thickness: Length::new::<meter>(0.2),
        };
        let materials = HashMap::new();

        let message = format!("{}", input.convert(&materials).unwrap_err());

        message
            .find("material")
            .expect("Error message should contain what type of object was missing");
        message
            .find("mat1")
            .expect("Error message should contain the name of the object");
    }

    #[test]
    fn convert_boundary_type_layered() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![
                as_loaded::BoundaryLayer {
                    material: "mat1".into(),
                    thickness: Length::new::<meter>(1.0),
                },
                as_loaded::BoundaryLayer {
                    material: "mat2".into(),
                    thickness: Length::new::<meter>(2.0),
                },
            ],
        };
        let materials = converted_materials_hashmap();

        let output = input.convert("somename".to_string(), &materials).unwrap();

        assert_matches!(output, BoundaryType::Layered { name, layers } => {
            assert_eq!(name, "somename");
            assert_eq!(layers.len(), 2);
            assert!(Rc::ptr_eq(&layers[0].material, &materials["mat1"]));
            assert!(Rc::ptr_eq(&layers[1].material, &materials["mat2"]));
            assert_eq!(layers[0].thickness, Length::new::<meter>(1.0));
            assert_eq!(layers[1].thickness, Length::new::<meter>(2.0));
        });
    }

    #[test]
    fn convert_boundary_type_simple() {
        let input = as_loaded::BoundaryType::Simple {
            u: HeatTransfer::new::<watt_per_square_meter_kelvin>(123.0),
            g: Ratio::new::<percent>(90.0),
        };
        let materials = HashMap::new();

        let output = input.convert("somename".to_string(), &materials).unwrap();

        assert_matches!(output, BoundaryType::Simple { name, u, g } => {
            assert_eq!(name, "somename");
            assert_eq!(u, HeatTransfer::new::<watt_per_square_meter_kelvin>(123.0));
            assert_eq!(g, Ratio::new::<percent>(90.0));
        });
    }

    #[test]
    fn convert_boundary_type_layered_missing_material() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![
                as_loaded::BoundaryLayer {
                    material: "matX".into(),
                    thickness: Length::new::<meter>(1.0),
                },
                as_loaded::BoundaryLayer {
                    material: "mat2".into(),
                    thickness: Length::new::<meter>(2.0),
                },
            ],
        };
        let materials = converted_materials_hashmap();

        let message = format!(
            "{}",
            input
                .convert("somename".to_string(), &materials)
                .unwrap_err()
        );

        message
            .find("material")
            .expect("Error message should contain what type of object was missing");
        message
            .find("matX")
            .expect("Error message should contain the name of the object");
    }

    #[test]
    fn convert_boundary_type_no_layers() {
        let input = as_loaded::BoundaryType::Layered { layers: vec![] };
        let materials = converted_materials_hashmap();

        let message = format!(
            "{}",
            input
                .convert("somename".to_string(), &materials)
                .unwrap_err()
        );

        message
            .find("somename")
            .expect("Error message should contain the name of the bad boundary type");
    }

    /// Tests the conversion of a minimal valid model
    #[test]
    fn convert_model_minimal() {
        let input = as_loaded::Model {
            zones: HashMap::new(),
            boundaries: vec![],
            materials: HashMap::from([("air".into(), Default::default())]),
            boundary_types: HashMap::new(),
        };

        let output: Model = input.try_into().unwrap();

        assert!(output.zones.is_empty());
        assert!(output.boundaries.is_empty());
    }

    #[test]
    fn convert_model_zones() {
        let input = as_loaded::Model {
            zones: HashMap::from([(
                "z1".into(),
                as_loaded::Zone::Inner {
                    volume: Volume::new::<cubic_meter>(123.0),
                    adjacent_zones: vec![
                        as_loaded::AdjacentZone {
                            suffix: "adj1".into(),
                            boundary_type: "bt".into(),
                            area: Area::new::<square_meter>(12.0),
                        },
                        as_loaded::AdjacentZone {
                            suffix: "adj2".into(),
                            boundary_type: "bt".into(),
                            area: Area::new::<square_meter>(32.0),
                        },
                    ],
                },
            )]),
            boundaries: Vec::new(),
            materials: HashMap::from([("air".into(), Default::default())]),
            boundary_types: HashMap::from([(
                "bt".into(),
                as_loaded::BoundaryType::Simple {
                    u: Default::default(),
                    g: Default::default(),
                },
            )]),
        };

        let output: Model = input.try_into().unwrap();

        let z1 = Rc::new(Zone {
            name: "z1".into(),
            volume: Some(Volume::new::<cubic_meter>(123.0)),
        });
        let z1_adj1 = Rc::new(Zone {
            name: "z1/adj1".into(),
            volume: Some(Volume::new::<cubic_meter>(0.0)),
        });
        let z1_adj2 = Rc::new(Zone {
            name: "z1/adj2".into(),
            volume: Some(Volume::new::<cubic_meter>(0.0)),
        });
        let bt = Rc::new(BoundaryType::Simple {
            name: "bt".into(),
            u: Default::default(),
            g: Default::default(),
        });

        assert_eq!(
            output.zones,
            HashMap::from([
                ("z1".into(), Rc::clone(&z1)),
                ("z1/adj1".into(), Rc::clone(&z1_adj1)),
                ("z1/adj2".into(), Rc::clone(&z1_adj2)),
            ])
        );
        // This is fragile wrt. ordering of boundaries. Any order is valid, but the comparison only accepts one.
        assert_eq!(
            output.boundaries,
            vec![
                Boundary {
                    boundary_type: Rc::clone(&bt),
                    zones: [Rc::clone(&z1), Rc::clone(&z1_adj1)],
                    area: Area::new::<square_meter>(12.0),
                },
                Boundary {
                    boundary_type: Rc::clone(&bt),
                    zones: [Rc::clone(&z1), Rc::clone(&z1_adj2)],
                    area: Area::new::<square_meter>(32.0),
                },
            ]
        );
    }

    #[test]
    fn convert_model_boundaries() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                ("z1".into(), as_loaded::Zone::Outer),
                ("z2".into(), as_loaded::Zone::Outer),
            ]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt1".into(),
                zones: ["z1".into(), "z2".into()],
                area: Area::new::<square_meter>(123.0),
                sub_boundaries: vec![
                    as_loaded::SubBoundary {
                        boundary_type: "bt2".into(),
                        area: Area::new::<square_meter>(1.0),
                    },
                    as_loaded::SubBoundary {
                        boundary_type: "bt3".into(),
                        area: Area::new::<square_meter>(2.0),
                    },
                ],
            }],
            materials: HashMap::from([("air".into(), Default::default())]),
            boundary_types: HashMap::from([
                (
                    "bt1".into(),
                    as_loaded::BoundaryType::Simple {
                        u: Default::default(),
                        g: Default::default(),
                    },
                ),
                (
                    "bt2".into(),
                    as_loaded::BoundaryType::Simple {
                        u: Default::default(),
                        g: Default::default(),
                    },
                ),
                (
                    "bt3".into(),
                    as_loaded::BoundaryType::Simple {
                        u: Default::default(),
                        g: Default::default(),
                    },
                ),
            ]),
        };

        let output: Model = input.try_into().unwrap();

        let z1 = Rc::new(Zone {
            name: "z1".into(),
            volume: None,
        });
        let z2 = Rc::new(Zone {
            name: "z2".into(),
            volume: None,
        });
        let bt1 = Rc::new(BoundaryType::Simple {
            name: "bt1".into(),
            u: Default::default(),
            g: Default::default(),
        });
        let bt2 = Rc::new(BoundaryType::Simple {
            name: "bt2".into(),
            u: Default::default(),
            g: Default::default(),
        });
        let bt3 = Rc::new(BoundaryType::Simple {
            name: "bt3".into(),
            u: Default::default(),
            g: Default::default(),
        });

        assert_eq!(
            output.zones,
            HashMap::from([("z1".into(), Rc::clone(&z1)), ("z2".into(), Rc::clone(&z2)),])
        );
        // This is fragile wrt. ordering of boundaries. Any order is valid, but the comparison only accepts one.
        assert_eq!(
            output.boundaries,
            vec![
                Boundary {
                    boundary_type: Rc::clone(&bt2),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(1.0),
                },
                Boundary {
                    boundary_type: Rc::clone(&bt3),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(2.0),
                },
                Boundary {
                    boundary_type: Rc::clone(&bt1),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(120.0),
                },
            ]
        );
    }

    #[test]
    fn convert_model_too_large_sub_boundaries() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                ("z1".into(), as_loaded::Zone::Outer),
                ("z2".into(), as_loaded::Zone::Outer),
            ]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["z1".into(), "z2".into()],
                area: Area::new::<square_meter>(1.0),
                sub_boundaries: vec![as_loaded::SubBoundary {
                    boundary_type: "bt".into(),
                    area: Area::new::<square_meter>(2.0),
                }],
            }],
            materials: HashMap::from([("air".into(), Default::default())]),
            boundary_types: HashMap::from([(
                "bt".into(),
                as_loaded::BoundaryType::Simple {
                    u: Default::default(),
                    g: Default::default(),
                },
            )]),
        };

        let message = format!("{}", Model::try_from(input).unwrap_err());
        message
            .find("sub-boundaries")
            .expect("Error message should say that there's a problem with sub boundary");
        message
            .find("z1")
            .expect("Error message should contain the name of the problematic zones");
        message
            .find("z2")
            .expect("Error message should contain the name of the problematic zones");
    }

    #[test]
    fn convert_model_bad_zone_link() {
        let input = as_loaded::Model {
            zones: HashMap::from([("goodzone".into(), as_loaded::Zone::Outer)]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["goodzone".into(), "badzone".into()],
                area: Area::new::<square_meter>(1.0),
                sub_boundaries: Vec::new(),
            }],
            materials: HashMap::from([("air".into(), Default::default())]),
            boundary_types: HashMap::from([(
                "bt".into(),
                as_loaded::BoundaryType::Simple {
                    u: Default::default(),
                    g: Default::default(),
                },
            )]),
        };

        let message = format!("{}", Model::try_from(input).unwrap_err());
        message
            .find("zone")
            .expect("Error message should say that there's a problem with a zone");
        message
            .find("badzone")
            .expect("Error message should contain the name of the problematic zone");
    }

    #[test]
    fn convert_model_missing_air() {
        let input = as_loaded::Model {
            zones: HashMap::new(),
            boundaries: vec![],
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
        };

        let message = format!("{}", Model::try_from(input).unwrap_err());
        message
            .find("material")
            .expect("Error message should say that there's a problem with a material");
        message
            .find("air")
            .expect("Error message should contain the name of the problematic material");
    }

    #[test]
    fn load_model() {
        let mut f = tempfile::NamedTempFile::new().unwrap();

        use std::io::Write;
        write!(
            f,
            "{}",
            r#"{
            materials: {
                air: {
                    thermal_conductivity: 0,
                    specific_heat_capacity: 0,
                    density: 0,
                },
                brick: {
                    thermal_conductivity: 1,
                    specific_heat_capacity: 2,
                    density: 3,
                }
            },
            boundary_types: {
                wall: {
                    layers: [
                        {
                            material: "brick",
                            thickness: 0.1,
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
                    boundary_type: "wall",
                    zones: ["a", "b"],
                    area: 10,
                    sub_boundaries: [
                        { boundary_type: "window", area: 1 }
                    ]
                }
            ],
        }"#
        )
        .unwrap();

        let model = Model::load(f.path()).unwrap();

        assert_matches!(model.zones["a"].as_ref(), Zone { name, volume } => {
            assert_eq!(name, "a");
            assert_eq!(volume, &Some(Volume::new::<cubic_meter>(123.0)));
        });

        assert_eq!(model.boundaries.len(), 2);
        assert_matches!(&model.boundaries[1].boundary_type.as_ref(), &BoundaryType::Layered{ name, layers } => {
            assert_eq!(name, "wall");
            assert_eq!(layers[0].material.name, "brick");
            assert_eq!(layers[0].thickness, Length::new::<meter>(0.1));
        });
    }

    /// Provide an example hash map with converted material
    fn converted_materials_hashmap() -> HashMap<String, Rc<Material>> {
        HashMap::from([
            (
                "mat1".into(),
                Rc::new(Material {
                    name: "mat1".into(),
                    thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(123.0),
                    specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(
                        456.0,
                    ),
                    density: MassDensity::new::<kilogram_per_cubic_meter>(789.0),
                }),
            ),
            (
                "mat2".into(),
                Rc::new(Material {
                    name: "mat2".into(),
                    thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(23.0),
                    specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(
                        56.0,
                    ),
                    density: MassDensity::new::<kilogram_per_cubic_meter>(89.0),
                }),
            ),
        ])
    }
}
