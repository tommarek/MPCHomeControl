extern crate nalgebra as na;

mod influxdb;
mod model;
mod tools;
use na::Vector3;

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
        get_iluminated_area(49.4949522, 17.4302361, Vector3::new(0.0, 0.0, 1.0), 1.0)?
    );

    anyhow::Result::Ok(())
}
