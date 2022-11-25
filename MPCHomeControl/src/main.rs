extern crate nalgebra as na;

mod influxdb;
mod model;
mod tools;

use chrono::prelude::*;
use na::Vector3;
use uom::si::angle::degree;
use uom::si::area::square_meter;
use uom::si::f64::{Angle, Area, Length, Pressure};
use uom::si::length::centimeter;
use uom::si::pressure::pascal;

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

    let now = &Utc::now();
    println!(
        "area: {}",
        get_effective_illuminated_area(
            49.4949522,
            17.4302361,
            &Vector3::new(0.0, 0.0, 1.0),
            &Area::new::<square_meter>(1_f64),
            now
        )
        .unwrap()
        .get::<square_meter>()
    );

    let csi = ClearSkyIrradiance::new_bird(
        now,
        49.4949522,
        17.4302361,
        &Length::new::<centimeter>(0.15),
        &Length::new::<centimeter>(0.1),
        &Length::new::<centimeter>(1.5),
        &Length::new::<centimeter>(0.3),
        &Pressure::new::<pascal>(100400.0),
        &0.85,
        &get_typical_albedo(now),
    );
    println!("{:?}", csi);

    let total_irradiance = csi.get_total_irradiance_on_tilted_surface(
        &Angle::new::<degree>(180.0),
        &Angle::new::<degree>(35.0),
    );
    println!("total irradiance: {:?}", total_irradiance);

    anyhow::Result::Ok(())
}
