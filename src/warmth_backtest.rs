//! `backtest-warmth --start <rfc3339, hour-aligned> --days <1..=7> [--step-hours 1]
//! [--plant-gain 1.0] [--config <path>] [--model <path>] [--dump <fixture>] [--from <fixture>]
//! [--out <json>]` — a rolling-horizon replay of the PRODUCTION planning pipeline (the SAME
//! `SolveJob` + `app::fix_and_round` fix-and-round pipeline a live tick runs) with the thermal
//! model as the plant, proving the priority-zone warmth reward (spec `priority-zones`,
//! `heating.zones.*.warmth_value_eur_per_kh`) on real house data.
//!
//! `backtest-warmth --live [--config <path>]` instead compares OLD (every `warmth_value_eur_per_kh`
//! zeroed) vs NEW (the config as written) on the CURRENT on-demand plan (`app::current_plan`),
//! mirroring `terminal_backtest --live`.
//!
//! **Two arms, one seed.** OLD = the live config with every zone's `warmth_value_eur_per_kh`
//! zeroed; NEW = the config as written. Both start from the SAME seeded thermal state and the
//! SAME measured battery SoC (research.md D3) — only the reward in the LP's objective differs, so
//! any cost/temperature/energy delta is attributable to the reward alone.
//!
//! **The plant is the MODEL**, not the real house: each executed step rolls the discretized
//! state-space (`KernelSet::disc`, the SAME ZOH the live LP's kernels are built from) forward with
//! the plan's own `heat_kw` injected at each zone's `"heating"` marker — so Kelvin·hours here are
//! MODEL Kelvin·hours, and the real house (per `docs/configuration.md`'s kernel-gain note) responds
//! at roughly 2/3 of them in cold weather. `--plant-gain 0.67` derates the executed heat pulse to
//! approximate that over-response without waiting for the kernel fit to be corrected.
//!
//! **Bounded reads**, mirroring `backtest-heating`/`backtest-terminal`: OTE prices
//! ([`terminal_backtest::read_prices_chunked`], from `start − 28 d` for the day-type-median price
//! history to `start + days + 3 d`), Growatt PV/load/SoC ([`terminal_backtest::read_growatt_chunked`]/
//! [`terminal_backtest::read_soc_seed`]), and outside temperature / cloud / radiation / measured
//! zone temperatures / relay events ([`heating_backtest::read_window`], from `start − 48 h` warm-up
//! to `start + days + 3 d`) — every read in ≤7-day chunks, one series at a time, paused between
//! chunks. `--dump` writes everything read to ONE fixture (schema `warmth-backtest-v1`); `--from`
//! replays it with no InfluxDB token.
//!
//! **Known-price window.** OTE publishes day D's prices on D-1 by ~14:00 local — the SAME rule
//! [`terminal_backtest`] enforces — so the window's END must be at least 37 h before `now()` or the
//! core window could not be executed at REAL (not estimated) prices; `run()` enforces this and
//! prints the bound it checked.
//!
//! **Simplifications vs the live tick** (documented, not hidden — see `docs/api.md`'s "Warmth
//! backtest" section): perfect-foresight measured PV/load/weather stand in for the forecast
//! (the item under test is the warmth reward, not the forecast accuracy); internal gains come from
//! the CONFIG baseline only (no live Kalman gain re-fit, no disturbance observer, no per-zone
//! solar-gain scale); no EV scheduling. These match research.md D3's stated scope.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Duration, Timelike, Utc};
use nalgebra::DVector;
use serde::{Deserialize, Serialize};
use uom::si::{angle::degree, f64::Angle};

use crate::app::{
    battery_spec, build_kernel_cache, current_plan, default_pv_array, fix_and_round,
    hourly_solar_to_blocks, hourly_to_blocks, pv_arrays, select_terminal_value, terminal_soc_value,
    PlanExtras, SolveJob,
};
use crate::estimate::{assemble_drive_data, drive, hour_key, DriveData};
use crate::forecast::consumption::ConsumptionModel;
use crate::heating_backtest::{self, from_pairs, parts_from_dump, to_pairs};
use crate::influxdb::{InfluxDB, PriceSample, TimeSample};
use crate::live_inputs::align_blocks_15min;
use crate::model::Model;
use crate::optimize::config::{ControlConfig, HeatingConfig};
use crate::optimize::coordinator::{known_thermal_inputs, ForecastContext, Outlook};
use crate::optimize::price_forecast::{day_type_median_curve, parse_month_day};
use crate::optimize::thermal::{KernelSet, HEATING_MARKER};
use crate::optimize::unified::SolveBudget;
use crate::rc_network::RcNetwork;
use crate::relay_duty::relay_duty_hourly;
use crate::solar_scale_backtest::seed_from_series;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::terminal_backtest::{
    floor_to_hour, forward_fill, prepare_hour, read_growatt_chunked, read_prices_chunked,
    read_soc_seed, HourPrep,
};
use crate::tools::k_to_c;

const FINE_SECONDS: f64 = 900.0;
const FINE_SECONDS_I: i64 = 900;
const WARMUP_HOURS: i64 = 48;
/// OTE publishes day D's prices on D-1 by ~14:00 local; the window end must sit at least this far
/// behind `now()` for every block in it to be executable at REAL prices (matches
/// `terminal_backtest::run_window`'s own bound).
const KNOWN_PRICE_LAG_H: i64 = 37;
const SCHEMA: &str = "warmth-backtest-v1";
/// Below this grid import (kW) a block counts as PV-surplus for the price-band split — a few watts
/// of solver/measurement dust must not read as "imported electricity".
const IMPORT_DUST_KW: f64 = 0.02;
const POST_HORIZON_BLOCKS: usize = 96;
const DEFAULT_PUBLISH_HOUR: u32 = 14;

// --- The dumped/loaded raw-series fixture ---------------------------------------------------------

/// The `--dump`/`--from` fixture. `series` carries the SAME keys
/// [`heating_backtest::ReplayDump`] does (`outside`, `weather:*`, `zone:<zone>`,
/// `relay_events:<zone>`, `relay_state_before:<zone>`) plus `price`, `pv`, `load`, `soc` — so the
/// weather/zone/relay half can be unpacked verbatim by [`heating_backtest::parts_from_dump`]
/// instead of re-deriving that logic here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ReplayDump {
    pub(crate) schema: String,
    pub(crate) series: HashMap<String, Vec<(String, f64)>>,
}

fn as_heating_dump(dump: &ReplayDump) -> heating_backtest::ReplayDump {
    heating_backtest::ReplayDump {
        schema: dump.schema.clone(),
        series: dump.series.clone(),
    }
}

// --- Pure: price-band classification (D5 unit test 1) --------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PriceBand {
    /// Zero (or dust-level) grid import this block — the heat was paid for by PV surplus (or
    /// battery already charged from it), never by an import price at all.
    PvSurplus,
    /// Imported, in a low-tariff (NT) local hour.
    Nt,
    /// Imported, in a high-tariff (VT) local hour.
    Vt,
}

/// Classify one block by what it actually cost: zero (dust-level) grid import is PV-surplus
/// regardless of the clock; otherwise NT/VT by the tariff's own local-hour mask.
pub(crate) fn classify_band(
    grid_import_kw: f64,
    local_hour: u32,
    low_tariff_mask: &[bool; 24],
) -> PriceBand {
    if grid_import_kw <= IMPORT_DUST_KW {
        PriceBand::PvSurplus
    } else if low_tariff_mask[(local_hour % 24) as usize] {
        PriceBand::Nt
    } else {
        PriceBand::Vt
    }
}

// --- Pure: K·h scoring (D5 unit test 2) -----------------------------------------------------------

/// Kelvin·hours one zone's air sat above its floor (capped at the ceiling), below its floor, and
/// above its ceiling, over a plant air-temperature series `temp_c` (one value per step) against
/// `[t_min, t_max]`, each step `dt_h[i]` hours long (`temp_c`/`dt_h` same length).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct KhScore {
    pub(crate) above_floor: f64,
    pub(crate) below_floor: f64,
    pub(crate) above_ceiling: f64,
}

pub(crate) fn score_kh(temp_c: &[f64], dt_h: &[f64], t_min: f64, t_max: f64) -> KhScore {
    let mut s = KhScore::default();
    for (&t, &dt) in temp_c.iter().zip(dt_h) {
        s.above_floor += (t.min(t_max) - t_min).max(0.0) * dt;
        s.below_floor += (t_min - t).max(0.0) * dt;
        s.above_ceiling += (t - t_max).max(0.0) * dt;
    }
    s
}

// --- Pure: the zeroed-config OLD arm (D5 unit test 3) ---------------------------------------------

/// OLD arm: `heating` with every zone's `warmth_value_eur_per_kh` set to `0.0`, nothing else
/// changed — per spec, this must reproduce bit-identical plans to a config with no warmth values
/// at all.
pub(crate) fn zeroed_warmth(heating: &HeatingConfig) -> HeatingConfig {
    let mut zeroed = heating.clone();
    for zc in zeroed.zones.values_mut() {
        zc.warmth_value_eur_per_kh = 0.0;
    }
    zeroed
}

// --- Plant: step the discretized model forward with the plan's own heat_kw -----------------------

/// Add `power` (W) to `node`'s flux-input column of `u` — unlike [`StateSpace::set_flux`] (which
/// OVERWRITES the column), this ACCUMULATES: `u` already carries the boundary/solar/gain fluxes
/// [`known_thermal_inputs`] wrote, including a possible window-solar mass-share landing on the
/// SAME `"heating"` marker node the executed heat pulse also targets (`thermal.rs`'s
/// `thermal_inputs_over`), so overwriting would silently drop it.
fn add_flux(ss: &StateSpace, u: &mut DVector<f64>, node: petgraph::graph::NodeIndex, watts: f64) {
    if let Some(col) = ss.flux_input_column(node) {
        u[col] += watts;
    }
}

/// Step the plant one FINE (15-minute) block forward: `x <- Ad*x + Bd*(u_known + heat)`, `heat`
/// being `plan_heat_kw[zone] * plant_gain` injected at each zone's `"heating"` marker node(s),
/// split equally — exactly `thermal.rs`'s own slab-pulse split (kernel-building side), applied
/// here to the EXECUTED heat instead of a unit impulse.
fn step_plant(
    ss: &StateSpace,
    net: &RcNetwork,
    disc: &crate::state_space::Discretized,
    x: &DVector<f64>,
    u_known: &DVector<f64>,
    plan_heat_kw: &HashMap<String, f64>,
    plant_gain: f64,
) -> DVector<f64> {
    let mut u = u_known.clone();
    for (zone, &kw) in plan_heat_kw {
        if kw == 0.0 {
            continue;
        }
        let Some(nodes) = net
            .marker_indices
            .get_vec(&(zone.clone(), HEATING_MARKER.to_string()))
        else {
            continue;
        };
        if nodes.is_empty() {
            continue;
        }
        let per_node_w = kw * plant_gain * 1000.0 / nodes.len() as f64;
        for &node in nodes {
            add_flux(ss, &mut u, node, per_node_w);
        }
    }
    ss.step(disc, x, &u)
}

// --- IO: bounded reads, merged into one fixture ---------------------------------------------------

async fn read_all(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    window_start: DateTime<Utc>,
    days: i64,
) -> Result<ReplayDump> {
    let read_stop = window_start + Duration::hours(days * 24) + Duration::days(3);
    let warmup_start = window_start - Duration::hours(WARMUP_HOURS);
    let array_start = window_start - Duration::days(28);

    let mut heated_zone_rooms: Vec<(String, String)> = Vec::new();
    for zone in config.heating.zones.keys() {
        if net
            .marker_indices
            .contains_key(&(zone.clone(), HEATING_MARKER.to_string()))
        {
            if let Some(room) = db.zone_room(zone) {
                heated_zone_rooms.push((zone.clone(), room.to_string()));
            } else {
                eprintln!(
                    "backtest-warmth: zone '{zone}' has no room mapping — recorded heating \
                     unavailable for it"
                );
            }
        }
    }
    heated_zone_rooms.sort();

    let weather = heating_backtest::read_window(db, &heated_zone_rooms, warmup_start, read_stop)
        .await
        .context("reading weather/zone/relay window")?;

    let prices = read_prices_chunked(db, array_start, read_stop)
        .await
        .context("reading OTE prices")?;
    let pv = read_growatt_chunked(db, "InputPower", window_start, read_stop).await;
    let load = read_growatt_chunked(db, "INVPowerToLocalLoad", window_start, read_stop).await;
    let soc0 = read_soc_seed(db, window_start, config).await;

    let mut series = weather.series;
    series.insert(
        "price".to_string(),
        prices
            .iter()
            .map(|p| (p.time.to_rfc3339(), p.price_eur_mwh))
            .collect(),
    );
    series.insert("pv".to_string(), to_pairs(&pv));
    series.insert("load".to_string(), to_pairs(&load));
    series.insert("soc".to_string(), vec![(window_start.to_rfc3339(), soc0)]);

    Ok(ReplayDump {
        schema: SCHEMA.to_string(),
        series,
    })
}

/// The unpacked fixture, ready for the replay loop.
struct Inputs {
    drive_data: DriveData,
    zone_series: HashMap<String, Vec<TimeSample>>,
    relay_events: HashMap<String, Vec<TimeSample>>,
    relay_state_before: HashMap<String, TimeSample>,
    real_spot_fine: Vec<Option<f64>>,
    array_start: DateTime<Utc>,
    pv_kw: Vec<f64>,
    load_kw: Vec<f64>,
    soc0_kwh: f64,
}

fn unpack(
    dump: &ReplayDump,
    config: &ControlConfig,
    window_start: DateTime<Utc>,
    days: i64,
) -> Result<Inputs> {
    let hb_dump = as_heating_dump(dump);
    let parts = parts_from_dump(&hb_dump);
    ensure!(
        !parts.zone_series.is_empty(),
        "no measured zone series available (check --from file contents or InfluxDB zone mappings)"
    );
    let drive_data = assemble_drive_data(&parts.drive, config.site.ground_temperature_c, 0.3)
        .context("assembling drive data")?;

    let read_stop = window_start + Duration::hours(days * 24) + Duration::days(3);
    let array_start = window_start - Duration::days(28);
    let price_pairs = dump.series.get("price").cloned().unwrap_or_default();
    let price_samples: Vec<PriceSample> = price_pairs
        .iter()
        .filter_map(|(t, v)| {
            DateTime::parse_from_rfc3339(t).ok().map(|dt| PriceSample {
                time: dt.with_timezone(&Utc),
                price_eur_mwh: *v,
            })
        })
        .collect();
    let n_array = ((read_stop - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let real_spot_fine = align_blocks_15min(&price_samples, array_start, n_array)
        .unwrap_or_else(|| vec![None; n_array]);

    let core_start = ((window_start - array_start).num_seconds() / FINE_SECONDS_I) as usize;
    let core_end = ((window_start + Duration::hours(days * 24) - array_start).num_seconds()
        / FINE_SECONDS_I) as usize;
    let missing_core = real_spot_fine
        [core_start.min(real_spot_fine.len())..core_end.min(real_spot_fine.len())]
        .iter()
        .filter(|p| p.is_none())
        .count();
    ensure!(
        missing_core == 0,
        "backtest-warmth: {missing_core} OTE price block(s) missing in the core window — cannot \
         execute at real prices"
    );

    let pv_pairs = dump.series.get("pv").cloned().unwrap_or_default();
    let load_pairs = dump.series.get("load").cloned().unwrap_or_default();
    let pv_samples = from_pairs(&pv_pairs);
    let load_samples = from_pairs(&load_pairs);
    let n_meas = ((read_stop - window_start).num_seconds() / FINE_SECONDS_I) as usize;
    let pv_raw = crate::what_if::align_15min(&pv_samples, window_start, n_meas);
    let load_raw = crate::what_if::align_15min(&load_samples, window_start, n_meas);
    let (pv_kw, _) = forward_fill(
        pv_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );
    let (load_kw, _) = forward_fill(
        load_raw
            .into_iter()
            .map(|v| v.map(|w| (w / 1000.0).max(0.0)))
            .collect(),
    );

    let soc0_kwh = dump
        .series
        .get("soc")
        .and_then(|v| v.first())
        .map(|(_, v)| *v)
        .unwrap_or_else(|| config.battery.min_soc_pct / 100.0 * config.battery.capacity_kwh);

    Ok(Inputs {
        drive_data,
        zone_series: parts.zone_series,
        relay_events: parts.relay_events,
        relay_state_before: parts.relay_state_before,
        real_spot_fine,
        array_start,
        pv_kw,
        load_kw,
        soc0_kwh,
    })
}

// --- Seeding: measured state at start-48h, driven 48h forward with measured relay heating --------

fn seed_x0(
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    config: &ControlConfig,
    inputs: &Inputs,
    window_start: DateTime<Utc>,
) -> Result<DVector<f64>> {
    let warmup_start = window_start - Duration::hours(WARMUP_HOURS);
    let x0_seed = seed_from_series(net, ss, &inputs.zone_series, hour_key(warmup_start));

    let mut data = inputs.drive_data.clone();
    let first_hour = hour_key(warmup_start);
    let last_hour = hour_key(window_start);
    // Nearest covered hour AT OR AFTER `first_hour`/AT OR BEFORE `last_hour`, not an exact match:
    // the measured series is STOP-stamped (`heating_backtest`'s own doc), so the earliest grid hour
    // `assemble_drive_data` derives from it can land up to an hour after `warmup_start`'s own exact
    // hour key — `data.hours` is a contiguous `first..=last` range (no gaps), so the first/last
    // index satisfying the bound is always the nearest one.
    let start_idx = data
        .hours
        .iter()
        .position(|&h| h >= first_hour)
        .context("warm-up start hour not covered by the weather read")?;
    let end_idx = data
        .hours
        .iter()
        .rposition(|&h| h <= last_hour)
        .context("warm-up end hour not covered by the weather read")?;
    ensure!(
        start_idx <= end_idx,
        "warm-up window not covered by the weather read (start hour {first_hour} after end hour \
         {last_hour} in the available range)"
    );
    data.hours = data.hours[start_idx..=end_idx].to_vec();
    data.grid_times = data.grid_times[start_idx..=end_idx].to_vec();
    data.outside_c = data.outside_c[start_idx..=end_idx].to_vec();
    data.cloud = data.cloud[start_idx..=end_idx].to_vec();
    if !data.solar.is_empty() {
        data.solar = data.solar[start_idx..=end_idx].to_vec();
    }
    data.internal_gain_w = config.heating.internal_gains();
    data.local_offset = config.site.offset_at(warmup_start);
    data.scheduled_loads = config.scheduled_loads.clone();
    data.scheduled_w = config
        .scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
        .collect();
    data.sensor_power_w = vec![None; config.scheduled_loads.len()];

    let hours: Vec<i64> = data.hours.clone();
    let mut heating_kw: HashMap<String, Vec<f64>> = HashMap::new();
    for (zone, spec) in &config.heating.zones {
        if !net
            .marker_indices
            .contains_key(&(zone.clone(), HEATING_MARKER.to_string()))
        {
            continue;
        }
        let events: Vec<(DateTime<Utc>, f64)> = inputs
            .relay_events
            .get(zone)
            .map(|s| s.iter().map(|x| (x.time, x.value)).collect())
            .unwrap_or_default();
        let state_before = inputs
            .relay_state_before
            .get(zone)
            .map(|s| (s.time, s.value));
        let duty = relay_duty_hourly(&events, state_before, &hours);
        heating_kw.insert(
            zone.clone(),
            duty.iter().map(|d| d * spec.max_heat_kw).collect(),
        );
    }
    data.heating_kw = heating_kw;

    let trajectory = drive(net, ss, latitude, longitude, &x0_seed, &data);
    Ok(trajectory.last().cloned().unwrap_or(x0_seed))
}

// --- Per-tick SolveJob assembly, mirroring `app::current_plan` / `solve_timing::catch_up_job` ----

#[allow(clippy::too_many_arguments)]
fn build_tick_ctx(
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
    t: DateTime<Utc>,
    hp: &HourPrep,
    inputs: &Inputs,
    heating: &HeatingConfig,
    public_holidays: &[(u32, u32)],
    distribution_eur_by_local_hour: &[f64; 24],
    amortisation: f64,
    round_trip_eta: f64,
    battery_min_soc_kwh: f64,
) -> Result<ForecastContext> {
    let local_offset = config.site.offset_at(t);
    let n_fine = hp.grid.n_fine();

    let offset_idx = inputs
        .drive_data
        .hours
        .iter()
        .position(|&h| h == hour_key(t))
        .context("tick hour not covered by the weather read")?;
    let outlook_hours = config.horizon.outlook_hours;
    let want = n_fine.div_ceil(4) + 1 + outlook_hours;
    let avail = inputs.drive_data.hours.len() - offset_idx;
    let have = want.min(avail);
    let outside_hourly = &inputs.drive_data.outside_c[offset_idx..offset_idx + have];
    let cloud_hourly = &inputs.drive_data.cloud[offset_idx..offset_idx + have];
    let solar_hourly: &[crate::tools::sun::SolarInput] = if inputs.drive_data.solar.is_empty() {
        &[]
    } else {
        &inputs.drive_data.solar[offset_idx..offset_idx + have]
    };

    let mut temperature_c = hourly_to_blocks(t, outside_hourly);
    let mut cloud_cover = hourly_to_blocks(t, cloud_hourly);
    let mut solar = if solar_hourly.is_empty() {
        Vec::new()
    } else {
        hourly_solar_to_blocks(t, solar_hourly)
    };
    temperature_c.truncate(n_fine);
    cloud_cover.truncate(n_fine);
    if !solar.is_empty() {
        solar.truncate(n_fine);
    }
    ensure!(
        temperature_c.len() == n_fine && cloud_cover.len() == n_fine,
        "backtest-warmth: weather read does not cover tick {t}'s full horizon"
    );

    // Post-horizon weather outlook — same anchor `current_plan` uses (the true grid end), truncated
    // to what the measured read actually covers.
    let outlook_start = hp.grid.block_end(hp.grid.len() - 1);
    let outlook_offset = offset_idx + n_fine.div_ceil(4);
    let outlook_have =
        (inputs.drive_data.hours.len().saturating_sub(outlook_offset)).min(outlook_hours);
    let outlook = if outlook_have > 0 {
        let oh = &inputs.drive_data.outside_c[outlook_offset..outlook_offset + outlook_have];
        let oc = &inputs.drive_data.cloud[outlook_offset..outlook_offset + outlook_have];
        let os: &[crate::tools::sun::SolarInput] = if inputs.drive_data.solar.is_empty() {
            &[]
        } else {
            &inputs.drive_data.solar[outlook_offset..outlook_offset + outlook_have]
        };
        Some(Outlook {
            temperature_c: hourly_to_blocks(outlook_start, oh),
            cloud_cover: hourly_to_blocks(outlook_start, oc),
            solar: if os.is_empty() {
                Vec::new()
            } else {
                hourly_solar_to_blocks(outlook_start, os)
            },
        })
    } else {
        None
    };

    // OLD in-horizon-median terminal value, on the REAL (non-placeholder) import blocks when there
    // are enough of them — exactly `current_plan`'s own `terminal_basis` rule.
    let real_import_fine: Vec<f64> = hp
        .import_fine
        .iter()
        .zip(&hp.mask_fine)
        .filter(|(_, &m)| !m)
        .map(|(&p, _)| p)
        .collect();
    let basis: &[f64] = if real_import_fine.len() >= 16 {
        &real_import_fine
    } else {
        &hp.import_fine
    };
    let horizon_median_terminal_value = terminal_soc_value(basis, amortisation, round_trip_eta);
    let post_curve = day_type_median_curve(
        &hp.history,
        outlook_start,
        FINE_SECONDS,
        POST_HORIZON_BLOCKS,
        |t| config.site.offset_at(t),
        public_holidays,
        config.site.easter_holidays,
        distribution_eur_by_local_hour,
    );
    let (terminal_value, _source, _note) = select_terminal_value(
        false,
        &post_curve,
        horizon_median_terminal_value,
        amortisation,
        round_trip_eta,
    );

    Ok(ForecastContext {
        latitude,
        longitude,
        start: t,
        step_seconds: FINE_SECONDS,
        grid: hp.grid.clone(),
        local_offset,
        temperature_c,
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover,
        solar,
        internal_gain_w: heating.internal_gains(),
        solar_scale: HashMap::new(),
        scheduled_loads: config.scheduled_loads.clone(),
        load_run_hours: HashMap::new(),
        scheduled_w: config
            .scheduled_loads
            .iter()
            .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
            .collect(),
        import_price: hp.import_fine.clone(),
        export_price: hp.export_fine.clone(),
        export_allowed: hp.export_allowed_fine.clone(),
        inverter_on: hp.inverter_on_fine.clone(),
        battery_amortisation: amortisation,
        export_needs_pv: config.battery.export_needs_pv,
        min_dispatch_kw: config.battery.min_dispatch_kw,
        terminal_value,
        terminal_heat_basis: horizon_median_terminal_value,
        min_final_soc_kwh: Some(battery_min_soc_kwh),
        price_is_placeholder: hp.mask_fine.clone(),
        max_import_kw: config.grid.max_import_kw,
        max_export_kw: config.grid.max_export_kw,
        pv_kw_override: Some(hp.pv_fine.clone()),
        load_kw_override: Some(hp.load_fine.clone()),
        load_scale: 1.0,
        outlook,
        price_history: hp.history.clone(),
        public_holidays: public_holidays.to_vec(),
        easter_holidays: config.site.easter_holidays,
        distribution_eur_by_local_hour: *distribution_eur_by_local_hour,
    })
}

/// Measured house load (kW, fine lattice) minus the measured heating electricity
/// (`relay duty * max_heat_kw / cop`, clamped so a zone with no marker contributes 0), clamped
/// `>= 0` — otherwise winter heating is counted twice (once in the measured load, once as the LP's
/// own `heat/cop` decision). `heating_kw_by_zone` is the SAME per-zone thermal-kW series
/// [`seed_x0`] builds for the warm-up drive, sliced to the tick's fine window.
fn load_minus_heating(load_fine: &[f64], heating_electrical_fine: &[f64]) -> Vec<f64> {
    load_fine
        .iter()
        .zip(heating_electrical_fine)
        .map(|(&l, &h)| (l - h).max(0.0))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_job(
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    kernels: Arc<KernelSet>,
    ctx: ForecastContext,
    x0: DVector<f64>,
    heating: HeatingConfig,
    battery: crate::optimize::battery::BatterySpec,
) -> SolveJob {
    let pv = pv_arrays(&config.pv)
        .first()
        .copied()
        .unwrap_or_else(default_pv_array);
    let hvac = config.hvac.clone().unwrap_or_default();
    SolveJob {
        pv,
        consumption: ConsumptionModel::default(),
        battery,
        heating,
        hvac,
        ss: ss.clone(),
        net: net.clone(),
        ctx,
        x0,
        ev_specs: Vec::new(),
        ev_monitored: Vec::new(),
        committed: None,
        kernels: Some(kernels),
    }
}

// --- Public entry points ---------------------------------------------------------------------------

struct Args {
    start: DateTime<Utc>,
    days: i64,
    step_hours: i64,
    plant_gain: f64,
    model_path: String,
    config_path: String,
    out: Option<String>,
    dump: Option<String>,
    from: Option<String>,
    live: bool,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut start = None;
    let mut days = None;
    let mut step_hours = 1i64;
    let mut plant_gain = 1.0f64;
    let mut model_path = "model.json5".to_string();
    let mut config_path = "config.json5".to_string();
    let mut out = None;
    let mut dump = None;
    let mut from = None;
    let mut live = false;

    let mut i = 0;
    while i < args.len() {
        macro_rules! val {
            ($name:expr) => {{
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("{} needs a value", $name))?;
                i += 2;
                v
            }};
        }
        match args[i].as_str() {
            "--start" => {
                let v = val!("--start");
                start = Some(
                    DateTime::parse_from_rfc3339(v)
                        .with_context(|| format!("parsing --start {v}"))?
                        .with_timezone(&Utc),
                );
            }
            "--days" => days = Some(val!("--days").parse::<i64>().context("parsing --days")?),
            "--step-hours" => {
                step_hours = val!("--step-hours")
                    .parse::<i64>()
                    .context("parsing --step-hours")?
            }
            "--plant-gain" => {
                plant_gain = val!("--plant-gain")
                    .parse::<f64>()
                    .context("parsing --plant-gain")?
            }
            "--model" => model_path = val!("--model").clone(),
            "--config" => config_path = val!("--config").clone(),
            "--out" => out = Some(val!("--out").clone()),
            "--dump" => dump = Some(val!("--dump").clone()),
            "--from" => from = Some(val!("--from").clone()),
            "--live" => {
                live = true;
                i += 1;
            }
            other => bail!("backtest-warmth: unrecognized argument {other:?}"),
        }
    }

    if live {
        return Ok(Args {
            start: Utc::now(),
            days: 0,
            step_hours,
            plant_gain,
            model_path,
            config_path,
            out,
            dump,
            from,
            live,
        });
    }

    let start = start.ok_or_else(|| anyhow::anyhow!("backtest-warmth needs --start"))?;
    ensure!(
        start.minute() == 0 && start.second() == 0 && start.nanosecond() == 0,
        "--start must be hour-aligned (minute/second 0), got {start}"
    );
    let days = days.ok_or_else(|| anyhow::anyhow!("backtest-warmth needs --days"))?;
    ensure!(
        (1..=7).contains(&days),
        "--days must be in 1..=7 (got {days})"
    );
    ensure!(
        step_hours >= 1 && step_hours * 24 >= 1,
        "--step-hours must be >= 1"
    );
    ensure!(
        plant_gain.is_finite() && plant_gain > 0.0,
        "--plant-gain must be finite and positive"
    );

    Ok(Args {
        start,
        days,
        step_hours,
        plant_gain,
        model_path,
        config_path,
        out,
        dump,
        from,
        live,
    })
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ZoneArmScore {
    pub(crate) heat_kwh_pv_surplus: f64,
    pub(crate) heat_kwh_nt: f64,
    pub(crate) heat_kwh_vt: f64,
    pub(crate) kh_above_floor: f64,
    pub(crate) kh_below_floor: f64,
    pub(crate) kh_above_ceiling: f64,
    pub(crate) end_temp_c: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ArmScore {
    pub(crate) cost_eur: f64,
    pub(crate) end_soc_kwh: f64,
    pub(crate) zones: BTreeMap<String, ZoneArmScore>,
    pub(crate) solve_seconds: Vec<f64>,
    pub(crate) time_limited: usize,
    pub(crate) failed: usize,
    /// Largest |plant − plan| air temperature (K) over every executed block and zone — a
    /// self-check: ~0 at plant-gain 1 (the plant IS the model the plan predicted with), and the
    /// size of the derating at any other gain.
    pub(crate) plant_vs_plan_max_k: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TickRow {
    pub(crate) t: DateTime<Utc>,
    pub(crate) arm: &'static str,
    pub(crate) heat_kw: BTreeMap<String, f64>,
    pub(crate) import_kwh: f64,
    pub(crate) export_kwh: f64,
    pub(crate) cost_eur: f64,
    /// The plant's own air temperature (°C) per zone at the END of this tick's executed step —
    /// the ledger's "plant temps" (spec: per-tick ledger incl. plant temps).
    pub(crate) plant_temp_c: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) window_start: DateTime<Utc>,
    pub(crate) window_end: DateTime<Utc>,
    pub(crate) plant_gain: f64,
    pub(crate) old: ArmScore,
    pub(crate) new: ArmScore,
    /// Total within-horizon-equivalent NEW-arm warmth reward, `Σ_z w_z · kh_above_floor_new[z]`
    /// (informational — NOT the acceptance-bar term, which is incremental; see
    /// `sum_w_delta_kh_eur`'s doc).
    pub(crate) warmth_reward_eur: f64,
    pub(crate) delta_cost_eur: f64,
    /// The acceptance bar's own term (spec D8): `Σ_z w_z · (kh_above_floor_new[z] −
    /// kh_above_floor_old[z])`, `w` from the NEW config — the extra reward the OLD arm's own
    /// (non-priority) K·h would NOT have earned. `delta_cost_eur <= sum_w_delta_kh_eur` is the
    /// bar: the LP paid less than the owner's stated value for the extra warmth it bought.
    pub(crate) sum_w_delta_kh_eur: f64,
    /// `delta_cost_eur <= sum_w_delta_kh_eur` (within a tiny float tolerance).
    pub(crate) acceptance_bar_met: bool,
    pub(crate) ledger: Vec<TickRow>,
}

struct Plant {
    x_old: DVector<f64>,
    x_new: DVector<f64>,
}

/// Solve and execute ONE arm's ONE tick: build the `SolveJob` (shared `ctx`/`hp`, this arm's own
/// `heating` and starting state `x`), run `fix_and_round` with a 30 s budget, score the executed
/// blocks' cost/price-band heating kWh, and step the plant (`x`, in place) over the executed fine
/// blocks with this plan's own `heat_kw` — or, on a failed solve, with no heating and the battery
/// idle (brief's fallback), flagged in `score.failed`.
#[allow(clippy::too_many_arguments)]
fn run_arm_tick(
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    kernels: &Arc<KernelSet>,
    ctx: &ForecastContext,
    hp: &HourPrep,
    arm_name: &'static str,
    heating: &HeatingConfig,
    x: &mut DVector<f64>,
    score: &mut ArmScore,
    t: DateTime<Utc>,
    n_fine_exec: usize,
    u_known_exec: &[DVector<f64>],
    prev_end_soc: f64,
    battery_spec0: &crate::optimize::battery::BatterySpec,
    amortisation: f64,
    low_tariff_mask: &[bool; 24],
    plant_gain: f64,
) -> TickRow {
    let dt_h = FINE_SECONDS / 3600.0;
    let disc = &kernels.disc;

    let mut ctx_arm = ctx.clone();
    ctx_arm.min_final_soc_kwh = Some(battery_spec0.min_soc_kwh);
    let mut battery_arm = battery_spec0.clone();
    battery_arm.initial_soc_kwh =
        prev_end_soc.clamp(battery_spec0.min_soc_kwh, battery_spec0.max_soc_kwh);

    let job = build_job(
        config,
        net,
        ss,
        Arc::clone(kernels),
        ctx_arm,
        x.clone(),
        heating.clone(),
        battery_arm.clone(),
    );
    let budget = SolveBudget {
        time_limit_s: Some(30.0),
    };
    let salvage: Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>> =
        Arc::new(Mutex::new(None));
    let t0 = Instant::now();
    let solved = fix_and_round(&job, budget, &salvage);
    let elapsed = t0.elapsed().as_secs_f64();
    score.solve_seconds.push(elapsed);
    if elapsed >= budget.time_limit_s.unwrap_or(f64::INFINITY) * 0.95 {
        score.time_limited += 1;
    }

    let (import_kwh, export_kwh, cost_eur, first_heat) = match solved {
        Ok((plan, _grade)) => {
            let mut import_kwh = 0.0;
            let mut export_kwh = 0.0;
            let mut cost_eur = 0.0;
            for b in 0..n_fine_exec.min(plan.grid_import_kw.len()) {
                let imp = plan.grid_import_kw[b];
                let exp = plan.grid_export_kw.get(b).copied().unwrap_or(0.0);
                let dis = plan.discharge_kw.get(b).copied().unwrap_or(0.0);
                let pi = hp.import_fine.get(b).copied().unwrap_or(0.0);
                let pe = hp.export_fine.get(b).copied().unwrap_or(0.0);
                import_kwh += dt_h * imp;
                export_kwh += dt_h * exp;
                cost_eur += dt_h * (imp * pi - exp * pe) + amortisation * dt_h * dis;
                let hour_at = t + Duration::minutes(15 * b as i64);
                let local = hour_at.with_timezone(&config.site.offset_at(hour_at));
                let band = classify_band(imp, local.hour(), low_tariff_mask);
                for (zone, heat_series) in &plan.heat_kw {
                    let kw = heat_series.get(b).copied().unwrap_or(0.0);
                    if kw <= 0.0 {
                        continue;
                    }
                    let kwh = kw * dt_h;
                    let z = score.zones.entry(zone.clone()).or_default();
                    match band {
                        PriceBand::PvSurplus => z.heat_kwh_pv_surplus += kwh,
                        PriceBand::Nt => z.heat_kwh_nt += kwh,
                        PriceBand::Vt => z.heat_kwh_vt += kwh,
                    }
                }
            }
            let soc_end = plan
                .soc_kwh
                .get(
                    n_fine_exec
                        .saturating_sub(1)
                        .min(plan.soc_kwh.len().saturating_sub(1)),
                )
                .copied()
                .unwrap_or(battery_arm.initial_soc_kwh);
            score.end_soc_kwh = soc_end;

            // Step the PLANT through the executed blocks and score K·h above/below floor and
            // above ceiling from its OWN air temperatures (not the plan's prediction — the two
            // coincide only at plant-gain 1, which `plant_vs_plan_max_k` checks), against this
            // arm's zone bands (identical arm to arm; only the trajectory differs).
            for (f, u_known) in u_known_exec.iter().enumerate().take(n_fine_exec) {
                let mut heat_this_block: HashMap<String, f64> = HashMap::new();
                for (zone, heat_series) in &plan.heat_kw {
                    heat_this_block
                        .insert(zone.clone(), heat_series.get(f).copied().unwrap_or(0.0));
                }
                *x = step_plant(ss, net, disc, x, u_known, &heat_this_block, plant_gain);
                for (zone, zc) in &heating.zones {
                    let Some(row) = net.zone_indices.get(zone).and_then(|&n| ss.state_index(n))
                    else {
                        continue;
                    };
                    let plant_c = k_to_c(x[row]);
                    if let Some(plan_c) = plan.zone_temp_c.get(zone).and_then(|s| s.get(f)) {
                        score.plant_vs_plan_max_k =
                            score.plant_vs_plan_max_k.max((plant_c - plan_c).abs());
                    }
                    let s = score_kh(&[plant_c], &[dt_h], zc.t_min, zc.t_max);
                    let z = score.zones.entry(zone.clone()).or_default();
                    z.kh_above_floor += s.above_floor;
                    z.kh_below_floor += s.below_floor;
                    z.kh_above_ceiling += s.above_ceiling;
                }
            }
            let first: HashMap<String, f64> = plan
                .heat_kw
                .iter()
                .map(|(z, v)| (z.clone(), v.first().copied().unwrap_or(0.0)))
                .collect();
            (import_kwh, export_kwh, cost_eur, first)
        }
        Err(e) => {
            // No heating, battery idle (brief's fallback) — the plant still steps forward (free
            // response) so the NEXT tick's state isn't stale, but cost/import/export are reported
            // as 0 rather than reconstructed from a balance this backtest doesn't otherwise model
            // (see docs/api.md's "Warmth backtest" section); a failed solve is rare and flagged
            // (`score.failed`), not silently folded into the cost totals.
            score.failed += 1;
            eprintln!("[warmth-backtest] {arm_name} arm solve failed at {t}: {e:#}");
            for u_known in u_known_exec.iter().take(n_fine_exec) {
                *x = step_plant(ss, net, disc, x, u_known, &HashMap::new(), plant_gain);
            }
            score.end_soc_kwh = battery_arm.initial_soc_kwh;
            (0.0, 0.0, 0.0, HashMap::new())
        }
    };
    score.cost_eur += cost_eur;
    let plant_temp_c: BTreeMap<String, f64> = net
        .zone_indices
        .iter()
        .filter_map(|(zone, &node)| {
            ss.state_index(node)
                .map(|row| (zone.clone(), k_to_c(x[row])))
        })
        .collect();
    TickRow {
        t,
        arm: arm_name,
        heat_kw: first_heat.into_iter().collect(),
        import_kwh,
        export_kwh,
        cost_eur,
        plant_temp_c,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_window(
    db: Option<&SourceClients>,
    model_path: &str,
    config_path: &str,
    window_start: DateTime<Utc>,
    days: i64,
    step_hours: i64,
    plant_gain: f64,
    dump_path: Option<&str>,
    from_path: Option<&str>,
) -> Result<Report> {
    let model = Model::load(model_path).with_context(|| format!("loading --model {model_path}"))?;
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load(config_path)
        .with_context(|| format!("loading --config {config_path}"))?;

    let now = Utc::now();
    let window_end = window_start + Duration::hours(days * 24);
    ensure!(
        window_end <= floor_to_hour(now - Duration::hours(KNOWN_PRICE_LAG_H)),
        "backtest-warmth: window end {window_end} must be at least {KNOWN_PRICE_LAG_H} h before \
         now ({now}) for every block to be executable at REAL (published) OTE prices — pick an \
         earlier --start or fewer --days"
    );

    let dump = if let Some(path) = from_path {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading --from {path}"))?;
        let d: ReplayDump =
            serde_json::from_str(&text).with_context(|| format!("parsing --from {path}"))?;
        ensure!(
            d.schema == SCHEMA,
            "unexpected fixture schema {:?}",
            d.schema
        );
        d
    } else {
        let db = db.context("backtest-warmth: no InfluxDB client and no --from fixture")?;
        read_all(db, &config, &net, window_start, days).await?
    };
    if let Some(path) = dump_path {
        std::fs::write(path, serde_json::to_string_pretty(&dump)?)
            .with_context(|| format!("writing --dump {path}"))?;
        println!("backtest-warmth: dumped raw series to {path}");
    }

    let inputs = unpack(&dump, &config, window_start, days)?;
    println!(
        "backtest-warmth: window {window_start} .. {window_end} ({days} day(s), step {step_hours} \
         h), plant-gain {plant_gain}, internal gains from CONFIG only (no live fit)"
    );

    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);

    let x0 = seed_x0(
        &net,
        &ss,
        latitude,
        longitude,
        &config,
        &inputs,
        window_start,
    )
    .context("seeding + warm-up drive")?;

    let kernels = Arc::new(build_kernel_cache(&config, &net, &ss));

    let public_holidays: Vec<(u32, u32)> = config
        .site
        .public_holidays
        .iter()
        .filter_map(|md| parse_month_day(md))
        .collect();
    let distribution_eur_by_local_hour: [f64; 24] = {
        let mask = config.tariff.low_tariff_mask();
        std::array::from_fn(|h| config.tariff.distribution_eur(h as u32, &mask))
    };
    let low_tariff_mask = config.tariff.low_tariff_mask();
    let export_floor = config.tariff.czk_to_eur(config.tariff.export_price_min_czk);
    let inverter_off_price = config
        .tariff
        .czk_to_eur(config.tariff.inverter_off_price_czk);
    let amortisation = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);
    let battery_spec0 = battery_spec(&config.battery);
    let round_trip_eta = battery_spec0.charge_efficiency * battery_spec0.discharge_efficiency;

    let zeroed_heating = zeroed_warmth(&config.heating);
    let new_heating = config.heating.clone();
    let warmth_w: HashMap<String, f64> = new_heating
        .zones
        .iter()
        .map(|(z, zc)| (z.clone(), zc.warmth_value_eur_per_kh))
        .collect();

    let mut plant = Plant {
        x_old: x0.clone(),
        x_new: x0,
    };
    let mut old_score = ArmScore::default();
    let mut new_score = ArmScore::default();
    let mut ledger = Vec::new();

    let n_hours = days * 24;
    let mut h = 0i64;
    while h < n_hours {
        let t = window_start + Duration::hours(h);
        let exec_hours = step_hours.min(n_hours - h);

        let hp = prepare_hour(
            t,
            &config,
            inputs.array_start,
            &inputs.real_spot_fine,
            &inputs.pv_kw,
            &inputs.load_kw,
            window_start,
            DEFAULT_PUBLISH_HOUR,
            |t| config.site.offset_at(t),
            &public_holidays,
            export_floor,
            inverter_off_price,
        );

        // Measured heating electricity this tick's fine window, from the SAME per-hour duty the
        // warm-up drive used, so `load_kw_override` never double-counts the heating that already
        // happened.
        let hours_for_tick: Vec<i64> = (0..hp.grid.n_fine().div_ceil(4) + 1)
            .map(|i| hour_key(t) + i as i64)
            .collect();
        let mut heating_elec_hourly = vec![0.0; hours_for_tick.len()];
        for (zone, spec) in &config.heating.zones {
            if !net
                .marker_indices
                .contains_key(&(zone.clone(), HEATING_MARKER.to_string()))
            {
                continue;
            }
            let events: Vec<(DateTime<Utc>, f64)> = inputs
                .relay_events
                .get(zone)
                .map(|s| s.iter().map(|x| (x.time, x.value)).collect())
                .unwrap_or_default();
            let state_before = inputs
                .relay_state_before
                .get(zone)
                .map(|s| (s.time, s.value));
            let duty = relay_duty_hourly(&events, state_before, &hours_for_tick);
            for (i, d) in duty.iter().enumerate() {
                heating_elec_hourly[i] += d * spec.max_heat_kw / config.heating.cop.max(1e-6);
            }
        }
        let heating_elec_fine = hourly_to_blocks(t, &heating_elec_hourly)
            .into_iter()
            .take(hp.grid.n_fine())
            .collect::<Vec<_>>();
        let load_fine = load_minus_heating(&hp.load_fine, &heating_elec_fine);
        let mut hp = hp;
        hp.load_fine = load_fine;

        let ctx = build_tick_ctx(
            &config,
            latitude,
            longitude,
            t,
            &hp,
            &inputs,
            &new_heating,
            &public_holidays,
            &distribution_eur_by_local_hour,
            amortisation,
            round_trip_eta,
            battery_spec0.min_soc_kwh,
        )
        .with_context(|| format!("building tick context at {t}"))?;

        let n_fine_exec = (exec_hours * 4) as usize;
        let u_known_exec = known_thermal_inputs(&ss, &net, &ctx, n_fine_exec);

        for (arm_name, heating, x, score) in [
            ("OLD", &zeroed_heating, &mut plant.x_old, &mut old_score),
            ("NEW", &new_heating, &mut plant.x_new, &mut new_score),
        ] {
            let prev_end_soc = if h == 0 {
                inputs.soc0_kwh
            } else {
                score.end_soc_kwh
            };
            let row = run_arm_tick(
                &config,
                &net,
                &ss,
                &kernels,
                &ctx,
                &hp,
                arm_name,
                heating,
                x,
                score,
                t,
                n_fine_exec,
                &u_known_exec,
                prev_end_soc,
                &battery_spec0,
                amortisation,
                &low_tariff_mask,
                plant_gain,
            );
            ledger.push(row);
        }

        h += exec_hours;
    }

    // Final per-zone end temperature, from the plant's own trajectory end state (K·h above/below
    // floor and above ceiling were already accumulated per-tick in `run_arm_tick`).
    for (name, heating) in [("OLD", &zeroed_heating), ("NEW", &new_heating)] {
        let score = if name == "OLD" {
            &mut old_score
        } else {
            &mut new_score
        };
        let x = if name == "OLD" {
            &plant.x_old
        } else {
            &plant.x_new
        };
        for zone in heating.zones.keys() {
            let Some(&node) = net.zone_indices.get(zone) else {
                continue;
            };
            let Some(row) = ss.state_index(node) else {
                continue;
            };
            let z = score.zones.entry(zone.clone()).or_default();
            z.end_temp_c = k_to_c(x[row]);
        }
    }

    let warmth_reward_eur: f64 = warmth_w
        .iter()
        .map(|(z, w)| {
            let new_kh = new_score
                .zones
                .get(z)
                .map(|s| s.kh_above_floor)
                .unwrap_or(0.0);
            w * new_kh
        })
        .sum();
    // Spec D8: the acceptance bar is the INCREMENTAL reward (NEW minus OLD K·h above floor),
    // `w` from the NEW config — only zones with a configured `warmth_value_eur_per_kh` count.
    let sum_w_delta_kh_eur: f64 = warmth_w
        .iter()
        .filter(|(_, &w)| w > 0.0)
        .map(|(z, w)| {
            let new_kh = new_score
                .zones
                .get(z)
                .map(|s| s.kh_above_floor)
                .unwrap_or(0.0);
            let old_kh = old_score
                .zones
                .get(z)
                .map(|s| s.kh_above_floor)
                .unwrap_or(0.0);
            w * (new_kh - old_kh)
        })
        .sum();

    let delta_cost_eur = new_score.cost_eur - old_score.cost_eur;
    let acceptance_bar_met = delta_cost_eur <= sum_w_delta_kh_eur + 1e-6;
    Ok(Report {
        window_start,
        window_end,
        plant_gain,
        old: old_score,
        new: new_score,
        warmth_reward_eur,
        sum_w_delta_kh_eur,
        acceptance_bar_met,
        delta_cost_eur,
        ledger,
    })
}

fn print_report(r: &Report) {
    println!(
        "\nbacktest-warmth: {} .. {} (plant-gain {})",
        r.window_start, r.window_end, r.plant_gain
    );
    println!(
        "  {:<6}{:>10}{:>10}{:>14}{:>10}{:>10}{:>16}",
        "arm", "cost EUR", "end SoC", "mean solve s", "t-limit", "failed", "plant-plan maxK"
    );
    for (label, s) in [("OLD", &r.old), ("NEW", &r.new)] {
        let mean = if s.solve_seconds.is_empty() {
            0.0
        } else {
            s.solve_seconds.iter().sum::<f64>() / s.solve_seconds.len() as f64
        };
        println!(
            "  {:<6}{:>10.3}{:>10.2}{:>14.2}{:>10}{:>10}{:>16.3}",
            label, s.cost_eur, s.end_soc_kwh, mean, s.time_limited, s.failed, s.plant_vs_plan_max_k
        );
    }
    let delta_cost = r.new.cost_eur - r.old.cost_eur;
    println!(
        "  delta cost (NEW - OLD): {delta_cost:.3} EUR; warmth reward (NEW, within-horizon): \
         {:.3} EUR",
        r.warmth_reward_eur
    );
    println!(
        "  acceptance bar (spec D8): delta_cost {delta_cost:.3} EUR <= sum w*delta(K·h above \
         floor) {:.3} EUR => {}",
        r.sum_w_delta_kh_eur,
        if r.acceptance_bar_met { "PASS" } else { "FAIL" }
    );
    println!(
        "\n  {:<16}{:>12}{:>10}{:>10}{:>12}{:>12}{:>12}{:>10}",
        "zone", "arm", "pv kWh", "NT kWh", "VT kWh", "K·h>floor", "K·h<floor", "end C"
    );
    let mut zones: Vec<&String> = r.new.zones.keys().chain(r.old.zones.keys()).collect();
    zones.sort();
    zones.dedup();
    for zone in zones {
        for (label, s) in [("OLD", &r.old), ("NEW", &r.new)] {
            let z = s.zones.get(zone).cloned().unwrap_or_default();
            println!(
                "  {:<16}{:>12}{:>10.2}{:>10.2}{:>12.2}{:>12.2}{:>12.2}{:>10.2}",
                zone,
                label,
                z.heat_kwh_pv_surplus,
                z.heat_kwh_nt,
                z.heat_kwh_vt,
                z.kh_above_floor,
                z.kh_below_floor,
                z.end_temp_c
            );
        }
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    if parsed.live {
        return run_live(&parsed.config_path).await;
    }

    let db = if parsed.from.is_some() {
        None
    } else {
        Some(SourceClients::with_signals(
            InfluxDB::from_config(&parsed.config_path)?,
            ControlConfig::load(&parsed.config_path)?
                .data_sources
                .clone(),
        ))
    };

    let report = run_window(
        db.as_ref(),
        &parsed.model_path,
        &parsed.config_path,
        parsed.start,
        parsed.days,
        parsed.step_hours,
        parsed.plant_gain,
        parsed.dump.as_deref(),
        parsed.from.as_deref(),
    )
    .await?;

    print_report(&report);
    if let Some(path) = &parsed.out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing --out {path}"))?;
        println!("backtest-warmth: wrote {path}");
    }
    Ok(())
}

async fn run_live(config_path: &str) -> Result<()> {
    let config = ControlConfig::load(config_path)?;
    let model = Model::load("model.json5")?;
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let db = SourceClients::with_signals(
        InfluxDB::from_config(config_path)?,
        config.data_sources.clone(),
    );
    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);

    let zeroed = zeroed_warmth(&config.heating);
    let mut old_config = config.clone();
    old_config.heating = zeroed;

    let old_plan = current_plan(
        &db,
        &net,
        &ss,
        &old_config,
        latitude,
        longitude,
        PlanExtras {
            cache: None,
            committed_heat: None,
            kernels: None,
            loop_caller: false,
            kalman: None,
            load_run_hours: HashMap::new(),
            replay_inputs: false,
            legacy_terminal_value: false,
        },
    )
    .await
    .context("OLD plan (current_plan, zeroed warmth)")?;
    let new_plan = current_plan(
        &db,
        &net,
        &ss,
        &config,
        latitude,
        longitude,
        PlanExtras {
            cache: None,
            committed_heat: None,
            kernels: None,
            loop_caller: false,
            kalman: None,
            load_run_hours: HashMap::new(),
            replay_inputs: false,
            legacy_terminal_value: false,
        },
    )
    .await
    .context("NEW plan (current_plan, config as written)")?;

    println!("backtest-warmth --live: OLD (zeroed warmth) vs NEW (config as written)");
    println!(
        "  cost EUR: OLD {:.3} / NEW {:.3} (delta {:+.3}); warmth reward (NEW): {:.3} EUR",
        old_plan.total_cost_eur,
        new_plan.total_cost_eur,
        new_plan.total_cost_eur - old_plan.total_cost_eur,
        new_plan.warmth_reward_eur
    );
    println!(
        "  {:<16}{:>12}{:>16}{:>20}",
        "zone", "warmth K·h", "break-even EUR/kWh", "warmth reward EUR"
    );
    let mut zones: Vec<&String> = new_plan.warmth_kh.keys().collect();
    zones.sort();
    for zone in &zones {
        let kh = new_plan.warmth_kh.get(*zone).copied().unwrap_or(0.0);
        let be = new_plan
            .warmth_break_even_eur_per_kwh
            .get(*zone)
            .copied()
            .unwrap_or(0.0);
        let w = config
            .heating
            .zones
            .get(*zone)
            .map(|z| z.warmth_value_eur_per_kh)
            .unwrap_or(0.0);
        println!("  {zone:<16}{kh:>12.3}{be:>16.4}{:>20.3}", w * kh);
    }

    // Per zone: heat kWh split by price band (this plan's own `grid_import_kw` per block for the
    // band) and the min/max temperature reached over the horizon — spec proof (a)'s remaining two
    // numbers.
    let low_tariff_mask = config.tariff.low_tariff_mask();
    let mut heat_kwh_band: HashMap<&str, [f64; 3]> = HashMap::new();
    for block in &new_plan.timeline {
        let dt_h = block.dt_minutes as f64 / 60.0;
        let local = block.t.with_timezone(&config.site.offset_at(block.t));
        let band = classify_band(block.grid_import_kw, local.hour(), &low_tariff_mask);
        for (zone, &kw) in &block.heat_kw {
            if kw <= 0.0 {
                continue;
            }
            let slot = heat_kwh_band.entry(zone.as_str()).or_default();
            match band {
                PriceBand::PvSurplus => slot[0] += kw * dt_h,
                PriceBand::Nt => slot[1] += kw * dt_h,
                PriceBand::Vt => slot[2] += kw * dt_h,
            }
        }
    }
    println!(
        "\n  {:<16}{:>10}{:>10}{:>10}{:>10}{:>10}",
        "zone", "pv kWh", "NT kWh", "VT kWh", "min C", "max C"
    );
    for zone in &zones {
        let band = heat_kwh_band
            .get(zone.as_str())
            .copied()
            .unwrap_or_default();
        let (mut min_c, mut max_c) = (f64::INFINITY, f64::NEG_INFINITY);
        for block in &new_plan.timeline {
            if let Some(&c) = block.temp_c.get(*zone) {
                min_c = min_c.min(c);
                max_c = max_c.max(c);
            }
        }
        println!(
            "  {zone:<16}{:>10.2}{:>10.2}{:>10.2}{:>10.2}{:>10.2}",
            band[0], band[1], band[2], min_c, max_c
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimize::grid::BlockGrid;
    use crate::optimize::thermal::build_context;
    use uom::si::f64::ThermodynamicTemperature;
    use uom::si::thermodynamic_temperature::{degree_celsius, kelvin};

    fn utc_test(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// One realistic insulated zone with an underfloor-heating slab (same shape as
    /// `unified.rs`'s `thermal_for_inner` test model) — returns `(net, ss, ThermalContext)` built
    /// from the SAME `net`/`ss` so [`step_plant`] and `ThermalContext::predict` can be
    /// cross-checked against each other rather than two independently-built models.
    fn small_model(
        outside_c: f64,
        ground_c: f64,
        x0_c: f64,
        n: usize,
    ) -> (
        RcNetwork,
        StateSpace,
        crate::optimize::thermal::ThermalContext,
    ) {
        let model = crate::model::Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [
                        { material: "concrete", thickness: 0.05 },
                        { marker: "heating" },
                        { material: "concrete", thickness: 0.05 },
                    ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { a: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["a", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["a", "outside"], area: 25 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(outside_c),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(ground_c),
        );
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(x0_c).get::<kelvin>(),
        );
        let grid = BlockGrid::uniform(utc_test("2024-01-15T00:00:00Z"), n, FINE_SECONDS);
        let ctx = build_context(&ss, &net, &x0, &vec![u0; n], &grid, &[], &[], &[], None).unwrap();
        (net, ss, ctx)
    }

    #[test]
    fn plant_step_matches_thermal_context_predict_and_scales_with_gain() {
        let n = 4;
        let (net, ss, ctx) = small_model(5.0, 10.0, 18.0, n);
        let disc = ss.discretize(FINE_SECONDS);
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(18.0).get::<kelvin>(),
        );
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(5.0),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(10.0),
        );

        let heat_kw = HashMap::from([("a".to_string(), 1.0)]);

        // Gain 1.0: step the plant every fine step, compare the final air temp to
        // `ThermalContext::predict` with a constant 1 kW schedule over all `n` blocks (a uniform
        // grid, so block == fine step here).
        let mut x_gain1 = x0.clone();
        for _ in 0..n {
            x_gain1 = step_plant(&ss, &net, &disc, &x_gain1, &u0, &heat_kw, 1.0);
        }
        let heat_schedule = HashMap::from([("a".to_string(), vec![1.0; n])]);
        let predicted_k = ctx.predict("a", n, &heat_schedule, &HashMap::new(), &HashMap::new());
        let row = ss.state_index(net.zone_indices["a"]).unwrap();
        assert!(
            (x_gain1[row] - predicted_k).abs() < 1e-6,
            "plant gain 1.0 ({}) must agree with ThermalContext::predict ({predicted_k})",
            x_gain1[row]
        );

        // Gain 0.5 halves the RESPONSE (predicted - free response with no heat at all).
        let mut x_gain_half = x0.clone();
        for _ in 0..n {
            x_gain_half = step_plant(&ss, &net, &disc, &x_gain_half, &u0, &heat_kw, 0.5);
        }
        let mut x_noheat = x0.clone();
        for _ in 0..n {
            x_noheat = step_plant(&ss, &net, &disc, &x_noheat, &u0, &HashMap::new(), 1.0);
        }
        let response_full = x_gain1[row] - x_noheat[row];
        let response_half = x_gain_half[row] - x_noheat[row];
        assert!(
            (response_half - response_full * 0.5).abs() < 1e-6,
            "gain 0.5 must halve the response: full={response_full} half={response_half}"
        );
    }

    #[test]
    fn classify_band_pv_surplus_below_dust() {
        let mask = [true; 24];
        assert_eq!(classify_band(0.0, 10, &mask), PriceBand::PvSurplus);
        assert_eq!(classify_band(0.01, 10, &mask), PriceBand::PvSurplus);
    }

    #[test]
    fn classify_band_nt_vs_vt_by_mask() {
        let mut mask = [false; 24];
        mask[3] = true;
        assert_eq!(classify_band(1.0, 3, &mask), PriceBand::Nt);
        assert_eq!(classify_band(1.0, 4, &mask), PriceBand::Vt);
    }

    #[test]
    fn score_kh_above_floor_capped_at_ceiling() {
        // 4 steps of 1h each: 19 (below floor 20), 21 (in band), 23 (above ceiling 22, capped at 22
        // for the above-floor credit), 20 (exactly at floor).
        let temp = [19.0, 21.0, 23.0, 20.0];
        let dt = [1.0, 1.0, 1.0, 1.0];
        let s = score_kh(&temp, &dt, 20.0, 22.0);
        // above_floor: 0 + 1 + 2(capped) + 0 = 3
        assert!((s.above_floor - 3.0).abs() < 1e-9, "{s:?}");
        // below_floor: 1 + 0 + 0 + 0 = 1
        assert!((s.below_floor - 1.0).abs() < 1e-9, "{s:?}");
        // above_ceiling: 0 + 0 + 1 + 0 = 1
        assert!((s.above_ceiling - 1.0).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn score_kh_empty_series_is_zero() {
        let s = score_kh(&[], &[], 18.0, 22.0);
        assert_eq!(s, KhScore::default());
    }

    fn zone(t_min: f64, t_max: f64, warmth: f64) -> crate::optimize::config::ZoneComfort {
        crate::optimize::config::ZoneComfort {
            max_heat_kw: 1.0,
            t_min,
            t_max,
            internal_gain_w: 0.0,
            windows: Vec::new(),
            overheat_c: 0.0,
            warmth_value_eur_per_kh: warmth,
        }
    }

    fn heating_cfg(zones: HashMap<String, crate::optimize::config::ZoneComfort>) -> HeatingConfig {
        HeatingConfig {
            cop: 1.0,
            comfort_penalty: 100.0,
            overheat_penalty: 1.0,
            zones,
            gain_groups: Vec::new(),
            extra_gain_zones: Vec::new(),
            coupling_min_k: 0.0,
            relay_duty: Default::default(),
        }
    }

    #[test]
    fn zeroed_warmth_zeroes_every_zone_and_nothing_else() {
        let heating = heating_cfg(HashMap::from([
            ("a".to_string(), zone(18.0, 22.0, 0.05)),
            ("b".to_string(), zone(19.0, 24.0, 0.1)),
        ]));
        let zeroed = zeroed_warmth(&heating);
        for (z, zc) in &zeroed.zones {
            assert_eq!(zc.warmth_value_eur_per_kh, 0.0, "{z}");
            let orig = &heating.zones[z];
            assert_eq!(zc.t_min, orig.t_min);
            assert_eq!(zc.t_max, orig.t_max);
            assert_eq!(zc.max_heat_kw, orig.max_heat_kw);
        }
        assert_eq!(zeroed.zones.len(), heating.zones.len());
    }

    #[test]
    fn zeroed_warmth_is_a_no_op_when_already_all_zero() {
        let heating = heating_cfg(HashMap::from([("a".to_string(), zone(18.0, 22.0, 0.0))]));
        let zeroed = zeroed_warmth(&heating);
        assert_eq!(zeroed.zones["a"].warmth_value_eur_per_kh, 0.0);
    }

    #[test]
    fn load_minus_heating_clamps_at_zero() {
        let load = [1.0, 0.5, 2.0];
        let heat = [0.3, 1.0, 0.5];
        let out = load_minus_heating(&load, &heat);
        assert!((out[0] - 0.7).abs() < 1e-9);
        assert_eq!(out[1], 0.0); // would be negative, clamped
        assert!((out[2] - 1.5).abs() < 1e-9);
    }

    #[test]
    fn fixture_round_trips_through_json() {
        let dump = ReplayDump {
            schema: SCHEMA.to_string(),
            series: HashMap::from([(
                "outside".to_string(),
                vec![("2026-01-01T00:00:00Z".to_string(), 5.0)],
            )]),
        };
        let text = serde_json::to_string(&dump).unwrap();
        let back: ReplayDump = serde_json::from_str(&text).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(back.series["outside"], dump.series["outside"]);
    }
}
