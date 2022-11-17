extern crate nalgebra as na;

use chrono::{DateTime, Utc};
use na::{Dot, Norm, Vector3};
use uom::si::{
    angle::degree,
    area::square_meter,
    catalytic_activity::zetta_enzyme_unit,
    f64::{Angle, Area, HeatFluxDensity, Length, Pressure, Ratio},
    heat_flux_density::watt_per_square_meter,
    length::centimeter,
    pressure::pascal,
    ratio::ratio,
};

/// Get three dimensional Vector from azimuth and zenith angle/wall normal.
/// This can be used to get the norm vector of a wall or sun vector
///
/// Output vector coordinate system is following:
/// - north -> positive x axis / south -> negative x axis
/// - east -> positive y axis / west -> negative y axis
/// - z: up
///
/// # Arguments
/// * `azimuth` - angle between north and the wall normal / sun vector
/// * `zenith_angle` - angle between the wall normal / sun vector and the z axis
///
/// # Returns
/// * `Vector3<f64>` - three dimensional vector
pub fn get_vector_from_angles(azimuth: &Angle, zenith_angle: &Angle) -> Vector3<f64> {
    let x = zenith_angle.sin().get::<ratio>() * azimuth.cos().get::<ratio>();
    let y = zenith_angle.sin().get::<ratio>() * azimuth.sin().get::<ratio>();
    let z = zenith_angle.cos().get::<ratio>();
    Vector3::new(x, y, z).normalize()
}

/// Get a coefficient value used for calculating effectively illuminated area.
///
/// # Arguments
/// * `sun_vector` - normalzied vector pointing to the sun
/// * `surface_normal` - normalized vector of a wall normal
/// # Returns
/// * `f64` - coefficient value [0-1]
fn get_illumination_coefficient(sun_vector: &Vector3<f64>, surface_normal: &Vector3<f64>) -> f64 {
    let cos_theta = sun_vector.dot(surface_normal);
    if cos_theta < 0.0 {
        0.0
    } else {
        cos_theta
    }
}

/// Get the effective illuminated area of a surface. This will be later on used to calculate the
/// solar energy gain of a wall, window, etc.
///
/// # Arguments
/// * `lat` - latitude of the location
/// * `lon` - longitude of the location
/// * `surface_normal` - vector of the surface normal
/// * `surface_area` - area of the surface
/// * `utc` - UTC time
///
/// # Returns
/// * `Area` - effective illuminated area
pub fn get_effective_illuminated_area(
    lat: f64,
    lon: f64,
    surface_normal: &Vector3<f64>,
    surface_area: &Area,
    utc: &DateTime<Utc>,
) -> anyhow::Result<Area> {
    let solar_position = spa::calc_solar_position(*utc, lat, lon)?;
    let sun_vector = get_vector_from_angles(
        &Angle::new::<degree>(solar_position.azimuth),
        &Angle::new::<degree>(solar_position.zenith_angle),
    );
    let surface_normal = surface_normal.normalize();

    let cos_theta = get_illumination_coefficient(&sun_vector, &surface_normal);
    let area: Area = Area::new::<square_meter>(surface_area.get::<square_meter>() * cos_theta);
    anyhow::Ok(area)
}

pub struct Sun {
    pub diffuse_horizontal_irradiance: HeatFluxDensity,
    pub direct_horizontal_irradiance: HeatFluxDensity,
    pub direct_normal_irradiance: HeatFluxDensity,
    pub global_horizontal_irradiance: HeatFluxDensity,
}
impl Sun {
    pub fn irradiance_bird(
        utc: &DateTime<Utc>,
        lat: f64,
        lon: f64,
        aod380: &Length,
        aod500: &Length,
        precipitable_water: &Length,
        ozone: &Length,
        pressure: &Pressure,
        dni_extra: &HeatFluxDensity,
        asymmetry: f64,
        albedo: &f64,
    ) -> Sun {
        // get zenith angle
        let solar_position = spa::calc_solar_position(*utc, lat, lon)?;
        let zenith: Angle = Angle::new::<degree>(solar_position.zenith_angle);

        // calculate air mass
        let airmass = 1.0
            / (zenith.cos().get::<ratio>() + 0.15 * (93.885 - zenith.get::<degree>()).powf(-1.25));
        let am_press = airmass * pressure.get::<pascal>() / 101325.0;

        // rayleigh scattering
        let t_rayleigh =
            (-0.0903 * am_press.powf(0.84) * (1.0 + am_press - am_press.powf(1.01))).exp();

        // ozone absorption
        let am_o3 = airmass * ozone.get::<centimeter>();
        let t_ozone = (1.0
            - 0.1611 * am_o3 * (1.0 + 139.48 * am_o3).powf(-0.3034)
            - 0.002715 * am_o3 / (1.0 + 0.044 * am_o3 + 0.0003 * am_o3.powf(2.0)));

        // gasses absorption
        let t_gases = (-0.0127 * am_press.powf(0.26)).exp();

        // water vapor absorption
        let am_h2o = airmass * precipitable_water.get::<centimeter>();
        let t_water =
            (1.0 - 2.4959 * am_h2o / ((1.0 + 79.034 * am_h2o).powf(0.6828) + 6.385 * am_h2o));

        // aerosol absorption
        let bird_huldstrom =
            0.27583 * aod380.get::<centimeter>() + 0.35 * aod500.get::<centimeter>();
        let t_aerosol = (-(bird_huldstrom.powf(0.873))
            * (1.0 + bird_huldstrom - bird_huldstrom.powf(0.7088))
            * airmass.powf(0.9108))
        .exp();
        let taa = 1.0 - 0.1 * (1.0 - airmass + airmass.powf(1.06)) * (1.0 - t_aerosol);
        let rs = 0.0685 + (1.0 - asymmetry) * (1.0 - t_aerosol / taa);

        // direct normal irradiance
        let direct_normal_irradiance = 0.9662
            * dni_extra.get::<watt_per_square_meter>()
            * t_aerosol
            * t_water
            * t_gases
            * t_ozone
            * t_rayleigh;

        // direct_horizontal_irradiance
        let ze_cos = if zenith.get::<degree>() < 90.0 {
            zenith.cos().get::<ratio>()
        } else {
            0.0
        };
        let direct_horizontal_irradiance = direct_normal_irradiance * ze_cos;

        // global horizontal irradiance
        let ias = dni_extra.get::<watt_per_square_meter>()
            * ze_cos
            * 0.79
            * t_ozone
            * t_gases
            * t_water
            * taa
            * (0.5 * (1.0 - t_rayleigh) + asymmetry * (1.0 - (t_aerosol / taa)))
            / (1.0 - airmass + airmass.powf(1.02));
        let global_horizontal_irradiance =
            (direct_horizontal_irradiance + ias) / (1.0 - albedo * rs);

        // diffuse horizontal irradiance
        let diffuse_horizontal_irradiance =
            global_horizontal_irradiance - direct_horizontal_irradiance;
        Sun {
            diffuse_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                diffuse_horizontal_irradiance,
            ),
            direct_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                direct_horizontal_irradiance,
            ),
            direct_normal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                direct_normal_irradiance,
            ),
            global_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                global_horizontal_irradiance,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use nalgebra::{assert_approx_eq_eps, ApproxEq, Vector3};
    use uom::si::angle::degree;
    use uom::si::f64::Angle;

    #[test]
    fn test_get_90_deg_north_wall_normal() {
        let azimuth = Angle::new::<degree>(0_f64);
        let wall_angle = Angle::new::<degree>(90_f64);
        let normal = super::get_vector_from_angles(&azimuth, &wall_angle);
        assert_approx_eq_eps!(Vector3::new(1.0, 0.0, 0.0), normal, 0.1);
    }

    #[test]
    fn test_get_illumination_coef_direct_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.0, 0.0, 1.0);
        let coef = super::get_illumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(1.0, coef, 0.1);
    }

    #[test]
    fn test_get_illumination_coef_45deg_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.7071067811865475, 0.0, 0.7071067811865476);
        let coef = super::get_illumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(0.7071, coef, 0.1);
    }
}
