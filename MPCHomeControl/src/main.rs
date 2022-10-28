mod influxdb;
mod model;

use influxdb::*;
use model::*;

fn main() {
    let model = Model::load("model.json5");
    let _db = InfluxDB::from_config("config.json5");
    println!("{:?}", model);
}
