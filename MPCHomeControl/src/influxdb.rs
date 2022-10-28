extern crate influxrs;

use influxrs::{InfluxClient, Query};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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

    pub fn build_query<'a>(&'a mut self) -> &'a mut InfluxQuery {
        self.query = Vec::new();
        self.query
            .push(format!("from(bucket: \"{}\")", self.bucket));
        self.query.push(format!(
            "|> filter(fn: (r) => r[\"_measurement\"] == \"{}\")",
            self.measurement
        ));
        self.query.push(format!(
            "|> filter(fn: (r) => r[\"_field\"] == \"{}\")",
            self.field
        ));

        for (tag, value) in &self.tags {
            self.query.push(format!(
                "|> filter(fn: (r) => r[\"{}\"] == \"{}\"",
                tag, value
            ));
        }
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
struct JSONConfigZoneMappings {
    measurements: HashMap<String, JSONConfigMeasurement>,
}
#[derive(Debug, Deserialize)]
struct JSONConfig {
    db: ConfigDB,
    zone_mappings: HashMap<String, JSONConfigZoneMappings>,
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
        let config: JSONConfig = json5::from_str(&string)?;
        let zones = HashMap::new();

        for (zone_name, mappings) in config.zone_mappings {
            for (measurement_name, mapping) in mappings.measurements {
                let query = InfluxQuery::new(
                    mapping.bucket,
                    mapping.measurement,
                    mapping.tags,
                    mapping.field,
                )
                .build_query()
                .last();

                zones
                    .entry(zone_name)
                    .or_insert(Vec::new())
                    .push(InfluxMeasurement {
                        measurement: measurement_name,
                        query: query,
                    });
                println!("Query: {}", query.get_query_string());
            }
        }

        let client = InfluxClient::builder(config.db.host, config.db.token, config.db.org).build();
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
}
