extern crate influxrs;

use influxrs::{InfluxClient, Query};
use std::collections::HashMap;

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
            bucket: bucket,
            measurement: measurement,
            tags: tags,
            field: field,
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

pub struct InfluxDB {
    client: InfluxClient,
}
impl InfluxDB {
    pub fn new(url: String, key: String, org: String) -> InfluxDB {
        let client = InfluxClient::builder(url, key, org).build();
        match client {
            Ok(client) => InfluxDB { client: client },
            Err(e) => {
                panic!("Error creating InfluxDB client: {}", e);
            }
        }
    }

    pub async fn read(&self, query: InfluxQuery) -> Vec<HashMap<String, String>> {
        let influxrs_query = Query::raw(query.get_query_string());
        let result = self.client.query(influxrs_query);
        match result.await {
            Ok(result) => {
                println!("Result: {:?}", result);
                return result;
            }
            Err(e) => {
                panic!("Error reading velue from the DB: {}", e);
            }
        }
    }
}
