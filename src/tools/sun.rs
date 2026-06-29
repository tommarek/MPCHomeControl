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

/// Calculate cloud cover factor
/// using formula from: Estimation of solar radiation from cloud cover data of **Bangladesh** :-D
/// https://sustainenergyres.springeropen.com/articles/10.1186/s40807-016-0031-7
///
/// Arguments:
/// * `cloud_cover` - cloud cover ratio
///
/// Returns:
/// * `Ratio` - cloud cover factor
fn cloud_factor(cloud_cover: Ratio) -> Ratio {
    Ratio::new::<ratio>(0.803) - 0.340 * cloud_cover - 0.458 * cloud_cover * cloud_cover
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

    let cloud_factor = cloud_factor(cloud_cover);
    let atmospheric_attenuation = atmospheric_attenuation(solar_zenith_angle);

    let tilted_irradiance =
        extraterrestrial_irradiance * cos_incidence_angle * cloud_factor * atmospheric_attenuation;

    // Clamp: a negative cos-incidence (sun behind the surface) means no direct irradiance.
    tilted_irradiance.max(watts_per_square_meter(0.0))
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
}
