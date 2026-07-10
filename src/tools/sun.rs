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
/// Returns:
/// * `HeatFluxDensity` - solar irradiance on tilted surface
pub fn calculate_tilted_irradiance(
    latitude: Angle,
    longitude: Angle,
    datetime: &DateTime<Utc>,
    cloud_cover: Ratio,
    surface_angle_from_horizontal: Angle,
    surface_azimuth: Angle,
) -> HeatFluxDensity {
    let degrees = Angle::new::<degree>;
    let watts_per_square_meter = HeatFluxDensity::new::<watt_per_square_meter>;

    let solar_position = spa::calc_solar_position(
        *datetime,
        latitude.get::<degree>(),
        longitude.get::<degree>(),
    )
    .unwrap();
    let solar_zenith_angle = degrees(solar_position.zenith_angle);

    // The sun is at or below the horizon: no direct beam reaches any surface.
    // This also avoids the negative / infinite air mass that cos(zenith) <= 0 would
    // otherwise feed into atmospheric_attenuation (which would amplify, not attenuate).
    if solar_zenith_angle >= degrees(90.0) {
        return watts_per_square_meter(0.0);
    }

    let solar_azimuth_angle = degrees(solar_position.azimuth);

    let cos_incidence_angle = (solar_zenith_angle.cos() * surface_angle_from_horizontal.cos())
        + (solar_zenith_angle.sin()
            * surface_angle_from_horizontal.sin()
            * (solar_azimuth_angle - surface_azimuth).cos());

    let extraterrestrial_irradiance = watts_per_square_meter(1361.0);
    let atmospheric_attenuation = atmospheric_attenuation(solar_zenith_angle);

    // Global horizontal → beam + isotropic diffuse (Kasten–Czeplak split): the beam projects onto
    // the surface via the incidence angle; the diffuse sees `(1 + cos β)/2` of the sky dome.
    // Ground-reflected irradiance is neglected.
    let ghi = extraterrestrial_irradiance
        * atmospheric_attenuation
        * solar_zenith_angle.cos()
        * cloud_transmittance(cloud_cover);
    let d_frac = diffuse_fraction(cloud_cover);
    let diffuse_h = ghi * d_frac;
    let beam_h = ghi * (1.0 - d_frac);
    // Beam on the tilt: normal beam (beam_h / cos z) times the incidence cosine, zero when the sun
    // is behind the surface.
    let beam_t = (beam_h / solar_zenith_angle.cos()) * cos_incidence_angle.get::<ratio>().max(0.0);
    let sky_view = (1.0 + surface_angle_from_horizontal.cos().get::<ratio>()) / 2.0;
    let diffuse_t = diffuse_h * sky_view;

    (beam_t + diffuse_t).max(watts_per_square_meter(0.0))
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
}
