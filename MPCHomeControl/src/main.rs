mod influxdb;
mod model;

use influxdb::*;
use model::*;

fn main() {
    let model = Model::load("model.json5");
    println!("{:?}", model);
}
