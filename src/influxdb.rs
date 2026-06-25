use anyhow::Context;
use chrono::{DateTime, Utc};
use influxrs::{InfluxClient, Query};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct InfluxQuery {
    query: Vec<String>,
}
impl InfluxQuery {
    pub fn new(bucket: &str, start: &str, stop: Option<&str>) -> InfluxQuery {
        let range = match stop {
            Some(stop) => format!("|> range(start: {start}, stop: {stop})"),
            None => format!("|> range(start: {start})"),
        };
        // The bucket is a config-sourced string literal like the filter tag/value — escape it the same
        // way so a name containing `"` can't break out of the `from(bucket: "…")` literal.
        InfluxQuery {
            query: vec![format!("from(bucket: \"{}\")", flux_escape(bucket)), range],
        }
    }

    pub fn filter(mut self, tag: &str, value: &str) -> InfluxQuery {
        // Escape `\` and `"` so a config value containing a quote (a stray zone/tag name) produces a
        // valid Flux string literal instead of a malformed query that silently returns nothing.
        let (tag, value) = (flux_escape(tag), flux_escape(value));
        self.query
            .push(format!("|> filter(fn: (r) => r[\"{tag}\"] == \"{value}\")"));
        self
    }

    pub fn filter_tags(mut self, tags: &HashMap<String, String>) -> InfluxQuery {
        for (tag, value) in tags {
            self = self.filter(tag, value);
        }
        self
    }

    pub fn last(mut self) -> InfluxQuery {
        self.query.push("|> last()".to_string());
        self
    }

    /// Down-sample to one mean value per `every` window (a Flux duration like `"1h"`). Empty
    /// windows are dropped, so the returned series can be sparse (callers must align it to a full
    /// grid). Each point is timestamped at its window's **stop** boundary (`timeSrc: "_stop"`,
    /// pinned explicitly): the mean over `[t, t+every)` is stamped at `t+every`.
    pub fn aggregate_window(mut self, every: &str) -> InfluxQuery {
        self.query.push(format!(
            "|> aggregateWindow(every: {every}, fn: mean, createEmpty: false, timeSrc: \"_stop\")"
        ));
        self
    }

    pub fn get_query_string(&self) -> String {
        self.query.join(" ")
    }
}

/// Escape a string for embedding inside a Flux double-quoted literal (`\` then `"`).
fn flux_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// True if `s` is a safe Flux **time expression** — `now()`, an RFC3339 instant, or a relative
/// duration like `-2d`/`+6h`. A `range(start:…, stop:…)` bound is NOT a quoted string literal, so
/// `flux_escape` can't protect it; user-supplied bounds must be validated against this allow-list
/// before interpolation, or an attacker could inject pipeline operations (`… |> from(bucket: …)`).
/// Anything with an operator, space, comma, quote, or paren (other than the literal `now()`) is
/// rejected.
pub fn valid_flux_time(s: &str) -> bool {
    let s = s.trim();
    if s == "now()" {
        return true;
    }
    if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
        return true;
    }
    // Relative duration: an optional sign then `<digits><unit>` over [0-9a-z] only — no operator,
    // space, paren, comma or quote can appear. Must start with a digit and end with a unit letter.
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && body.starts_with(|c: char| c.is_ascii_digit())
        && body.ends_with(|c: char| c.is_ascii_lowercase())
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

/// One measured signal for a zone (e.g. its `temperature`), kept as the raw query spec so we can
/// build either a `last()` lookup or a windowed time-series read on demand.
pub struct InfluxMeasurement {
    name: String,
    bucket: String,
    measurement: String,
    field: String,
    tags: HashMap<String, String>,
}
impl InfluxMeasurement {
    fn base_query<'a>(&'a self, start: &'a str, stop: Option<&'a str>) -> InfluxQuery {
        InfluxQuery::new(&self.bucket, start, stop)
            .filter("_measurement", &self.measurement)
            .filter("_field", &self.field)
            .filter_tags(&self.tags)
    }

    fn last_query(&self) -> InfluxQuery {
        self.base_query("-30d", None).last()
    }
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
                anyhow::bail!("Error parsing config file: {}", e);
            }
        };
        let mut zones: HashMap<String, Vec<InfluxMeasurement>> = HashMap::new();

        for (zone_name, mappings) in config.zone_mappings {
            for (measurement_name, mapping) in mappings {
                zones
                    .entry(zone_name.clone())
                    .or_default()
                    .push(InfluxMeasurement {
                        name: measurement_name,
                        bucket: mapping.bucket,
                        measurement: mapping.measurement,
                        field: mapping.field,
                        tags: mapping.tags,
                    });
            }
        }

        // An empty env var counts as unset, so it falls through to the next token candidate.
        let token_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let key = token_env("INFLUX_TOKEN")
            .or_else(|| token_env("INFLUXDB_TOKEN"))
            .context("INFLUX_TOKEN (or INFLUXDB_TOKEN) environment variable must be set")?;
        // INFLUX_HOST overrides the config host — e.g. the compose service DNS (`http://influxdb:8086`)
        // when run as a container alongside the loxone stack, vs `localhost:8086` on the host.
        let host = std::env::var("INFLUX_HOST")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or(config.db.host);
        let client = InfluxClient::builder(host, key, config.db.org).build()?;
        Ok(InfluxDB { client, zones })
    }

    /// Build a client for a **named extra** InfluxDB instance (host/org/token explicit, no zone
    /// mappings — those belong to the default instance). Used by the pluggable data-source layer when
    /// a house keeps some signals in a second InfluxDB.
    pub fn from_parts(host: &str, org: &str, token: &str) -> anyhow::Result<Self> {
        let client =
            InfluxClient::builder(host.to_string(), token.to_string(), org.to_string()).build()?;
        Ok(InfluxDB {
            client,
            zones: HashMap::new(),
        })
    }

    async fn read(&self, query: &InfluxQuery) -> anyhow::Result<Vec<HashMap<String, String>>> {
        let influxrs_query = Query::raw(query.get_query_string());
        let result = self.client.query(influxrs_query).await?;
        Ok(result)
    }

    /// Run a query and return the raw rows (column name → value, including tags and `_time`). Use
    /// this for non-scalar fields such as the Solcast `hourly_json` blob, which the typed series
    /// readers can't parse. Build the [`InfluxQuery`] with the public builder.
    pub async fn read_rows(
        &self,
        query: &InfluxQuery,
    ) -> anyhow::Result<Vec<HashMap<String, String>>> {
        self.read(query).await
    }

    pub async fn read_zone(&self, zone: &str) -> anyhow::Result<HashMap<String, Vec<String>>> {
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        let measurements = self
            .zones
            .get(zone)
            .ok_or_else(|| anyhow::anyhow!("Zone {} not found", zone))?;
        for measurement in measurements {
            let query_result = self.read(&measurement.last_query()).await?;
            let values = result.entry(measurement.name.clone()).or_default();
            for row in query_result {
                let value = row.get("_value").ok_or_else(|| {
                    anyhow::anyhow!(
                        "No _value in query result for measurement {}",
                        measurement.name
                    )
                })?;
                values.push(value.clone());
            }
        }
        Ok(result)
    }

    /// Read a zone's `temperature` as an evenly-spaced time series (one mean value per `every`
    /// window) over `[start, stop]`. `start`/`stop` are Flux range expressions (RFC3339 instants
    /// or relative like `"-2d"`/`"now()"`); `every` is a Flux duration like `"1h"`. Samples are
    /// returned sorted ascending by time.
    pub async fn read_zone_temperature_series(
        &self,
        zone: &str,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        let m = self
            .zones
            .get(zone)
            .ok_or_else(|| anyhow::anyhow!("Zone {zone} not found"))?
            .iter()
            .find(|m| m.name == "temperature")
            .ok_or_else(|| anyhow::anyhow!("Zone {zone} has no temperature mapping"))?;
        let tags: Vec<(&str, &str)> = m
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.read_series(
            &m.bucket,
            &m.measurement,
            &m.field,
            &tags,
            start,
            stop,
            every,
        )
        .await
    }

    /// Read an arbitrary field as an evenly-spaced mean series over `[start, stop]` — generalizes
    /// [`Self::read_zone_temperature_series`] for non-zone signals (e.g. weather cloud cover or
    /// solar radiation). Samples are returned sorted ascending by time.
    #[allow(clippy::too_many_arguments)] // a thin query primitive; the series selectors are all distinct
    pub async fn read_series(
        &self,
        bucket: &str,
        measurement: &str,
        field: &str,
        tags: &[(&str, &str)],
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        let mut query = InfluxQuery::new(bucket, start, Some(stop))
            .filter("_measurement", measurement)
            .filter("_field", field);
        for (tag, value) in tags {
            query = query.filter(tag, value);
        }
        let mut samples = self
            .read(&query.aggregate_window(every))
            .await?
            .iter()
            .map(parse_time_sample)
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Sanitize at the ingestion boundary: Rust's `f64` parse accepts "NaN"/"Infinity", so a corrupt
        // Influx row could otherwise seed a non-finite sample into the thermal backtest / consumption
        // model / forecasts. Drop those here so every series consumer gets only finite values.
        samples.retain(|s| s.value.is_finite());
        samples.sort_by_key(|s| s.time);
        Ok(samples)
    }

    /// The InfluxDB `room` tag configured for a zone's measurements — used to address the per-zone
    /// **heating relay** (`measurement=relay`, `tag1=heating`, tagged by the same room).
    pub fn zone_room(&self, zone: &str) -> Option<&str> {
        self.zones
            .get(zone)?
            .iter()
            .find_map(|m| m.tags.get("room").map(String::as_str))
    }

    /// Read day-ahead electricity spot prices into EUR/MWh samples (sorted by time). The
    /// bucket/measurement/field and a `scale` come from the caller (the pluggable signal map), so a
    /// house on a different market/feed reads prices without a code change — the default OTE feed is
    /// `ote_prices`/`electricity_prices`/`price` at `scale` 1.0 (`SourceClients::read_prices`).
    /// `start`/`stop` are Flux range expressions; pass an explicit `stop` for the **future** day-ahead
    /// curve (an open-ended range defaults its stop to `now()`, returning only past prices).
    pub async fn read_prices_at(
        &self,
        bucket: &str,
        measurement: &str,
        field: &str,
        scale: f64,
        start: &str,
        stop: &str,
    ) -> anyhow::Result<Vec<PriceSample>> {
        let query = InfluxQuery::new(bucket, start, Some(stop))
            .filter("_measurement", measurement)
            .filter("_field", field);
        let mut samples = self
            .read(&query)
            .await?
            .iter()
            .map(parse_price_row)
            .collect::<anyhow::Result<Vec<_>>>()?;
        if (scale - 1.0).abs() > f64::EPSILON {
            for s in &mut samples {
                s.price_eur_mwh *= scale;
            }
        }
        // Drop non-finite prices (a "NaN"/"Infinity" Influx string parses to a non-finite f64, and a
        // huge scale can overflow) before they reach the tariff/gates/LP — same finitude invariant the
        // scalar/series reads enforce, rather than poisoning the optimizer's cost coefficients.
        samples.retain(|s| s.price_eur_mwh.is_finite());
        samples.sort_by_key(|s| s.time);
        Ok(samples)
    }
}

/// A day-ahead electricity spot-price sample.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSample {
    pub time: DateTime<Utc>,
    pub price_eur_mwh: f64,
}

/// A timestamped scalar measurement (e.g. a zone temperature in °C).
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSample {
    pub time: DateTime<Utc>,
    pub value: f64,
}

fn parse_time_sample(row: &HashMap<String, String>) -> anyhow::Result<TimeSample> {
    let time = row.get("_time").context("sample row missing _time")?;
    let value = row.get("_value").context("sample row missing _value")?;
    Ok(TimeSample {
        time: DateTime::parse_from_rfc3339(time)
            .with_context(|| format!("invalid _time in sample row: {time}"))?
            .with_timezone(&Utc),
        value: value
            .parse::<f64>()
            .with_context(|| format!("invalid _value in sample row: {value}"))?,
    })
}

/// The `(min, max)` price (EUR/MWh) across a set of samples, or `None` if empty.
pub fn price_range(samples: &[PriceSample]) -> Option<(f64, f64)> {
    let mut prices = samples.iter().map(|s| s.price_eur_mwh);
    let first = prices.next()?;
    Some(prices.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
}

fn parse_price_row(row: &HashMap<String, String>) -> anyhow::Result<PriceSample> {
    let time = row.get("_time").context("price row missing _time")?;
    let value = row.get("_value").context("price row missing _value")?;
    Ok(PriceSample {
        time: DateTime::parse_from_rfc3339(time)
            .with_context(|| format!("invalid _time in price row: {time}"))?
            .with_timezone(&Utc),
        price_eur_mwh: value
            .parse::<f64>()
            .with_context(|| format!("invalid _value in price row: {value}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_flux_time_accepts_safe_bounds_rejects_injection() {
        // The legitimate forms: now(), RFC3339 instants, signed relative durations.
        for ok in [
            "now()",
            "-2d",
            "+6h",
            "30m",
            "2026-06-21T12:00:00Z",
            "2026-06-21T12:00:00+02:00",
            "  -1h  ", // surrounding whitespace is trimmed
        ] {
            assert!(valid_flux_time(ok), "{ok:?} should be accepted");
        }
        // Anything carrying a Flux operator, separator, quote, or paren is rejected so it can't break
        // out of `range(start: …)` and inject pipeline stages.
        for bad in [
            "-2d) |> drop(columns: [\"_value\"])",
            "now(), stop: now()",
            "-2d\"",
            "2d -1h",
            "",
            "-",
            "abc", // no leading digit
            "-2",  // no trailing unit
            "-2D", // uppercase unit (Flux units are lowercase)
        ] {
            assert!(!valid_flux_time(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn query_string_without_stop() {
        let q = InfluxQuery::new("mybucket", "-30d", None);
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "mybucket") |> range(start: -30d)"#
        );
    }

    #[test]
    fn query_string_with_stop() {
        let q = InfluxQuery::new("b", "-1h", Some("now()"));
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "b") |> range(start: -1h, stop: now())"#
        );
    }

    #[test]
    fn bucket_name_is_escaped() {
        // A config-sourced bucket containing a quote must be escaped, not break out of the literal.
        let q = InfluxQuery::new(r#"b"x"#, "-1h", None);
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "b\"x") |> range(start: -1h)"#
        );
    }

    #[test]
    fn filter_and_last_appended_in_order() {
        let q = InfluxQuery::new("b", "-1h", None)
            .filter("_measurement", "temperature")
            .last();
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "b") |> range(start: -1h) |> filter(fn: (r) => r["_measurement"] == "temperature") |> last()"#
        );
    }

    #[test]
    fn filter_tags_adds_one_filter_per_tag() {
        let tags = HashMap::from([("room".to_string(), "kitchen".to_string())]);
        let q = InfluxQuery::new("b", "-1h", None).filter_tags(&tags);
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "b") |> range(start: -1h) |> filter(fn: (r) => r["room"] == "kitchen")"#
        );
    }

    #[test]
    fn aggregate_window_query_string() {
        let q = InfluxQuery::new("loxone", "-2d", Some("now()"))
            .filter("_measurement", "temperature")
            .aggregate_window("1h");
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "loxone") |> range(start: -2d, stop: now()) |> filter(fn: (r) => r["_measurement"] == "temperature") |> aggregateWindow(every: 1h, fn: mean, createEmpty: false, timeSrc: "_stop")"#
        );
    }

    #[test]
    fn parse_time_sample_ok() {
        let row = HashMap::from([
            ("_time".to_string(), "2026-06-21T12:00:00Z".to_string()),
            ("_value".to_string(), "22.5".to_string()),
        ]);
        let sample = parse_time_sample(&row).unwrap();
        assert_eq!(sample.value, 22.5);
        assert_eq!(
            sample.time,
            DateTime::parse_from_rfc3339("2026-06-21T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn parse_time_sample_missing_time_errors() {
        let row = HashMap::from([("_value".to_string(), "22.5".to_string())]);
        assert!(parse_time_sample(&row).is_err());
    }

    #[test]
    fn price_query_string() {
        // Assert the InfluxQuery builder emits the right Flux for the OTE price bucket/measurement/field.
        let q = InfluxQuery::new("ote_prices", "-2d", None)
            .filter("_measurement", "electricity_prices")
            .filter("_field", "price");
        assert_eq!(
            q.get_query_string(),
            r#"from(bucket: "ote_prices") |> range(start: -2d) |> filter(fn: (r) => r["_measurement"] == "electricity_prices") |> filter(fn: (r) => r["_field"] == "price")"#
        );
    }

    #[test]
    fn filter_escapes_quotes_and_backslashes() {
        // A config value with a `"` must produce a valid Flux string literal, not a broken query.
        let q = InfluxQuery::new("b", "-1h", None).filter("room", r#"kit"chen"#);
        assert!(q.get_query_string().contains(r#"r["room"] == "kit\"chen""#));
    }

    #[test]
    fn parse_price_row_ok() {
        let row = HashMap::from([
            ("_time".to_string(), "2024-01-15T12:00:00Z".to_string()),
            ("_value".to_string(), "85.4".to_string()),
        ]);
        let sample = parse_price_row(&row).unwrap();
        assert_eq!(sample.price_eur_mwh, 85.4);
        assert_eq!(
            sample.time,
            DateTime::parse_from_rfc3339("2024-01-15T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn parse_price_row_missing_field_errors() {
        let row = HashMap::from([("_value".to_string(), "85.4".to_string())]);
        assert!(parse_price_row(&row).is_err());
    }

    #[test]
    fn price_range_min_max() {
        let sample = |v: f64| PriceSample {
            time: DateTime::parse_from_rfc3339("2024-01-15T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            price_eur_mwh: v,
        };
        assert_eq!(
            price_range(&[sample(90.0), sample(40.0), sample(75.0)]),
            Some((40.0, 90.0))
        );
        assert_eq!(price_range(&[]), None);
    }
}
