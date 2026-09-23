use chrono::{DateTime, Utc};
use uom::si::angle::degree;
use uom::si::f64::*;
use uom::si::heat_flux_density::watt_per_square_meter;
use uom::si::ratio::ratio;

/// Calculate atmospheric attenuation estimate based on sun angle
/// https://en.wikipedia.org/wiki/Air_mass_(astronomy)#Plane-parallel_atmosphere
/// For many solar energy applications when high accuracy near the horizon is not required
/// air mass is commonly determined using the simple secant formula described in the section
/// Plane-parallel atmosphere.
///
/// https://asterism.org/resources/atmospheric-extinction-and-refraction/
/// The average total effect at sea level is the sum of these factors,
/// in the order of 0.28 magnitudes per air mass at Standard Temperature and Pressure,
/// (STP = 760 mm Hg, 00 C). Note that stellar objects are, therefore, 0.28 magnitudes
/// brighter at the top of our atmosphere. At elevations of 0.5 km, 1.0 km, and 2.0 km,
/// the extinction effects are about 0.24, 0.21, and 0.16 magnitudes per air mass, respectively.
///
/// Arguments:
/// * `zenith_angle` - zenith_angle: sun zenith angle
///
/// Returns:
/// * `Ratio` - atmospheric attenuation ratio
fn atmospheric_attenuation(zenith_angle: Angle) -> Ratio {
    let airmass = zenith_angle.cos().recip();
    let attenuation_magnitude = 0.28 * airmass; // ~0.28 magnitudes per air mass at sea level (STP)
    Ratio::new::<ratio>(1e2f64.powf(-attenuation_magnitude.get::<ratio>() / 5.0))
}

/// Global-irradiance cloud transmittance, Kasten & Czeplak (1980): `G = G_clear (1 − 0.75 c³·⁴)`.
fn cloud_transmittance(cloud_cover: Ratio) -> f64 {
    1.0 - 0.75 * cloud_cover.get::<ratio>().clamp(0.0, 1.0).powf(3.4)
}

/// Diffuse fraction of the global irradiance, Kasten & Czeplak: `D/G = 0.3 + 0.7 c²`. Clear skies
/// are ~30 % diffuse; a fully overcast sky is all-diffuse — which is what keeps window/wall gains
/// nonzero on overcast days (the previous beam-only model transmitted essentially nothing at
/// full cloud, while a real house still gains 30–70 W/m² of diffuse through glazing).
fn diffuse_fraction(cloud_cover: Ratio) -> f64 {
    let c = cloud_cover.get::<ratio>().clamp(0.0, 1.0);
    (0.3 + 0.7 * c * c).min(1.0)
}

/// Calculate solar irradiance on tilted surface
///
/// Arguments:
/// * `latitude` - latitude of the location
/// * `longitude` - longitude of the location
/// * `datetime` - datetime of the calculation
/// * `cloud_cover` - cloud cover ratio
/// * `surface_angle_from_horizontal` - surface angle
/// * `surface_azimuth` - surface azimuth
///
/// Per-hour solar boundary input, best available first. The three variants form the runtime
/// fallback chain (per HOUR — a partially-written radiation feed degrades block by block):
/// measured/forecast horizontal components → global horizontal with a cloud-driven split →
/// today's pure clear-sky × cloud model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SolarInput {
    /// Forecast/measured horizontal direct + diffuse irradiance (W/m²) — open-meteo's
    /// `direct_radiation` / `diffuse_radiation` (both on the HORIZONTAL plane).
    Radiation { direct_h: f64, diffuse_h: f64 },
    /// Global horizontal only (`shortwave_radiation`): split into beam + diffuse via the
    /// Kasten–Czeplak diffuse fraction from cloud cover.
    Ghi { ghi: f64, cloud: f64 },
    /// No radiation data: the clear-sky × cloud-transmittance model.
    Cloud { cloud: f64 },
}

/// The beam / diffuse / reflected split of [`tilted_irradiance`], plus the surface's cosine of
/// solar incidence (the raw geometric value — negative when the sun is behind the surface, i.e.
/// *before* [`tilted_irradiance`]'s `max(0)` clamp is applied to the beam term). Ground-reflected
/// irradiance is always zero today (no albedo model); the field is kept so a future reflection term
/// is a value change here, not another call-site signature change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IrradianceComponents {
    pub beam: HeatFluxDensity,
    pub diffuse: HeatFluxDensity,
    pub reflected: HeatFluxDensity,
    pub cos_incidence: f64,
}

/// Solar irradiance on a tilted surface for the given per-hour [`SolarInput`], split into beam /
/// diffuse / reflected components. Shared tilt projection: beam-on-tilt = (beam_h / cos z) × max(cos
/// incidence, 0), diffuse × the isotropic sky view `(1 + cos β)/2`; ground-reflected irradiance is
/// neglected. For measured beam the horizontal→normal conversion clamps `cos z` at cos 85° (the
/// secant explodes at the horizon, and low-sun measured values are mostly diffuse anyway); measured
/// diffuse KEEPS contributing at twilight (z ≥ 90°), unlike the modelled `Cloud` path which stays
/// bit-identical to the original (0 below the horizon). [`tilted_irradiance`] is exactly
/// `beam + diffuse + reflected`, clamped at 0 — see that function's doc.
pub fn tilted_irradiance_components(
    latitude: Angle,
    longitude: Angle,
    datetime: &DateTime<Utc>,
    input: SolarInput,
    surface_angle_from_horizontal: Angle,
    surface_azimuth: Angle,
) -> IrradianceComponents {
    let degrees = Angle::new::<degree>;
    let watts_per_square_meter = HeatFluxDensity::new::<watt_per_square_meter>;
    let zero = watts_per_square_meter(0.0);

    let solar_position = spa::calc_solar_position(
        *datetime,
        latitude.get::<degree>(),
        longitude.get::<degree>(),
    )
    .unwrap();
    let solar_zenith_angle = degrees(solar_position.zenith_angle);
    let solar_azimuth_angle = degrees(solar_position.azimuth);
    let below_horizon = solar_zenith_angle >= degrees(90.0);

    let sky_view = (1.0 + surface_angle_from_horizontal.cos().get::<ratio>()) / 2.0;
    // The raw geometric incidence cosine — computed even below the horizon (cheap, and it makes
    // `cos_incidence` meaningful for every caller, e.g. "facing the sun but it's set").
    let cos_incidence_angle = (solar_zenith_angle.cos() * surface_angle_from_horizontal.cos())
        + (solar_zenith_angle.sin()
            * surface_angle_from_horizontal.sin()
            * (solar_azimuth_angle - surface_azimuth).cos());
    let cos_incidence = cos_incidence_angle.get::<ratio>();

    // The sun is at or below the horizon: no direct beam reaches any surface. (This also avoids
    // the negative / infinite air mass that cos(zenith) <= 0 would feed into
    // atmospheric_attenuation.) Measured diffuse still lights the sky at twilight.
    if below_horizon {
        let diffuse = match input {
            SolarInput::Radiation { diffuse_h, .. } => {
                watts_per_square_meter((diffuse_h * sky_view).max(0.0))
            }
            _ => zero,
        };
        return IrradianceComponents {
            beam: zero,
            diffuse,
            reflected: zero,
            cos_incidence,
        };
    }

    // Horizontal (beam_h, diffuse_h) per the input variant.
    let (beam_h, diffuse_h, cos_z_for_beam) = match input {
        SolarInput::Radiation {
            direct_h,
            diffuse_h,
        } => {
            // Clamp the horizontal→normal conversion at 85°: near the horizon the secant blows a
            // small measured direct_h into an absurd normal beam.
            let cos_z = solar_zenith_angle
                .cos()
                .get::<ratio>()
                .max(degrees(85.0).cos().get::<ratio>());
            (
                watts_per_square_meter(direct_h.max(0.0)),
                watts_per_square_meter(diffuse_h.max(0.0)),
                cos_z,
            )
        }
        SolarInput::Ghi { ghi, cloud } => {
            let d_frac = diffuse_fraction(Ratio::new::<ratio>(cloud));
            let g = watts_per_square_meter(ghi.max(0.0));
            // Same 85° clamp as the Radiation arm: the measured GHI is external (NOT proportional
            // to cos z like the Cloud model, where the secant cancels), so an unclamped division
            // would blow a small near-horizon shortwave value into an absurd normal beam.
            let cos_z = solar_zenith_angle
                .cos()
                .get::<ratio>()
                .max(degrees(85.0).cos().get::<ratio>());
            (g * (1.0 - d_frac), g * d_frac, cos_z)
        }
        SolarInput::Cloud { cloud } => {
            let cloud = Ratio::new::<ratio>(cloud);
            let extraterrestrial_irradiance = watts_per_square_meter(1361.0);
            let atmospheric_attenuation = atmospheric_attenuation(solar_zenith_angle);
            // Global horizontal → beam + isotropic diffuse (Kasten–Czeplak split).
            let ghi = extraterrestrial_irradiance
                * atmospheric_attenuation
                * solar_zenith_angle.cos()
                * cloud_transmittance(cloud);
            let d_frac = diffuse_fraction(cloud);
            (
                ghi * (1.0 - d_frac),
                ghi * d_frac,
                solar_zenith_angle.cos().get::<ratio>(),
            )
        }
    };

    // Beam on the tilt: normal beam (beam_h / cos z) times the incidence cosine, zero when the
    // sun is behind the surface.
    let beam_t = (beam_h / cos_z_for_beam) * cos_incidence.max(0.0);
    let diffuse_t = diffuse_h * sky_view;

    IrradianceComponents {
        beam: beam_t.max(zero),
        diffuse: diffuse_t.max(zero),
        reflected: zero,
        cos_incidence,
    }
}

/// Solar irradiance on a tilted surface for the given per-hour [`SolarInput`] — the sum of
/// [`tilted_irradiance_components`]'s beam, diffuse and reflected terms, clamped at 0.
pub fn tilted_irradiance(
    latitude: Angle,
    longitude: Angle,
    datetime: &DateTime<Utc>,
    input: SolarInput,
    surface_angle_from_horizontal: Angle,
    surface_azimuth: Angle,
) -> HeatFluxDensity {
    let c = tilted_irradiance_components(
        latitude,
        longitude,
        datetime,
        input,
        surface_angle_from_horizontal,
        surface_azimuth,
    );
    (c.beam + c.diffuse + c.reflected).max(HeatFluxDensity::new::<watt_per_square_meter>(0.0))
}

/// The original cloud-model entry point — delegates to [`tilted_irradiance`] with
/// [`SolarInput::Cloud`], bit-identically (the keystone thermal test and every existing caller
/// rely on that).
pub fn calculate_tilted_irradiance(
    latitude: Angle,
    longitude: Angle,
    datetime: &DateTime<Utc>,
    cloud_cover: Ratio,
    surface_angle_from_horizontal: Angle,
    surface_azimuth: Angle,
) -> HeatFluxDensity {
    tilted_irradiance(
        latitude,
        longitude,
        datetime,
        SolarInput::Cloud {
            cloud: cloud_cover.get::<ratio>(),
        },
        surface_angle_from_horizontal,
        surface_azimuth,
    )
}

/// The sun's current position at `latitude`/`longitude`: `(azimuth°, elevation°)`, where elevation is
/// degrees above the horizon (negative when the sun is down).
pub fn sun_azimuth_elevation(
    latitude: Angle,
    longitude: Angle,
    datetime: &DateTime<Utc>,
) -> (f64, f64) {
    let p = spa::calc_solar_position(
        *datetime,
        latitude.get::<degree>(),
        longitude.get::<degree>(),
    )
    .unwrap();
    (p.azimuth, 90.0 - p.zenith_angle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Location used by the project's demo entrypoint (central Europe).
    fn location() -> (Angle, Angle) {
        (
            Angle::new::<degree>(49.4949522),
            Angle::new::<degree>(17.4302361),
        )
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn irradiance_is_zero_at_night() {
        let (lat, lon) = location();
        // Local solar midnight in winter — the sun is far below the horizon.
        let night = utc("2023-12-21T23:00:00Z");
        // A vertical south-facing wall is the worst case for the old below-horizon bug.
        let irradiance = calculate_tilted_irradiance(
            lat,
            lon,
            &night,
            Ratio::new::<ratio>(0.0),
            Angle::new::<degree>(90.0),
            Angle::new::<degree>(180.0),
        );
        assert_eq!(irradiance.get::<watt_per_square_meter>(), 0.0);
    }

    #[test]
    fn irradiance_is_positive_on_a_clear_summer_day() {
        let (lat, lon) = location();
        // Around solar noon in summer (solar noon at ~17.4°E is ~10:45 UTC).
        let noon = utc("2023-06-21T11:00:00Z");
        let irradiance = calculate_tilted_irradiance(
            lat,
            lon,
            &noon,
            Ratio::new::<ratio>(0.0),
            Angle::new::<degree>(0.0),
            Angle::new::<degree>(180.0),
        );
        assert!(irradiance.get::<watt_per_square_meter>() > 0.0);
    }
    #[test]
    fn overcast_day_still_delivers_diffuse_irradiance() {
        let (lat, lon) = location();
        let noon = utc("2023-06-21T11:00:00Z");
        // Fully overcast, vertical south window: the beam is gone but the isotropic diffuse keeps
        // a real gain — the old beam-only model returned ~0 here while a real facade sees tens
        // of W/m².
        let overcast = calculate_tilted_irradiance(
            lat,
            lon,
            &noon,
            Ratio::new::<ratio>(1.0),
            Angle::new::<degree>(90.0),
            Angle::new::<degree>(180.0),
        );
        let w = overcast.get::<watt_per_square_meter>();
        assert!(
            (10.0..200.0).contains(&w),
            "diffuse-only ≈ tens of W/m²: {w}"
        );
        // …and clear beats overcast by a wide margin.
        let clear = calculate_tilted_irradiance(
            lat,
            lon,
            &noon,
            Ratio::new::<ratio>(0.0),
            Angle::new::<degree>(90.0),
            Angle::new::<degree>(180.0),
        );
        assert!(clear.get::<watt_per_square_meter>() > 3.0 * w);
    }
    #[test]
    fn radiation_input_on_horizontal_returns_direct_plus_diffuse() {
        let (lat, lon) = location();
        let noon = utc("2023-06-21T11:00:00Z");
        let w = tilted_irradiance(
            lat,
            lon,
            &noon,
            SolarInput::Radiation {
                direct_h: 500.0,
                diffuse_h: 120.0,
            },
            Angle::new::<degree>(0.0), // horizontal: incidence = zenith, sky view = 1
            Angle::new::<degree>(180.0),
        )
        .get::<watt_per_square_meter>();
        // beam_t = (direct_h / cos z)·cos z = direct_h; diffuse_t = diffuse_h·1.
        assert!(
            (w - 620.0).abs() < 1.0,
            "horizontal = direct + diffuse: {w}"
        );
    }

    #[test]
    fn ghi_input_split_matches_cloud_proportions() {
        let (lat, lon) = location();
        let noon = utc("2023-06-21T11:00:00Z");
        // Same GHI, clear vs overcast: overcast is all-diffuse, so a NORTH-facing vertical wall
        // (no beam either way at noon) sees MORE of it than of the clear split.
        let wall = |cloud: f64| {
            tilted_irradiance(
                lat,
                lon,
                &noon,
                SolarInput::Ghi { ghi: 400.0, cloud },
                Angle::new::<degree>(90.0),
                Angle::new::<degree>(0.0), // north
            )
            .get::<watt_per_square_meter>()
        };
        assert!(
            wall(1.0) > wall(0.0) + 50.0,
            "overcast GHI is diffuse-heavy: {} vs {}",
            wall(1.0),
            wall(0.0)
        );
    }

    #[test]
    fn low_sun_beam_conversion_is_clamped() {
        let (lat, lon) = location();
        // ~07:00 local midsummer: sun well up but low — and near-sunset values even lower. Use a
        // twilight-adjacent instant: 2023-06-21T19:30:00Z is ~21:30 local, sun ~2-3° up.
        let low = utc("2023-06-21T19:30:00Z");
        let w = tilted_irradiance(
            lat,
            lon,
            &low,
            SolarInput::Radiation {
                direct_h: 50.0,
                diffuse_h: 30.0,
            },
            Angle::new::<degree>(90.0),
            Angle::new::<degree>(315.0), // NW — facing the setting sun
        )
        .get::<watt_per_square_meter>();
        // Unclamped, 50 W/m² / cos(88°) would be ~1.4 kW/m²; the 85° clamp bounds the normal beam
        // at 50/cos(85°) ≈ 574, and the incidence cosine shrinks it further.
        assert!(w < 650.0, "clamped low-sun beam: {w}");
    }

    #[test]
    fn low_sun_ghi_beam_is_clamped_too() {
        let (lat, lon) = location();
        // Same twilight-adjacent instant as the Radiation clamp test: measured shortwave is
        // external (not ∝ cos z), so the GHI arm needs the identical 85° secant clamp.
        let low = utc("2023-06-21T19:30:00Z");
        let w = tilted_irradiance(
            lat,
            lon,
            &low,
            SolarInput::Ghi {
                ghi: 80.0,
                cloud: 0.0,
            },
            Angle::new::<degree>(90.0),
            Angle::new::<degree>(315.0), // NW — facing the setting sun
        )
        .get::<watt_per_square_meter>();
        // Unclamped, 80 W/m² of mostly-beam GHI over cos(~88°) would exceed 1.5 kW/m² normal.
        assert!(w < 900.0, "clamped low-sun GHI beam: {w}");
    }

    /// Keystone: the beam/diffuse/reflected split sums to exactly what `tilted_irradiance` (the
    /// existing, still-unchanged-in-output function) returns, over every input variant and a mix
    /// of daytime/twilight/night instants and surface orientations.
    #[test]
    fn components_sum_matches_tilted_irradiance() {
        let (lat, lon) = location();
        let instants = [
            utc("2023-06-21T11:00:00Z"), // clear summer noon
            utc("2023-06-21T19:30:00Z"), // low sun / twilight-adjacent
            utc("2023-12-21T23:00:00Z"), // night
        ];
        let inputs = [
            SolarInput::Cloud { cloud: 0.0 },
            SolarInput::Cloud { cloud: 0.7 },
            SolarInput::Ghi {
                ghi: 300.0,
                cloud: 0.4,
            },
            SolarInput::Radiation {
                direct_h: 500.0,
                diffuse_h: 120.0,
            },
        ];
        // A NE wall (azimuth 50°, tilt 90°) is the brief's own example of a beam-clamped-to-zero
        // surface at mid-morning.
        let surfaces = [(90.0, 180.0), (90.0, 50.0), (0.0, 0.0)];
        for &dt in &instants {
            for &input in &inputs {
                for &(tilt, az) in &surfaces {
                    let tilt = Angle::new::<degree>(tilt);
                    let az = Angle::new::<degree>(az);
                    let sum = tilted_irradiance(lat, lon, &dt, input, tilt, az)
                        .get::<watt_per_square_meter>();
                    let c = tilted_irradiance_components(lat, lon, &dt, input, tilt, az);
                    let split = (c.beam + c.diffuse + c.reflected).get::<watt_per_square_meter>();
                    assert!(
                        (sum - split).abs() < 1e-9,
                        "sum {sum} vs split {split} at {dt} tilt {tilt:?} az {az:?} input {input:?}"
                    );
                    assert!(c.beam.get::<watt_per_square_meter>() >= 0.0);
                    assert!(c.diffuse.get::<watt_per_square_meter>() >= 0.0);
                    assert_eq!(c.reflected.get::<watt_per_square_meter>(), 0.0);
                }
            }
        }
    }

    #[test]
    fn measured_diffuse_survives_twilight() {
        let (lat, lon) = location();
        let night = utc("2023-12-21T23:00:00Z"); // sun far below the horizon
        let w = tilted_irradiance(
            lat,
            lon,
            &night,
            SolarInput::Radiation {
                direct_h: 0.0,
                diffuse_h: 20.0,
            },
            Angle::new::<degree>(0.0),
            Angle::new::<degree>(180.0),
        )
        .get::<watt_per_square_meter>();
        assert!(
            (w - 20.0).abs() < 1e-9,
            "measured diffuse kept at night: {w}"
        );
        // …while the modelled Cloud path stays exactly 0 (bit-compat with the original).
        let c = tilted_irradiance(
            lat,
            lon,
            &night,
            SolarInput::Cloud { cloud: 0.0 },
            Angle::new::<degree>(0.0),
            Angle::new::<degree>(180.0),
        )
        .get::<watt_per_square_meter>();
        assert_eq!(c, 0.0);
    }
}
