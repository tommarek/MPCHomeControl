//! `GET /api/rooms` — the per-room card data behind the dashboard's floor plan, merged from sources
//! the brain already holds: the building model, the comfort targets, the Kalman state estimate, the
//! loop's latest published plan, the decision ledger and one bounded read of the latest sensor
//! values. Rooms are the model's interior zones; the floor-plan facts (room names, floors, areas,
//! polygons) belong to the dashboard, which merges them from its own `floorplan.json`.
//!
//! API CONTRACT: the Home Assistant dashboard in ~/git/selfhosted (homeassistant/config/mpc-dashboard/,
//! served by the mpc_brain integration) consumes this API. Any change to an endpoint's path, query,
//! or JSON shape MUST be mirrored there in the same piece of work; bump `web::API_VERSION` on
//! breaking changes.
//!
//! This module is **pure and IO-free** — [`merge`] takes plain-data inputs and returns the report;
//! `web::get_rooms` is the thin handler that gathers them. Nothing here plans, solves or writes:
//! the endpoint sits entirely off the planning path.
//!
//! Honesty rule: every field of every room is always present, and anything the brain does not know
//! is `null` — never a default, never an invented value.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Utc};
use serde::Serialize;

use crate::influxdb::SignalSpec;

/// How far ahead the per-room plan slice looks by default, and the window `plan_heat_kwh_12h` sums.
pub const PLAN_HORIZON_HOURS: i64 = 12;
/// The longest plan slice a caller may ask for (`/api/rooms?hours=`); the plan itself is shorter.
pub const PLAN_HORIZON_MAX_HOURS: i64 = 72;
/// At or below this planned power (kW) a heater counts as off: the dashboard's "ignore solver dust"
/// cutoff (`isRelayOn` in the Home Assistant dashboard's app.js), so the room cards and the other
/// views agree.
const HEATER_ON_KW: f64 = 0.05;

// --- Sensor signals: locator derivation + latest-value pick ---------------------------------

/// The three per-room signals read from the sensor store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Temperature,
    Humidity,
    Presence,
}

/// Where one zone's three signals live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneSignals {
    pub temperature: Option<SignalSpec>,
    pub humidity: Option<SignalSpec>,
    pub presence: Option<SignalSpec>,
}

impl ZoneSignals {
    pub fn get(&self, signal: Signal) -> Option<&SignalSpec> {
        match signal {
            Signal::Temperature => self.temperature.as_ref(),
            Signal::Humidity => self.humidity.as_ref(),
            Signal::Presence => self.presence.as_ref(),
        }
    }
}

/// Resolve a zone's signal locators from the three `zone_mappings.<zone>` entries (each already an
/// explicit `SignalSpec` when configured). A configured `humidity`/`presence` entry wins; otherwise
/// it is derived from the zone's `temperature` mapping — same bucket and `room` tag, measurement
/// `humidity` / `presence`, field `humidity_<room>` / `motion_<room>` (the loxone logging scheme).
/// A zone without a `room` tag on its temperature mapping derives nothing.
pub fn resolve_signals(
    temperature: Option<SignalSpec>,
    humidity: Option<SignalSpec>,
    presence: Option<SignalSpec>,
) -> ZoneSignals {
    let derive = |measurement: &str, field_prefix: &str| -> Option<SignalSpec> {
        let t = temperature.as_ref()?;
        let room = t.tags.get("room")?;
        Some(SignalSpec {
            bucket: t.bucket.clone(),
            measurement: measurement.to_string(),
            tags: HashMap::from([("room".to_string(), room.clone())]),
            field: format!("{field_prefix}_{room}"),
        })
    };
    ZoneSignals {
        humidity: humidity.or_else(|| derive("humidity", "humidity")),
        presence: presence.or_else(|| derive("presence", "motion")),
        temperature,
    }
}

/// The reads needed to fetch every signal: per bucket, the measurements and fields to ask for.
/// Sorted and de-duplicated, so the query is deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorQuery {
    pub bucket: String,
    pub measurements: Vec<String>,
    pub fields: Vec<String>,
}

pub fn sensor_queries<'a>(zones: impl IntoIterator<Item = &'a ZoneSignals>) -> Vec<SensorQuery> {
    let mut by_bucket: std::collections::BTreeMap<&str, (HashSet<&str>, HashSet<&str>)> =
        Default::default();
    for z in zones {
        for spec in [&z.temperature, &z.humidity, &z.presence]
            .into_iter()
            .flatten()
        {
            let (m, f) = by_bucket.entry(spec.bucket.as_str()).or_default();
            m.insert(spec.measurement.as_str());
            f.insert(spec.field.as_str());
        }
    }
    let sorted = |s: HashSet<&str>| {
        let mut v: Vec<String> = s.into_iter().map(str::to_string).collect();
        v.sort();
        v
    };
    by_bucket
        .into_iter()
        .map(|(bucket, (m, f))| SensorQuery {
            bucket: bucket.to_string(),
            measurements: sorted(m),
            fields: sorted(f),
        })
        .collect()
}

/// One latest sensor value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub value: f64,
    pub at: DateTime<Utc>,
}

/// The newest finite value among raw `last()` rows that match `spec` (measurement, field and every
/// configured tag). Flux `last()` emits one row per series, so several can match; the newest by
/// PARSED `_time` wins (RFC3339 strings with differing fractional widths don't sort).
pub fn pick_latest(rows: &[HashMap<String, String>], spec: &SignalSpec) -> Option<Reading> {
    rows.iter()
        .filter(|r| {
            r.get("_measurement") == Some(&spec.measurement)
                && r.get("_field") == Some(&spec.field)
                && spec.tags.iter().all(|(k, v)| r.get(k) == Some(v))
        })
        .filter_map(|r| {
            let at = DateTime::parse_from_rfc3339(r.get("_time")?)
                .ok()?
                .with_timezone(&Utc);
            let value: f64 = r.get("_value")?.parse().ok()?;
            value.is_finite().then_some(Reading { value, at })
        })
        .max_by_key(|r| r.at)
}

/// The latest readings per zone, plus how the read went.
#[derive(Debug, Clone, Default)]
pub struct SensorReadout {
    /// When the read completed; `None` if it failed or was not attempted.
    pub at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub zones: HashMap<String, ZoneReadings>,
}

#[derive(Debug, Clone, Default)]
pub struct ZoneReadings {
    pub temperature: Option<Reading>,
    pub humidity: Option<Reading>,
    pub presence: Option<Reading>,
    /// The zone's temperature signal lives in a bucket whose read failed.
    pub temperature_read_failed: bool,
}

/// Distribute raw rows, keyed by the bucket they were read from, over the zones' locators. The rows
/// carry no bucket column, so a locator only ever sees its own bucket's rows. `failed_buckets` are
/// the buckets whose read failed.
pub fn readings_from_rows(
    rows: &HashMap<String, Vec<HashMap<String, String>>>,
    failed_buckets: &HashSet<String>,
    signals: &HashMap<String, ZoneSignals>,
) -> HashMap<String, ZoneReadings> {
    signals
        .iter()
        .map(|(zone, s)| {
            let pick = |sig| {
                s.get(sig)
                    .and_then(|spec| pick_latest(rows.get(&spec.bucket)?, spec))
            };
            (
                zone.clone(),
                ZoneReadings {
                    temperature: pick(Signal::Temperature),
                    humidity: pick(Signal::Humidity),
                    presence: pick(Signal::Presence),
                    temperature_read_failed: s
                        .temperature
                        .as_ref()
                        .is_some_and(|spec| failed_buckets.contains(&spec.bucket)),
                },
            )
        })
        .collect()
}

// --- Merge inputs ----------------------------------------------------------------------------

/// A model zone's static facts.
#[derive(Debug, Clone)]
pub struct ModelZone {
    pub name: String,
    pub volume_m3: Option<f64>,
    /// Air changes per hour to outside; `0` = not modelled.
    pub ach: f64,
}

/// A heated zone's equipment (`heating.zones.<zone>`) and its pull weight.
#[derive(Debug, Clone)]
pub struct HeaterFacts {
    pub max_heat_kw: f64,
    pub internal_gain_w: f64,
    /// `comfort.pull_eur_per_kh × the room's priority` (EUR per K·h below target; `0` ⇒ no pull).
    pub pull_eur_per_kh: f64,
}

/// One comfort band in force now: `{target, low, high}` (°C), as `/api/zones` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BandNow {
    pub target: f64,
    pub low: f64,
    pub high: f64,
}

/// A cool band in force now, plus whether an HVAC unit serves the room (`active`); an inactive cool
/// band is prepared but inert — it adds nothing to the plan.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CoolNow {
    pub target: f64,
    pub low: f64,
    pub high: f64,
    pub active: bool,
}

/// A room's comfort targets, exactly as `/api/zones` reports them (a missing/`null` heat band ⇒
/// no entry).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comfort {
    pub heat: BandNow,
    pub cool: Option<CoolNow>,
    pub priority: f64,
    /// The room's heating / cooling mode, when the brain has decided one.
    pub mode: Option<ModeNow>,
}

/// A room's heating / cooling mode and the free-drift tendency behind it (see
/// `optimize::room_mode`), as `/api/zones` reports them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModeNow {
    pub mode: crate::optimize::room_mode::RoomMode,
    pub since: DateTime<Utc>,
    /// K per day, + = the room gains heat with every actuator off.
    pub drift_k_per_day: f64,
}

impl Comfort {
    /// Read one entry of `/api/zones`' `data` array.
    pub fn from_zone_json(v: &serde_json::Value) -> Option<Comfort> {
        let num = |o: &serde_json::Value, k: &str| o.get(k).and_then(serde_json::Value::as_f64);
        let heat = v.get("heat")?;
        let heat = BandNow {
            target: num(heat, "target")?,
            low: num(heat, "low")?,
            high: num(heat, "high")?,
        };
        let cool = v.get("cool").filter(|c| c.is_object()).and_then(|c| {
            Some(CoolNow {
                target: num(c, "target")?,
                low: num(c, "low")?,
                high: num(c, "high")?,
                active: c.get("active")?.as_bool()?,
            })
        });
        let mode = (|| {
            Some(ModeNow {
                mode: match v.get("mode")?.as_str()? {
                    "heating" => crate::optimize::room_mode::RoomMode::Heating,
                    "cooling" => crate::optimize::room_mode::RoomMode::Cooling,
                    _ => return None,
                },
                since: DateTime::parse_from_rfc3339(v.get("mode_since")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc),
                drift_k_per_day: num(v, "drift_k_per_day")?,
            })
        })();
        Some(Comfort {
            heat,
            cool,
            priority: num(v, "priority")?,
            mode,
        })
    }
}

/// `/api/state`'s `data` payload, reduced to the maps the merge reads.
#[derive(Debug, Clone, Default)]
pub struct StateInput {
    pub estimate_c: HashMap<String, f64>,
    pub disturbance_w: HashMap<String, f64>,
    pub solar_scale: HashMap<String, f64>,
}

impl StateInput {
    pub fn from_json(v: &serde_json::Value) -> Self {
        let map = |key: &str| -> HashMap<String, f64> {
            v.get(key)
                .and_then(serde_json::Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, x)| Some((k.clone(), x.as_f64()?)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let estimate_c = v
            .get("zones")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|z| {
                        Some((
                            z.get("zone")?.as_str()?.to_string(),
                            z.get("temp_c")?.as_f64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            estimate_c,
            disturbance_w: map("disturbance_w"),
            solar_scale: map("solar_scale"),
        }
    }
}

/// One plan block, reduced to what the room cards read.
#[derive(Debug, Clone)]
pub struct PlanBlock {
    pub t: DateTime<Utc>,
    pub dt_minutes: u32,
    pub heat_kw: HashMap<String, f64>,
    pub temp_c: HashMap<String, f64>,
}

/// The loop's latest published plan, reduced to what the room cards read.
#[derive(Debug, Clone, Default)]
pub struct PlanInput {
    pub computed_at: Option<DateTime<Utc>>,
    /// Older than `/readyz`'s limit: no longer what the controllers apply.
    pub stale: bool,
    pub blocks: Vec<PlanBlock>,
    pub pull_kh: HashMap<String, f64>,
    pub pull_break_even_eur_per_kwh: HashMap<String, f64>,
    pub heat_credit_eur_per_kwh: HashMap<String, f64>,
    pub disturbance_w: HashMap<String, f64>,
    pub solar_scale: HashMap<String, f64>,
}

impl PlanInput {
    /// Called under the `latest` lock, so it copies only what the cards read. Age is the plan's
    /// monotonic publish instant, judged exactly as `/readyz` does (whole seconds, inclusive limit).
    pub fn from_published(tp: &crate::app::TimestampedPlan, max_age_s: u64) -> Self {
        let p = &tp.plan;
        Self {
            computed_at: Some(tp.computed_at),
            stale: tp.published.elapsed().as_secs() > max_age_s,
            blocks: p
                .timeline
                .iter()
                .map(|b| PlanBlock {
                    t: b.t,
                    dt_minutes: b.dt_minutes,
                    heat_kw: b.heat_kw.clone(),
                    temp_c: b.temp_c.clone(),
                })
                .collect(),
            pull_kh: p.pull_kh.clone(),
            pull_break_even_eur_per_kwh: p.pull_break_even_eur_per_kwh.clone(),
            heat_credit_eur_per_kwh: p.terminal_heat_credit_eur_per_kwh.clone(),
            disturbance_w: p.disturbance_w.clone(),
            solar_scale: p.solar_scale.clone(),
        }
    }
}

/// One ledger block's MEASURED per-zone heating energy (blocks with no measured side are omitted by
/// the caller).
#[derive(Debug, Clone)]
pub struct LedgerHeat {
    pub t: DateTime<Utc>,
    pub heat_kwh: HashMap<String, Option<f64>>,
}

pub struct Inputs<'a> {
    pub now: DateTime<Utc>,
    /// The site's UTC offset at a given instant (DST-aware). "Today" is the site-local calendar
    /// date at `now`; each ledger row is classified with the offset at ITS OWN instant, so a row
    /// before a DST change that day lands on the right date.
    pub offset_at: &'a dyn Fn(DateTime<Utc>) -> FixedOffset,
    /// Interior model zones (not `outside`/`ground`).
    pub model_zones: &'a [ModelZone],
    pub heaters: &'a HashMap<String, HeaterFacts>,
    pub bands: &'a HashMap<String, Comfort>,
    pub state: Option<&'a StateInput>,
    pub plan: Option<&'a PlanInput>,
    pub ledger: &'a [LedgerHeat],
    pub sensors: &'a SensorReadout,
    pub site: Site,
    /// How far ahead each room's `plan` slice reaches (hours, clamped to
    /// 1..=[`PLAN_HORIZON_MAX_HOURS`]); `plan_heat_kwh_12h` always sums [`PLAN_HORIZON_HOURS`].
    pub plan_hours: i64,
}

/// The house's location (decimal degrees, from `config.json5` `site`), passed through so the
/// dashboard's 3D view can place the sun for any instant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Site {
    pub lat: f64,
    pub lon: f64,
}

// --- Output ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RoomsReport {
    pub plan_at: Option<String>,
    pub plan_stale: Option<bool>,
    pub sensors_at: Option<String>,
    pub sensor_error: Option<String>,
    pub site: Site,
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub zone: String,
    pub volume_m3: Option<f64>,
    pub ach: Option<f64>,
    pub heated: bool,
    pub max_heat_kw: Option<f64>,
    pub internal_gain_w: Option<f64>,
    pub measured: Option<Measured>,
    pub sensor_failed: bool,
    pub humidity: Option<Humidity>,
    pub presence: Option<Presence>,
    pub estimate_c: Option<f64>,
    pub disturbance_w: Option<f64>,
    pub solar_scale: Option<f64>,
    /// The heat band in force now (`{target, low, high}`).
    pub heat: Option<BandNow>,
    /// The cool band in force now (`{target, low, high, active}`); `active` is false until an HVAC
    /// unit serves the room.
    pub cool: Option<CoolNow>,
    /// Weight of this room's pull toward its target (`0` = band only).
    pub priority: Option<f64>,
    /// `"heating"` or `"cooling"`: the side of the band the room works on (exclusive; decided from
    /// the free drift). `null` until the loop has decided one.
    pub mode: Option<&'static str>,
    /// When the room entered its current mode (UTC).
    pub mode_since: Option<String>,
    /// Free-drift slope with every actuator off, K per day (+ = gaining heat).
    pub drift_k_per_day: Option<f64>,
    pub heating: Option<Heating>,
    pub heating_today_kwh: Option<f64>,
    pub plan: Option<PlanSlice>,
    pub plan_heat_kwh_12h: Option<f64>,
    pub pull: Option<Pull>,
    pub heat_credit_eur_per_kwh: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measured {
    pub temp_c: f64,
    pub at: String,
    pub age_s: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Humidity {
    pub pct: f64,
    pub at: String,
    pub age_s: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Presence {
    pub motion: bool,
    pub at: String,
    pub age_s: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Heating {
    pub on: bool,
    pub kw: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanSlice {
    pub t: Vec<String>,
    pub dt_minutes: Vec<u32>,
    pub heat_kw: Vec<Option<f64>>,
    pub temp_c: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pull {
    /// `comfort.pull_eur_per_kh × priority`.
    pub eur_per_kh: f64,
    pub kh: Option<f64>,
    pub break_even_eur_per_kwh: Option<f64>,
}

fn round(x: f64, dp: i32) -> f64 {
    let m = 10f64.powi(dp);
    (x * m).round() / m
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The zone ids the report covers: every interior model zone, alphabetically (deduplicated).
pub fn zone_ids(model_zones: &[ModelZone]) -> Vec<String> {
    let mut out: Vec<String> = model_zones.iter().map(|z| z.name.clone()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The merge. Pure: same inputs, same report.
pub fn merge(inp: &Inputs) -> RoomsReport {
    let today = inp
        .now
        .with_timezone(&(inp.offset_at)(inp.now))
        .date_naive();
    let horizon_end = inp.now + Duration::hours(inp.plan_hours.clamp(1, PLAN_HORIZON_MAX_HOURS));
    let kwh_end = inp.now + Duration::hours(PLAN_HORIZON_HOURS);
    let age = |at: DateTime<Utc>| (inp.now - at).num_seconds().max(0);

    let model_zone = |zone: &str| inp.model_zones.iter().find(|z| z.name == zone);

    let rooms = zone_ids(inp.model_zones)
        .into_iter()
        .map(|zone| {
            let mz = model_zone(&zone);
            let heater = inp.heaters.get(&zone);
            let readings = inp.sensors.zones.get(&zone);

            let measured = readings.and_then(|r| r.temperature).map(|r| Measured {
                temp_c: round(r.value, 2),
                at: rfc3339(r.at),
                age_s: age(r.at),
            });
            let humidity = readings.and_then(|r| r.humidity).map(|r| Humidity {
                pct: round(r.value, 1),
                at: rfc3339(r.at),
                age_s: age(r.at),
            });
            let presence = readings.and_then(|r| r.presence).map(|r| Presence {
                motion: r.value > 0.5,
                at: rfc3339(r.at),
                age_s: age(r.at),
            });

            // The plan's blocks that are still running or start within the horizon.
            let slice: Vec<&PlanBlock> = inp
                .plan
                .map(|p| {
                    p.blocks
                        .iter()
                        .filter(|b| {
                            b.t < horizon_end
                                && b.t + Duration::minutes(b.dt_minutes.into()) > inp.now
                        })
                        .collect()
                })
                .unwrap_or_default();
            let in_plan = slice
                .iter()
                .any(|b| b.heat_kw.contains_key(&zone) || b.temp_c.contains_key(&zone));
            let plan_slice = in_plan.then(|| PlanSlice {
                t: slice.iter().map(|b| rfc3339(b.t)).collect(),
                dt_minutes: slice.iter().map(|b| b.dt_minutes).collect(),
                heat_kw: slice
                    .iter()
                    .map(|b| b.heat_kw.get(&zone).map(|x| round(*x, 3)))
                    .collect(),
                temp_c: slice
                    .iter()
                    .map(|b| b.temp_c.get(&zone).map(|x| round(*x, 2)))
                    .collect(),
            });
            // Each block clipped to [now, kwh_end], independent of the `plan_hours` slice.
            let plan_kwh = inp.plan.and_then(|p| {
                let mut known = p
                    .blocks
                    .iter()
                    .filter_map(|b| {
                        let start = b.t.max(inp.now);
                        let end = (b.t + Duration::minutes(b.dt_minutes.into())).min(kwh_end);
                        if start >= end {
                            return None;
                        }
                        let hours = (end - start).num_seconds() as f64 / 3600.0;
                        Some(b.heat_kw.get(&zone)? * hours)
                    })
                    .peekable();
                known.peek().is_some().then(|| round(known.sum::<f64>(), 3))
            });

            // The block running right now (the relay-pinned, committed one); none from a stale plan.
            let current = inp.plan.filter(|p| !p.stale).and_then(|p| {
                p.blocks.iter().find(|b| {
                    b.t <= inp.now && inp.now < b.t + Duration::minutes(b.dt_minutes.into())
                })
            });
            let heating = heater.and(current).and_then(|block| {
                let kw = *block.heat_kw.get(&zone)?;
                Some(Heating {
                    on: kw > HEATER_ON_KW,
                    kw: round(kw, 3),
                })
            });
            let heating_today_kwh = heater.and_then(|_| {
                let mut measured_today = inp
                    .ledger
                    .iter()
                    .filter(|r| r.t.with_timezone(&(inp.offset_at)(r.t)).date_naive() == today)
                    .filter_map(|r| r.heat_kwh.get(&zone).copied().flatten())
                    .peekable();
                measured_today
                    .peek()
                    .is_some()
                    .then(|| round(measured_today.sum::<f64>(), 3))
            });

            let pull = heater.filter(|h| h.pull_eur_per_kh > 0.0).map(|h| Pull {
                eur_per_kh: h.pull_eur_per_kh,
                kh: inp
                    .plan
                    .and_then(|p| p.pull_kh.get(&zone))
                    .map(|x| round(*x, 2)),
                break_even_eur_per_kwh: inp
                    .plan
                    .and_then(|p| p.pull_break_even_eur_per_kwh.get(&zone))
                    .map(|x| round(*x, 4)),
            });

            // Disturbance / solar scale: the state estimate's, else the plan's (same quantities).
            let from_state_or_plan = |state: Option<&HashMap<String, f64>>,
                                      plan: Option<&HashMap<String, f64>>,
                                      dp: i32| {
                state
                    .and_then(|m| m.get(&zone))
                    .or_else(|| plan.and_then(|m| m.get(&zone)))
                    .map(|x| round(*x, dp))
            };

            Room {
                volume_m3: mz.and_then(|z| z.volume_m3).map(|v| round(v, 1)),
                ach: mz.map(|z| z.ach).filter(|a| *a > 0.0),
                heated: heater.is_some(),
                max_heat_kw: heater.map(|h| h.max_heat_kw),
                internal_gain_w: heater.map(|h| h.internal_gain_w),
                measured,
                sensor_failed: readings.is_some_and(|r| r.temperature_read_failed),
                humidity,
                presence,
                estimate_c: inp
                    .state
                    .and_then(|s| s.estimate_c.get(&zone))
                    .map(|x| round(*x, 2)),
                disturbance_w: from_state_or_plan(
                    inp.state.map(|s| &s.disturbance_w),
                    inp.plan.map(|p| &p.disturbance_w),
                    1,
                ),
                solar_scale: from_state_or_plan(
                    inp.state.map(|s| &s.solar_scale),
                    inp.plan.map(|p| &p.solar_scale),
                    3,
                ),
                heat: inp.bands.get(&zone).map(|c| c.heat),
                cool: inp.bands.get(&zone).and_then(|c| c.cool),
                priority: inp.bands.get(&zone).map(|c| c.priority),
                mode: inp
                    .bands
                    .get(&zone)
                    .and_then(|c| c.mode)
                    .map(|m| m.mode.as_str()),
                mode_since: inp
                    .bands
                    .get(&zone)
                    .and_then(|c| c.mode)
                    .map(|m| rfc3339(m.since)),
                drift_k_per_day: inp
                    .bands
                    .get(&zone)
                    .and_then(|c| c.mode)
                    .map(|m| round(m.drift_k_per_day, 3)),
                heating,
                heating_today_kwh,
                plan: plan_slice,
                plan_heat_kwh_12h: plan_kwh,
                pull,
                heat_credit_eur_per_kwh: inp
                    .plan
                    .and_then(|p| p.heat_credit_eur_per_kwh.get(&zone))
                    .map(|x| round(*x, 4)),
                zone,
            }
        })
        .collect();

    RoomsReport {
        plan_at: inp.plan.and_then(|p| p.computed_at).map(rfc3339),
        plan_stale: inp.plan.map(|p| p.stale),
        sensors_at: inp.sensors.at.map(rfc3339),
        sensor_error: inp.sensors.error.clone(),
        site: inp.site,
        rooms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn spec(bucket: &str, measurement: &str, room: &str, field: &str) -> SignalSpec {
        SignalSpec {
            bucket: bucket.into(),
            measurement: measurement.into(),
            tags: HashMap::from([("room".to_string(), room.to_string())]),
            field: field.into(),
        }
    }

    const NOW: &str = "2026-10-09T10:07:00Z";
    const SITE: Site = Site {
        lat: 49.49,
        lon: 17.43,
    };

    #[test]
    fn all_null_inputs_give_honest_nulls() {
        let sensors = SensorReadout::default();
        let (heaters, bands) = (HashMap::new(), HashMap::new());
        let mz = [ModelZone {
            name: "hall".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let report = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: None,
            ledger: &[],
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert_eq!(report.plan_at, None);
        assert_eq!(report.sensors_at, None);
        assert_eq!(report.sensor_error, None);
        // Indexing a `Value` by a missing key also yields `Null`, so presence is checked explicitly.
        let present_and_null = |o: &serde_json::Value, k: &str| {
            o.as_object().unwrap().get(k).is_some_and(|x| x.is_null())
        };
        let whole = serde_json::to_value(&report).unwrap();
        for k in ["plan_at", "plan_stale", "sensors_at", "sensor_error"] {
            assert!(present_and_null(&whole, k), "{k} must be present and null");
        }
        assert_eq!(report.rooms.len(), 1);
        let v = serde_json::to_value(&report.rooms[0]).unwrap();
        for k in [
            "volume_m3",
            "ach",
            "max_heat_kw",
            "internal_gain_w",
            "measured",
            "humidity",
            "presence",
            "estimate_c",
            "disturbance_w",
            "solar_scale",
            "heat",
            "cool",
            "priority",
            "mode",
            "mode_since",
            "drift_k_per_day",
            "heating",
            "heating_today_kwh",
            "plan",
            "plan_heat_kwh_12h",
            "pull",
            "heat_credit_eur_per_kwh",
        ] {
            assert!(
                present_and_null(&v, k),
                "{k} must be present and null, got {:?}",
                v.get(k)
            );
        }
        assert_eq!(v["zone"], "hall");
        assert_eq!(v["heated"], false);
        assert_eq!(v["sensor_failed"], false);
        // The floor-plan facts belong to the dashboard's own floorplan.json, not to this API.
        for k in [
            "name",
            "floor",
            "area_m2",
            "area_source",
            "source",
            "polygon",
            "label",
        ] {
            assert!(
                v.get(k).is_none(),
                "{k} must not be served, got {:?}",
                v.get(k)
            );
        }
        // the site rides along whatever else is missing: the 3D view's sun needs it
        let whole = serde_json::to_value(&report).unwrap();
        assert_eq!(whole["site"], json!({ "lat": 49.49, "lon": 17.43 }));
    }

    fn block(start: &str, dt: u32, kw: Option<f64>, temp: Option<f64>) -> PlanBlock {
        PlanBlock {
            t: t(start),
            dt_minutes: dt,
            heat_kw: kw
                .map(|k| HashMap::from([("kitchen".to_string(), k)]))
                .unwrap_or_default(),
            temp_c: temp
                .map(|k| HashMap::from([("kitchen".to_string(), k)]))
                .unwrap_or_default(),
        }
    }

    #[test]
    fn full_fixture_every_field() {
        let mz = [
            ModelZone {
                name: "kitchen".into(),
                volume_m3: Some(62.04),
                ach: 0.5,
            },
            ModelZone {
                name: "attic".into(),
                volume_m3: Some(80.0),
                ach: 0.3,
            },
            ModelZone {
                name: "hall".into(),
                volume_m3: Some(10.0),
                ach: 0.0,
            },
        ];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 40.0,
                pull_eur_per_kh: 0.1,
            },
        )]);
        let band = Comfort::from_zone_json(&json!({
            "heat": { "target": 22.0, "low": 21.0, "high": 24.0 },
            "cool": { "target": 25.0, "low": 24.0, "high": 26.0, "active": false },
            "priority": 1.0
        }))
        .unwrap();
        let bands = HashMap::from([("kitchen".to_string(), band)]);
        let state = StateInput::from_json(&json!({
            "zones": [{"zone": "kitchen", "temp_c": 21.234}],
            "disturbance_w": {"kitchen": 12.34},
            "solar_scale": {"kitchen": 1.1234}
        }));
        let plan = PlanInput {
            computed_at: Some(t("2026-10-09T10:05:00Z")),
            stale: false,
            blocks: vec![
                block("2026-10-09T10:00:00Z", 15, Some(1.5), Some(21.0)),
                block("2026-10-09T10:15:00Z", 15, Some(0.0), Some(21.2)),
                block("2026-10-09T11:00:00Z", 60, Some(1.0), None),
                block("2026-10-09T22:00:00Z", 60, Some(2.0), Some(22.0)), // starts inside 12 h
                block("2026-10-09T22:07:00Z", 60, Some(9.0), Some(9.0)), // starts at the edge: excluded
            ],
            pull_kh: HashMap::from([("kitchen".to_string(), 3.456)]),
            pull_break_even_eur_per_kwh: HashMap::from([("kitchen".to_string(), 0.12345)]),
            heat_credit_eur_per_kwh: HashMap::from([("kitchen".to_string(), 0.05)]),
            disturbance_w: HashMap::from([("kitchen".to_string(), 99.0)]),
            solar_scale: HashMap::new(),
        };
        let ledger = [
            LedgerHeat {
                t: t("2026-10-09T05:00:00Z"),
                heat_kwh: HashMap::from([("kitchen".to_string(), Some(0.25))]),
            },
            LedgerHeat {
                t: t("2026-10-09T05:15:00Z"),
                heat_kwh: HashMap::from([("kitchen".to_string(), None)]),
            },
            LedgerHeat {
                t: t("2026-10-09T06:00:00Z"),
                heat_kwh: HashMap::from([("kitchen".to_string(), Some(0.5))]),
            },
        ];
        let sensors = SensorReadout {
            at: Some(t(NOW)),
            error: None,
            zones: HashMap::from([(
                "kitchen".to_string(),
                ZoneReadings {
                    temperature: Some(Reading {
                        value: 21.349,
                        at: t("2026-10-09T10:05:00Z"),
                    }),
                    humidity: Some(Reading {
                        value: 53.56,
                        at: t("2026-10-09T10:02:00Z"),
                    }),
                    presence: Some(Reading {
                        value: 1.0,
                        at: t("2026-10-09T10:06:30Z"),
                    }),
                    temperature_read_failed: false,
                },
            )]),
        };
        let report = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: Some(&state),
            plan: Some(&plan),
            ledger: &ledger,
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert_eq!(report.plan_at.as_deref(), Some("2026-10-09T10:05:00Z"));
        assert_eq!(report.plan_stale, Some(false));
        assert_eq!(report.sensors_at.as_deref(), Some("2026-10-09T10:07:00Z"));
        // Every interior model zone, alphabetically — no other source adds or orders rooms.
        let order: Vec<&str> = report.rooms.iter().map(|r| r.zone.as_str()).collect();
        assert_eq!(order, ["attic", "hall", "kitchen"]);

        let k = serde_json::to_value(&report.rooms[2]).unwrap();
        assert_eq!(k["zone"], "kitchen");
        assert_eq!(k["volume_m3"], 62.0);
        assert_eq!(k["ach"], 0.5);
        assert_eq!(k["heated"], true);
        assert_eq!(k["max_heat_kw"], 1.5);
        assert_eq!(k["internal_gain_w"], 40.0);
        assert_eq!(
            k["measured"],
            json!({"temp_c": 21.35, "at": "2026-10-09T10:05:00Z", "age_s": 120})
        );
        assert_eq!(
            k["humidity"],
            json!({"pct": 53.6, "at": "2026-10-09T10:02:00Z", "age_s": 300})
        );
        assert_eq!(
            k["presence"],
            json!({"motion": true, "at": "2026-10-09T10:06:30Z", "age_s": 30})
        );
        assert_eq!(k["estimate_c"], 21.23);
        assert_eq!(k["disturbance_w"], 12.3); // the state's value wins over the plan's 99
        assert_eq!(k["solar_scale"], 1.123);
        assert_eq!(
            k["heat"],
            json!({"target": 22.0, "low": 21.0, "high": 24.0})
        );
        assert_eq!(
            k["cool"],
            json!({"target": 25.0, "low": 24.0, "high": 26.0, "active": false})
        );
        assert_eq!(k["priority"], 1.0);
        // Current block = the 10:00 one (1.5 kW); today's measured kWh = 0.25 + 0.5 (the None skipped).
        assert_eq!(k["heating"], json!({"on": true, "kw": 1.5}));
        assert_eq!(k["heating_today_kwh"], 0.75);
        // Plan slice: the 10:00, 10:15, 11:00 and 22:00 blocks; the 22:07 one starts after now+12 h.
        assert_eq!(k["plan"]["t"].as_array().unwrap().len(), 4);
        assert_eq!(k["plan"]["dt_minutes"], json!([15, 15, 60, 60]));
        assert_eq!(k["plan"]["heat_kw"], json!([1.5, 0.0, 1.0, 2.0]));
        assert_eq!(k["plan"]["temp_c"], json!([21.0, 21.2, null, 22.0]));
        // Each block clipped to [now, now+12 h]: the running 10:00 block counts its remaining 8 min,
        // the 22:00 one its first 7: 1.5·(8/60) + 0 + 1.0·1 + 2.0·(7/60) = 1.433
        assert_eq!(k["plan_heat_kwh_12h"], 1.433);
        assert_eq!(
            k["pull"],
            json!({"eur_per_kh": 0.1, "kh": 3.46, "break_even_eur_per_kwh": 0.1235})
        );
        assert_eq!(k["heat_credit_eur_per_kwh"], 0.05);

        // An unheated zone: model facts only, no heating/plan/warmth.
        let a = serde_json::to_value(&report.rooms[0]).unwrap();
        assert_eq!(a["zone"], "attic");
        assert_eq!(a["volume_m3"], 80.0);
        assert_eq!(a["ach"], 0.3);
        assert_eq!(a["heated"], false);
        assert!(a["heating"].is_null() && a["plan"].is_null() && a["pull"].is_null());
        assert!(a["heat"].is_null() && a["cool"].is_null() && a["priority"].is_null());
        assert!(a["measured"].is_null() && a["plan_heat_kwh_12h"].is_null());
        assert!(a["heating_today_kwh"].is_null());
        // ach 0 (not modelled) ⇒ null.
        let h = serde_json::to_value(&report.rooms[1]).unwrap();
        assert_eq!(h["zone"], "hall");
        assert_eq!(h["volume_m3"], 10.0);
        assert!(h["ach"].is_null());

        // A longer slice (the dashboard's timeline asks for the whole plan): the 22:07 block and a
        // far one join `plan`, but `plan_heat_kwh_12h` still sums only the next 12 h. Two more
        // blocks start 60 h and 73 h after now, either side of the 72 h cap.
        let mut long = plan.clone();
        long.blocks.extend([
            block("2026-10-10T20:00:00Z", 60, Some(3.0), Some(23.0)),
            block("2026-10-11T22:07:00Z", 60, Some(4.0), Some(23.0)),
            block("2026-10-12T11:07:00Z", 60, Some(5.0), Some(23.0)),
        ]);
        let wide = |hours| {
            let r = merge(&Inputs {
                now: t(NOW),
                offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
                model_zones: &mz,
                heaters: &heaters,
                bands: &bands,
                state: Some(&state),
                plan: Some(&long),
                ledger: &ledger,
                sensors: &sensors,
                site: SITE,
                plan_hours: hours,
            });
            serde_json::to_value(&r.rooms[2]).unwrap()
        };
        let k48 = wide(48);
        assert_eq!(k48["plan"]["dt_minutes"], json!([15, 15, 60, 60, 60, 60]));
        assert_eq!(
            k48["plan"]["heat_kw"],
            json!([1.5, 0.0, 1.0, 2.0, 9.0, 3.0])
        );
        assert_eq!(k48["plan_heat_kwh_12h"], 1.433);
        // out-of-range requests clamp: 0 h is 1 h (blocks starting before 11:07), 1000 h the 72 h cap
        assert_eq!(wide(0)["plan"]["dt_minutes"], json!([15, 15, 60]));
        // a slice shorter than 12 h leaves the 12 h total alone
        assert_eq!(wide(1)["plan"]["dt_minutes"], json!([15, 15, 60]));
        assert_eq!(wide(1)["plan_heat_kwh_12h"], 1.433);
        let to_72h = json!([1.5, 0.0, 1.0, 2.0, 9.0, 3.0, 4.0]);
        assert_eq!(wide(72)["plan"]["heat_kw"], to_72h);
        assert_eq!(wide(1000)["plan"]["heat_kw"], to_72h);
    }

    #[test]
    fn heater_with_no_current_block_has_null_heating() {
        // A stale plan (its last block ended before now) must not report a heating state, but the
        // ledger's measured energy today does not depend on the plan.
        let mz = [ModelZone {
            name: "kitchen".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 0.0,
                pull_eur_per_kh: 0.0,
            },
        )]);
        let plan = PlanInput {
            blocks: vec![block("2026-10-09T08:00:00Z", 15, Some(1.5), Some(21.0))],
            ..Default::default()
        };
        let ledger = [LedgerHeat {
            t: t("2026-10-09T08:00:00Z"),
            heat_kwh: HashMap::from([("kitchen".to_string(), Some(0.375))]),
        }];
        let (bands, sensors) = (HashMap::new(), SensorReadout::default());
        let r = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: Some(&plan),
            ledger: &ledger,
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert!(r.rooms[0].heating.is_none());
        assert!(r.rooms[0].plan.is_none());
        assert!(r.rooms[0].pull.is_none());
        assert_eq!(r.rooms[0].heating_today_kwh, Some(0.375));
    }

    #[test]
    fn heater_missing_from_the_current_block_has_null_heating() {
        // The running block plans other zones but not this one: unknown, not "off at 0 kW".
        let mz = [ModelZone {
            name: "kitchen".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 0.0,
                pull_eur_per_kh: 0.0,
            },
        )]);
        let plan = PlanInput {
            blocks: vec![PlanBlock {
                t: t("2026-10-09T10:00:00Z"),
                dt_minutes: 15,
                heat_kw: HashMap::from([("guestroom".to_string(), 1.0)]),
                temp_c: HashMap::new(),
            }],
            ..Default::default()
        };
        let (bands, sensors) = (HashMap::new(), SensorReadout::default());
        let r = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: Some(&plan),
            ledger: &[],
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert!(r.rooms[0].heating.is_none());
        assert!(r.rooms[0].plan_heat_kwh_12h.is_none());
    }

    fn kitchen_heating(plan: &PlanInput) -> RoomsReport {
        let mz = [ModelZone {
            name: "kitchen".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 0.0,
                pull_eur_per_kh: 0.0,
            },
        )]);
        let (bands, sensors) = (HashMap::new(), SensorReadout::default());
        merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(7200).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: Some(plan),
            ledger: &[],
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        })
    }

    #[test]
    fn stale_plan_reports_no_heating_but_keeps_its_slice() {
        // A block still covers now, but the loop stopped publishing: nobody applies it.
        let plan = PlanInput {
            stale: true,
            blocks: vec![block("2026-10-09T10:00:00Z", 15, Some(1.5), Some(21.0))],
            ..Default::default()
        };
        let r = kitchen_heating(&plan);
        assert_eq!(r.plan_stale, Some(true));
        assert!(r.rooms[0].heating.is_none());
        assert_eq!(r.rooms[0].plan.as_ref().unwrap().heat_kw, [Some(1.5)]);

        let fresh = kitchen_heating(&PlanInput {
            stale: false,
            ..plan
        });
        assert_eq!(fresh.plan_stale, Some(false));
        assert!(fresh.rooms[0].heating.as_ref().is_some_and(|h| h.on));
    }

    #[test]
    fn solver_dust_in_the_current_block_is_off() {
        let at = |kw| {
            let plan = PlanInput {
                blocks: vec![block("2026-10-09T10:00:00Z", 15, Some(kw), None)],
                ..Default::default()
            };
            kitchen_heating(&plan).rooms[0].heating.as_ref().unwrap().on
        };
        assert!(!at(0.03));
        assert!(!at(0.05));
        assert!(at(0.06));
    }

    #[test]
    fn today_kwh_uses_the_site_local_date() {
        let mz = [ModelZone {
            name: "kitchen".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 0.0,
                pull_eur_per_kh: 0.0,
            },
        )]);
        let plan = PlanInput {
            // now = 2026-10-09 00:30 local (UTC+2) = 2026-10-08T22:30Z
            blocks: vec![block("2026-10-08T22:30:00Z", 15, Some(0.0), None)],
            ..Default::default()
        };
        let row = |s: &str, kwh: f64| LedgerHeat {
            t: t(s),
            heat_kwh: HashMap::from([("kitchen".to_string(), Some(kwh))]),
        };
        let ledger = [
            row("2026-10-08T21:45:00Z", 1.0), // 23:45 local on the 8th: yesterday
            row("2026-10-08T22:00:00Z", 0.25), // 00:00 local on the 9th: today
            row("2026-10-08T22:15:00Z", 0.5),
        ];
        let (bands, sensors) = (HashMap::new(), SensorReadout::default());
        let now = t("2026-10-08T22:30:00Z");
        let run = |offset_s| {
            let r = merge(&Inputs {
                now,
                offset_at: &|_| FixedOffset::east_opt(offset_s).unwrap(),
                model_zones: &mz,
                heaters: &heaters,
                bands: &bands,
                state: None,
                plan: Some(&plan),
                ledger: &ledger,
                sensors: &sensors,
                site: SITE,
                plan_hours: PLAN_HORIZON_HOURS,
            });
            r.rooms[0].heating_today_kwh
        };
        assert_eq!(run(7200), Some(0.75)); // UTC+2: 9th began at 22:00Z
        assert_eq!(run(0), Some(1.75)); // UTC: it is still the 8th, all three rows count
    }

    #[test]
    fn today_kwh_uses_each_rows_own_offset_across_a_dst_change() {
        // Europe/Prague fall-back: 2026-10-25 01:00Z (CEST +2 -> CET +1).
        let change = t("2026-10-25T01:00:00Z");
        let offset_at = move |at: DateTime<Utc>| {
            FixedOffset::east_opt(if at < change { 7200 } else { 3600 }).unwrap()
        };
        let mz = [ModelZone {
            name: "kitchen".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let heaters = HashMap::from([(
            "kitchen".to_string(),
            HeaterFacts {
                max_heat_kw: 1.5,
                internal_gain_w: 0.0,
                pull_eur_per_kh: 0.0,
            },
        )]);
        let now = t("2026-10-25T10:00:00Z"); // 11:00 CET on the 25th
        let plan = PlanInput {
            blocks: vec![block("2026-10-25T10:00:00Z", 15, Some(0.0), None)],
            ..Default::default()
        };
        let row = |s: &str, kwh: f64| LedgerHeat {
            t: t(s),
            heat_kwh: HashMap::from([("kitchen".to_string(), Some(kwh))]),
        };
        let ledger = [
            row("2026-10-24T21:59:00Z", 1.0), // 23:59 CEST on the 24th: yesterday
            // 00:30 CEST on the 25th — today. Judged with the offset at NOW (+1) it would read
            // 23:30 on the 24th and be dropped.
            row("2026-10-24T22:30:00Z", 0.25),
            row("2026-10-25T00:30:00Z", 0.5), // 02:30 CEST (before the change): today
            row("2026-10-25T02:00:00Z", 0.125), // 03:00 CET: today
        ];
        let (bands, sensors) = (HashMap::new(), SensorReadout::default());
        let r = merge(&Inputs {
            now,
            offset_at: &offset_at,
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: Some(&plan),
            ledger: &ledger,
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert_eq!(r.rooms[0].heating_today_kwh, Some(0.875));
    }

    #[test]
    fn humidity_and_presence_default_to_the_temperature_room() {
        let temp = spec("loxone", "temperature", "obyvak", "temperature_obyvak");
        let s = resolve_signals(Some(temp.clone()), None, None);
        assert_eq!(s.temperature, Some(temp));
        assert_eq!(
            s.humidity,
            Some(spec("loxone", "humidity", "obyvak", "humidity_obyvak"))
        );
        assert_eq!(
            s.presence,
            Some(spec("loxone", "presence", "obyvak", "motion_obyvak"))
        );
        // No temperature mapping (or no `room` tag) ⇒ nothing derived.
        assert_eq!(resolve_signals(None, None, None), ZoneSignals::default());
        let mut no_room = spec("loxone", "temperature", "x", "t");
        no_room.tags.clear();
        let s = resolve_signals(Some(no_room), None, None);
        assert!(s.humidity.is_none() && s.presence.is_none());
    }

    #[test]
    fn explicit_locators_override_the_derived_ones() {
        let temp = spec("loxone", "temperature", "obyvak", "temperature_obyvak");
        let hum = spec("other", "hum", "living", "rh");
        let s = resolve_signals(Some(temp), Some(hum.clone()), None);
        assert_eq!(s.humidity, Some(hum));
        // The unset one is still derived.
        assert_eq!(
            s.presence,
            Some(spec("loxone", "presence", "obyvak", "motion_obyvak"))
        );
    }

    #[test]
    fn sensor_queries_group_by_bucket() {
        let a = resolve_signals(
            Some(spec("loxone", "temperature", "a", "temperature_a")),
            None,
            None,
        );
        let b = resolve_signals(
            Some(spec("loxone", "temperature", "b", "temperature_b")),
            Some(spec("alt", "hum", "b", "rh_b")),
            None,
        );
        let q = sensor_queries([&a, &b]);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].bucket, "alt");
        assert_eq!(q[0].measurements, ["hum"]);
        assert_eq!(q[1].bucket, "loxone");
        assert_eq!(q[1].measurements, ["humidity", "presence", "temperature"]);
        assert_eq!(
            q[1].fields,
            [
                "humidity_a",
                "motion_a",
                "motion_b",
                "temperature_a",
                "temperature_b"
            ]
        );
    }

    fn row(m: &str, f: &str, room: &str, time: &str, v: &str) -> HashMap<String, String> {
        HashMap::from([
            ("_measurement".into(), m.into()),
            ("_field".into(), f.into()),
            ("room".into(), room.into()),
            ("_time".into(), time.into()),
            ("_value".into(), v.into()),
        ])
    }

    #[test]
    fn pick_latest_matches_tags_and_takes_the_newest_parsed_time() {
        let s = spec("loxone", "humidity", "obyvak", "humidity_obyvak");
        let rows = vec![
            // `…:01Z` sorts AFTER `…:01.5Z` as a string ('Z' > '.') but is the OLDER instant, so a
            // lexicographic max would pick the wrong row.
            row(
                "humidity",
                "humidity_obyvak",
                "obyvak",
                "2026-10-09T10:00:01Z",
                "50",
            ),
            row(
                "humidity",
                "humidity_obyvak",
                "obyvak",
                "2026-10-09T10:00:01.5Z",
                "51.5",
            ),
            row(
                "humidity",
                "humidity_obyvak",
                "kuchyne",
                "2026-10-09T10:05:00Z",
                "99",
            ), // wrong room tag
            row(
                "temperature",
                "humidity_obyvak",
                "obyvak",
                "2026-10-09T10:05:00Z",
                "98",
            ), // wrong measurement
            row(
                "humidity",
                "humidity_obyvak",
                "obyvak",
                "2026-10-09T10:06:00Z",
                "NaN",
            ), // non-finite dropped
        ];
        let r = pick_latest(&rows, &s).unwrap();
        assert_eq!(r.value, 51.5);
        assert_eq!(r.at, t("2026-10-09T10:00:01.5Z"));
        assert!(pick_latest(&[], &s).is_none());
    }

    #[test]
    fn readings_are_distributed_per_zone() {
        let signals = HashMap::from([
            (
                "living".to_string(),
                resolve_signals(
                    Some(spec(
                        "loxone",
                        "temperature",
                        "obyvak",
                        "temperature_obyvak",
                    )),
                    None,
                    None,
                ),
            ),
            (
                "attic".to_string(),
                resolve_signals(
                    Some(spec(
                        "loxone",
                        "temperature",
                        "podkrovi",
                        "temperature_podkrovi",
                    )),
                    None,
                    None,
                ),
            ),
        ]);
        let rows = vec![
            row(
                "temperature",
                "temperature_obyvak",
                "obyvak",
                "2026-10-09T10:00:00Z",
                "21.5",
            ),
            row(
                "presence",
                "motion_obyvak",
                "obyvak",
                "2026-10-09T10:01:00Z",
                "0",
            ),
            row(
                "humidity",
                "humidity_podkrovi",
                "podkrovi",
                "2026-10-09T10:02:00Z",
                "40",
            ),
        ];
        let rows = HashMap::from([("loxone".to_string(), rows)]);
        let got = readings_from_rows(&rows, &HashSet::new(), &signals);
        assert_eq!(got["living"].temperature.unwrap().value, 21.5);
        assert_eq!(got["living"].presence.unwrap().value, 0.0);
        assert!(got["living"].humidity.is_none());
        assert!(got["attic"].temperature.is_none());
        assert_eq!(got["attic"].humidity.unwrap().value, 40.0);
    }

    #[test]
    fn a_locator_reads_only_its_own_buckets_rows() {
        let series = |time, v| row("temperature", "t_o", "obyvak", time, v);
        let signals = HashMap::from([
            (
                "living".to_string(),
                resolve_signals(
                    Some(spec("loxone", "temperature", "obyvak", "t_o")),
                    None,
                    None,
                ),
            ),
            (
                "office".to_string(),
                resolve_signals(
                    Some(spec("failed", "temperature", "obyvak", "t_o")),
                    None,
                    None,
                ),
            ),
        ]);
        // The mirror holds the identical series with a newer value; pooled rows would let it win.
        let rows = HashMap::from([
            (
                "loxone".to_string(),
                vec![series("2026-10-09T10:00:00Z", "21.0")],
            ),
            (
                "mirror".to_string(),
                vec![series("2026-10-09T10:05:00Z", "25.0")],
            ),
        ]);
        let got = readings_from_rows(&rows, &HashSet::from(["failed".to_string()]), &signals);
        assert_eq!(got["living"].temperature.unwrap().value, 21.0);
        assert!(got["office"].temperature.is_none());
        assert!(got["office"].temperature_read_failed);
    }

    #[test]
    fn sensor_error_is_reported_with_null_readings() {
        let sensors = SensorReadout {
            at: None,
            error: Some("boom".into()),
            zones: HashMap::new(),
        };
        let mz = [ModelZone {
            name: "hall".into(),
            volume_m3: None,
            ach: 0.0,
        }];
        let (heaters, bands) = (HashMap::new(), HashMap::new());
        let r = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(0).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: None,
            ledger: &[],
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        assert_eq!(r.sensor_error.as_deref(), Some("boom"));
        assert!(r.sensors_at.is_none() && r.rooms[0].measured.is_none());
    }

    #[test]
    fn sensor_failed_follows_the_temperature_bucket() {
        let signals = HashMap::from([
            // temperature in the failed bucket
            (
                "living".to_string(),
                resolve_signals(
                    Some(spec("alt", "temperature", "obyvak", "t_o")),
                    None,
                    None,
                ),
            ),
            // temperature read fine; only its humidity sits in the failed bucket
            (
                "kitchen".to_string(),
                resolve_signals(
                    Some(spec("loxone", "temperature", "kuchyne", "t_k")),
                    Some(spec("alt", "humidity", "kuchyne", "h_k")),
                    None,
                ),
            ),
            // read fine, but nothing in the window
            (
                "office".to_string(),
                resolve_signals(
                    Some(spec("loxone", "temperature", "pracovna", "t_p")),
                    None,
                    None,
                ),
            ),
            // no temperature signal at all
            ("hall".to_string(), ZoneSignals::default()),
        ]);
        let rows = vec![row(
            "temperature",
            "t_k",
            "kuchyne",
            "2026-10-09T10:00:00Z",
            "21.0",
        )];
        let sensors = SensorReadout {
            at: None,
            error: Some("alt: boom".into()),
            zones: readings_from_rows(
                &HashMap::from([("loxone".to_string(), rows)]),
                &HashSet::from(["alt".to_string()]),
                &signals,
            ),
        };
        let mz: Vec<ModelZone> = ["living", "kitchen", "office", "hall"]
            .into_iter()
            .map(|n| ModelZone {
                name: n.into(),
                volume_m3: None,
                ach: 0.0,
            })
            .collect();
        let (heaters, bands) = (HashMap::new(), HashMap::new());
        let r = merge(&Inputs {
            now: t(NOW),
            offset_at: &|_| FixedOffset::east_opt(0).unwrap(),
            model_zones: &mz,
            heaters: &heaters,
            bands: &bands,
            state: None,
            plan: None,
            ledger: &[],
            sensors: &sensors,
            site: SITE,
            plan_hours: PLAN_HORIZON_HOURS,
        });
        let failed: HashMap<&str, (bool, bool)> = r
            .rooms
            .iter()
            .map(|room| {
                (
                    room.zone.as_str(),
                    (room.sensor_failed, room.measured.is_some()),
                )
            })
            .collect();
        assert_eq!(failed["living"], (true, false));
        assert_eq!(failed["kitchen"], (false, true));
        assert_eq!(failed["office"], (false, false));
        assert_eq!(failed["hall"], (false, false));
        assert_eq!(r.sensor_error.as_deref(), Some("alt: boom"));
    }

    #[test]
    fn band_with_nan_limits_is_null() {
        // /api/zones serialises NaN as null.
        assert!(Comfort::from_zone_json(&json!({"heat": null, "priority": 1.0})).is_none());
        assert!(Comfort::from_zone_json(&json!({"zone": "x"})).is_none());
        // A heat-only room (no cool target) parses with `cool: None`.
        let c = Comfort::from_zone_json(&json!({
            "heat": {"target": 18.0, "low": 18.0, "high": 25.0}, "cool": null, "priority": 0.0
        }))
        .unwrap();
        assert!(c.cool.is_none() && c.priority == 0.0);
        assert!(c.mode.is_none(), "no decided mode parses as None");
    }

    #[test]
    fn mode_fields_are_read_from_the_zones_entry_and_served_per_room() {
        let band = Comfort::from_zone_json(&json!({
            "heat": {"target": 21.5, "low": 20.5, "high": 24.0},
            "cool": null, "priority": 1.0,
            "mode": "cooling", "mode_since": "2026-07-01T04:30:00Z", "drift_k_per_day": 1.5
        }))
        .unwrap();
        let m = band.mode.unwrap();
        assert_eq!(m.mode, crate::optimize::room_mode::RoomMode::Cooling);
        assert_eq!(m.drift_k_per_day, 1.5);
        // An unknown mode string is treated as undecided, not an error.
        let bad = Comfort::from_zone_json(&json!({
            "heat": {"target": 21.5, "low": 20.5, "high": 24.0}, "priority": 1.0,
            "mode": "sideways", "mode_since": "2026-07-01T04:30:00Z", "drift_k_per_day": 1.5
        }))
        .unwrap();
        assert!(bad.mode.is_none());
    }
}
