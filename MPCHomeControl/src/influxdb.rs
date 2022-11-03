extern crate influxrs;

use influxrs::{InfluxClient, Query};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone)]
pub struct InfluxQuery {
    query: Vec<String>,
}
impl InfluxQuery {
    pub fn new(bucket: &str, start: &str, stop: Option<&str>) -> InfluxQuery {
        let mut query = Vec::new();
        query.push(format!("from(bucket: \"{}\")", bucket));

        match stop {
            Some(stop) => {
                query.push(format!("|> range(start: {}, stop: {})", start, stop));
            }
            None => {
                query.push(format!("|> range(start: {})", start));
            }
        }
        InfluxQuery { query }
    }

    pub fn filter(&mut self, tag: &str, value: &str) -> &mut InfluxQuery {
        self.query.push(format!(
            "|> filter(fn: (r) => r[\"{}\"] == \"{}\")",
            tag, value
        ));
        self
    }

    pub fn filter_tags(&mut self, tags: &HashMap<String, String>) -> &mut InfluxQuery {
        for (tag, value) in tags {
            self.filter(tag, value);
        }
        self
    }

    pub fn last(&mut self) -> &mut InfluxQuery {
        self.query.push("|> last()".to_string());
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
                let query = InfluxQuery::new(&mapping.bucket, "-30d", None)
                    .filter("_measurement", &mapping.measurement)
                    .filter("_field", &mapping.field)
                    .filter_tags(&mapping.tags)
                    .last()
                    .clone();

                zones
                    .entry(zone_name.clone())
                    .or_insert_with(Vec::new)
                    .push(InfluxMeasurement {
                        measurement: measurement_name,
                        query,
                    });
            }
        }

        let key = std::env::var("INFLUX_TOKEN")?;
        let client = InfluxClient::builder(config.db.host, key, config.db.org).build()?;
        Ok(InfluxDB { client, zones })
    }

    pub async fn read(&self, query: &InfluxQuery) -> anyhow::Result<Vec<HashMap<String, String>>> {
        let influxrs_query = Query::raw(query.get_query_string());
        let result = self.client.query(influxrs_query).await?;
        Ok(result)
    }

    pub async fn read_zone(&self, zone: &str) -> anyhow::Result<HashMap<String, Vec<String>>> {
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        let measurements = self
            .zones
            .get(zone)
            .ok_or_else(|| anyhow::anyhow!("Zone {} not found", zone))?;
        for measurement in measurements {
            result
                .entry(measurement.measurement.clone())
                .or_insert_with(Vec::new);
            println!("Query: {}", measurement.query.get_query_string());
            let query_result = self.read(&measurement.query).await?;
            for row in query_result {
                let value = row.get("_value").ok_or_else(|| {
                    anyhow::anyhow!(
                        "No _value in query result for measurement {}",
                        measurement.measurement
                    )
                })?;
                result
                    .get_mut(measurement.measurement.as_str())
                    .unwrap()
                    .push(value.clone());
            }
        }
        Ok(result.clone())
    }
}
