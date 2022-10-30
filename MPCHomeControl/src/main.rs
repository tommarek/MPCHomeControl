mod influxdb;
//mod model;

use influxdb::*;
//use model::*;

#[tokio::main]
async fn main() {
    //let model = Model::load("model.json5");
    let db = InfluxDB::from_config("config.json5");
    match db {
        Ok(db) => {
            db.read_zone("livingroom".to_owned()).await;
        }
        Err(e) => {
            println!("Error: {}", e);
        }
    }
    //println!("{:?}", model);
}
