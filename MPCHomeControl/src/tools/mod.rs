extern crate nalgebra as na;

use chrono::prelude::*;
use na::{Dot, Norm, Vector3};
use spa;

pub fn calc_solar_vector(azimuth: f64, zenith_angle: f64) -> Vector3<f64> {
    let azimuth = azimuth.to_radians();
    let elevation_deg = 90_f64 - zenith_angle;
    let elevation = elevation_deg.to_radians();
    let x = elevation.cos() * azimuth.cos();
    let y = elevation.cos() * azimuth.sin();
    let z = elevation.sin();
    Vector3::new(x, y, z)
}

pub fn get_iluminated_area(
    lat: f64,
    lon: f64,
    surface_normal: Vector3<f64>,
    area: f64,
) -> anyhow::Result<f64> {
    let solar_position = spa::calc_solar_position(Utc::now(), lat, lon)?;
    println!("solar_position: {:?}", solar_position);
    let solar_vector =
        calc_solar_vector(solar_position.azimuth, solar_position.zenith_angle).normalize();
    println!("solar_vector: {:?}", solar_vector);
    let surface_normal = surface_normal.normalize();
    println!("surface_normal: {:?}", surface_normal);

    let cos_theta = solar_vector.dot(&surface_normal);
    let area = area * cos_theta;
    anyhow::Ok(area)
}
