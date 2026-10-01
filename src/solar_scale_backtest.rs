//! `backtest-kalman-solar <days> [--from <file>] [--dump <file>] [--sigma-dist <W>] [--json <out>]`
//! — the real-data proof for the Kalman per-zone solar-gain scale (spec item 4 / decision 6):
//! every hour `t` in the scored window, run BOTH arms (`estimator.solar_scale` off/on, otherwise
//! identical config) over the 72 h of history ending at `t`, predict 24 h ahead with REALISED
//! weather (perfect foresight — isolates the estimator from the forecast), and score predicted vs
//! measured air temperature per zone × lead bin, plus the night-bias-after-a-sunny-afternoon
//! metric and how often the constant disturbance flux still hits its clamp.
//!
//! **Bounded reads.** `--from <file>` skips InfluxDB entirely (loads a dumped/hand-written raw
//! series fixture — no `SourceClients`/token needed, just `net`/`ss`/`config` for the zone→room
//! mapping and the replay math); otherwise the `days + 3` day window is read ONCE, in ≤7-day
//! chunks (`CHUNK_DAYS`), one series at a time, paused between chunks
//! (`export_audit::INTER_DAY_PAUSE_S`) — see `chunk_windows`. `--dump <file>` writes the raw
//! series read this way to a JSON fixture (schema `kalman-solar-replay-v1`) so a later run can
//! replay the exact same data with `--from`, without the server.
//!
//! **The exact series this needs** (bucket/measurement/field/tags/aggregateWindow/time-stamping —
//! reproduce these in Flux to hand-build a `--from` fixture): see
//! [`crate::estimate::DriveSeries`]'s doc for `outside` and the `weather:<open-meteo field>` keys
//! (`cloudcover`, `direct_radiation`, `diffuse_radiation`, `shortwave_radiation`); `relay:<zone>` is
//! `bucket=loxone`, `measurement=relay`, `tag1=heating`, the zone's room (the `room` tag of its
//! `zone_mappings` entry in `config.json5`), `1h` mean (duty 0..1),
//! stop-stamped — the same read [`crate::validate::read_heating_kw`] performs.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use nalgebra::DVector;
use serde::{Deserialize, Serialize};
use uom::si::{angle::degree, f64::Angle, heat_flux_density::watt_per_square_meter};

use crate::estimate::{assemble_drive_data, hour_key, DriveData, DriveSeries};
use crate::export_audit::INTER_DAY_PAUSE_S;
use crate::influxdb::TimeSample;
use crate::kalman::KalmanFilter;
use crate::optimize::config::ControlConfig;
use crate::rc_network::RcNetwork;
use crate::source::{RadiationField, SourceClients};
use crate::state_space::StateSpace;
use crate::tools::sun::{tilted_irradiance, SolarInput};

/// Influx-read chunk size (days) — COMMON.md's ≤7-day bound, same constant name/value as
/// `terminal_backtest::CHUNK_DAYS`.
const CHUNK_DAYS: i64 = 7;
/// History a filter run needs before its origin hour (72 h — the standard warm-up window the live
/// estimator and `backtest_passive_detail` both re-run from a flat seed over).
const HISTORY_HOURS: i64 = 72;
/// How far ahead each origin predicts.
const FORECAST_HOURS: i64 = 24;

// --- The dumped/loaded raw-series fixture ---------------------------------------------------------

/// The `--dump`/`--from` fixture: raw per-source series, keyed exactly as the module doc lists.
/// Pure (de)serialization only — no InfluxDB-specific types — so it loads with no token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayDump {
    pub schema: String,
    /// `series[key] = [(rfc3339 timestamp, value), …]`, oldest first within each key.
    pub series: HashMap<String, Vec<(String, f64)>>,
}

const SCHEMA: &str = "kalman-solar-replay-v1";

fn zone_key(zone: &str) -> String {
    format!("zone:{zone}")
}
fn weather_key(field: &str) -> String {
    format!("weather:{field}")
}
fn relay_key(zone: &str) -> String {
    format!("relay:{zone}")
}

fn to_pairs(series: &[TimeSample]) -> Vec<(String, f64)> {
    series
        .iter()
        .map(|s| (s.time.to_rfc3339(), s.value))
        .collect()
}

/// Malformed timestamps are dropped (not fatal) — a hand-written fixture typo should lose one
/// point, not the whole replay.
fn from_pairs(pairs: &[(String, f64)]) -> Vec<TimeSample> {
    pairs
        .iter()
        .filter_map(|(t, v)| {
            DateTime::parse_from_rfc3339(t).ok().map(|dt| TimeSample {
                time: dt.with_timezone(&Utc),
                value: *v,
            })
        })
        .collect()
}

/// Pack the raw series this replay needs into a [`ReplayDump`] (`--dump`'s pure half).
pub(crate) fn dump_from_series(
    drive: &DriveSeries,
    zone_series: &HashMap<String, Vec<TimeSample>>,
    relay_series: &HashMap<String, Vec<TimeSample>>,
) -> ReplayDump {
    let mut series = HashMap::new();
    series.insert("outside".to_string(), to_pairs(&drive.outside));
    series.insert(weather_key("cloudcover"), to_pairs(&drive.cloud));
    series.insert(weather_key("direct_radiation"), to_pairs(&drive.direct));
    series.insert(weather_key("diffuse_radiation"), to_pairs(&drive.diffuse));
    series.insert(
        weather_key("shortwave_radiation"),
        to_pairs(&drive.shortwave),
    );
    for (zone, s) in zone_series {
        series.insert(zone_key(zone), to_pairs(s));
    }
    for (zone, s) in relay_series {
        series.insert(relay_key(zone), to_pairs(s));
    }
    ReplayDump {
        schema: SCHEMA.to_string(),
        series,
    }
}

/// Unpack a [`ReplayDump`] back into [`DriveSeries`] + per-zone measured + per-zone relay-duty
/// series (`--from`'s pure half) — the inverse of [`dump_from_series`].
/// `(outside/weather series, zone -> measured temperature series, zone -> relay-duty series)`.
type DumpSeries = (
    DriveSeries,
    HashMap<String, Vec<TimeSample>>,
    HashMap<String, Vec<TimeSample>>,
);

pub(crate) fn series_from_dump(dump: &ReplayDump) -> DumpSeries {
    let get = |k: &str| {
        dump.series
            .get(k)
            .map(|v| from_pairs(v))
            .unwrap_or_default()
    };
    let drive = DriveSeries {
        outside: get("outside"),
        cloud: get(&weather_key("cloudcover")),
        direct: get(&weather_key("direct_radiation")),
        diffuse: get(&weather_key("diffuse_radiation")),
        shortwave: get(&weather_key("shortwave_radiation")),
    };
    let mut zone_series = HashMap::new();
    let mut relay_series = HashMap::new();
    for key in dump.series.keys() {
        if let Some(zone) = key.strip_prefix("zone:") {
            zone_series.insert(zone.to_string(), get(key));
        } else if let Some(zone) = key.strip_prefix("relay:") {
            relay_series.insert(zone.to_string(), get(key));
        }
    }
    (drive, zone_series, relay_series)
}

fn floor_to_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    t - Duration::minutes(t.minute() as i64)
        - Duration::seconds(t.second() as i64)
        - Duration::nanoseconds(t.nanosecond() as i64)
}

pub(crate) fn chunk_windows(
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut s = start;
    while s < stop {
        let e = (s + Duration::days(CHUNK_DAYS)).min(stop);
        out.push((s, e));
        s = e;
    }
    out
}

/// Read the raw series over `[start, stop)` in ≤7-day chunks, one series at a time, paused
/// between chunks — the IO half `--from` skips entirely.
async fn read_window(
    db: &SourceClients,
    config: &ControlConfig,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Result<ReplayDump> {
    let windows = chunk_windows(start, stop);
    let n = windows.len();
    // Every zone with a temperature mapping (the live estimator's `seed_state` set — includes the
    // unheated attic/garage); relays only for the heated ones.
    let zones: Vec<String> = db
        .mapped_zones()
        .into_iter()
        .filter(|z| z != "outside")
        .collect();

    let mut outside = Vec::new();
    let mut cloud = Vec::new();
    let mut direct = Vec::new();
    let mut diffuse = Vec::new();
    let mut shortwave = Vec::new();
    let mut zone_series: HashMap<String, Vec<TimeSample>> = HashMap::new();
    let mut relay_series: HashMap<String, Vec<TimeSample>> = HashMap::new();

    for (i, (s, e)) in windows.into_iter().enumerate() {
        let (s3, e3) = (s.to_rfc3339(), e.to_rfc3339());
        // Radiation is indexed hour-ENDING by the solar chain and Flux's stop is exclusive, so
        // the LAST chunk reads one hour past the window end (`read_drive_data`'s `rad_stop`);
        // inner chunks abut exactly, so each stays within the 7-day bound.
        let e3_rad = if i + 1 == n {
            (e + Duration::hours(1)).to_rfc3339()
        } else {
            e3.clone()
        };
        outside.extend(
            db.read_zone_temperature_series("outside", &s3, &e3, "1h")
                .await
                .context("reading outside temperature series")?,
        );
        cloud.extend(
            db.weather_cloud_series(&s3, &e3, "1h")
                .await
                .unwrap_or_default(),
        );
        direct.extend(
            db.weather_radiation_series(RadiationField::Direct, &s3, &e3_rad, "1h")
                .await
                .unwrap_or_default(),
        );
        diffuse.extend(
            db.weather_radiation_series(RadiationField::Diffuse, &s3, &e3_rad, "1h")
                .await
                .unwrap_or_default(),
        );
        shortwave.extend(
            db.weather_radiation_series(RadiationField::Shortwave, &s3, &e3_rad, "1h")
                .await
                .unwrap_or_default(),
        );
        for zone in &zones {
            let zs = db
                .read_zone_temperature_series(zone, &s3, &e3, "1h")
                .await
                .unwrap_or_default();
            zone_series.entry(zone.clone()).or_default().extend(zs);
            if let Some(room) = config
                .heating
                .zones
                .contains_key(zone)
                .then(|| db.zone_room(zone))
                .flatten()
            {
                let rs = db
                    .heating_relay_series(room, &s3, &e3, "1h")
                    .await
                    .unwrap_or_default();
                relay_series.entry(zone.clone()).or_default().extend(rs);
            }
        }
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }

    Ok(dump_from_series(
        &DriveSeries {
            outside,
            cloud,
            direct,
            diffuse,
            shortwave,
        },
        &zone_series,
        &relay_series,
    ))
}

// --- Pure replay math ------------------------------------------------------------------------------

/// A half-open lead-hour bin, `lead ∈ 1..=24`.
fn lead_bin(lead_h: i64) -> Option<&'static str> {
    match lead_h {
        1..=3 => Some("0-3h"),
        4..=6 => Some("3-6h"),
        7..=12 => Some("6-12h"),
        13..=24 => Some("12-24h"),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ErrorStats {
    pub n: usize,
    sum_err: f64,
    sum_sq_err: f64,
    sum_abs_err: f64,
}

impl ErrorStats {
    fn add(&mut self, predicted: f64, measured: f64) {
        let e = predicted - measured;
        self.n += 1;
        self.sum_err += e;
        self.sum_sq_err += e * e;
        self.sum_abs_err += e.abs();
    }
    pub fn rmse(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            (self.sum_sq_err / self.n as f64).sqrt()
        }
    }
    pub fn bias(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_err / self.n as f64
        }
    }
    pub fn mae(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_abs_err / self.n as f64
        }
    }
}

/// Horizontal (tilt 0) irradiance under `input` (W/m²) — used as a shared "GHI-like" proxy for
/// both the measured driving input and the clear-sky reference (`input = Cloud{cloud:0.0}`), so
/// `k_D = Σ measured / Σ clear-sky` reuses the SAME physics for both sides of the ratio.
fn ghi_proxy_wm2(latitude: Angle, longitude: Angle, when: DateTime<Utc>, input: SolarInput) -> f64 {
    tilted_irradiance(
        latitude,
        longitude,
        &when,
        input,
        Angle::new::<degree>(0.0),
        Angle::new::<degree>(0.0),
    )
    .get::<watt_per_square_meter>()
}

/// The clear-sky index over local 12-16 on the date `grid_times[idx]` falls in (research.md's
/// night-bias metric): `Σ ghi_proxy(measured) / Σ ghi_proxy(clear-sky)` over the grid hours whose
/// LOCAL hour is in `12..16`. `None` if the day has no such hours in the window (edge of the
/// replay) or the clear-sky sum is ~0 (polar night / bad input — avoid a division blow-up).
pub(crate) fn clear_sky_index(
    latitude: Angle,
    longitude: Angle,
    site: &crate::optimize::config::SiteConfig,
    data: &DriveData,
    date: NaiveDate,
) -> Option<f64> {
    let (mut num, mut den) = (0.0, 0.0);
    for (h, &t) in data.grid_times.iter().enumerate() {
        let local = t.with_timezone(&site.offset_at(t));
        if local.date_naive() != date || !(12..16).contains(&local.hour()) {
            continue;
        }
        let input = data.solar.get(h).copied().unwrap_or(SolarInput::Cloud {
            cloud: data.cloud[h],
        });
        num += ghi_proxy_wm2(latitude, longitude, t, input);
        den += ghi_proxy_wm2(latitude, longitude, t, SolarInput::Cloud { cloud: 0.0 });
    }
    (den > 1e-6).then_some(num / den)
}

/// Is `local_hour` (the hour-ENDING stamp of a measured hourly mean) in the night target window
/// 23:00..=06:00 (wraps past midnight)?
pub(crate) fn is_night_target_hour(local_hour: u32) -> bool {
    !(7..23).contains(&local_hour)
}

/// Reference day/night split for the disturbance clamp-hit metric (a plain daylight bound, not
/// the solar gate — the constant flux can be clamped at any hour, this just buckets the report).
pub(crate) fn is_daytime_hour(local_hour: u32) -> bool {
    (7..20).contains(&local_hour)
}

/// Slice `full`'s per-hour vectors to `[from, to)` (grid-index bounds), rebuilding a standalone
/// [`DriveData`] a filter run can be given its own `x0`/`updates_until_hour` over — the in-memory
/// half of "read once, slice many" (the brief's `--from`/chunked read is the IO half).
pub(crate) fn slice_drive_data(full: &DriveData, from: usize, to: usize) -> DriveData {
    let to = to.min(full.grid_times.len());
    let slice_hkw: HashMap<String, Vec<f64>> = full
        .heating_kw
        .iter()
        .map(|(z, v)| (z.clone(), v[from..to].to_vec()))
        .collect();
    DriveData {
        grid_times: full.grid_times[from..to].to_vec(),
        hours: full.hours[from..to].to_vec(),
        outside_c: full.outside_c[from..to].to_vec(),
        cloud: full.cloud[from..to].to_vec(),
        solar: if full.solar.is_empty() {
            Vec::new()
        } else {
            full.solar[from..to].to_vec()
        },
        ground_c: full.ground_c,
        heating_kw: slice_hkw,
        internal_gain_w: full.internal_gain_w.clone(),
        scheduled_loads: full.scheduled_loads.clone(),
        scheduled_w: full.scheduled_w.clone(),
        sensor_power_w: full.sensor_power_w.clone(),
        local_offset: full.local_offset,
    }
}

/// Seed each measured zone's air state at the latest sample at-or-before `at_hour` (falling back
/// to the earliest available sample, then to a flat 20 °C); other (wall/slab) states at the mean
/// of the seeded zones — the per-origin equivalent of [`crate::estimate::seed_state`], but pure
/// (no IO — the series are already in memory).
pub(crate) fn seed_from_series(
    net: &RcNetwork,
    ss: &StateSpace,
    zone_series: &HashMap<String, Vec<TimeSample>>,
    at_hour: i64,
) -> DVector<f64> {
    let mut per_zone: HashMap<&str, f64> = HashMap::new();
    for (zone, series) in zone_series {
        let mut candidate: Option<&TimeSample> = None;
        for s in series {
            let hk = hour_key(s.time);
            if hk <= at_hour {
                candidate = Some(s);
            } else {
                break;
            }
        }
        let picked = candidate.or_else(|| series.first());
        if let Some(s) = picked {
            per_zone.insert(zone.as_str(), s.value);
        }
    }
    // Summed in zone order so the seed is bit-reproducible run to run.
    let per_zone: BTreeMap<&str, f64> = per_zone.into_iter().collect();
    let base_c = if per_zone.is_empty() {
        20.0
    } else {
        per_zone.values().sum::<f64>() / per_zone.len() as f64
    };
    let mut x = DVector::from_element(ss.n_states(), crate::tools::c_to_k(base_c));
    for (zone, c) in &per_zone {
        if let Some(&node) = net.zone_indices.get(*zone) {
            if let Some(row) = ss.state_index(node) {
                x[row] = crate::tools::c_to_k(*c);
            }
        }
    }
    x
}

// --- The driver: CLI parsing + the per-origin replay loop + the report -----------------------------

#[derive(Debug, Clone, PartialEq)]
struct Args {
    days: i64,
    from: Option<String>,
    dump: Option<String>,
    sigma_dist: Option<f64>,
    json_out: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut days = None;
    let mut from = None;
    let mut dump = None;
    let mut sigma_dist = None;
    let mut json_out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                from = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--from needs a file path"))?,
                );
                i += 2;
            }
            "--dump" => {
                dump = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--dump needs a file path"))?,
                );
                i += 2;
            }
            "--sigma-dist" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--sigma-dist needs a value (W)"))?;
                sigma_dist = Some(v.parse::<f64>().context("parsing --sigma-dist")?);
                i += 2;
            }
            "--json" => {
                json_out = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--json needs a file path"))?,
                );
                i += 2;
            }
            other => {
                if days.is_none() {
                    days = Some(other.parse::<i64>().context("parsing <days>")?);
                    i += 1;
                } else {
                    bail!("backtest-kalman-solar: unrecognized argument {other:?}");
                }
            }
        }
    }
    let days = days.ok_or_else(|| anyhow::anyhow!("backtest-kalman-solar needs <days>"))?;
    anyhow::ensure!(days > 0, "backtest-kalman-solar: <days> must be positive");
    Ok(Args {
        days,
        from,
        dump,
        sigma_dist,
        json_out,
    })
}

/// A sunny/control day classification by the clear-sky index (research.md's night-bias metric).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DayClass {
    Sunny,
    Control,
}

fn classify_day(k_d: f64) -> Option<DayClass> {
    if k_d >= 0.6 {
        Some(DayClass::Sunny)
    } else if k_d < 0.3 {
        Some(DayClass::Control)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct ClampStats {
    n: usize,
    hits: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReplayReport {
    /// `zone -> bin -> (old, new)` RMSE/bias error stats.
    bins_old: BTreeMap<String, BTreeMap<&'static str, ErrorStats>>,
    bins_new: BTreeMap<String, BTreeMap<&'static str, ErrorStats>>,
    /// `zone -> "sunny"/"control" -> (old, new)` night-bias stats.
    night_old: BTreeMap<String, BTreeMap<&'static str, ErrorStats>>,
    night_new: BTreeMap<String, BTreeMap<&'static str, ErrorStats>>,
    /// `zone -> "day"/"night" -> clamp stats`, old vs new arm.
    clamp_old: BTreeMap<String, BTreeMap<&'static str, ClampStats>>,
    clamp_new: BTreeMap<String, BTreeMap<&'static str, ClampStats>>,
    /// `zone -> mean s_z over every origin` (new arm only).
    mean_solar_scale: BTreeMap<String, f64>,
    /// `zone -> "sunny"/"cloudy" -> mean |Δδ| per hour` from ONE filter run over the whole
    /// window (every measurement, no cutoff), each hour bucketed by its day's clear-sky index —
    /// research.md's "δ moving on cloudy days" check. A cloudy mean comparable to the sunny one
    /// means the gate is not doing its job.
    mean_abs_delta_per_hour: BTreeMap<String, BTreeMap<&'static str, f64>>,
    origins_scored: usize,
}

#[allow(clippy::too_many_arguments)]
fn print_report(report: &ReplayReport, days: i64) {
    println!(
        "backtest-kalman-solar: {days} day(s), {} origin(s) scored",
        report.origins_scored
    );
    println!();
    println!("-- RMSE / bias (K) per zone x lead bin, old (flag off) -> new (flag on) --");
    let bins = ["0-3h", "3-6h", "6-12h", "12-24h"];
    let mut zones: Vec<&String> = report
        .bins_old
        .keys()
        .chain(report.bins_new.keys())
        .collect();
    zones.sort();
    zones.dedup();
    for zone in &zones {
        print!("{zone:<16}");
        for bin in bins {
            let old = report
                .bins_old
                .get(*zone)
                .and_then(|m| m.get(bin))
                .cloned()
                .unwrap_or_default();
            let new = report
                .bins_new
                .get(*zone)
                .and_then(|m| m.get(bin))
                .cloned()
                .unwrap_or_default();
            print!(
                "  {bin}: rmse {:.2}->{:.2} ({:+.2}) bias {:+.2}->{:+.2} n={}",
                old.rmse(),
                new.rmse(),
                new.rmse() - old.rmse(),
                old.bias(),
                new.bias(),
                new.n
            );
        }
        println!();
    }
    println!();
    println!("-- Night bias after sunny afternoons (local 23:00-06:00), old -> new --");
    for zone in &zones {
        for class in ["sunny", "control"] {
            let old = report
                .night_old
                .get(*zone)
                .and_then(|m| m.get(class))
                .cloned()
                .unwrap_or_default();
            let new = report
                .night_new
                .get(*zone)
                .and_then(|m| m.get(class))
                .cloned()
                .unwrap_or_default();
            if old.n == 0 && new.n == 0 {
                continue;
            }
            println!(
                "  {zone:<16} {class:<8} bias {:+.2}->{:+.2} mae {:.2}->{:.2} n={}",
                old.bias(),
                new.bias(),
                old.mae(),
                new.mae(),
                new.n.max(old.n)
            );
        }
    }
    println!();
    println!("-- Constant-disturbance-flux clamp hits (old / new) --");
    for zone in &zones {
        for part in ["day", "night"] {
            let old = report
                .clamp_old
                .get(*zone)
                .and_then(|m| m.get(part))
                .cloned()
                .unwrap_or_default();
            let new = report
                .clamp_new
                .get(*zone)
                .and_then(|m| m.get(part))
                .cloned()
                .unwrap_or_default();
            if old.n == 0 && new.n == 0 {
                continue;
            }
            println!(
                "  {zone:<16} {part:<6} old {}/{} new {}/{}",
                old.hits, old.n, new.hits, new.n
            );
        }
    }
    println!();
    println!("-- Mean s_z at origins / mean |Δδ| per hour over the full-window run (new arm) --");
    for zone in &zones {
        let Some(&mean_s) = report.mean_solar_scale.get(*zone) else {
            continue;
        };
        let per_class = |class: &str| {
            report
                .mean_abs_delta_per_hour
                .get(*zone)
                .and_then(|m| m.get(class))
                .copied()
                .unwrap_or(0.0)
        };
        println!(
            "  {zone:<16} mean s_z {mean_s:.3}  |Δδ|/h sunny {:.5} cloudy {:.5}",
            per_class("sunny"),
            per_class("cloudy")
        );
    }
}

/// `db` is `None` only when `--from` was given — no InfluxDB connection is needed in that path.
pub async fn run(
    db: Option<&SourceClients>,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    args: &[String],
) -> Result<()> {
    let parsed = parse_args(args)?;
    let now = Utc::now();
    // Hour-aligned so every chunk boundary is a whole hourly-mean window (a mid-hour boundary
    // would make the first sample of each chunk a partial-hour mean).
    let stop = floor_to_hour(now);
    let start = stop - Duration::days(parsed.days + 3);

    let dump = if let Some(path) = &parsed.from {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading --from {path}"))?;
        serde_json::from_str::<ReplayDump>(&text)
            .with_context(|| format!("parsing --from {path}"))?
    } else {
        let db = db.ok_or_else(|| {
            anyhow::anyhow!(
                "backtest-kalman-solar: no --from file given and no InfluxDB connection available"
            )
        })?;
        read_window(db, config, start, stop).await?
    };
    if let Some(path) = &parsed.dump {
        std::fs::write(path, serde_json::to_string_pretty(&dump)?)
            .with_context(|| format!("writing --dump {path}"))?;
        println!("backtest-kalman-solar: dumped raw series to {path}");
    }

    let (drive_series, zone_series, relay_series) = series_from_dump(&dump);
    anyhow::ensure!(
        !zone_series.is_empty(),
        "no measured zone series available (check --from file contents or InfluxDB zone mappings)"
    );
    let ground_c = config.site.ground_temperature_c;
    let mut full = assemble_drive_data(&drive_series, ground_c, 0.5)?;

    let mut heating_kw: HashMap<String, Vec<f64>> = HashMap::new();
    for (zone, relay) in &relay_series {
        let Some(spec) = config.heating.zones.get(zone) else {
            continue;
        };
        let by_hour = crate::estimate::keep_first_by_hour(relay);
        let powers: Vec<f64> = full
            .hours
            .iter()
            .map(|h| by_hour.get(h).copied().unwrap_or(0.0).clamp(0.0, 1.0) * spec.max_heat_kw)
            .collect();
        heating_kw.insert(zone.clone(), powers);
    }
    full.heating_kw = heating_kw;
    full.internal_gain_w = config.heating.internal_gains();
    full.scheduled_loads = config.scheduled_loads.clone();
    full.scheduled_w = config
        .scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
        .collect();
    full.sensor_power_w = vec![None; full.scheduled_loads.len()];
    full.local_offset = config.site.offset_at(now);

    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);

    // Sorted, like `mapped_zones()` live: the gains apply zone by zone in this order.
    let mut measured_zones: Vec<String> = zone_series.keys().cloned().collect();
    measured_zones.sort();
    let mut cfg_off = config.estimator.clone();
    cfg_off.solar_scale = false;
    let mut cfg_on = config.estimator.clone();
    cfg_on.solar_scale = true;
    if let Some(s) = parsed.sigma_dist {
        cfg_off.sigma_disturbance_w = s;
        cfg_on.sigma_disturbance_w = s;
    }
    let filter_off = KalmanFilter::build(net, ss, &cfg_off, &measured_zones)?;
    let filter_on = KalmanFilter::build(net, ss, &cfg_on, &measured_zones)?;

    let zone_rows: HashMap<String, usize> = measured_zones
        .iter()
        .filter_map(|z| {
            net.zone_indices
                .get(z)
                .and_then(|&n| ss.state_index(n))
                .map(|r| (z.clone(), r))
        })
        .collect();
    let zone_by_hour: HashMap<String, HashMap<i64, f64>> = zone_series
        .iter()
        .map(|(z, s)| (z.clone(), crate::estimate::keep_first_by_hour(s)))
        .collect();

    let n = full.hours.len();
    let history = HISTORY_HOURS as usize;
    let lead = FORECAST_HOURS as usize;
    anyhow::ensure!(
        n > history + lead,
        "not enough history for even one origin ({n} hour(s) read, need > {})",
        history + lead
    );

    let mut report = ReplayReport::default();
    // Site-local calendar day → clear-sky index, with the offset taken per instant so a window
    // across a DST change buckets each hour by its own local time.
    let mut kd_cache: HashMap<NaiveDate, Option<f64>> = HashMap::new();
    let mut kd_of = |date: NaiveDate| {
        *kd_cache
            .entry(date)
            .or_insert_with(|| clear_sky_index(latitude, longitude, &config.site, &full, date))
    };
    let mut mean_s_sum: BTreeMap<String, (f64, usize)> = BTreeMap::new();

    for idx in history..(n - lead) {
        let from = idx - history;
        let to = idx + lead + 1;
        let window = slice_drive_data(&full, from, to);
        let origin_key = full.hours[idx];
        let seed = seed_from_series(net, ss, &zone_series, full.hours[from]);
        let measured_trunc: HashMap<String, Vec<TimeSample>> = zone_series
            .iter()
            .map(|(z, s)| {
                (
                    z.clone(),
                    s.iter()
                        .filter(|x| hour_key(x.time) < origin_key)
                        .cloned()
                        .collect(),
                )
            })
            .collect();

        let est_off = filter_off.filter(
            net,
            ss,
            latitude,
            longitude,
            &seed,
            &window,
            &measured_trunc,
            Some(origin_key),
        );
        let est_on = filter_on.filter(
            net,
            ss,
            latitude,
            longitude,
            &seed,
            &window,
            &measured_trunc,
            Some(origin_key),
        );
        report.origins_scored += 1;

        let origin_offset = config.site.offset_at(full.grid_times[idx]);
        let origin_local = full.grid_times[idx].with_timezone(&origin_offset);
        let origin_date = origin_local.date_naive();
        let origin_k_d = kd_of(origin_date);
        let origin_class_15_18 = (15..=18)
            .contains(&origin_local.hour())
            .then_some(origin_k_d)
            .flatten()
            .and_then(classify_day);

        for (zone, &row) in &zone_rows {
            if let Some(&mean_s) = est_on.solar_scale.get(zone) {
                let e = mean_s_sum.entry(zone.clone()).or_insert((0.0, 0));
                e.0 += mean_s;
                e.1 += 1;
            }

            for h in 1..=lead {
                let Some(bin) = lead_bin(h as i64) else {
                    continue;
                };
                let target_key = full.hours[idx + h];
                let Some(&meas_c) = zone_by_hour.get(zone).and_then(|m| m.get(&target_key)) else {
                    continue;
                };
                let pred_off_c = crate::tools::k_to_c(est_off.trajectory[history + h][row]);
                let pred_on_c = crate::tools::k_to_c(est_on.trajectory[history + h][row]);
                report
                    .bins_old
                    .entry(zone.clone())
                    .or_default()
                    .entry(bin)
                    .or_default()
                    .add(pred_off_c, meas_c);
                report
                    .bins_new
                    .entry(zone.clone())
                    .or_default()
                    .entry(bin)
                    .or_default()
                    .add(pred_on_c, meas_c);

                if let Some(class) = origin_class_15_18 {
                    let target_local = full.grid_times[idx + h]
                        .with_timezone(&config.site.offset_at(full.grid_times[idx + h]));
                    if is_night_target_hour(target_local.hour()) {
                        let key = match class {
                            DayClass::Sunny => "sunny",
                            DayClass::Control => "control",
                        };
                        report
                            .night_old
                            .entry(zone.clone())
                            .or_default()
                            .entry(key)
                            .or_default()
                            .add(pred_off_c, meas_c);
                        report
                            .night_new
                            .entry(zone.clone())
                            .or_default()
                            .entry(key)
                            .or_default()
                            .add(pred_on_c, meas_c);
                    }
                }
            }
        }

        let day_part = if is_daytime_hour(origin_local.hour()) {
            "day"
        } else {
            "night"
        };
        for (zone, &d) in &est_off.disturbance_w {
            let stats = report
                .clamp_old
                .entry(zone.clone())
                .or_default()
                .entry(day_part)
                .or_default();
            stats.n += 1;
            if d.abs() >= config.estimator.max_disturbance_w - 1.0 {
                stats.hits += 1;
            }
        }
        for (zone, &d) in &est_on.disturbance_w {
            let stats = report
                .clamp_new
                .entry(zone.clone())
                .or_default()
                .entry(day_part)
                .or_default();
            stats.n += 1;
            if d.abs() >= config.estimator.max_disturbance_w - 1.0 {
                stats.hits += 1;
            }
        }
    }

    report.mean_solar_scale = mean_s_sum
        .into_iter()
        .map(|(z, (sum, n))| (z, if n == 0 { 1.0 } else { sum / n as f64 }))
        .collect();
    // One run over the whole window with every measurement: how much δ moves per hour on sunny
    // vs cloudy days (each hour bucketed by its own day's clear-sky index).
    let seed_full = seed_from_series(net, ss, &zone_series, full.hours[0]);
    let est_full = filter_on.filter(
        net,
        ss,
        latitude,
        longitude,
        &seed_full,
        &full,
        &zone_series,
        None,
    );
    let mut delta_acc: BTreeMap<(String, &'static str), (f64, usize)> = BTreeMap::new();
    for h in 1..est_full.solar_scale_trace.len() {
        let date = full.grid_times[h]
            .with_timezone(&config.site.offset_at(full.grid_times[h]))
            .date_naive();
        let Some(class) = kd_of(date).and_then(classify_day) else {
            continue;
        };
        let key = match class {
            DayClass::Sunny => "sunny",
            DayClass::Control => "cloudy",
        };
        for (i, zone) in est_full.solar_scale_zones.iter().enumerate() {
            let step =
                (est_full.solar_scale_trace[h][i] - est_full.solar_scale_trace[h - 1][i]).abs();
            let e = delta_acc.entry((zone.clone(), key)).or_insert((0.0, 0));
            e.0 += step;
            e.1 += 1;
        }
    }
    for ((zone, class), (sum, n)) in delta_acc {
        report
            .mean_abs_delta_per_hour
            .entry(zone)
            .or_default()
            .insert(class, if n == 0 { 0.0 } else { sum / n as f64 });
    }

    print_report(&report, parsed.days);
    if let Some(path) = &parsed.json_out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing --json {path}"))?;
        println!("backtest-kalman-solar: wrote {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn lead_bin_covers_1_through_24_half_open() {
        assert_eq!(lead_bin(0), None);
        assert_eq!(lead_bin(1), Some("0-3h"));
        assert_eq!(lead_bin(3), Some("0-3h"));
        assert_eq!(lead_bin(4), Some("3-6h"));
        assert_eq!(lead_bin(6), Some("3-6h"));
        assert_eq!(lead_bin(7), Some("6-12h"));
        assert_eq!(lead_bin(12), Some("6-12h"));
        assert_eq!(lead_bin(13), Some("12-24h"));
        assert_eq!(lead_bin(24), Some("12-24h"));
        assert_eq!(lead_bin(25), None);
    }

    #[test]
    fn error_stats_rmse_bias_mae() {
        let mut s = ErrorStats::default();
        s.add(21.0, 20.0); // +1
        s.add(18.0, 20.0); // -2
        assert_eq!(s.n, 2);
        assert!((s.bias() - (-0.5)).abs() < 1e-9);
        assert!((s.mae() - 1.5).abs() < 1e-9);
        assert!((s.rmse() - (2.5_f64).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn night_target_hour_wraps_midnight() {
        assert!(is_night_target_hour(23));
        assert!(is_night_target_hour(0));
        assert!(is_night_target_hour(6));
        assert!(!is_night_target_hour(7));
        assert!(!is_night_target_hour(12));
        assert!(!is_night_target_hour(22));
    }

    #[test]
    fn daytime_hour_bound() {
        assert!(!is_daytime_hour(6));
        assert!(is_daytime_hour(7));
        assert!(is_daytime_hour(19));
        assert!(!is_daytime_hour(20));
    }

    #[test]
    fn dump_round_trips_through_rfc3339() {
        let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let drive = DriveSeries {
            outside: vec![TimeSample {
                time: t0,
                value: 15.0,
            }],
            cloud: vec![TimeSample {
                time: t0,
                value: 10.0,
            }],
            direct: vec![TimeSample {
                time: t0,
                value: 200.0,
            }],
            diffuse: vec![TimeSample {
                time: t0,
                value: 50.0,
            }],
            shortwave: Vec::new(),
        };
        let zones = HashMap::from([(
            "livingroom".to_string(),
            vec![TimeSample {
                time: t0,
                value: 21.5,
            }],
        )]);
        let relays = HashMap::from([(
            "livingroom".to_string(),
            vec![TimeSample {
                time: t0,
                value: 0.4,
            }],
        )]);
        let dump = dump_from_series(&drive, &zones, &relays);
        assert_eq!(dump.schema, "kalman-solar-replay-v1");

        // Round-trip through JSON — exercises `--from`'s actual parse path.
        let json = serde_json::to_string(&dump).unwrap();
        let back: ReplayDump = serde_json::from_str(&json).unwrap();
        let (d2, z2, r2) = series_from_dump(&back);
        assert_eq!(d2.outside.len(), 1);
        assert!((d2.outside[0].value - 15.0).abs() < 1e-9);
        assert_eq!(z2["livingroom"].len(), 1);
        assert!((z2["livingroom"][0].value - 21.5).abs() < 1e-9);
        assert_eq!(r2["livingroom"].len(), 1);
        assert!((r2["livingroom"][0].value - 0.4).abs() < 1e-9);
    }

    #[test]
    fn seed_from_series_picks_the_latest_sample_at_or_before() {
        let model = crate::model::Model::from_json(
            r#"{
                materials: { m: { thermal_conductivity: 1, specific_heat_capacity: 1000, density: 1000 } },
                boundary_types: { wall: { layers: [ { material: "m", thickness: 0.2 } ] } },
                zones: { room: { volume: 50 } },
                boundaries: [ { boundary_type: "wall", zones: ["room", "outside"], area: 20 } ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let series = vec![
            TimeSample {
                time: t0,
                value: 18.0,
            },
            TimeSample {
                time: t0 + Duration::hours(2),
                value: 21.0,
            },
            TimeSample {
                time: t0 + Duration::hours(5),
                value: 23.0,
            },
        ];
        let zones = HashMap::from([("room".to_string(), series)]);
        let row = ss.state_index(net.zone_indices["room"]).unwrap();

        let x = seed_from_series(&net, &ss, &zones, hour_key(t0 + Duration::hours(3)));
        assert!((crate::tools::k_to_c(x[row]) - 21.0).abs() < 1e-9);

        // Before the first sample: falls back to the earliest available.
        let x_early = seed_from_series(&net, &ss, &zones, hour_key(t0 - Duration::hours(10)));
        assert!((crate::tools::k_to_c(x_early[row]) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn slice_drive_data_keeps_per_zone_heating_aligned() {
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let full = DriveData {
            grid_times: (0..10).map(|h| t0 + Duration::hours(h)).collect(),
            hours: (0..10)
                .map(|h| (t0 + Duration::hours(h)).timestamp().div_euclid(3600))
                .collect(),
            outside_c: (0..10).map(|h| h as f64).collect(),
            cloud: vec![0.5; 10],
            solar: Vec::new(),
            ground_c: 10.0,
            heating_kw: HashMap::from([(
                "room".to_string(),
                (0..10).map(|h| h as f64 * 0.1).collect(),
            )]),
            internal_gain_w: HashMap::new(),
            scheduled_loads: Vec::new(),
            scheduled_w: Vec::new(),
            sensor_power_w: Vec::new(),
            local_offset: chrono::FixedOffset::east_opt(0).unwrap(),
        };
        let sliced = slice_drive_data(&full, 3, 7);
        assert_eq!(sliced.grid_times.len(), 4);
        assert_eq!(sliced.outside_c, vec![3.0, 4.0, 5.0, 6.0]);
        for (got, want) in sliced.heating_kw["room"].iter().zip([0.3, 0.4, 0.5, 0.6]) {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }
    }
}
