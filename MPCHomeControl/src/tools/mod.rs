extern crate nalgebra as na;

use chrono::{DateTime, Utc};
use na::{Dot, Norm, Vector3};
use uom::si::{
    angle::degree,
    area::square_meter,
    f64::{Angle, Area},
    ratio::ratio,
};

/**
 * Get effective iluminated area. This can be used to get the norm vector of a wall or sun vector
 *
 * @param azimuth Azimuth of the sun or the direction of wall normal vector
 * @param zenith_angle Zenith angle of the sun or the angle between wall normal
 *                     vector and the vertical vector
 */
pub fn get_vector_from_angles(azimuth: Angle, zenith_angle: Angle) -> Vector3<f64> {
    let x = zenith_angle.sin().get::<ratio>() * azimuth.cos().get::<ratio>();
    let y = zenith_angle.sin().get::<ratio>() * azimuth.sin().get::<ratio>();
    let z = zenith_angle.cos().get::<ratio>();
    Vector3::new(x, y, z).normalize()
}

fn get_ilumination_coefficient(sun_vector: &Vector3<f64>, surface_normal: &Vector3<f64>) -> f64 {
    let cos_theta = sun_vector.dot(surface_normal);
    if cos_theta < 0.0 {
        0.0
    } else {
        cos_theta
    }
}
pub fn get_effective_iluminated_area(
    lat: f64,
    lon: f64,
    surface_normal: Vector3<f64>,
    area: Area,
    utc: DateTime<Utc>,
) -> anyhow::Result<Area> {
    let solar_position = spa::calc_solar_position(utc, lat, lon)?;
    let sun_vector = get_vector_from_angles(
        Angle::new::<degree>(solar_position.azimuth),
        Angle::new::<degree>(solar_position.zenith_angle),
    );
    let surface_normal = surface_normal.normalize();

    let cos_theta = get_ilumination_coefficient(&sun_vector, &surface_normal);
    let area: Area = Area::new::<square_meter>(area.get::<square_meter>() * cos_theta);
    anyhow::Ok(area)
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
        let normal = super::get_vector_from_angles(azimuth, wall_angle);
        assert_approx_eq_eps!(Vector3::new(1.0, 0.0, 0.0), normal, 0.1);
    }

    #[test]
    fn test_get_ilumination_coef_direct_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.0, 0.0, 1.0);
        let coef = super::get_ilumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(1.0, coef, 0.1);
    }

    #[test]
    fn test_get_ilumination_coef_45deg_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.7071067811865475, 0.0, 0.7071067811865476);
        let coef = super::get_ilumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(0.7071, coef, 0.1);
    }
}
