extern crate nalgebra as na;

mod influxdb;
mod model;
mod tools;
use std::sync::Arc;

use chrono::prelude::*;
use na::Vector3;
use uom::si::area::square_meter;
use uom::si::f64::Area;

use influxdb::*;
use model::*;
use tools::*;

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

    println!(
        "area: {}",
        Arc::new(
            get_effective_illuminated_area(
                49.4949522,
                17.4302361,
                Vector3::new(0.0, 0.0, 1.0),
                Area::new::<square_meter>(1_f64),
                Utc::now()
            )
            .unwrap()
            .get::<square_meter>()
        )
    );

    anyhow::Result::Ok(())
}
