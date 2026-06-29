//! A plain, serializable snapshot of the building model's **thermal envelope** — every zone and the
//! boundaries between them, with area, orientation, construction (layer stack), and U-value.
//!
//! Like [`crate::rc_network::RcNetwork`], this holds **no `Rc`**: it is extracted from [`Model`] once
//! at startup so the read-only web layer can serve `/api/model/topology` from `Send + Sync` state,
//! without plumbing the `Rc`-laden `Model` itself across the axum handlers.

use serde::Serialize;
use uom::si::heat_transfer::watt_per_square_meter_kelvin;
use uom::si::{
    angle::degree, area::square_meter, length::meter, length::millimeter,
    thermal_conductivity::watt_per_meter_kelvin, volume::cubic_meter,
};

use crate::model::{BoundaryType, Model};

/// ISO 6946 surface-film resistances (m²K/W) for a boundary `kind`: `(interior, exterior)`. Adding
/// them to the layer stack gives the conventional U-value (a film-free conductance over-states U).
fn surface_films(kind: &str) -> (f64, f64) {
    match kind {
        "exterior" | "roof" => (0.13, 0.04), // Rsi + Rse
        "ground" => (0.13, 0.0),             // interior film only; the ground side has no air film
        _ => (0.13, 0.13),                   // interior boundary: a film each side
    }
}

/// The whole envelope: zones (nodes) and boundaries (edges).
#[derive(Clone, Debug, Serialize)]
pub struct ModelTopology {
    pub zones: Vec<ZoneInfo>,
    pub boundaries: Vec<BoundaryInfo>,
}

/// One thermal zone (a room, or a reserved reservoir).
#[derive(Clone, Debug, Serialize)]
pub struct ZoneInfo {
    pub name: String,
    /// Air volume (m³); `None` for the reserved infinite-capacity reservoirs (`outside`/`ground`).
    pub volume_m3: Option<f64>,
    /// `interior` | `outside` | `ground`.
    pub role: String,
}

/// One boundary (a wall / floor / ceiling / roof) between two zones.
#[derive(Clone, Debug, Serialize)]
pub struct BoundaryInfo {
    pub id: usize,
    pub zone_a: String,
    pub zone_b: String,
    pub area_m2: f64,
    /// Compass azimuth of an exterior surface (°); `None` for interior boundaries.
    pub azimuth_deg: Option<f64>,
    /// Tilt from horizontal (°); 90 = vertical wall, 0 = flat roof/floor.
    pub tilt_deg: Option<f64>,
    /// `interior` | `exterior` | `roof` | `ground`.
    pub kind: String,
    pub type_name: String,
    /// Thermal transmittance (W/m²K) and its reciprocal resistance (m²K/W).
    pub u_value: f64,
    pub r_value: f64,
    /// Conductive loss coefficient = area × U (W/K) — drives the heat-loss ranking.
    pub ua: f64,
    /// Fraction of incident solar absorbed at the outer surface (`Layered` only).
    pub solar_absorptance: Option<f64>,
    /// Layer stack in the model's `zones[0]`→`zones[1]` order (exterior-first for walls, room-first
    /// for floors/roofs/inter-floor slabs — so a consumer must orient it, as the dashboard does);
    /// `None` for a `Simple` U/g boundary.
    pub layers: Option<Vec<LayerInfo>>,
    /// An addressable interface authored at the zone↔first-layer surface (`Layered.initial_marker`),
    /// distinct from a layer's trailing marker; `None` when absent.
    pub initial_marker: Option<String>,
}

/// One material layer within a [`BoundaryInfo`].
#[derive(Clone, Debug, Serialize)]
pub struct LayerInfo {
    pub material: String,
    pub thickness_mm: f64,
    /// Thermal conductivity λ (W/mK).
    pub conductivity: f64,
    /// The addressable interface after this layer (e.g. the underfloor `heating` slab marker).
    pub marker: Option<String>,
}

fn role(name: &str) -> &'static str {
    match name {
        "outside" => "outside",
        "ground" => "ground",
        _ => "interior",
    }
}

impl From<&Model> for ModelTopology {
    fn from(model: &Model) -> Self {
        let mut zones: Vec<ZoneInfo> = model
            .zones
            .values()
            .map(|z| ZoneInfo {
                name: z.name.clone(),
                volume_m3: z.volume.map(|v| v.get::<cubic_meter>()),
                role: role(&z.name).to_string(),
            })
            .collect();
        zones.sort_by(|a, b| a.name.cmp(&b.name));

        let boundaries = model
            .boundaries
            .iter()
            .enumerate()
            .map(|(id, b)| {
                let area_m2 = b.area.get::<square_meter>();
                let za = b.zones[0].name.clone();
                let zb = b.zones[1].name.clone();
                let touches = |n: &str| za == n || zb == n;
                let tilt_deg = b.tilt.map(|t| t.get::<degree>());
                // A surface touching `outside` is a roof if it's notably off-vertical (a pitched roof
                // sits ~30-50° from horizontal; walls/gables are ~90°), else an exterior wall.
                let kind = if touches("ground") {
                    "ground"
                } else if touches("outside") {
                    match tilt_deg {
                        Some(t) if t < 60.0 => "roof",
                        _ => "exterior",
                    }
                } else {
                    "interior"
                };
                let (rsi, rse) = surface_films(kind);

                let (type_name, u_value, solar_absorptance, layers, initial_marker) =
                    match &*b.boundary_type {
                        // A `Simple` U/g pane's `u` is the conventional U-value (films included).
                        BoundaryType::Simple { name, u, .. } => (
                            name.clone(),
                            u.get::<watt_per_square_meter_kelvin>(),
                            None,
                            None,
                            None,
                        ),
                        BoundaryType::Layered {
                            name,
                            layers,
                            solar_absorptance,
                            initial_marker,
                        } => {
                            // Conventional U: 1 / (Rsi + Σ thickness/λ + Rse) with ISO 6946 films — the
                            // U-value people expect. (The RC model uses its own air-convection coefficient
                            // internally, so its node conductances differ slightly from this display U.)
                            let r_layers: f64 = layers
                                .iter()
                                .map(|l| {
                                    l.thickness.get::<meter>()
                                        / l.material
                                            .thermal_conductivity
                                            .get::<watt_per_meter_kelvin>()
                                })
                                .sum();
                            let r_total = rsi + r_layers + rse;
                            let u = if r_total > 0.0 { 1.0 / r_total } else { 0.0 };
                            let infos = layers
                                .iter()
                                .map(|l| LayerInfo {
                                    material: l.material.name.clone(),
                                    thickness_mm: l.thickness.get::<millimeter>(),
                                    conductivity: l
                                        .material
                                        .thermal_conductivity
                                        .get::<watt_per_meter_kelvin>(),
                                    marker: l.following_marker.clone(),
                                })
                                .collect();
                            (
                                name.clone(),
                                u,
                                Some(*solar_absorptance),
                                Some(infos),
                                initial_marker.clone(),
                            )
                        }
                    };

                BoundaryInfo {
                    id,
                    zone_a: za,
                    zone_b: zb,
                    area_m2,
                    azimuth_deg: b.azimuth.map(|a| a.get::<degree>()),
                    tilt_deg,
                    kind: kind.to_string(),
                    type_name,
                    u_value,
                    r_value: if u_value > 0.0 { 1.0 / u_value } else { 0.0 },
                    ua: area_m2 * u_value,
                    solar_absorptance,
                    layers,
                    initial_marker,
                }
            })
            .collect();

        ModelTopology { zones, boundaries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_from_real_model() {
        let model = Model::load("model.json5").expect("model.json5 loads");
        let topo = ModelTopology::from(&model);

        // The reserved reservoirs and the real rooms are present.
        assert!(topo.zones.iter().any(|z| z.role == "outside"));
        assert!(topo.zones.iter().any(|z| z.role == "ground"));
        assert!(topo.zones.iter().filter(|z| z.role == "interior").count() > 5);

        // Layered assemblies get a finite, positive U/R; UA is exactly area × U.
        assert!(!topo.boundaries.is_empty());
        let layered: Vec<_> = topo
            .boundaries
            .iter()
            .filter(|b| b.layers.is_some())
            .collect();
        assert!(!layered.is_empty());
        assert!(layered
            .iter()
            .all(|b| b.u_value.is_finite() && b.u_value > 0.0 && b.r_value > 0.0));
        for b in &topo.boundaries {
            assert!((b.ua - b.area_m2 * b.u_value).abs() < 1e-6);
        }

        // The underfloor-heating marker surfaces on at least one ground-floor slab.
        assert!(topo.boundaries.iter().any(|b| b
            .layers
            .as_ref()
            .is_some_and(|ls| ls.iter().any(|l| l.marker.as_deref() == Some("heating")))));
    }
}
