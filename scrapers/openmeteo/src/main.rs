//! Open-meteo → InfluxDB scraper.
//!
//! The first of the **modular scrapers** that will gradually take over data collection from
//! `loxone_smart_home`: fetches the open-meteo hourly forecast (temperature, cloud, wind, and the
//! **radiation** fields the MPC's solar model consumes) and writes every future hour as its own
//! **future-dated** point into the same InfluxDB series the house has always used
//! (`weather_forecast` / measurement `weather_forecast`, tags `room=outside, type=hour,
//! source=openmeteo`) — so the brain's readers and any existing writer are untouched, and the two
//! writers merge idempotently (same series + timestamp ⇒ last write wins per field).
//!
//! Design rules for every scraper in this workspace:
//! - One crate per source (this file is the template): fetch → pure record extraction → line
//!   protocol → one batched write. Pure parts unit-tested; the IO layer stays thin.
//! - **Honest nulls**: a field the source reports as `null` is *skipped*, never 0-coerced — an
//!   absent radiation value must read as "unknown" downstream, not "no sun".
//! - The Influx token comes only from the `INFLUXDB_TOKEN` env var; it is never configured or
//!   logged.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;

/// Every hourly field the loxone scraper requests today — kept identical so this scraper can
/// eventually replace it field-for-field. The MPC itself reads `temperature_2m`, `cloudcover`,
/// `shortwave_radiation`, `direct_radiation`, `diffuse_radiation`.
const HOURLY_FIELDS: &str = "temperature_2m,relativehumidity_2m,dewpoint_2m,apparent_temperature,\
    precipitation,rain,showers,snowfall,surface_pressure,cloudcover,\
    cloudcover_low,cloudcover_mid,cloudcover_high,visibility,\
    windspeed_10m,winddirection_10m,windgusts_10m,temperature_80m,\
    shortwave_radiation,direct_radiation,diffuse_radiation,\
    direct_normal_irradiance,terrestrial_radiation,\
    shortwave_radiation_instant,direct_radiation_instant,\
    diffuse_radiation_instant,direct_normal_irradiance_instant,\
    terrestrial_radiation_instant";

#[derive(Debug, Deserialize)]
struct Config {
    influx: InfluxConfig,
    site: SiteConfig,
    /// Scrape cadence (minutes). Default 30 — the same cadence the house scraper runs at.
    #[serde(default = "default_interval_minutes")]
    interval_minutes: u64,
    /// How many future hours to write per scrape (open-meteo returns up to 7 days; the MPC reads
    /// at most 2–3 days ahead). Default 72.
    #[serde(default = "default_horizon_hours")]
    horizon_hours: usize,
    /// Open-meteo endpoint (overridable for a self-hosted instance).
    #[serde(default = "default_openmeteo_url")]
    openmeteo_url: String,
}

#[derive(Debug, Deserialize)]
struct InfluxConfig {
    /// e.g. `http://influxdb:8086` on `caddy_net`, or the SSH tunnel port for local runs.
    url: String,
    org: String,
    #[serde(default = "default_bucket")]
    bucket: String,
}

#[derive(Debug, Deserialize)]
struct SiteConfig {
    latitude: f64,
    longitude: f64,
}

fn default_interval_minutes() -> u64 {
    30
}
fn default_horizon_hours() -> usize {
    72
}
fn default_bucket() -> String {
    "weather_forecast".to_string()
}
fn default_openmeteo_url() -> String {
    "https://api.open-meteo.com/v1/forecast".to_string()
}

/// One future hour's numeric fields, keyed by the open-meteo field name.
type Record = BTreeMap<String, f64>;

/// Extract the future hourly records from an open-meteo response: one `(unix_seconds, fields)`
/// per hour ≥ `now_hour`, capped at `horizon_hours`. `null` values are **skipped** (see the
/// module docs), non-numeric values ignored. Pure.
fn hourly_records(
    hourly: &serde_json::Value,
    now_hour: i64,
    horizon_hours: usize,
) -> Vec<(i64, Record)> {
    let Some(times) = hourly.get("time").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, t) in times.iter().enumerate() {
        let Some(ts) = t.as_i64() else { continue };
        if ts < now_hour {
            continue;
        }
        if out.len() >= horizon_hours {
            break;
        }
        let mut fields = Record::new();
        if let Some(obj) = hourly.as_object() {
            for (name, values) in obj {
                if name == "time" {
                    continue;
                }
                if let Some(v) = values.get(i).and_then(|v| v.as_f64()) {
                    fields.insert(name.clone(), v);
                }
            }
        }
        if !fields.is_empty() {
            out.push((ts, fields));
        }
    }
    out
}

/// Escape a line-protocol string element (tag values / field keys): `,`, `=`, and space.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace('=', "\\=")
        .replace(' ', "\\ ")
}

/// One line-protocol line for a record, second-precision timestamp. The tag set matches the
/// house's existing writer exactly, so both merge into the same series.
fn to_line(ts: i64, fields: &Record) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    let fieldset = fields
        .iter()
        .map(|(k, v)| format!("{}={}", escape(k), v))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!(
        "weather_forecast,room=outside,source=openmeteo,type=hour {fieldset} {ts}"
    ))
}

/// Fetch the forecast and return the batched line-protocol body.
fn scrape(config: &Config) -> Result<String> {
    let response: serde_json::Value = ureq::get(&config.openmeteo_url)
        .query("latitude", &config.site.latitude.to_string())
        .query("longitude", &config.site.longitude.to_string())
        .query("hourly", HOURLY_FIELDS)
        .query("models", "best_match")
        .query("windspeed_unit", "ms")
        .query("timeformat", "unixtime")
        .query("timezone", "GMT")
        .call()
        .context("open-meteo request failed")?
        .into_json()
        .context("open-meteo response is not JSON")?;
    let hourly = response
        .get("hourly")
        .context("open-meteo response has no `hourly` block")?;
    let now = chrono::Utc::now().timestamp();
    let now_hour = now - now.rem_euclid(3600);
    let records = hourly_records(hourly, now_hour, config.horizon_hours);
    ensure!(!records.is_empty(), "open-meteo returned no future hours");
    Ok(records
        .iter()
        .filter_map(|(ts, fields)| to_line(*ts, fields))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// POST the batch to the InfluxDB v2 write API.
fn write_influx(config: &Config, token: &str, body: &str) -> Result<()> {
    let url = format!(
        "{}/api/v2/write?org={}&bucket={}&precision=s",
        config.influx.url.trim_end_matches('/'),
        config.influx.org,
        config.influx.bucket
    );
    ureq::post(&url)
        .set("Authorization", &format!("Token {token}"))
        .set("Content-Type", "text/plain; charset=utf-8")
        .send_string(body)
        .context("influx write failed")?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "scraper.json5".to_string());
    let once = args.iter().any(|a| a == "--once");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let config: Config = json5::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
    )
    .with_context(|| format!("parsing {path}"))?;
    let token = if dry_run {
        String::new()
    } else {
        std::env::var("INFLUXDB_TOKEN").context("INFLUXDB_TOKEN not set")?
    };

    loop {
        match scrape(&config) {
            Ok(body) => {
                let hours = body.lines().count();
                if dry_run {
                    println!("{body}");
                    eprintln!("[openmeteo] dry-run: {hours} hourly points (not written)");
                } else {
                    match write_influx(&config, &token, &body) {
                        Ok(()) => eprintln!(
                            "[openmeteo] wrote {hours} future hourly points to {}",
                            config.influx.bucket
                        ),
                        Err(e) => eprintln!("[openmeteo] {e:#}"),
                    }
                }
            }
            Err(e) => eprintln!("[openmeteo] {e:#}"),
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(config.interval_minutes.max(1) * 60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now_hour: i64) -> serde_json::Value {
        serde_json::json!({
            "time": [now_hour - 3600, now_hour, now_hour + 3600, now_hour + 7200],
            "temperature_2m": [10.0, 11.0, 12.0, 13.0],
            "cloudcover": [50, 60, 70, 80],
            // Radiation goes null past the provider's radiation horizon.
            "direct_radiation": [100.0, 120.0, null, null],
            "diffuse_radiation": [40.0, 50.0, 60.0, null],
        })
    }

    #[test]
    fn hourly_records_skip_past_and_nulls() {
        let now_hour = 1_700_000_400; // any hour boundary
        let recs = hourly_records(&sample(now_hour), now_hour, 72);
        // The past hour is dropped; three future hours remain.
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].0, now_hour);
        assert_eq!(recs[0].1["direct_radiation"], 120.0);
        // Null radiation is SKIPPED, never 0-coerced.
        assert!(!recs[1].1.contains_key("direct_radiation"));
        assert_eq!(recs[1].1["diffuse_radiation"], 60.0);
        assert!(!recs[2].1.contains_key("diffuse_radiation"));
        // Integers deserialize as numbers too.
        assert_eq!(recs[0].1["cloudcover"], 60.0);
    }

    #[test]
    fn hourly_records_cap_at_horizon() {
        let now_hour = 1_700_000_400;
        let recs = hourly_records(&sample(now_hour), now_hour, 2);
        assert_eq!(recs.len(), 2);
    }

    #[test]
    fn line_protocol_matches_the_house_series() {
        let mut fields = Record::new();
        fields.insert("temperature_2m".to_string(), 11.5);
        fields.insert("direct_radiation".to_string(), 120.0);
        let line = to_line(1_700_000_400, &fields).unwrap();
        assert_eq!(
            line,
            "weather_forecast,room=outside,source=openmeteo,type=hour \
             direct_radiation=120,temperature_2m=11.5 1700000400"
        );
        assert!(to_line(1, &Record::new()).is_none());
    }

    #[test]
    fn escape_handles_line_protocol_specials() {
        assert_eq!(escape("a b,c=d"), "a\\ b\\,c\\=d");
    }
}
