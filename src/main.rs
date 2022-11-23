extern crate nalgebra as na;

mod influxdb;
mod model;
mod rc_network;
mod tools;

use chrono::prelude::*;
use uom::si::heat_flux_density::watt_per_square_meter;
use uom::si::{
    angle::degree,
    f64::{Angle, Ratio},
    ratio::percent,
};

use influxdb::*;
use model::*;
use tools::sun::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model = Model::load("model.json5")?;
    println!("{:?}", model);

    let db = InfluxDB::from_config("config.json5");
    match db {
        Ok(db) => {
            let livingroom = db.read_zone("livingroom").await;
            println!("livingroom: {:?}", livingroom);
        }
        Err(e) => {
            println!("Error: {}", e);
        }
    }

    let latitude = Angle::new::<degree>(49.4949522);
    let longitude = Angle::new::<degree>(17.4302361);
    let datetime = DateTime::parse_from_rfc3339("2023-06-29T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let cloud_cover = Ratio::new::<percent>(30.0);

    let surface_angle = Angle::new::<degree>(0.0);
    let surface_azimuth = Angle::new::<degree>(180.0);

    let tilted_irradiance = calculate_tilted_irradiance(
        latitude,
        longitude,
        &datetime,
        cloud_cover,
        surface_angle,
        surface_azimuth,
    );
    println!(
        "Total irradiance on tilted surface: {:.2} W/m^2",
        tilted_irradiance.get::<watt_per_square_meter>()
    );

    anyhow::Result::Ok(())
}
