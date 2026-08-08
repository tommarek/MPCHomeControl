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

/// Air-quality fields (a second open-meteo endpoint), merged into the same hourly records like
/// the loxone scraper does — so retiring it loses nothing (Grafana pm/UV charts keep working).
const AIR_FIELDS: &str = "pm10,pm2_5,ozone,aerosol_optical_depth,uv_index";

/// Daily-summary fields (`type=day` record, one per day) — the loxone scraper's "today" record.
const DAILY_FIELDS: &str = "sunrise,sunset,precipitation_sum,rain_sum,precipitation_hours,\
    shortwave_radiation_sum";

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

/// HTTP agent with explicit timeouts — ureq 2 sets NONE by default, and a half-open socket in
/// this long-running `loop { scrape; write; sleep }` would otherwise wedge the loop forever
/// (the process stays alive, so Docker's restart policy never fires and the data silently goes
/// stale). Same pattern as the brain's own HTTP reads.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build()
}

/// Merge the air-quality endpoint's hourly fields into the forecast's `hourly` block, **realigned
/// by timestamp**.
///
/// The two endpoints are separate requests and their `time` arrays need not agree (different
/// horizon or start hour), while `hourly_records` reads every field positionally against the
/// FORECAST time array — so merging the raw arrays would silently attribute pm/UV values to the
/// wrong hours. Each air field is rebuilt onto the forecast's grid, with `null` (skipped downstream
/// by the honest-nulls rule) wherever that hour has no air sample. A missing/!array `time` on
/// either side skips the merge entirely rather than guessing.
fn merge_air_quality(hourly: &mut serde_json::Value, air: &serde_json::Value) {
    let (Some(fc_times), Some(air_obj)) = (
        hourly.get("time").and_then(|t| t.as_array()).cloned(),
        air.as_object(),
    ) else {
        return;
    };
    let Some(air_times) = air_obj.get("time").and_then(|t| t.as_array()) else {
        eprintln!("[openmeteo] air-quality response has no `time` array; not merging");
        return;
    };
    // air timestamp → its index, so each forecast hour can pull its own value.
    let index: BTreeMap<i64, usize> = air_times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.as_i64().map(|ts| (ts, i)))
        .collect();
    let Some(h) = hourly.as_object_mut() else {
        return;
    };
    for (name, values) in air_obj {
        if name == "time" || !is_known_field(name) {
            continue;
        }
        let Some(arr) = values.as_array() else {
            continue;
        };
        let realigned: Vec<serde_json::Value> = fc_times
            .iter()
            .map(|t| {
                t.as_i64()
                    .and_then(|ts| index.get(&ts))
                    .and_then(|&i| arr.get(i))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();
        h.insert(name.clone(), serde_json::Value::Array(realigned));
    }
}

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
                if !is_known_field(name) {
                    continue; // never write a field we didn't request (see `is_known_field`)
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
        // Line protocol is NEWLINE-delimited: an unescaped \n or \r in a field key would end the
        // line and let the rest be parsed as a whole extra point (arbitrary measurement/tags).
        // Escaping is defence in depth behind `is_known_field`, not a substitute for it.
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Is `name` a field we actually asked open-meteo for?
///
/// The response's keys are remote input: `hourly_records` and `merge_air_quality` used to copy
/// EVERY key straight into the line protocol, so a changed/compromised/proxied API could introduce
/// arbitrary field names into the house's own `weather_forecast` series. Only the fields this
/// scraper requested are written; anything else is ignored (and logged once per scrape by the
/// caller). `time` is structural and handled separately.
fn is_known_field(name: &str) -> bool {
    fn split(list: &str) -> impl Iterator<Item = &str> {
        list.split(',').map(str::trim).filter(|s| !s.is_empty())
    }
    split(HOURLY_FIELDS)
        .chain(split(AIR_FIELDS))
        .chain(split(DAILY_FIELDS))
        .any(|f| f == name)
}

/// One line-protocol line for a record, second-precision timestamp. The tag set matches the
/// house's existing writer exactly, so both merge into the same series. `kind` is the `type`
/// tag: `hour` for the hourly records, `day` for the daily summary.
fn to_line(ts: i64, fields: &Record, kind: &str) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    let fieldset = fields
        .iter()
        .map(|(k, v)| format!("{}={}", escape(k), v))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!(
        "weather_forecast,room=outside,source=openmeteo,type={kind} {fieldset} {ts}"
    ))
}

/// Fetch the forecast (+ air quality, best-effort) and return the batched line-protocol body.
fn scrape(config: &Config) -> Result<String> {
    let response: serde_json::Value = http_agent()
        .get(&config.openmeteo_url)
        .query("latitude", &config.site.latitude.to_string())
        .query("longitude", &config.site.longitude.to_string())
        .query("hourly", HOURLY_FIELDS)
        .query("daily", DAILY_FIELDS)
        .query("models", "best_match")
        .query("windspeed_unit", "ms")
        .query("timeformat", "unixtime")
        .query("timezone", "GMT")
        .call()
        .context("open-meteo request failed")?
        .into_json()
        .context("open-meteo response is not JSON")?;
    let mut hourly = response
        .get("hourly")
        .context("open-meteo response has no `hourly` block")?
        .clone();
    // Air quality (separate endpoint), merged into the same hourly records like the loxone
    // scraper — best-effort: a failure loses only the pm/UV fields, never the forecast.
    match air_quality(config) {
        Ok(air) => merge_air_quality(&mut hourly, &air),
        Err(e) => eprintln!("[openmeteo] air-quality fetch failed ({e:#}); continuing without"),
    }
    let now = chrono::Utc::now().timestamp();
    let now_hour = now - now.rem_euclid(3600);
    let records = hourly_records(&hourly, now_hour, config.horizon_hours);
    ensure!(!records.is_empty(), "open-meteo returned no future hours");
    let mut lines: Vec<String> = records
        .iter()
        .filter_map(|(ts, fields)| to_line(*ts, fields, "hour"))
        .collect();
    // Today's daily summary (`type=day`) — the loxone scraper's "today" record. Sunrise/sunset
    // come back as unixtime numbers, so they pass the numeric filter unchanged.
    if let Some(daily) = response.get("daily") {
        // Reuse the hourly extractor: daily records are day-stamped, today = the first ≥ today 00Z.
        let day_start = now - now.rem_euclid(86_400);
        if let Some((ts, fields)) = hourly_records(daily, day_start, 1).first() {
            lines.extend(to_line(*ts, fields, "day"));
        }
    }
    Ok(lines.join("\n"))
}

/// The open-meteo air-quality hourly block (separate host; the loxone scraper merges the same).
fn air_quality(config: &Config) -> Result<serde_json::Value> {
    let response: serde_json::Value = http_agent()
        .get("https://air-quality-api.open-meteo.com/v1/air-quality")
        .query("latitude", &config.site.latitude.to_string())
        .query("longitude", &config.site.longitude.to_string())
        .query("hourly", AIR_FIELDS)
        .query("timeformat", "unixtime")
        .query("timezone", "GMT")
        .call()
        .context("air-quality request failed")?
        .into_json()
        .context("air-quality response is not JSON")?;
    response
        .get("hourly")
        .cloned()
        .context("air-quality response has no `hourly` block")
}

/// POST the batch to the InfluxDB v2 write API.
fn write_influx(config: &Config, token: &str, body: &str) -> Result<()> {
    let url = format!(
        "{}/api/v2/write?org={}&bucket={}&precision=s",
        config.influx.url.trim_end_matches('/'),
        urlencode(&config.influx.org),
        urlencode(&config.influx.bucket)
    );
    http_agent()
        .post(&url)
        .set("Authorization", &format!("Token {token}"))
        .set("Content-Type", "text/plain; charset=utf-8")
        .send_string(body)
        .context("influx write failed")?;
    Ok(())
}

/// Fail at STARTUP on a config that cannot work, rather than once per cycle for ever. The brain
/// bounds the very same coordinates (`ControlConfig::validate_site`) because they feed the solar
/// model, and this scraper writes the exact `weather_forecast` series the brain plans from — so an
/// out-of-range or NaN coordinate here means a request that fails every cycle, whose only symptom
/// is a log line and a silently frozen forecast series. Mirrors the solcast scraper's `validate`.
fn validate(config: &Config) -> Result<()> {
    let (lat, lon) = (config.site.latitude, config.site.longitude);
    ensure!(
        lat.is_finite() && (-90.0..=90.0).contains(&lat),
        "site.latitude must be finite and within ±90 (got {lat})"
    );
    ensure!(
        lon.is_finite() && (-180.0..=180.0).contains(&lon),
        "site.longitude must be finite and within ±180 (got {lon})"
    );
    ensure!(
        config.horizon_hours >= 1,
        "horizon_hours must be ≥ 1 (got {})",
        config.horizon_hours
    );
    ensure!(
        config.interval_minutes >= 1,
        "interval_minutes must be ≥ 1 (got {})",
        config.interval_minutes
    );
    ensure!(
        !config.influx.org.is_empty(),
        "influx.org must not be empty"
    );
    ensure!(
        !config.influx.bucket.is_empty(),
        "influx.bucket must not be empty"
    );
    ensure!(
        config.influx.url.starts_with("http://") || config.influx.url.starts_with("https://"),
        "influx.url must be an http(s) URL (got {:?})",
        config.influx.url
    );
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
    validate(&config)?;
    // Print the site on every start: this scraper writes the SAME series the brain reads, so a
    // coordinate mismatch with `config.json5` plans the house against another town's weather and is
    // otherwise completely silent — the numbers look perfectly plausible.
    eprintln!(
        "[openmeteo] site {:.6}, {:.6} → bucket {:?} (must match config.json5's `site`)",
        config.site.latitude, config.site.longitude, config.influx.bucket
    );
    let token = if dry_run {
        String::new()
    } else {
        std::env::var("INFLUXDB_TOKEN").context("INFLUXDB_TOKEN not set")?
    };

    loop {
        // Tracked so `--once` (the cut-over smoke test — "verify one write lands") can exit
        // NONZERO on failure; logging the error and exiting 0 made a failed check indistinguishable
        // from a passing one to any script or operator running it.
        let mut cycle: anyhow::Result<()> = Ok(());
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
                        Err(e) => {
                            eprintln!("[openmeteo] {e:#}");
                            cycle = Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[openmeteo] {e:#}");
                cycle = Err(e);
            }
        }
        if once {
            return cycle;
        }
        std::thread::sleep(Duration::from_secs(config.interval_minutes.max(1) * 60));
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept). `org`/`bucket` come from
/// local config, so this is robustness not injection defence — but a name containing a space, `&`
/// or `+` otherwise produces a malformed write URL that 400s every cycle (or, with `&`, silently
/// targets a different bucket) while the loop just logs and sleeps. Mirrors the mqtt-bridge writer,
/// which already encodes both.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The air-quality endpoint is a separate request whose `time` grid can be offset from the
    /// forecast's. Values must land on their OWN hour (realigned), not at their raw array position,
    /// and forecast hours the air series doesn't cover must read `null` (skipped downstream).
    #[test]
    fn air_quality_merges_by_timestamp_not_position() {
        let t0 = 1_700_000_000_i64 - 1_700_000_000_i64.rem_euclid(3600);
        let mut hourly = serde_json::json!({
            "time": [t0, t0 + 3600, t0 + 7200],
            "temperature_2m": [10.0, 11.0, 12.0],
        });
        // Air starts one hour LATER and runs past the forecast: pm10 5.0 belongs to t0+3600.
        let air = serde_json::json!({
            "time": [t0 + 3600, t0 + 7200, t0 + 10800],
            "pm10": [5.0, 6.0, 7.0],
        });
        merge_air_quality(&mut hourly, &air);
        assert_eq!(
            hourly["pm10"],
            serde_json::json!([serde_json::Value::Null, 5.0, 6.0]),
            "positional merge would have written [5.0, 6.0, 7.0]"
        );
        // A missing `time` on the air side leaves the forecast untouched.
        let mut untouched = serde_json::json!({"time": [t0], "temperature_2m": [1.0]});
        merge_air_quality(&mut untouched, &serde_json::json!({"pm10": [9.0]}));
        assert!(untouched.get("pm10").is_none());
    }

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
        let line = to_line(1_700_000_400, &fields, "hour").unwrap();
        assert_eq!(
            line,
            "weather_forecast,room=outside,source=openmeteo,type=hour \
             direct_radiation=120,temperature_2m=11.5 1700000400"
        );
        let day = to_line(1_700_000_400, &fields, "day").unwrap();
        assert!(day.starts_with("weather_forecast,room=outside,source=openmeteo,type=day "));
        assert!(to_line(1, &Record::new(), "hour").is_none());
    }

    #[test]
    fn escape_handles_line_protocol_specials() {
        assert_eq!(escape("a b,c=d"), "a\\ b\\,c\\=d");
        // Line protocol is newline-delimited: an unescaped \n would terminate the point and let
        // the remainder be parsed as an entirely separate (attacker-chosen) measurement.
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\rb"), "a\\rb");
    }

    /// The response's keys are remote input. Only fields this scraper actually requested may reach
    /// the line protocol — otherwise a changed/compromised/proxied API could write arbitrary field
    /// names (or, before the escaping fix, whole extra points) into the house's own series.
    #[test]
    fn unrequested_response_fields_are_never_written() {
        let t0 = 1_700_000_000_i64 - 1_700_000_000_i64.rem_euclid(3600);
        let hourly = serde_json::json!({
            "time": [t0],
            "temperature_2m": [11.0],           // requested → kept
            "evil\nweather_forecast,room=x v": [1.0], // not requested → dropped
            "totally_unrequested": [42.0],      // not requested → dropped
        });
        let recs = hourly_records(&hourly, t0, 10);
        assert_eq!(recs.len(), 1);
        let fields = &recs[0].1;
        assert_eq!(fields.get("temperature_2m"), Some(&11.0));
        assert_eq!(fields.len(), 1, "only requested fields survive: {fields:?}");

        // Same guard on the air-quality merge path.
        let mut h = serde_json::json!({ "time": [t0], "temperature_2m": [11.0] });
        merge_air_quality(
            &mut h,
            &serde_json::json!({ "time": [t0], "pm10": [5.0], "not_requested": [9.0] }),
        );
        assert!(h.get("pm10").is_some());
        assert!(h.get("not_requested").is_none());
    }
    /// The scraper writes the series the brain plans from, so an unusable site must fail at startup
    /// rather than once per cycle for ever.
    #[test]
    fn validate_rejects_an_unusable_site() {
        let base = Config {
            influx: InfluxConfig {
                url: "http://influxdb:8086".to_string(),
                org: "loxone".to_string(),
                bucket: "weather_forecast".to_string(),
            },
            site: SiteConfig {
                latitude: 49.49,
                longitude: 17.43,
            },
            interval_minutes: 30,
            horizon_hours: 72,
            openmeteo_url: default_openmeteo_url(),
        };
        assert!(validate(&base).is_ok(), "the shipped shape must be valid");
        let bad = |f: fn(&mut Config)| {
            let mut c = Config {
                influx: InfluxConfig {
                    url: base.influx.url.clone(),
                    org: base.influx.org.clone(),
                    bucket: base.influx.bucket.clone(),
                },
                site: SiteConfig {
                    latitude: base.site.latitude,
                    longitude: base.site.longitude,
                },
                interval_minutes: base.interval_minutes,
                horizon_hours: base.horizon_hours,
                openmeteo_url: base.openmeteo_url.clone(),
            };
            f(&mut c);
            assert!(validate(&c).is_err());
        };
        bad(|c| c.site.latitude = f64::NAN);
        bad(|c| c.site.latitude = 91.0);
        bad(|c| c.site.longitude = -181.0);
        bad(|c| c.horizon_hours = 0);
        bad(|c| c.interval_minutes = 0);
        bad(|c| c.influx.org.clear());
        bad(|c| c.influx.bucket.clear());
    }
}
