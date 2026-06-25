use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use uom::si::{
    angle::degree,
    f64::{
        Angle, Area, HeatCapacity, HeatTransfer, Length, MassDensity, Ratio, SpecificHeatCapacity,
        ThermalConductance, ThermalConductivity, Volume,
    },
    heat_capacity::joule_per_kelvin,
    mass_density::kilogram_per_cubic_meter,
    specific_heat_capacity::joule_per_kilogram_kelvin,
    thermal_conductivity::watt_per_meter_kelvin,
};

#[cfg(test)]
use proptest::{
    arbitrary::Arbitrary,
    prelude::{prop, prop_oneof},
    strategy::{BoxedStrategy, Strategy},
};
#[cfg(test)]
use uom::si::{
    area::square_meter, heat_transfer::watt_per_square_meter_kelvin, length::meter, ratio::percent,
    thermal_conductance::watt_per_kelvin, volume::cubic_meter,
};

#[derive(Clone, Debug)]
pub struct Model {
    pub zones: HashMap<String, Rc<Zone>>,
    pub boundaries: Vec<Boundary>,
    pub air: Rc<Material>,
}

impl Model {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        Self::from_json(&string)
    }

    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let loaded: as_loaded::Model = json5::from_str(json)?;
        let converted = loaded.try_into()?;
        Ok(converted)
    }
}

impl TryFrom<as_loaded::Model> for Model {
    type Error = anyhow::Error;
    fn try_from(value: as_loaded::Model) -> Result<Self, Self::Error> {
        let reserved_outer_zones = ["outside", "ground"];
        for z in reserved_outer_zones.iter() {
            if value.zones.contains_key(*z) {
                anyhow::bail!(
                    "'{}' is a reserved zone name and must not be defined in model",
                    z
                );
            }
        }

        let mut converted_materials: HashMap<_, _> = value
            .materials
            .into_iter()
            .map(|(name, material)| (name.clone(), Rc::new(material.convert(name))))
            .collect();

        let default_air = Material::default_air();
        if !converted_materials.contains_key(&default_air.name) {
            converted_materials.insert(default_air.name.clone(), Rc::new(default_air));
        }

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
        let mut converted_zones = value
            .zones
            .into_iter()
            .map(|(name, zone)| {
                (
                    name.clone(),
                    Rc::new(Zone {
                        name,
                        volume: Some(zone.volume),
                    }),
                )
            })
            .collect::<HashMap<_, _>>();
        for z in reserved_outer_zones.iter() {
            converted_zones.insert(
                (*z).into(),
                Rc::new(Zone {
                    name: (*z).into(),
                    volume: None,
                }),
            );
        }

        let mut converted_boundaries = Vec::new();

        for boundary in value.boundaries.into_iter() {
            if boundary.zones[0] == boundary.zones[1] {
                anyhow::bail!("Boundary connects zone {:?} to itself", boundary.zones[0]);
            }
            if boundary.area < Area::default() {
                anyhow::bail!("Boundary {:?} has negative area", boundary.zones);
            }

            let azimuth = boundary.azimuth.map(Angle::new::<degree>);
            let tilt = boundary.angle.map(Angle::new::<degree>);

            let mut remaining_area = boundary.area;
            let zone_pair = [
                get(&converted_zones, &boundary.zones[0], "zone")?,
                get(&converted_zones, &boundary.zones[1], "zone")?,
            ];
            for sub_boundary in boundary.sub_boundaries {
                if sub_boundary.area < Area::default() {
                    anyhow::bail!(
                        "Boundary {:?} has a sub-boundary with negative area",
                        boundary.zones
                    );
                }
                if sub_boundary.area > remaining_area {
                    anyhow::bail!(
                        "Boundary {:?} has less area than the sum of its sub-boundaries",
                        boundary.zones
                    )
                }
                remaining_area -= sub_boundary.area;

                // Skip zero-area boundaries: they would only add inert
                // (zero-capacity, zero-conductance) nodes and edges to the network.
                if sub_boundary.area > Area::default() {
                    converted_boundaries.push(Boundary {
                        boundary_type: get(
                            &converted_boundary_types,
                            &sub_boundary.boundary_type,
                            "boundary type",
                        )?,
                        zones: zone_pair.clone(),
                        area: sub_boundary.area,
                        azimuth,
                        tilt,
                    })
                }
            }

            if remaining_area > Area::default() {
                converted_boundaries.push(Boundary {
                    boundary_type: get(
                        &converted_boundary_types,
                        &boundary.boundary_type,
                        "boundary type",
                    )?,
                    zones: zone_pair,
                    area: remaining_area,
                    azimuth,
                    tilt,
                })
            }
        }

        let air = get(&converted_materials, "air", "material")?;

        Ok(Model {
            zones: converted_zones,
            boundaries: converted_boundaries,
            air,
        })
    }
}

#[cfg(test)]
impl Arbitrary for Model {
    type Parameters = ();
    type Strategy = BoxedStrategy<Model>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        prop::collection::vec(Material::arbitrary().prop_map(Rc::new), 1..10)
            .prop_flat_map(|materials| {
                let materials = Rc::new(materials);
                (
                    prop::strategy::Just(Rc::clone(&materials)),
                    prop::collection::vec(
                        BoundaryType::arbitrary_with(materials).prop_map(Rc::new),
                        1..20,
                    ),
                    prop::collection::vec(Zone::arbitrary().prop_map(Rc::new), 2..10),
                )
            })
            .prop_flat_map(|(materials, boundary_types, zones)| {
                let boundary_types = Rc::new(boundary_types);
                let zones = Rc::new(zones);
                (
                    prop::strategy::Just(materials),
                    prop::strategy::Just(Rc::clone(&zones)),
                    prop::collection::vec(Boundary::arbitrary_with((boundary_types, zones)), 1..10),
                )
            })
            .prop_map(|(materials, mut zones, boundaries)| Model {
                zones: Rc::make_mut(&mut zones)
                    .drain(0..)
                    .map(|z| (z.name.clone(), z))
                    .collect::<HashMap<_, _>>(),
                boundaries,
                air: Rc::clone(materials.iter().next().unwrap()),
            })
            .boxed()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub name: String,
    pub volume: Option<Volume>,
}

impl Zone {
    pub fn heat_capacity(&self, content: &Material) -> HeatCapacity {
        if let Some(volume) = self.volume {
            volume * content.density * content.specific_heat_capacity
        } else {
            HeatCapacity::new::<joule_per_kelvin>(f64::INFINITY)
        }
    }
}

#[cfg(test)]
impl Arbitrary for Zone {
    type Parameters = ();
    type Strategy = BoxedStrategy<Zone>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        ("[a-z]*", prop::option::of(0.1f64..1000f64))
            .prop_map(|tuple| Zone {
                name: tuple.0,
                volume: tuple.1.map(Volume::new::<cubic_meter>),
            })
            .boxed()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Boundary {
    pub boundary_type: Rc<BoundaryType>,
    pub zones: [Rc<Zone>; 2],
    pub area: Area,
    /// Compass azimuth of an exterior surface (degrees) — its orientation for solar gain, read into
    /// the network's `solar_surfaces`.
    pub azimuth: Option<Angle>,
    /// Tilt from horizontal (90° = vertical wall).
    pub tilt: Option<Angle>,
}

#[cfg(test)]
impl Arbitrary for Boundary {
    type Parameters = (Rc<Vec<Rc<BoundaryType>>>, Rc<Vec<Rc<Zone>>>);
    type Strategy = BoxedStrategy<Boundary>;

    fn arbitrary_with(params: (Rc<Vec<Rc<BoundaryType>>>, Rc<Vec<Rc<Zone>>>)) -> Self::Strategy {
        let (boundary_types, zones) = params;
        assert!(!boundary_types.is_empty());
        assert!(zones.len() > 1);
        (
            0..boundary_types.len(),
            0..zones.len(),
            0..(zones.len() - 1),
            1e-6f64..1000f64,
        )
            .prop_map(move |params| {
                let z1 = params.1;
                let z2 = if params.2 < params.1 {
                    params.2
                } else {
                    params.2 + 1
                };
                assert_ne!(z1, z2);
                Boundary {
                    boundary_type: Rc::clone(&boundary_types[params.0]),
                    zones: [Rc::clone(&zones[z1]), Rc::clone(&zones[z2])],
                    area: Area::new::<square_meter>(params.3),
                    azimuth: None,
                    tilt: None,
                }
            })
            .boxed()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BoundaryType {
    Layered {
        name: String,
        /// List of layers, non empty
        layers: Vec<BoundaryLayer>,
        /// A name that can be used to address the interface between the zone and
        /// the first layer.
        initial_marker: Option<String>,
    },
    Simple {
        name: String,
        u: HeatTransfer,
        g: Ratio,
    },
}

#[cfg(test)]
impl Arbitrary for BoundaryType {
    type Parameters = Rc<Vec<Rc<Material>>>;
    type Strategy = BoxedStrategy<BoundaryType>;

    fn arbitrary_with(materials: Rc<Vec<Rc<Material>>>) -> Self::Strategy {
        prop_oneof![
            ("[a-z]*", 1e-6f64..10f64, 0f64..100f64).prop_map(|tuple| BoundaryType::Simple {
                name: tuple.0,
                u: HeatTransfer::new::<watt_per_square_meter_kelvin>(tuple.1),
                g: Ratio::new::<percent>(tuple.2),
            }),
            (
                "[a-z]*",
                prop::collection::vec(BoundaryLayer::arbitrary_with(materials), 1..10),
                prop::option::of("[a-z]*"),
            )
                .prop_map(|tuple| BoundaryType::Layered {
                    name: tuple.0,
                    layers: tuple.1,
                    initial_marker: tuple.2
                }),
        ]
        .boxed()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundaryLayer {
    pub material: Rc<Material>,
    pub thickness: Length,
    /// A name that can be used to address the interface following this layer.
    /// (between this layer and the next, or between this layer and the zone, if this is the last
    /// layer)
    pub following_marker: Option<String>,
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

#[cfg(test)]
impl Arbitrary for BoundaryLayer {
    type Parameters = Rc<Vec<Rc<Material>>>;
    type Strategy = BoxedStrategy<BoundaryLayer>;

    fn arbitrary_with(materials: Rc<Vec<Rc<Material>>>) -> Self::Strategy {
        assert!(!materials.is_empty());
        (
            0..materials.len(),
            1e-6f64..5f64,
            prop::option::of("[a-z]*"),
        )
            .prop_map(move |tuple| BoundaryLayer {
                material: Rc::clone(&materials[tuple.0]),
                thickness: Length::new::<meter>(tuple.1),
                following_marker: tuple.2,
            })
            .boxed()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub name: String,
    pub thermal_conductivity: ThermalConductivity,
    pub specific_heat_capacity: SpecificHeatCapacity,
    pub density: MassDensity,
}

impl Material {
    /// Return a default implementation of air material, used if air is not
    /// explicitly defined in the model
    fn default_air() -> Material {
        Material {
            name: "air".into(),
            thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(0.026),
            specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(1012.0),
            density: MassDensity::new::<kilogram_per_cubic_meter>(1.199),
        }
    }
}

#[cfg(test)]
impl Arbitrary for Material {
    type Parameters = ();
    type Strategy = BoxedStrategy<Material>;

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            "[a-z]*",
            1e-6f64..100f64,
            1e-6f64..100f64,
            1e-6f64..10000f64,
        )
            .prop_map(|tuple| Material {
                name: tuple.0,
                thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(tuple.1),
                specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(
                    tuple.2,
                ),
                density: MassDensity::new::<kilogram_per_cubic_meter>(tuple.3),
            })
            .boxed()
    }
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
    pub struct Zone {
        pub volume: Volume,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    pub struct Boundary {
        pub boundary_type: String,
        pub zones: [String; 2],
        pub area: Area,
        #[serde(default)]
        pub sub_boundaries: Vec<SubBoundary>,
        /// Surface orientation in **degrees**, kept as raw `f64` because `uom`'s serde reads
        /// angles in radians. `azimuth` is the compass bearing; `angle` is the tilt from
        /// horizontal (90 = vertical wall). Sub-boundaries inherit their parent's orientation.
        #[serde(default)]
        pub azimuth: Option<f64>,
        #[serde(default)]
        pub angle: Option<f64>,
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
                BoundaryType::Layered { layers } => {
                    let mut prev_is_marker = false;
                    let mut have_non_marker = false;
                    for layer in layers.iter() {
                        let is_marker = layer.is_marker();
                        if is_marker && prev_is_marker {
                            anyhow::bail!("Boundary type {:?} has two consecutive markers", name);
                        }
                        have_non_marker |= !is_marker;
                        prev_is_marker = is_marker;
                    }
                    if !have_non_marker {
                        anyhow::bail!(
                            "Boundary type {:?} does not have at least non-marker layer",
                            name
                        );
                    };

                    let mut out_layers: Vec<super::BoundaryLayer> =
                        Vec::with_capacity(layers.len());

                    // Peek the first layer and consume it only if it's the leading marker.
                    let first_is_marker = layers.first().unwrap().is_marker();
                    let mut it = layers.into_iter();
                    let initial_marker = if first_is_marker {
                        match it.next() {
                            Some(BoundaryLayer::Marker { marker }) => Some(marker),
                            _ => unreachable!("leading layer is a marker"),
                        }
                    } else {
                        None
                    };

                    // Convert the individual layers and assign markers
                    for layer in it {
                        if let BoundaryLayer::Marker { marker } = layer {
                            let following_marker =
                                &mut out_layers.last_mut().unwrap().following_marker;
                            // A non-marker layer carries at most one following marker.
                            anyhow::ensure!(
                                following_marker.is_none(),
                                "consecutive markers are not allowed in a layered boundary"
                            );
                            *following_marker = Some(marker);
                        } else {
                            out_layers.push(layer.convert(materials)?);
                        }
                    }

                    super::BoundaryType::Layered {
                        name,
                        layers: out_layers,
                        initial_marker,
                    }
                }
                BoundaryType::Simple { u, g } => super::BoundaryType::Simple { name, u, g },
            })
        }
    }

    #[derive(Clone, Debug, Deserialize, PartialEq)]
    #[serde(untagged)]
    pub enum BoundaryLayer {
        Layer { material: String, thickness: Length },
        Marker { marker: String },
    }

    impl BoundaryLayer {
        pub fn convert(
            self,
            materials: &HashMap<String, Rc<super::Material>>,
        ) -> anyhow::Result<super::BoundaryLayer> {
            Ok(match self {
                BoundaryLayer::Layer {
                    material,
                    thickness,
                } => super::BoundaryLayer {
                    material: get(materials, &material, "material")?,
                    thickness,
                    following_marker: None,
                },
                BoundaryLayer::Marker { marker: _ } => unreachable!(),
            })
        }

        pub fn is_marker(&self) -> bool {
            matches!(self, Self::Marker { .. })
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

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;
    use assert_matches::assert_matches;
    use test_case::test_case;
    use test_strategy::proptest;
    use uom::si::{
        area::square_meter, heat_transfer::watt_per_square_meter_kelvin, length::meter,
        mass_density::kilogram_per_cubic_meter, ratio::percent,
        specific_heat_capacity::joule_per_kilogram_kelvin,
        thermal_conductivity::watt_per_meter_kelvin, volume::cubic_meter,
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
        let input = as_loaded::BoundaryLayer::Layer {
            material: "mat1".into(),
            thickness: Length::new::<meter>(0.2),
        };
        let materials = converted_materials_hashmap();
        let output = input.convert(&materials).unwrap();
        assert_eq!(
            output,
            BoundaryLayer {
                thickness: Length::new::<meter>(0.2),
                material: Rc::clone(&materials["mat1"]),
                following_marker: None
            }
        );
    }

    #[test]
    fn convert_boundary_type_layered_intial_marker() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![
                as_loaded::BoundaryLayer::Marker {
                    marker: "A DUCK!".into(),
                },
                as_loaded::BoundaryLayer::Layer {
                    material: "mat1".into(),
                    thickness: Length::new::<meter>(1.0),
                },
                as_loaded::BoundaryLayer::Layer {
                    material: "mat2".into(),
                    thickness: Length::new::<meter>(2.0),
                },
            ],
        };
        let materials = converted_materials_hashmap();
        let output = input.convert("somename".to_string(), &materials).unwrap();
        assert_eq!(
            output,
            BoundaryType::Layered {
                name: "somename".into(),
                layers: vec![
                    BoundaryLayer {
                        thickness: Length::new::<meter>(1.0),
                        material: Rc::clone(&materials["mat1"]),
                        following_marker: None,
                    },
                    BoundaryLayer {
                        thickness: Length::new::<meter>(2.0),
                        material: Rc::clone(&materials["mat2"]),
                        following_marker: None,
                    },
                ],
                initial_marker: Some("A DUCK!".into()),
            }
        );
    }

    #[proptest]
    fn convert_boundary_type_layered_marker_inside(#[strategy(1usize..4usize)] i: usize) {
        let mut layers = vec![
            as_loaded::BoundaryLayer::Layer {
                material: "mat1".into(),
                thickness: Length::new::<meter>(1.0),
            },
            as_loaded::BoundaryLayer::Layer {
                material: "mat2".into(),
                thickness: Length::new::<meter>(2.0),
            },
            as_loaded::BoundaryLayer::Layer {
                material: "mat2".into(),
                thickness: Length::new::<meter>(3.0),
            },
        ];
        layers.insert(
            i,
            as_loaded::BoundaryLayer::Marker {
                marker: "asdf".into(),
            },
        );
        let input = as_loaded::BoundaryType::Layered { layers };
        let materials = converted_materials_hashmap();
        let output = input.convert("somename".to_string(), &materials).unwrap();

        assert_matches!(output, BoundaryType::Layered { name: _, layers, initial_marker } => {
            assert!(initial_marker.is_none());
            assert_eq!(layers.len(), 3);
            assert!(layers.iter().enumerate().all(|(j, l)| (j == (i - 1)) || l.following_marker.is_none()));
            assert_eq!(layers[i - 1].following_marker, Some("asdf".into()));
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
        assert_eq!(
            output,
            BoundaryType::Simple {
                name: "somename".into(),
                u: HeatTransfer::new::<watt_per_square_meter_kelvin>(123.0),
                g: Ratio::new::<percent>(90.0)
            }
        );
    }

    #[test]
    fn convert_boundary_type_layered_missing_material() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![
                as_loaded::BoundaryLayer::Layer {
                    material: "matX".into(),
                    thickness: Length::new::<meter>(1.0),
                },
                as_loaded::BoundaryLayer::Layer {
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

    #[test]
    fn convert_boundary_type_only_marker() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![as_loaded::BoundaryLayer::Marker { marker: "X".into() }],
        };
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

    #[test]
    fn convert_boundary_type_successive_markers() {
        let input = as_loaded::BoundaryType::Layered {
            layers: vec![
                as_loaded::BoundaryLayer::Layer {
                    material: "mat1".into(),
                    thickness: Length::new::<meter>(1.0),
                },
                as_loaded::BoundaryLayer::Marker {
                    marker: "ONE DUCK!".into(),
                },
                as_loaded::BoundaryLayer::Marker {
                    marker: "TWO DUCK!".into(),
                },
                as_loaded::BoundaryLayer::Layer {
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
            .find("somename")
            .expect("Error message should contain the name of the bad boundary type");
    }

    /// Tests the conversion of a minimal valid model
    #[test]
    fn convert_model_minimal() {
        let input = as_loaded::Model {
            zones: HashMap::new(),
            boundaries: vec![],
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
        };

        let output: Model = input.try_into().unwrap();

        assert_eq!(output.zones.len(), 2); // Outside and ground are always there
        assert!(output.boundaries.is_empty());
    }

    #[test]
    fn convert_model_zones() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                (
                    "z1".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(1.0),
                    },
                ),
                (
                    "z2".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(2.0),
                    },
                ),
            ]),
            boundaries: vec![],
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
        };

        let output: Model = input.try_into().unwrap();

        assert_eq!(
            output.zones,
            HashMap::from([
                (
                    "outside".into(),
                    Rc::new(Zone {
                        name: "outside".into(),
                        volume: None
                    })
                ),
                (
                    "ground".into(),
                    Rc::new(Zone {
                        name: "ground".into(),
                        volume: None
                    })
                ),
                (
                    "z1".into(),
                    Rc::new(Zone {
                        name: "z1".into(),
                        volume: Some(Volume::new::<cubic_meter>(1.0))
                    })
                ),
                (
                    "z2".into(),
                    Rc::new(Zone {
                        name: "z2".into(),
                        volume: Some(Volume::new::<cubic_meter>(2.0))
                    })
                ),
            ])
        );
    }

    #[test_case("outside")]
    #[test_case("ground")]
    fn convert_model_override_builtin_zone(defined_zone: &str) {
        let input = as_loaded::Model {
            zones: HashMap::from([(
                defined_zone.into(),
                as_loaded::Zone {
                    volume: Volume::new::<cubic_meter>(1.0),
                },
            )]),
            boundaries: vec![],
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
        };

        let message = format!("{}", Model::try_from(input).unwrap_err());
        message
            .find("reserved zone")
            .expect("Error message should say that there's a problem with a reserved zone");
        message
            .find(defined_zone)
            .expect("Error message should contain the name of the problematic zone");
    }

    #[test]
    fn convert_model_boundaries() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                (
                    "z1".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(1.0),
                    },
                ),
                (
                    "z2".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(2.0),
                    },
                ),
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
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
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
            volume: Some(Volume::new::<cubic_meter>(1.0)),
        });
        let z2 = Rc::new(Zone {
            name: "z2".into(),
            volume: Some(Volume::new::<cubic_meter>(2.0)),
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

        // Conversion preserves input boundary order, so assert the exact order.
        assert_eq!(
            output.boundaries,
            vec![
                Boundary {
                    boundary_type: Rc::clone(&bt2),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(1.0),
                    azimuth: None,
                    tilt: None,
                },
                Boundary {
                    boundary_type: Rc::clone(&bt3),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(2.0),
                    azimuth: None,
                    tilt: None,
                },
                Boundary {
                    boundary_type: Rc::clone(&bt1),
                    zones: [Rc::clone(&z1), Rc::clone(&z2)],
                    area: Area::new::<square_meter>(120.0),
                    azimuth: None,
                    tilt: None,
                },
            ]
        );
    }

    #[test]
    fn convert_model_too_large_sub_boundaries() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                (
                    "z1".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(1.0),
                    },
                ),
                (
                    "z2".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(2.0),
                    },
                ),
            ]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["z1".into(), "z2".into()],
                area: Area::new::<square_meter>(1.0),
                sub_boundaries: vec![as_loaded::SubBoundary {
                    boundary_type: "bt".into(),
                    area: Area::new::<square_meter>(2.0),
                }],
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
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
            zones: HashMap::from([(
                "goodzone".into(),
                as_loaded::Zone {
                    volume: Volume::new::<cubic_meter>(1.0),
                },
            )]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["goodzone".into(), "badzone".into()],
                area: Area::new::<square_meter>(1.0),
                sub_boundaries: Vec::new(),
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
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
    fn convert_model_self_referential_boundary() {
        let input = as_loaded::Model {
            zones: HashMap::from([(
                "z1".into(),
                as_loaded::Zone {
                    volume: Volume::new::<cubic_meter>(1.0),
                },
            )]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["z1".into(), "z1".into()],
                area: Area::new::<square_meter>(1.0),
                sub_boundaries: Vec::new(),
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
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
            .find("z1")
            .expect("Error message should contain the name of the offending zone");
        message
            .find("itself")
            .expect("Error message should say the boundary connects a zone to itself");
    }

    #[test]
    fn convert_model_negative_area() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                (
                    "z1".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(1.0),
                    },
                ),
                (
                    "z2".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(2.0),
                    },
                ),
            ]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["z1".into(), "z2".into()],
                area: Area::new::<square_meter>(-1.0),
                sub_boundaries: Vec::new(),
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
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
            .find("negative")
            .expect("Error message should mention the negative area");
    }

    #[test]
    fn convert_model_zero_area_boundary_skipped() {
        let input = as_loaded::Model {
            zones: HashMap::from([
                (
                    "z1".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(1.0),
                    },
                ),
                (
                    "z2".into(),
                    as_loaded::Zone {
                        volume: Volume::new::<cubic_meter>(2.0),
                    },
                ),
            ]),
            boundaries: vec![as_loaded::Boundary {
                boundary_type: "bt".into(),
                zones: ["z1".into(), "z2".into()],
                area: Area::new::<square_meter>(0.0),
                sub_boundaries: Vec::new(),
                azimuth: None,
                angle: None,
            }],
            materials: HashMap::new(),
            boundary_types: HashMap::from([(
                "bt".into(),
                as_loaded::BoundaryType::Simple {
                    u: Default::default(),
                    g: Default::default(),
                },
            )]),
        };

        let output: Model = input.try_into().unwrap();
        assert!(
            output.boundaries.is_empty(),
            "a zero-area boundary should be skipped"
        );
    }

    #[test]
    fn convert_model_boundary_orientation() {
        let model = Model::from_json(
            r#"{
                materials: {},
                boundary_types: { wall: { u: 1, g: 0 }, win: { u: 1, g: 0 } },
                zones: { a: { volume: 1 } },
                boundaries: [
                    {
                        boundary_type: "wall",
                        zones: ["a", "outside"],
                        area: 5,
                        sub_boundaries: [ { boundary_type: "win", area: 1 } ],
                        azimuth: 140,
                        angle: 90,
                    },
                    { boundary_type: "wall", zones: ["a", "ground"], area: 5 },
                ],
            }"#,
        )
        .unwrap();

        // The exterior wall and its window sub-boundary both carry the orientation, parsed as
        // degrees (not radians, which is what uom's serde would do for a bare number).
        let exterior: Vec<_> = model
            .boundaries
            .iter()
            .filter(|b| b.zones.iter().any(|z| z.name == "outside"))
            .collect();
        assert_eq!(exterior.len(), 2); // window + remaining wall
        for b in exterior {
            assert_eq!(b.azimuth, Some(Angle::new::<degree>(140.0)));
            assert_eq!(b.tilt, Some(Angle::new::<degree>(90.0)));
        }

        // The boundary without orientation has none.
        let to_ground = model
            .boundaries
            .iter()
            .find(|b| b.zones.iter().any(|z| z.name == "ground"))
            .unwrap();
        assert_eq!(to_ground.azimuth, None);
        assert_eq!(to_ground.tilt, None);
    }

    #[test]
    fn convert_model_defined_air() {
        let test_air = as_loaded::Material {
            thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(999.0),
            specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(999.0),
            density: MassDensity::new::<kilogram_per_cubic_meter>(999.0),
        };

        let input = as_loaded::Model {
            zones: HashMap::new(),
            boundaries: vec![],
            materials: HashMap::from([("air".into(), test_air.clone())]),
            boundary_types: HashMap::new(),
        };
        let output: Model = input.try_into().unwrap();
        assert_eq!(output.air.as_ref(), &test_air.convert("air".into()));
    }

    #[test]
    fn convert_model_default_air() {
        let input = as_loaded::Model {
            zones: HashMap::new(),
            boundaries: vec![],
            materials: HashMap::new(),
            boundary_types: HashMap::new(),
        };
        let output: Model = input.try_into().unwrap();
        assert_eq!(output.air.as_ref(), &Material::default_air());
    }

    #[test]
    fn load_model() {
        let mut f = tempfile::NamedTempFile::new().unwrap();

        use std::io::Write;
        write!(f, "{}", sample_model_json()).unwrap();

        let model = Model::load(f.path()).unwrap();

        check_sample_model(model);
    }

    #[test]
    fn model_from_json() {
        let model = Model::from_json(sample_model_json()).unwrap();
        check_sample_model(model);
    }

    #[test_case(Some(1.0), 12.0; "finite")]
    #[test_case(None, f64::INFINITY; "infinite")]
    fn zone_heat_capacity(v: Option<f64>, expected: f64) {
        let z = Zone {
            name: Default::default(),
            volume: v.map(Volume::new::<cubic_meter>),
        };
        let m = Material {
            name: Default::default(),
            thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(2.0),
            specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(3.0),
            density: MassDensity::new::<kilogram_per_cubic_meter>(4.0),
        };
        assert_eq!(
            z.heat_capacity(&m),
            HeatCapacity::new::<joule_per_kelvin>(expected)
        );
    }

    #[test]
    fn zone_heat_capacity_pathological() {
        let z = Zone {
            name: Default::default(),
            volume: None,
        };
        let m = Material {
            name: Default::default(),
            thermal_conductivity: Default::default(),
            specific_heat_capacity: Default::default(),
            density: Default::default(),
        };
        assert_eq!(
            z.heat_capacity(&m),
            HeatCapacity::new::<joule_per_kelvin>(f64::INFINITY)
        );
    }

    #[test]
    fn boundary_layer_heat_capacity() {
        let bl = BoundaryLayer {
            material: Rc::new(Material {
                name: "water".into(),
                thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(0.598),
                specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(
                    4180.0,
                ),
                density: MassDensity::new::<kilogram_per_cubic_meter>(997.0),
            }),
            thickness: Length::new::<meter>(1.0),
            following_marker: None,
        };
        assert_abs_diff_eq!(
            bl.heat_capacity(Area::new::<square_meter>(1.0))
                .get::<joule_per_kelvin>(),
            4168000.0,
            epsilon = 1000.0
        );
    }

    #[test]
    fn boundary_layer_conductance() {
        let bl = BoundaryLayer {
            material: Rc::new(Material {
                name: "water".into(),
                thermal_conductivity: ThermalConductivity::new::<watt_per_meter_kelvin>(0.598),
                specific_heat_capacity: SpecificHeatCapacity::new::<joule_per_kilogram_kelvin>(
                    4180.0,
                ),
                density: MassDensity::new::<kilogram_per_cubic_meter>(997.0),
            }),
            thickness: Length::new::<meter>(2.0),
            following_marker: None,
        };
        assert_eq!(
            bl.conductance(Area::new::<square_meter>(4.0)),
            ThermalConductance::new::<watt_per_kelvin>(1.196)
        );
    }

    /// Provide string with sample JSON5 model
    fn sample_model_json() -> &'static str {
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
                b: { volume: 234 },
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
    }

    /// This does the actual check of loaded sample_model
    /// Needs to be separate in order to check both file loading and string loading
    /// without code duplication
    fn check_sample_model(model: Model) {
        assert_eq!(
            model.zones,
            HashMap::from([
                (
                    "a".into(),
                    Rc::new(Zone {
                        name: "a".into(),
                        volume: Some(Volume::new::<cubic_meter>(123.0))
                    })
                ),
                (
                    "b".into(),
                    Rc::new(Zone {
                        name: "b".into(),
                        volume: Some(Volume::new::<cubic_meter>(234.0))
                    })
                ),
                (
                    "outside".into(),
                    Rc::new(Zone {
                        name: "outside".into(),
                        volume: None
                    })
                ),
                (
                    "ground".into(),
                    Rc::new(Zone {
                        name: "ground".into(),
                        volume: None
                    })
                ),
            ])
        );

        assert_eq!(model.boundaries.len(), 2);
        assert_matches!(&model.boundaries[1].boundary_type.as_ref(), &BoundaryType::Layered{ name, layers: _, initial_marker: _ } => {
            assert_eq!(name, "wall");
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
