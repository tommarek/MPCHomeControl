extern crate nalgebra as na;

mod influxdb;
mod model;
mod tools;

use chrono::prelude::*;
use uom::si::{
    angle::degree,
    f64::{Angle, Length, Pressure, Ratio, TemperatureInterval},
    length::centimeter,
    pressure::pascal,
    ratio::percent,
    temperature_interval::degree_celsius,
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

    let now = &Utc::now();

    let csi = ClearSkyIrradiance::new(
        now,
        &Angle::new::<degree>(49.4949522),
        &Angle::new::<degree>(17.4302361),
        &Length::new::<centimeter>(0.15),
        &Length::new::<centimeter>(0.1),
        &get_total_precipitable_water(
            &TemperatureInterval::new::<degree_celsius>(10.0),
            &Ratio::new::<percent>(60.0),
        ),
        &Length::new::<centimeter>(0.3),
        &Pressure::new::<pascal>(100400.0),
        &0.85,
        &get_typical_albedo(now),
    );
    println!("CSI: {:?}", csi);

    println!("clear_sky_irradiance: {:?}", csi.get_clear_sky_irradiance());
    let total_clear_sky_irradiance = csi.irradiance.get_total_irradiance_on_tilted_surface(
        now,
        get_typical_albedo(now),
        &Angle::new::<degree>(180.0),
        &Angle::new::<degree>(35.0),
        &csi.solar_azimuth,
        &csi.solar_zenith,
    );
    println!(
        "total clear_sky_irradiance: {:?}",
        total_clear_sky_irradiance
    );

    // cloud sky
    let cloud_sky_irradiance = csi.get_cloud_sky_irradiance(
        &Ratio::new::<percent>(31.2),
        &Ratio::new::<percent>(0.0),
        &Ratio::new::<percent>(74.6),
        &Ratio::new::<percent>(74.6),
        false,
    );
    println!("cloud_sky_irradiance: {:?}", cloud_sky_irradiance);
    let total_cloud_sky_irradiance = cloud_sky_irradiance.get_total_irradiance_on_tilted_surface(
        now,
        get_typical_albedo(now),
        &Angle::new::<degree>(222.0),
        &Angle::new::<degree>(35.0),
        &csi.solar_azimuth,
        &csi.solar_zenith,
    );
    println!(
        "total cloud_sky_irradiance: {:?}",
        total_cloud_sky_irradiance
    );

    anyhow::Result::Ok(())
}
