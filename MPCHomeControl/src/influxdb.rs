extern crate influxrs;

use influxrs::{InfluxClient, Query};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone)]
pub struct InfluxQuery {
    bucket: String,
    measurement: String,
    tags: HashMap<String, String>,
    field: String,
    query: Vec<String>,
}
impl InfluxQuery {
    pub fn new(
        bucket: String,
        measurement: String,
        tags: HashMap<String, String>,
        field: String,
    ) -> InfluxQuery {
        InfluxQuery {
            bucket,
            measurement,
            tags,
            field,
            query: Vec::new(),
        }
    }

    pub fn start_query<'a>(
        &'a mut self,
        start: String,
        stop: Option<String>,
    ) -> &'a mut InfluxQuery {
        self.query = Vec::new();
        self.query
            .push(format!("from(bucket: \"{}\")", self.bucket));

        match stop {
            Some(stop) => {
                self.query
                    .push(format!("|> range(start: {}, stop: {})", start, stop));
            }
            None => {
                self.query.push(format!("|> range(start: {})", start));
            }
        }
        self
    }

    pub fn filter<'a>(&'a mut self, tag: String, value: String) -> &'a mut InfluxQuery {
        self.query.push(format!(
            "|> filter(fn: (r) => r[\"{}\"] == \"{}\")",
            tag, value
        ));
        self
    }

    pub fn last<'a>(&'a mut self) -> &'a mut InfluxQuery {
        self.query.push("|> last()".to_string());
        self
    }

    pub fn range<'a>(&'a mut self, start: String, stop: Option<String>) -> &'a mut InfluxQuery {
        match stop {
            Some(stop) => {
                self.query
                    .push(format!("|> range(start: {}, stop: {})", start, stop));
            }
            None => {
                self.query.push(format!("|> range(start: {})", start));
            }
        }
        self
    }

    pub fn get_query_string(&self) -> String {
        self.query.join(" ")
    }
}

#[derive(Debug, Deserialize)]
struct ConfigDB {
    host: String,
    org: String,
    token: String,
}
#[derive(Debug, Deserialize)]
struct JSONConfigMeasurement {
    bucket: String,
    measurement: String,
    tags: HashMap<String, String>,
    field: String,
}
#[derive(Debug, Deserialize)]
struct JSONConfig {
    db: ConfigDB,
    zone_mappings: HashMap<String, HashMap<String, JSONConfigMeasurement>>,
}

pub struct InfluxMeasurement {
    measurement: String,
    query: InfluxQuery,
}
pub struct InfluxDB {
    client: InfluxClient,
    zones: HashMap<String, Vec<InfluxMeasurement>>,
}
impl InfluxDB {
    pub fn new(url: String, key: String, org: String) -> anyhow::Result<Self> {
        let client = InfluxClient::builder(url, key, org).build();
        match client {
            Ok(client) => Ok(InfluxDB {
                client,
                zones: HashMap::new(),
            }),
            Err(e) => {
                anyhow::bail!("Error creating InfluxDB client: {}", e);
            }
        }
    }

    pub fn from_config<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let string = fs::read_to_string(path)?;
        let config: JSONConfig = match json5::from_str(&string) {
            Ok(config) => config,
            Err(e) => {
                println!("e: {}", e);
                anyhow::bail!("Error parsing config file: {}", e);
            }
        };
        let mut zones = HashMap::new();

        for (zone_name, mappings) in config.zone_mappings {
            for (measurement_name, mapping) in mappings {
                let mut query = InfluxQuery::new(
                    mapping.bucket.clone(),
                    mapping.measurement.clone(),
                    mapping.tags.clone(),
                    mapping.field.clone(),
                );
                query
                    .start_query("-30d".to_owned(), None)
                    .filter("_measurement".to_owned(), mapping.measurement)
                    .filter("_field".to_owned(), mapping.field);
                for (tag, value) in &mapping.tags {
                    query.filter(tag.to_string(), value.to_string());
                }
                query.last();

                zones
                    .entry(zone_name.clone())
                    .or_insert(Vec::new())
                    .push(InfluxMeasurement {
                        measurement: measurement_name,
                        query: query.clone(),
                    });
            }
        }

        let client =
            InfluxClient::builder(config.db.host, config.db.token, config.db.org).build()?;
        Ok(InfluxDB { client, zones })
    }

    pub async fn read(&self, query: InfluxQuery) -> anyhow::Result<Vec<HashMap<String, String>>> {
        let influxrs_query = Query::raw(query.get_query_string());
        let result = self.client.query(influxrs_query);
        match result.await {
            Ok(result) => {
                println!("Result: {:?}", result);
                Ok(result)
            }
            Err(e) => {
                anyhow::bail!("Error reading velue from the DB: {}", e);
            }
        }
    }

    pub async fn read_zone(&self, zone: String) -> anyhow::Result<HashMap<String, String>> {
        let mut result = HashMap::new();
        match self.zones.get(&zone) {
            Some(measurements) => {
                for measurement in measurements {
                    println!("Query: {}", measurement.query.get_query_string());
                    let query_result = self.read(measurement.query.clone()).await?;
                    if query_result.len() > 0 {
                        result.insert(
                            measurement.measurement.clone(),
                            query_result[0]["_value"].clone(),
                        );
                    }
                }
                Ok(result)
            }
            None => {
                anyhow::bail!("Zone {} not found", zone);
            }
        }
    }
}
