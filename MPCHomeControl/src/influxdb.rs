extern crate influxrs;

use std::collections::HashMap;
use influxrs::{ Measurement, InfluxClient, Query };


mod influxdb {
    pub struct InfluxQuery {
        bucket: String,
        measurement: String,
        tags: HashMap<String, String>,
        field: String,
        query: String,
    }
    impl InfluxQuery {
        pub fn new(bucket: String, measurement: String, tags: HashMap<String, String>, field: String) -> InfluxQuery {
            InfluxQuery {
                bucket: bucket,
                measurement: measurement,
                tags: tags,
                field: field,
                query: String::new(),
            }
        }

        pub fn get_query(&self) -> InfluxQuery {
            self.query = String::new();
            self.query.push_str("from(bucket: \"");
            self.query.push_str(&self.bucket);
            self.query.push_str("\")");

            self.query.push_str(" |> filter(fn: (r) => r[\"_measurement\"] == \"");
            self.query.push_str(&self.measurement);
            self.query.push_str("\")");

            self.query.push_str("|> filter(fn: (r) => r._field == \"");
            self.query.push_str(&field);
            self.query.push_str("\")");

            for (tag, value) in &self.tags {
                self.query.push_str("|> filter(fn: (r) => r[\"");
                self.query.push_str(&tag);
                self.query.push_str("\"] == \"");
                self.query.push_str(&value);
                self.query.push_str("\")");
            }
            self
        }

        pub fn last(&self) -> String {
            self.query.push_str("|> last()");
        }

        pub fn range(&self, start: String, stop: Option<String>) -> String {
            self.query.push_str("|> range(start: ");
            self.query.push_str(&start);
            match stop {
                Some(stop) => {
                    self.query.push_str(", stop: ");
                    self.query.push_str(&stop);
                }
                None => {}
            }
            self.query.push_str(")");

        }
        pub fn get_last_value_query(&self) -> String {
            self.get_query()
                .last();
        }
    }

    pub struct InfluxDB {
        client: InfluxClient,
    }
    impl InfluxDB {
        pub fn new(host: String, org: String) -> InfluxDB {
            InfluxDB {
                client: InfluxClient::new(host, org),
            }
        }

        pub fn read(&self, query: InfluxQuery) -> String {
            let result = self.client.query(query.get_query());
            match result {
                Ok(result) => {
                    println!("Result: {:?}", result);
                    return result;
                }
                Err(e) => {
                    println!("Error: {:?}", e);
                    return e.to_string();
                }
            }
        }

        pub fn write(&self, measurement: Measurement) {
            self.client.write(measurement);
        }
    }
}