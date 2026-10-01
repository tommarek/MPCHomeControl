//! `backtest-heating --start <rfc3339, hour-aligned> --days <1..=7> [--warmup-h 48] [--model
//! <path>] [--config <path>] [--out <json>] [--dump <fixture>] [--from <fixture>] [--legacy-duty]
//! [--on-duty 0.7] [--min-on-h 2] [--off-h 4] [--off-duty 0.1]` — validate the condensed heating
//! kernels (`optimize::thermal::build_kernels`) against one bounded winter window of measured
//! data: last winter's relays, outside temperature and solar drove the real house: did the model
//! get the gain (K per kWh) and the speed (lag) right?
//!
//! **The on-change relay bug this tool exists to quantify.** The heating relays
//! (`loxone`/`relay`/`tag1=heating`) log only on a state CHANGE — a handful of points per room per
//! day — so an `aggregateWindow(1h, mean, createEmpty: false)` zero-fills any hour with no event,
//! even one the relay spent fully ON (confirmed on real data: 2026-01-10 the livingroom was ON
//! 04:15→06:30; the 05–06 h hour, with no event in it, reads 0, while 02–03 h reads 0.5 off a
//! single 1-second blip — 2.90 true ON-hours that day vs 1.67 "measured"). This tool reads the RAW
//! events and reconstructs the true time-weighted duty (`crate::relay_duty::relay_duty_hourly`);
//! `--legacy-duty` replays the old zero-fill semantics on the SAME raw events so the two can be
//! scored side by side. `validate::read_heating_kw` now uses the same event-based duty live by
//! default (`heating.relay_duty: "events"`), with `"legacy"` as a config revert — see
//! `crate::relay_duty`.
//!
//! **Bounded reads.** Every InfluxDB read goes through [`crate::solar_scale_backtest::chunk_windows`]
//! (≤7-day chunks, one series at a time, paused between chunks) exactly like `backtest-kalman-solar`;
//! `--from <file>` skips InfluxDB entirely (no token needed) and `--dump <file>` saves the raw read so
//! a later run can replay it. Fixture schema `heating-backtest-v1` ([`ReplayDump`]).
//!
//! **What it reports** (stdout tables; `--out` adds the same data as JSON): the active-backtest
//! RMSE/bias per zone, split into all / heating-on / after-pulse (≤6 h after the last on-hour) /
//! other hours, pre- (config) and post- (per-window NNLS fit, [`crate::validate::fit_gains`])
//! gains — the fit itself keeps `fit_gains`' 3600 s end-of-hour drive, the live loop's own; only the
//! scoring and the kernel check below use the hour-mean drive); a whole-window least-squares **kernel check** (own gain `k̂` and speed `ŝ` per heated
//! zone, against the model's own impulse response); an **episode table** (raw / model-drift- /
//! pre-trend-corrected measured response vs modelled, at 1/3/6/12 h leads); the **lag** (time to
//! peak / 63 %) of the event-averaged per-kWh response curve; and **catch-up** minutes (modelled,
//! measured-implied, and direct-from-episodes) to +1 K at `max_heat_kw`. See `docs/api.md`
//! "Winter heating backtest" for how to run this each winter and read the output.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Timelike, Utc};
use nalgebra::{DMatrix, DVector};
use serde::{Deserialize, Serialize};
use uom::si::{angle::degree, f64::Angle, heat_flux_density::watt_per_square_meter};

use crate::estimate::{assemble_drive_data, build_input, hour_key, DriveData, DriveSeries};
use crate::export_audit::INTER_DAY_PAUSE_S;
use crate::influxdb::{InfluxDB, TimeSample};
use crate::model::Model;
use crate::optimize::config::ControlConfig;
use crate::optimize::thermal::build_kernels;
use crate::rc_network::RcNetwork;
use crate::relay_duty::{legacy_duty_hourly, relay_duty_hourly};
use crate::solar_scale_backtest::{chunk_windows, seed_from_series};
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::sun::tilted_irradiance;
use crate::validate::fit_gains;

/// Fine-lattice step (s) the kernels are built on — matches the live LP's grid (`unified.rs`).
const KERNEL_DT_S: f64 = 900.0;
/// Minimum kernel horizon in fine steps (192 × 15 min = 48 h). `run()` builds the kernel to cover
/// the WHOLE read window instead (`4 × hours.len()`, floored at this), not a fixed 48 h — a real
/// slab kernel has not decayed by 48 h (rework-1 C3: livingroom's `g[191]/peak` was 0.24), so a
/// kernel shorter than the window both truncates genuine memory and — via `interp_kernel`'s flat
/// extrapolation past its end — distorts `rescale_kernel`'s speed profiling at `s ≠ 1`.
const MIN_KERNEL_N: usize = 192;
/// Speed-profile grid for the whole-window least-squares fit (research.md's estimator B).
const S_GRID: [f64; 16] = [
    0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9, 2.0,
];
const LEADS_H: [i64; 4] = [1, 3, 6, 12];
/// GHI above which a lead's episode response is flagged as solar-confounded (research.md).
const GHI_FLAG_WM2: f64 = 50.0;
/// Below this own kWh in the scored window, the whole-window fit can't see enough signal.
const MIN_OWN_KWH: f64 = 5.0;
/// Above this variance-inflation factor, the heating regressor is too collinear with the
/// per-day nuisance terms to trust.
const MAX_VIF: f64 = 10.0;
/// Heating-on threshold for the backtest-table score split (brief item 2 — distinct from the
/// (tunable) episode-detection `--on-duty`, which is about whole ON STREAKS, not single hours).
const SCORE_ON_DUTY: f64 = 0.5;

// --- The dumped/loaded raw-series fixture ---------------------------------------------------------

const SCHEMA: &str = "heating-backtest-v1";

/// The `--dump`/`--from` fixture: raw per-source series, keyed by source. Pure (de)serialization
/// only, so a `--from` replay needs no InfluxDB token. Same shape as
/// [`crate::solar_scale_backtest::ReplayDump`], different schema tag (relay keys are zone-keyed
/// raw events here, not an hourly duty mean).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayDump {
    pub schema: String,
    /// `series[key] = [(rfc3339 timestamp, value), …]`, oldest first within each key. Keys:
    /// `outside`, `weather:{cloudcover,direct_radiation,diffuse_radiation,shortwave_radiation}`,
    /// `zone:<zone>` (every mapped zone except `outside`), `relay_events:<zone>` (raw on-change
    /// events, every heated zone), `relay_state_before:<zone>` (zero-or-one sample: the last event
    /// before the read window started).
    pub series: HashMap<String, Vec<(String, f64)>>,
}

fn to_pairs(series: &[TimeSample]) -> Vec<(String, f64)> {
    series
        .iter()
        .map(|s| (s.time.to_rfc3339(), s.value))
        .collect()
}

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

fn zone_key(zone: &str) -> String {
    format!("zone:{zone}")
}
fn weather_key(field: &str) -> String {
    format!("weather:{field}")
}
fn relay_events_key(zone: &str) -> String {
    format!("relay_events:{zone}")
}
fn relay_state_before_key(zone: &str) -> String {
    format!("relay_state_before:{zone}")
}

/// The unpacked fixture: raw outside/weather series, per-zone measured temperature series,
/// per-zone raw relay events, and per-zone last-event-before-window-start (absent ⇒ unknown).
struct DumpParts {
    drive: DriveSeries,
    zone_series: HashMap<String, Vec<TimeSample>>,
    relay_events: HashMap<String, Vec<TimeSample>>,
    relay_state_before: HashMap<String, TimeSample>,
}

fn parts_from_dump(dump: &ReplayDump) -> DumpParts {
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
    let mut relay_events = HashMap::new();
    let mut relay_state_before = HashMap::new();
    for key in dump.series.keys() {
        if let Some(zone) = key.strip_prefix("zone:") {
            zone_series.insert(zone.to_string(), get(key));
        } else if let Some(zone) = key.strip_prefix("relay_events:") {
            relay_events.insert(zone.to_string(), get(key));
        } else if let Some(zone) = key.strip_prefix("relay_state_before:") {
            if let Some(s) = get(key).into_iter().next() {
                relay_state_before.insert(zone.to_string(), s);
            }
        }
    }
    DumpParts {
        drive,
        zone_series,
        relay_events,
        relay_state_before,
    }
}

fn dump_from_parts(parts: &DumpParts) -> ReplayDump {
    let mut series = HashMap::new();
    series.insert("outside".to_string(), to_pairs(&parts.drive.outside));
    series.insert(weather_key("cloudcover"), to_pairs(&parts.drive.cloud));
    series.insert(
        weather_key("direct_radiation"),
        to_pairs(&parts.drive.direct),
    );
    series.insert(
        weather_key("diffuse_radiation"),
        to_pairs(&parts.drive.diffuse),
    );
    series.insert(
        weather_key("shortwave_radiation"),
        to_pairs(&parts.drive.shortwave),
    );
    for (zone, s) in &parts.zone_series {
        series.insert(zone_key(zone), to_pairs(s));
    }
    for (zone, s) in &parts.relay_events {
        series.insert(relay_events_key(zone), to_pairs(s));
    }
    for (zone, s) in &parts.relay_state_before {
        series.insert(
            relay_state_before_key(zone),
            to_pairs(std::slice::from_ref(s)),
        );
    }
    ReplayDump {
        schema: SCHEMA.to_string(),
        series,
    }
}

// --- IO: bounded reads -------------------------------------------------------------------------

/// Read every raw series the replay needs over `[read_start, read_stop)`, in ≤7-day chunks, one
/// series at a time, paused between chunks (mirrors
/// [`crate::solar_scale_backtest::read_window`]). `heated_zone_rooms` is `(zone, room)` for every
/// zone with a `"heating"` marker and a room mapping — relays are read once per chunk for ALL
/// rooms in a single call ([`SourceClients::heating_relay_events`]) and split by room here; this is
/// intentional (not a "one series at a time" violation) — the relay log is ONE InfluxDB
/// measurement with a field per room, so one query already reads it at the narrowest grain
/// available, same as reading one multi-field measurement for any other single series.
async fn read_window(
    db: &SourceClients,
    heated_zone_rooms: &[(String, String)],
    read_start: DateTime<Utc>,
    read_stop: DateTime<Utc>,
) -> Result<ReplayDump> {
    let windows = chunk_windows(read_start, read_stop);
    let n = windows.len();
    let zones: Vec<String> = db
        .mapped_zones()
        .into_iter()
        .filter(|z| z != "outside")
        .collect();

    let mut drive = DriveSeries::default();
    let mut zone_series: HashMap<String, Vec<TimeSample>> = HashMap::new();
    let mut relay_events: HashMap<String, Vec<TimeSample>> = HashMap::new();

    for (i, (s, e)) in windows.into_iter().enumerate() {
        let (s3, e3) = (s.to_rfc3339(), e.to_rfc3339());
        drive.outside.extend(
            db.read_zone_temperature_series("outside", &s3, &e3, "1h")
                .await
                .context("reading outside temperature series")?,
        );
        drive.cloud.extend(
            db.weather_cloud_series(&s3, &e3, "1h")
                .await
                .unwrap_or_default(),
        );
        drive.direct.extend(
            db.weather_radiation_series(crate::source::RadiationField::Direct, &s3, &e3, "1h")
                .await
                .unwrap_or_default(),
        );
        drive.diffuse.extend(
            db.weather_radiation_series(crate::source::RadiationField::Diffuse, &s3, &e3, "1h")
                .await
                .unwrap_or_default(),
        );
        drive.shortwave.extend(
            db.weather_radiation_series(crate::source::RadiationField::Shortwave, &s3, &e3, "1h")
                .await
                .unwrap_or_default(),
        );
        // The solar chain needs radiation one hour PAST the window end (`read_drive_data`'s
        // `rad_stop`), but a chunk can already be a full 7 days (`chunk_windows`), so extending
        // ITS OWN query by an hour would break the ≤7-day bound. Read the extra hour as its own
        // tiny separate query instead.
        if i + 1 == n {
            let extra_start = e3.clone();
            let extra_stop = (e + Duration::hours(1)).to_rfc3339();
            drive.direct.extend(
                db.weather_radiation_series(
                    crate::source::RadiationField::Direct,
                    &extra_start,
                    &extra_stop,
                    "1h",
                )
                .await
                .unwrap_or_default(),
            );
            drive.diffuse.extend(
                db.weather_radiation_series(
                    crate::source::RadiationField::Diffuse,
                    &extra_start,
                    &extra_stop,
                    "1h",
                )
                .await
                .unwrap_or_default(),
            );
            drive.shortwave.extend(
                db.weather_radiation_series(
                    crate::source::RadiationField::Shortwave,
                    &extra_start,
                    &extra_stop,
                    "1h",
                )
                .await
                .unwrap_or_default(),
            );
        }
        for zone in &zones {
            let zs = db
                .read_zone_temperature_series(zone, &s3, &e3, "1h")
                .await
                .unwrap_or_default();
            zone_series.entry(zone.clone()).or_default().extend(zs);
        }
        // Loud, never silent: a failed relay read must not be allowed to read as "no heating" (the
        // same hazard `validate::read_heating_kw`'s own doc warns about for a skipped zone).
        let by_room = db
            .heating_relay_events(&s3, &e3)
            .await
            .context("reading heating relay events")?;
        for (zone, room) in heated_zone_rooms {
            if let Some(events) = by_room.get(room) {
                relay_events
                    .entry(zone.clone())
                    .or_default()
                    .extend(events.iter().cloned());
            }
        }
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    for events in relay_events.values_mut() {
        events.sort_by_key(|s| s.time);
    }

    // The relay state AT the window start, from one bounded (≤7-day) `last()` lookup. Query bound
    // never exceeds 7 days even if the lookback finds nothing (the caller treats absence as OFF —
    // a legitimate "no prior event", flagged downstream — but a QUERY failure must not be folded
    // into that same silent-OFF fallback, so this propagates loudly like the raw events above).
    let lookback_start = (read_start - Duration::days(7)).to_rfc3339();
    let before = read_start.to_rfc3339();
    let last_by_room = db
        .heating_relay_last_before(&lookback_start, &before)
        .await
        .context("reading the relay state before the window")?;
    let mut relay_state_before = HashMap::new();
    for (zone, room) in heated_zone_rooms {
        if let Some(sample) = last_by_room.get(room) {
            relay_state_before.insert(zone.clone(), sample.clone());
        }
    }

    Ok(dump_from_parts(&DumpParts {
        drive,
        zone_series,
        relay_events,
        relay_state_before,
    }))
}

// --- Pure half: backtest-table scoring (item 2) --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HourClass {
    On,
    After,
    Other,
}

/// Classify every hour as heating-ON (own duty ≥ [`SCORE_ON_DUTY`]), within 6 h after the last ON
/// hour (and not itself ON), or other.
fn classify_hours(duty: &[f64]) -> Vec<HourClass> {
    let mut out = vec![HourClass::Other; duty.len()];
    let mut last_on: Option<usize> = None;
    for (i, &d) in duty.iter().enumerate() {
        if d >= SCORE_ON_DUTY {
            out[i] = HourClass::On;
            last_on = Some(i);
        } else if let Some(lo) = last_on {
            if (1..=6).contains(&(i - lo)) {
                out[i] = HourClass::After;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Stats {
    pub n: usize,
    sum_err: f64,
    sum_sq: f64,
    max_abs: f64,
}
impl Stats {
    fn add(&mut self, predicted_c: f64, measured_c: f64) {
        let e = predicted_c - measured_c;
        self.n += 1;
        self.sum_err += e;
        self.sum_sq += e * e;
        self.max_abs = self.max_abs.max(e.abs());
    }
    pub fn rmse(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            (self.sum_sq / self.n as f64).sqrt()
        }
    }
    pub fn bias(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_err / self.n as f64
        }
    }
    pub fn max_abs(&self) -> f64 {
        self.max_abs
    }
}

/// Score `predicted_c`/`measured` (full-index arrays aligned to `hours`) over `[from, to)`, split
/// into all / heating-on / after-pulse / other by `classes`. Returns `None` if coverage (fraction
/// of the window with a measurement) is below 50 % (the brief's skip-and-say-so rule).
fn score_categories(
    predicted_c: &[f64],
    measured: &[Option<f64>],
    classes: &[HourClass],
    from: usize,
    to: usize,
) -> Option<BTreeMap<&'static str, Stats>> {
    let window = to.saturating_sub(from);
    if window == 0 {
        return None;
    }
    let have = (from..to).filter(|&i| measured[i].is_some()).count();
    if (have as f64) < 0.5 * window as f64 {
        return None;
    }
    let mut out: BTreeMap<&'static str, Stats> = BTreeMap::new();
    for i in from..to {
        let Some(m) = measured[i] else { continue };
        let p = predicted_c[i];
        out.entry("all").or_default().add(p, m);
        let cat = match classes[i] {
            HourClass::On => "heating-on",
            HourClass::After => "after-pulse",
            HourClass::Other => "other",
        };
        out.entry(cat).or_default().add(p, m);
    }
    Some(out)
}

// --- Pure half: the own kernel + whole-window least-squares fit (item 3, estimator B) ------------

/// `g[k]` = own-zone air-temperature response at fine-lag `k+1` (`k=0..g.len()-1`) to a 1 kW pulse
/// held for one fine step. Linear interpolation for a continuous lag; plateaus past the horizon —
/// `run()` builds `g` to ≥ 2× the read window (rework-2 C2) specifically so this NEVER plateaus
/// inside the range `rescale_kernel` actually asks for (the regressor only needs lags up to the
/// window, and the slowest/fastest grid speeds (`s` = 0.5 .. 2.0) need `g` evaluated up to
/// `window / 0.5 = 2·window`) — a flat-extrapolated tail was rework-1 C3's bug.
fn interp_kernel(g: &[f64], lag: f64) -> f64 {
    if lag <= 0.0 {
        return 0.0;
    }
    let point = |m: f64| -> f64 {
        if m <= 0.0 {
            0.0
        } else {
            let idx = (m.round() as usize).saturating_sub(1);
            if idx < g.len() {
                g[idx]
            } else {
                *g.last().unwrap_or(&0.0)
            }
        }
    };
    let lo = lag.floor();
    let frac = lag - lo;
    point(lo) * (1.0 - frac) + point(lo + 1.0) * frac
}

/// `g_s[j] = g((j+1)/s) / s` — research.md's speed profile (`s > 1` stretches the response in
/// time/slower, `s < 1` compresses it/faster). NO renormalisation (rework-2 C2 — dropped): with
/// `g` built to ≥ 2× the read window, `interp_kernel` never plateaus inside the range this
/// actually evaluates, so the continuous time-rescaling already preserves the kernel's integral
/// exactly; forcing `Σ g_s == Σ g` on a still-truncated kernel (rework-1's fix) instead preserved
/// the TRUNCATED mass, biasing `k̂` by about the renormalisation's own size (rework-2 Refuter: +17%
/// at s=1.5, −18% at s=0.5, opposite-signed to the `ŝ` error — exactly the proposal bar's margin).
fn rescale_kernel(g: &[f64], s: f64) -> Vec<f64> {
    (1..=g.len())
        .map(|j| interp_kernel(g, j as f64 / s) / s)
        .collect()
}

/// Hourly-MEAN value of the fine-lattice (15-min) causal convolution of kernel `g` with an
/// hourly-held-constant (ZOH) kW series: hour `i`'s value (`i = 0..kw_hourly.len()-1`) is the
/// trapezoid mean of the fine responses spanning it ([`hour_mean_trapezoid`]), matching how
/// [`drive_hourly_mean`] reduces the fine drive AND how the measured series itself is read
/// (InfluxDB `aggregateWindow(mean)`, not a point sample) — rework-2 C1: comparing this convolution
/// (or the drive trajectory) as an END-of-hour POINT against a measured HOURLY MEAN gave a perfect
/// model k̂ 1.3–1.5, no flag, on the real kernels.
fn fine_convolution_hourly(g: &[f64], kw_hourly: &[f64]) -> Vec<f64> {
    let n_hours = kw_hourly.len();
    let n_fine = n_hours * 4;
    let mut resp_fine = vec![0.0; n_fine];
    for (j, slot) in resp_fine.iter_mut().enumerate() {
        let kmax = g.len().min(j + 1);
        let mut acc = 0.0;
        for (k, &gk) in g.iter().enumerate().take(kmax) {
            acc += gk * kw_hourly[(j - k) / 4];
        }
        *slot = acc;
    }
    (0..n_hours)
        .map(|i| hour_mean_trapezoid(|k| if k == 0 { 0.0 } else { resp_fine[k - 1] }, 4 * i))
        .collect()
}

/// Trapezoid mean over one hour of a fine lattice: the hour starting at fine index `base`
/// (`fine(base)` = its start state, `fine(base+4)` = its end) weighted (½, 1, 1, 1, ½)/4. The
/// measured series is the time mean over the whole hour (`aggregateWindow(mean)` of samples spread
/// through it); a right-Riemann mean of the four end-of-step states sits 22.5 min before the stamp
/// instead of 30 and read a perfect model as k̂ 1.09, ŝ 1.1 (Refuter cycle 2). The trapezoid is
/// within 0.004 K of the continuous mean on the real kernels.
fn hour_mean_trapezoid(fine: impl Fn(usize) -> f64, base: usize) -> f64 {
    (0.5 * fine(base) + fine(base + 1) + fine(base + 2) + fine(base + 3) + 0.5 * fine(base + 4))
        / 4.0
}

fn local_day(t: DateTime<Utc>, offset: FixedOffset) -> NaiveDate {
    t.with_timezone(&offset).date_naive()
}
fn local_hour_fraction(t: DateTime<Utc>, offset: FixedOffset) -> f64 {
    let lt = t.with_timezone(&offset);
    lt.hour() as f64 + lt.minute() as f64 / 60.0
}

/// One regression row for the whole-window least-squares fit: the TARGET to explain (`r + x_1`,
/// see [`fit_kernel_gain`]'s doc), the heating-kernel regressor (at the profiled `s`), and which
/// local day it falls in (for the per-day intercept + trend nuisance columns).
struct Row {
    target: f64,
    x: f64,
    day: NaiveDate,
    tau: f64,
}

/// OLS via SVD: solve `design · coeffs ≈ target` in the least-squares sense. `None` if the SVD
/// can't solve (degenerate design — treated as "not identifiable" by the caller).
fn ols(design: &DMatrix<f64>, target: &DVector<f64>) -> Option<(DVector<f64>, f64)> {
    let svd = design.clone().svd(true, true);
    let coeffs = svd.solve(target, 1e-9).ok()?;
    let resid = design * &coeffs - target;
    let sse = resid.dot(&resid);
    Some((coeffs, sse))
}

/// Build the `[x | per-day intercept | per-day trend | (sin, cos)?]` design matrix + target vector
/// for `rows`, restricted to the given `days` (in order — column order must stay stable across the
/// main fit and each jackknife refit). `diurnal` appends ONE global sin/cos pair of local
/// hour-of-day (not per-day, not dummies) — [`KernelFit::k_hat_diurnal_ctrl`]'s nuisance set
/// (rework-1 H1): a model error shaped like the daily solar/occupancy cycle is otherwise
/// indistinguishable from night-clustered heating.
fn build_design(rows: &[&Row], days: &[NaiveDate], diurnal: bool) -> (DMatrix<f64>, DVector<f64>) {
    let cols = 1 + 2 * days.len() + if diurnal { 2 } else { 0 };
    let mut design = DMatrix::<f64>::zeros(rows.len(), cols);
    let mut target = DVector::<f64>::zeros(rows.len());
    for (i, row) in rows.iter().enumerate() {
        design[(i, 0)] = row.x;
        if let Some(d) = days.iter().position(|&d| d == row.day) {
            design[(i, 1 + 2 * d)] = 1.0;
            design[(i, 2 + 2 * d)] = row.tau;
        }
        if diurnal {
            let theta = row.tau / 24.0 * std::f64::consts::TAU;
            design[(i, cols - 2)] = theta.sin();
            design[(i, cols - 1)] = theta.cos();
        }
        target[i] = row.target;
    }
    (design, target)
}

/// Variance-inflation factor of column 0 (`x`) against the remaining (nuisance) columns: regress
/// `x` on them and return `1 / (1 - R²)`. `f64::INFINITY` on a perfect (degenerate) fit.
fn vif_of_x(design: &DMatrix<f64>) -> f64 {
    let n = design.nrows();
    if n == 0 || design.ncols() <= 1 {
        return 1.0;
    }
    let x = design.column(0).clone_owned();
    let nuisance = design.columns(1, design.ncols() - 1).clone_owned();
    let Some((coeffs, sse)) = ols(&nuisance, &x) else {
        return 1.0;
    };
    let _ = coeffs;
    let mean = x.sum() / n as f64;
    let sst: f64 = x.iter().map(|v| (v - mean).powi(2)).sum();
    if sst <= 1e-12 {
        return f64::INFINITY;
    }
    let r2 = (1.0 - sse / sst).clamp(0.0, 1.0 - 1e-9);
    1.0 / (1.0 - r2)
}

/// Threshold (relative) between [`KernelFit::k_hat`] and [`KernelFit::k_hat_diurnal_ctrl`] above
/// which [`KernelFit::diurnal_sensitive`] is set (rework-1 H1).
const DIURNAL_SENSITIVITY: f64 = 0.15;

/// The whole-window least-squares fit result for one zone (research.md's estimator B).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct KernelFit {
    pub n_hours: usize,
    pub own_kwh: f64,
    /// `None` ⇒ not identifiable (see `reason`); `Some` but [`Self::identified`] `false` ⇒ fit
    /// ran but landed somewhere physically impossible (see `reason`) — the NUMBER is still here so
    /// nothing is hidden, but it must not drive a proposal.
    pub k_hat: Option<f64>,
    pub s_hat: Option<f64>,
    /// Leave-one-day-out jackknife 95 % CI of `k_hat`, `None` if fewer than 2 days or not
    /// identifiable.
    pub k_ci95: Option<(f64, f64)>,
    pub vif: f64,
    /// Set whenever `k_hat` is `None` (not identifiable) OR `identified` is `false` (an impossible
    /// fit) — read `identified` to tell the two apart.
    pub reason: Option<String>,
    /// `k_hat` from an INDEPENDENT re-fit with one extra global sin/cos (local hour-of-day) pair in
    /// the nuisance set, re-profiling `s` from scratch over [`S_GRID`] with that set (rework-2 H1 —
    /// reusing the plain fit's `s_hat` let a confounded SPEED survive the control unchanged). A
    /// diagnostic for a diurnally-shaped model error masquerading as a gain error. `None` whenever
    /// `k_hat` is `None`.
    pub k_hat_diurnal_ctrl: Option<f64>,
    /// `true` when `k_hat` and `k_hat_diurnal_ctrl` differ by more than [`DIURNAL_SENSITIVITY`] —
    /// a proposal built on this zone's `k_hat` needs BOTH fits to agree before it's trusted.
    pub diurnal_sensitive: bool,
    /// `false` ⇒ `k_hat`/`s_hat` are populated but NOT trustworthy: `k_hat ≤ 0` (impossible — a
    /// heater that cools the room) or `s_hat` pinned to a grid edge (0.5 or 2.0 — the true speed is
    /// outside the searched range, so the profile minimum is an artifact of the boundary, not a
    /// real optimum). The doc's proposal bar requires `identified` before trusting `k_hat`/`s_hat`
    /// at all, on top of the diurnal/CI checks (rework-2 M1).
    pub identified: bool,
}

/// Fit zone `zone`'s own gain (`k_hat`) and speed (`s_hat`) from the scored-window residual
/// against the own-kernel convolution of its recorded heating, profiling `s` over [`S_GRID`] and
/// picking the min-SSE value (research.md estimator B, rework-1 C2 regression form).
///
/// `residual[i] = measured_c[i] − full_post_fit_c[i]`, where `full_post_fit_c` is the FULL model
/// trajectory already driven by this zone's recorded heating at the model's own (`s=1, k=1`)
/// kernel — call that own-response `x_1`. So `residual = measured − x_1 − (everything else the
/// model gets right)`, and the TRUE response obeys `measured ≈ k·x_s + (everything else)` for the
/// true `(k, s)`. Subtracting: `residual + x_1 ≈ k·x_s` — the regression TARGET is `residual + x_1`
/// (not `residual` alone, which only equals `(k−1)·x_s` at `s = 1`), and the fitted coefficient of
/// `x_s` is `k` DIRECTLY (not `k − 1`). `kw_hourly` is the zone's full (warm-up + scored) hourly
/// heating-kW series the kernel convolves (so the kernel's memory sees the warm-up's heating, not
/// just the scored window's); `residual`/`x_1`/`x_s` must ALL be the hourly-MEAN convention
/// (rework-2 C1 — `residual` comes from [`drive_hourly_mean`], not the end-of-hour point).
#[allow(clippy::too_many_arguments)]
fn fit_kernel_gain(
    g: &[f64],
    kw_hourly: &[f64],
    residual: &[Option<f64>],
    grid_times: &[DateTime<Utc>],
    offset: FixedOffset,
    from: usize,
    to: usize,
) -> KernelFit {
    // rework-2 C2: the fastest grid speed (s = S_GRID[0] = 0.5) needs `g` evaluated up to lag
    // `4·kw_hourly.len() / 0.5 = 8·kw_hourly.len()` to stay inside `interp_kernel`'s real (not
    // plateaued) range — see `interp_kernel_does_not_plateau_within_the_regressor_used_range`.
    debug_assert!(
        g.len() as f64 >= 4.0 * kw_hourly.len() as f64 / S_GRID[0],
        "kernel too short for the fastest grid speed: g.len()={}, kw_hourly.len()={}",
        g.len(),
        kw_hourly.len()
    );
    let own_kwh: f64 = kw_hourly[from..to].iter().sum();
    let n_hours = to.saturating_sub(from);
    let not_identifiable = |reason: String, s_hat: Option<f64>, vif: f64| KernelFit {
        n_hours,
        own_kwh,
        k_hat: None,
        s_hat,
        k_ci95: None,
        vif,
        reason: Some(reason),
        k_hat_diurnal_ctrl: None,
        diurnal_sensitive: false,
        identified: false,
    };
    if own_kwh < MIN_OWN_KWH {
        return not_identifiable(
            format!("own heating {own_kwh:.1} kWh < {MIN_OWN_KWH} kWh in the scored window"),
            None,
            f64::NAN,
        );
    }

    // x_1: the model's own (s=1) hourly-mean convolution response — the FIXED term added back
    // into the residual to form the regression target (this fn's doc comment).
    let x1_full = fine_convolution_hourly(g, kw_hourly);

    // Profile s over S_GRID for a given nuisance-set shape (plain or diurnal-augmented), returning
    // the min-SSE (s, rows). Shared by the main fit and the diurnal-control refit, which — unlike
    // rework-1's version — profiles s INDEPENDENTLY under its own nuisance set rather than reusing
    // the plain fit's s_hat (rework-2 H1: a confounded SPEED otherwise survives the control
    // unchanged, since only the day/trend terms were being varied, never s itself).
    let profile = |diurnal: bool| -> Option<(f64, Vec<Row>)> {
        let mut best: Option<(f64, Vec<Row>, f64)> = None; // (s, rows, sse)
        for &s in &S_GRID {
            let g_s = rescale_kernel(g, s);
            let x_s_full = fine_convolution_hourly(&g_s, kw_hourly);
            let rows: Vec<Row> = (from..to)
                .filter_map(|i| {
                    residual[i].map(|r| Row {
                        target: r + x1_full[i],
                        x: x_s_full[i],
                        day: local_day(grid_times[i], offset),
                        tau: local_hour_fraction(grid_times[i], offset),
                    })
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            let mut days: Vec<NaiveDate> = rows.iter().map(|r| r.day).collect();
            days.sort();
            days.dedup();
            let refs: Vec<&Row> = rows.iter().collect();
            let (design, target) = build_design(&refs, &days, diurnal);
            let Some((_, sse)) = ols(&design, &target) else {
                continue;
            };
            if best.as_ref().map(|(_, _, b)| sse < *b).unwrap_or(true) {
                best = Some((s, rows, sse));
            }
        }
        best.map(|(s, rows, _)| (s, rows))
    };

    let Some((s_hat, rows)) = profile(false) else {
        return not_identifiable(
            "no scored hour had both a measurement and a heating input".to_string(),
            None,
            f64::NAN,
        );
    };
    let mut days: Vec<NaiveDate> = rows.iter().map(|r| r.day).collect();
    days.sort();
    days.dedup();
    let refs: Vec<&Row> = rows.iter().collect();
    let (design, target) = build_design(&refs, &days, false);
    let vif = vif_of_x(&design);
    if vif > MAX_VIF {
        return not_identifiable(
            format!("VIF {vif:.1} > {MAX_VIF} (collinear with the day terms)"),
            Some(s_hat),
            vif,
        );
    }
    let Some((coeffs, _)) = ols(&design, &target) else {
        return not_identifiable("least-squares solve failed".to_string(), Some(s_hat), vif);
    };
    let k_hat = coeffs[0];

    // Leave-one-day-out jackknife 95 % CI (normal approximation), `s_hat` held fixed.
    let k_ci95 = if days.len() >= 2 {
        let mut estimates = Vec::with_capacity(days.len());
        for &held_out in &days {
            let remaining: Vec<NaiveDate> =
                days.iter().filter(|&&d| d != held_out).copied().collect();
            let refs: Vec<&Row> = rows.iter().filter(|r| r.day != held_out).collect();
            if refs.is_empty() {
                continue;
            }
            let (d, t) = build_design(&refs, &remaining, false);
            if let Some((c, _)) = ols(&d, &t) {
                estimates.push(c[0]);
            }
        }
        if estimates.len() >= 2 {
            let d = estimates.len() as f64;
            let mean = estimates.iter().sum::<f64>() / d;
            let var = (d - 1.0) / d * estimates.iter().map(|e| (e - mean).powi(2)).sum::<f64>();
            let se = var.sqrt();
            Some((k_hat - 1.96 * se, k_hat + 1.96 * se))
        } else {
            None
        }
    } else {
        None
    };

    // Diurnal-confound diagnostic (rework-2 H1): an INDEPENDENT re-profile of s under the day
    // terms plus one global sin/cos pair; a k_hat that moves a lot once the diurnal shape is
    // absorbed (at whatever speed the CONTROLLED fit itself prefers) means the plain fit was
    // reading a model-error cycle as a gain/speed error, not this zone's real response.
    let k_hat_diurnal_ctrl = profile(true).and_then(|(_, rows_d)| {
        let mut days_d: Vec<NaiveDate> = rows_d.iter().map(|r| r.day).collect();
        days_d.sort();
        days_d.dedup();
        let refs_d: Vec<&Row> = rows_d.iter().collect();
        let (d, t) = build_design(&refs_d, &days_d, true);
        ols(&d, &t).map(|(c, _)| c[0])
    });
    let diurnal_sensitive = k_hat_diurnal_ctrl
        .map(|kd| (k_hat - kd).abs() / k_hat.abs().max(1e-6) > DIURNAL_SENSITIVITY)
        .unwrap_or(false);

    // Flag impossible fits (rework-2 M1): the numbers are still reported (nothing hidden), but
    // `identified: false` keeps them out of a proposal. `s_hat` pinned to a grid edge means the
    // true speed is outside S_GRID's 0.5..2.0 range, so the "minimum" there is a boundary artifact.
    let (identified, reason) = if k_hat <= 0.0 {
        (false, Some("k_hat <= 0".to_string()))
    } else if s_hat <= S_GRID[0] || s_hat >= S_GRID[S_GRID.len() - 1] {
        (false, Some("s_hat at grid edge".to_string()))
    } else {
        (true, None)
    };

    KernelFit {
        n_hours,
        own_kwh,
        k_hat: Some(k_hat),
        s_hat: Some(s_hat),
        k_ci95,
        vif,
        reason,
        k_hat_diurnal_ctrl,
        diurnal_sensitive,
        identified,
    }
}

// --- Pure half: episode detection (item 4) --------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Episode {
    /// Index (into the full hours array) of the last OFF hour before the ON streak.
    t0: usize,
    run_start: usize,
    run_end: usize, // exclusive
}

/// Find ON streaks (`duty ≥ on_duty` for ≥ `min_on_h` consecutive hours) preceded by ≥ `off_h`
/// hours at `duty ≤ off_duty`. `t0` is clipped to need a full `[t0-4, t0+24]` in bounds (so every
/// downstream lead/lag computation has data to work with) — episodes near either edge of the read
/// window are dropped, not partially scored. `min_t0` additionally requires `t0 ≥ min_t0` — pass
/// the scored-window start so episodes are never detected inside the warm-up (rework-1 M2): the
/// seeded wall/slab masses are still settling there, so the drift correction is unreliable.
fn detect_episodes(
    duty: &[f64],
    on_duty: f64,
    min_on_h: i64,
    off_h: i64,
    off_duty: f64,
    min_t0: usize,
) -> Vec<Episode> {
    let n = duty.len();
    let mut episodes = Vec::new();
    let mut i = 0usize;
    while i < n {
        if duty[i] >= on_duty {
            let run_start = i;
            let mut j = i;
            while j < n && duty[j] >= on_duty {
                j += 1;
            }
            let run_len = (j - run_start) as i64;
            if run_len >= min_on_h && run_start as i64 >= off_h && run_start >= 1 {
                let off_ok = (run_start - off_h as usize..run_start).all(|k| duty[k] <= off_duty);
                let t0 = run_start - 1;
                if off_ok && t0 >= 4 && t0 >= min_t0 && t0 + 24 < n {
                    episodes.push(Episode {
                        t0,
                        run_start,
                        run_end: j,
                    });
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    episodes
}

/// Measured coverage over `[t0-4, t0+12]` (inclusive) — episodes below 90 % are dropped.
fn episode_coverage(measured: &[Option<f64>], t0: usize) -> f64 {
    let from = t0 - 4;
    let to = (t0 + 12).min(measured.len() - 1);
    let span = to - from + 1;
    let have = measured[from..=to].iter().filter(|m| m.is_some()).count();
    have as f64 / span as f64
}

/// `ghi_proxy` (W/m², horizontal) at grid hour `i` — a shared solar-confounding flag, reusing the
/// same clear-sky-index physics as [`crate::solar_scale_backtest::clear_sky_index`] (duplicated as
/// a small pure function rather than widening that module's visibility for one call site).
fn ghi_proxy_wm2(
    latitude: Angle,
    longitude: Angle,
    when: DateTime<Utc>,
    data: &DriveData,
    i: usize,
) -> f64 {
    let input = data
        .solar
        .get(i)
        .copied()
        .unwrap_or(crate::tools::sun::SolarInput::Cloud {
            cloud: data.cloud[i],
        });
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

/// Simple linear-regression slope of `(x, y)` pairs (least squares), `0.0` if fewer than 2 points
/// or `x` has no spread.
fn linear_slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mx = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let my = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let num: f64 = points.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = points.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    if den <= 1e-9 {
        0.0
    } else {
        num / den
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// One lead's measured (raw / model-drift-corrected / pre-trend-corrected) vs modelled response,
/// normalised by kWh delivered in `[t0, t0+L)`.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct LeadStats {
    pub n: usize,
    pub raw_per_kwh: Option<f64>,
    pub drift_corrected_per_kwh: Option<f64>,
    pub pretrend_corrected_per_kwh: Option<f64>,
    pub modelled_per_kwh: Option<f64>,
    pub ratio_drift_vs_modelled: Option<f64>,
    pub n_solar_flagged: usize,
}

/// Build the episode table (item 4) for one zone: per lead in [`LEADS_H`], the median
/// measured/modelled response per kWh across qualifying episodes.
#[allow(clippy::too_many_arguments)]
fn episode_table(
    episodes: &[Episode],
    g: &[f64],
    kw_hourly: &[f64],
    measured: &[Option<f64>],
    full_post_fit_c: &[f64],
    latitude: Angle,
    longitude: Angle,
    grid_times: &[DateTime<Utc>],
    data: &DriveData,
) -> BTreeMap<i64, LeadStats> {
    #[derive(Default)]
    struct Accum {
        raw: Vec<f64>,
        drift: Vec<f64>,
        pretrend: Vec<f64>,
        model: Vec<f64>,
        n_flagged: usize,
    }
    let mut by_lead: HashMap<i64, Accum> =
        LEADS_H.iter().map(|&l| (l, Default::default())).collect();

    for ep in episodes {
        if episode_coverage(measured, ep.t0) < 0.9 {
            continue;
        }
        let Some(m0) = measured[ep.t0] else { continue };
        let max_l = *LEADS_H.iter().max().unwrap() as usize;
        // `local_kw[o]` = the hour ENDING at `t0 + 1 + o` (the first ON hour is `local_kw[0]`,
        // stop-stamped like `kw_hourly` itself), so `local_resp[lead-1]` is the response at the
        // hour ending `t0 + lead` — lag `lead` AFTER t0, matching `full_post_fit_c[t0 + lead]`.
        let local_kw: Vec<f64> = (0..max_l).map(|o| kw_hourly[ep.t0 + 1 + o]).collect();
        let local_resp = fine_convolution_hourly(g, &local_kw);
        // Pre-trend slope (K/h) fitted on the MEASURED temperature itself over [t0-3, t0] (rework-1
        // H2 — the residual's slope double-subtracts whatever the model already gets right: with a
        // drift the house and model share, the residual is flat and its slope is ~0, so the old
        // `raw − slope_resid·L` barely corrected anything).
        let slope = linear_slope(
            &(ep.t0 - 3..=ep.t0)
                .filter_map(|i| measured[i].map(|m| ((i as f64 - ep.t0 as f64), m)))
                .collect::<Vec<_>>(),
        );
        let flagged = grid_times
            .iter()
            .zip(0usize..)
            .skip(ep.t0 + 1)
            .take(max_l)
            .any(|(&t, i)| {
                ghi_proxy_wm2(latitude, longitude, t + Duration::minutes(30), data, i)
                    > GHI_FLAG_WM2
            });
        for &lead in &LEADS_H {
            let li = ep.t0 + lead as usize;
            let Some(ml) = measured.get(li).copied().flatten() else {
                continue;
            };
            // kWh delivered in [t0, t0+L): the L hours ENDING at t0+1 .. t0+L (the hour ending at
            // t0 itself is the pre-streak OFF hour, excluded).
            let kwh: f64 = kw_hourly[ep.t0 + 1..=li].iter().sum();
            if kwh <= 1e-6 {
                continue;
            }
            let raw = ml - m0;
            let model_full_delta = full_post_fit_c[li] - full_post_fit_c[ep.t0];
            let kernel_delta = local_resp[lead as usize - 1];
            let drift_corrected = raw - (model_full_delta - kernel_delta);
            let pretrend_corrected = raw - slope * lead as f64;
            let entry = by_lead.entry(lead).or_default();
            entry.raw.push(raw / kwh);
            entry.drift.push(drift_corrected / kwh);
            entry.pretrend.push(pretrend_corrected / kwh);
            entry.model.push(kernel_delta / kwh);
            if flagged {
                entry.n_flagged += 1;
            }
        }
    }

    by_lead
        .into_iter()
        .map(|(lead, mut acc)| {
            let n = acc.raw.len();
            let raw_m = median(&mut acc.raw);
            let drift_m = median(&mut acc.drift);
            let pretrend_m = median(&mut acc.pretrend);
            let model_m = median(&mut acc.model);
            let ratio = match (drift_m, model_m) {
                (Some(d), Some(m)) if m.abs() > 1e-9 => Some(d / m),
                _ => None,
            };
            (
                lead,
                LeadStats {
                    n,
                    raw_per_kwh: raw_m,
                    drift_corrected_per_kwh: drift_m,
                    pretrend_corrected_per_kwh: pretrend_m,
                    modelled_per_kwh: model_m,
                    ratio_drift_vs_modelled: ratio,
                    n_solar_flagged: acc.n_flagged,
                },
            )
        })
        .collect()
}

// --- Pure half: lag (item 5) ----------------------------------------------------------------------

const LAG_HORIZON_H: usize = 24;

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct LagResult {
    pub n_episodes: usize,
    pub t_peak_measured_h: Option<f64>,
    pub t63_measured_h: Option<f64>,
    pub t_peak_model_h: Option<f64>,
    pub t63_model_h: Option<f64>,
    /// `s_hat * t_peak_model_h` / `s_hat * t63_model_h` — the estimator-B-predicted lag, for
    /// comparison against the directly measured one.
    pub t_peak_model_scaled_h: Option<f64>,
    pub t63_model_scaled_h: Option<f64>,
}

/// Parabolic interpolation around the argmax of `curve[1..curve.len()-1]`, then linear
/// interpolation back to the 63 %-of-peak crossing (scanning up from index 0).
fn peak_and_t63(curve: &[f64]) -> (Option<f64>, Option<f64>) {
    if curve.len() < 3 {
        return (None, None);
    }
    let last = curve.len() - 1;
    let lm = curve[1..last]
        .iter()
        .enumerate()
        .map(|(i, v)| (i + 1, *v))
        .fold((1usize, f64::NEG_INFINITY), |best, cur| {
            if cur.1 > best.1 {
                cur
            } else {
                best
            }
        })
        .0;
    // Still rising (or not yet past) at the horizon: the interior-only search above can't see a
    // peak past `last`, so a curve that's actually still climbing would otherwise be mis-read as
    // peaking near `last - 1` (rework-1 L3) — censor instead of reporting a false peak/t63.
    if curve[last] >= curve[lm] {
        return (None, None);
    }
    let (ym1, y0, yp1) = (curve[lm - 1], curve[lm], curve[lm + 1]);
    let denom = ym1 - 2.0 * y0 + yp1;
    let offset = if denom.abs() > 1e-9 {
        (0.5 * (ym1 - yp1) / denom).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let t_peak = lm as f64 + offset;
    let peak_val = y0;
    if peak_val <= 0.0 {
        return (Some(t_peak), None);
    }
    let target = 0.63 * peak_val;
    let mut t63 = None;
    for i in 0..lm {
        if curve[i] <= target && curve[i + 1] > target {
            let span = curve[i + 1] - curve[i];
            let frac = if span.abs() > 1e-9 {
                (target - curve[i]) / span
            } else {
                0.0
            };
            t63 = Some(i as f64 + frac);
            break;
        }
    }
    (Some(t_peak), t63)
}

#[allow(clippy::too_many_arguments)]
fn lag_result(
    episodes: &[Episode],
    g: &[f64],
    kw_hourly: &[f64],
    measured: &[Option<f64>],
    full_post_fit_c: &[f64],
    s_hat: Option<f64>,
) -> LagResult {
    let mut sum_measured = [0.0; LAG_HORIZON_H + 1];
    let mut sum_model = [0.0; LAG_HORIZON_H + 1];
    let mut n = [0usize; LAG_HORIZON_H + 1];
    let mut n_episodes = 0usize;
    for ep in episodes {
        if ep.t0 + LAG_HORIZON_H >= measured.len() || episode_coverage(measured, ep.t0) < 0.9 {
            continue;
        }
        let Some(m0) = measured[ep.t0] else { continue };
        let kwh_total: f64 = kw_hourly[ep.run_start..ep.run_end].iter().sum();
        if kwh_total <= 1e-6 {
            continue;
        }
        // See `episode_table`'s comment: `local_kw[o]` is the hour ending at `t0 + 1 + o`.
        let local_kw: Vec<f64> = (0..LAG_HORIZON_H)
            .map(|o| kw_hourly[ep.t0 + 1 + o])
            .collect();
        let local_resp = fine_convolution_hourly(g, &local_kw);
        n_episodes += 1;
        for l in 0..=LAG_HORIZON_H {
            if l == 0 {
                n[l] += 1;
                continue;
            }
            let Some(ml) = measured.get(ep.t0 + l).copied().flatten() else {
                continue;
            };
            let model_full_delta = full_post_fit_c[ep.t0 + l] - full_post_fit_c[ep.t0];
            let kernel_delta = local_resp[l - 1];
            let drift_corrected = (ml - m0) - (model_full_delta - kernel_delta);
            sum_measured[l] += drift_corrected / kwh_total;
            sum_model[l] += kernel_delta / kwh_total;
            n[l] += 1;
        }
    }
    if n_episodes == 0 {
        return LagResult::default();
    }
    let avg_measured: Vec<f64> = (0..=LAG_HORIZON_H)
        .map(|l| {
            if n[l] > 0 {
                sum_measured[l] / n[l] as f64
            } else {
                0.0
            }
        })
        .collect();
    let avg_model: Vec<f64> = (0..=LAG_HORIZON_H)
        .map(|l| {
            if n[l] > 0 {
                sum_model[l] / n[l] as f64
            } else {
                0.0
            }
        })
        .collect();
    let (t_peak_measured_h, t63_measured_h) = peak_and_t63(&avg_measured);
    let (t_peak_model_h, t63_model_h) = peak_and_t63(&avg_model);
    LagResult {
        n_episodes,
        t_peak_measured_h,
        t63_measured_h,
        t_peak_model_h,
        t63_model_h,
        t_peak_model_scaled_h: t_peak_model_h.and_then(|t| s_hat.map(|s| t * s)),
        t63_model_scaled_h: t63_model_h.and_then(|t| s_hat.map(|s| t * s)),
    }
}

// --- Pure half: catch-up (item 6) -----------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CatchUp {
    /// Minutes for the own-kernel step response at `max_heat_kw` to reach +1 K, linearly
    /// interpolated between fine steps (rework-2 L1 — no 15-min ceiling: a perfect point model
    /// read 180 min against a true 168, 105 against 94). `None` ⇒ never, within the kernel's own
    /// horizon (`run()` builds it to ≥ 2× the read window) — see `modelled_censored`.
    pub modelled_min: Option<f64>,
    /// `true` when `modelled_min` is `None` because the horizon was reached without hitting +1 K
    /// (print as "> `kernel_horizon_h` h", never just "None" — rework-2 L1).
    pub modelled_censored: bool,
    /// Same, with the fitted `k_hat · g_(s_hat)` kernel (`None` if not identifiable).
    pub measured_implied_min: Option<f64>,
    /// Same meaning as `modelled_censored`, for `measured_implied_min`.
    pub measured_implied_censored: bool,
    /// Median minutes-to-+1K directly from qualifying episodes (drift-corrected), counting only
    /// episodes whose first 3 ON hours ran at duty ≥ 0.9 (a thermostat-cut streak never reaches
    /// `max_heat_kw` for long enough to be a step response). `None` unless at least 2/3 of those
    /// qualifying episodes actually reached +1 K (`n_reached`/`n_censored` are always reported
    /// regardless) — a median over "reached" episodes alone is a LOWER bound on the true time,
    /// since the slow half that never got there is silently dropped.
    pub direct_median_min: Option<f64>,
    pub n_reached: usize,
    pub n_censored: usize,
}

/// Minutes for the cumulative step response (`g` held at `max_heat_kw` from fine-step 0) to first
/// reach `+1 K`, linearly interpolated between the bracketing fine steps (rework-2 L1); `None` if
/// it never does within the kernel's horizon (censored).
fn step_response_minutes(g: &[f64], max_heat_kw: f64) -> Option<f64> {
    let mut acc = 0.0;
    for (k, &gk) in g.iter().enumerate() {
        let prev = acc;
        acc += gk * max_heat_kw;
        if acc >= 1.0 {
            let step_start_min = k as f64 * KERNEL_DT_S / 60.0;
            let span = acc - prev;
            let frac = if span.abs() > 1e-9 {
                (1.0 - prev) / span
            } else {
                0.0
            };
            return Some(step_start_min + frac * (KERNEL_DT_S / 60.0));
        }
    }
    None
}

/// Fraction of "reached" episodes [`catch_up`] requires before reporting `direct_median_min` at
/// all (rework-1 M1) — below this the censored half dominates and a median over the rest is not a
/// meaningful "typical" time, just the fastest episodes that happened to qualify.
const CATCH_UP_MIN_REACHED_FRACTION: f64 = 2.0 / 3.0;
/// Minimum first-3-ON-hours duty for an episode to count toward the "direct" catch-up measurement
/// (rework-1 M1) — a thermostat that cycles off early never ran a step at `max_heat_kw` long
/// enough to be compared against the modelled step response.
const CATCH_UP_MIN_STEP_DUTY: f64 = 0.9;

#[allow(clippy::too_many_arguments)]
fn catch_up(
    episodes: &[Episode],
    g: &[f64],
    kw_hourly: &[f64],
    duty: &[f64],
    measured: &[Option<f64>],
    full_post_fit_c: &[f64],
    max_heat_kw: f64,
    k_hat: Option<f64>,
    s_hat: Option<f64>,
) -> CatchUp {
    let modelled_min = step_response_minutes(g, max_heat_kw);
    let modelled_censored = modelled_min.is_none();
    let measured_implied_min = match (k_hat, s_hat) {
        (Some(k), Some(s)) => {
            let g_hat: Vec<f64> = rescale_kernel(g, s).iter().map(|v| v * k).collect();
            step_response_minutes(&g_hat, max_heat_kw)
        }
        _ => None,
    };
    // Only "never reached the horizon" counts as censored — `None` because k_hat/s_hat are
    // themselves unavailable is a different, already-explained absence.
    let measured_implied_censored =
        k_hat.is_some() && s_hat.is_some() && measured_implied_min.is_none();

    let mut reached_minutes = Vec::new();
    let mut n_censored = 0usize;
    for ep in episodes {
        if ep.t0 + 12 >= measured.len() || episode_coverage(measured, ep.t0) < 0.9 {
            continue;
        }
        let near_step = duty[ep.run_start..(ep.run_start + 3).min(duty.len())]
            .iter()
            .all(|&d| d >= CATCH_UP_MIN_STEP_DUTY);
        if !near_step {
            continue;
        }
        let Some(m0) = measured[ep.t0] else { continue };
        // See `episode_table`'s comment: `local_kw[o]` is the hour ending at `t0 + 1 + o`.
        let local_kw: Vec<f64> = (0..12).map(|o| kw_hourly[ep.t0 + 1 + o]).collect();
        let local_resp = fine_convolution_hourly(g, &local_kw);
        // Hourly means sit at MID-hour: the hour ending at t0+l is centred at l − 0.5 h after t0's
        // own centre, so the interpolation runs on those stamps (the modelled step response is a
        // point curve; a right-stamped mean read a perfect model 23 min slow).
        let mut prev: Option<(f64, f64)> = Some((0.0, 0.0)); // (hours after t0, drift-corrected delta)
        let mut found = None;
        for l in 1..=12usize {
            let Some(ml) = measured.get(ep.t0 + l).copied().flatten() else {
                break;
            };
            let model_full_delta = full_post_fit_c[ep.t0 + l] - full_post_fit_c[ep.t0];
            let kernel_delta = local_resp[l - 1];
            let drift_corrected = (ml - m0) - (model_full_delta - kernel_delta);
            if drift_corrected >= 1.0 {
                if let Some((ph, pv)) = prev {
                    let span = drift_corrected - pv;
                    let frac = if span.abs() > 1e-9 {
                        (1.0 - pv) / span
                    } else {
                        0.0
                    };
                    found = Some((ph + frac * (l as f64 - ph)) * 60.0);
                }
                break;
            }
            prev = Some((l as f64 - 0.5, drift_corrected));
        }
        match found {
            Some(m) => reached_minutes.push(m),
            None => n_censored += 1,
        }
    }
    let n_reached = reached_minutes.len();
    let total = n_reached + n_censored;
    let direct_median_min =
        if total > 0 && (n_reached as f64) >= CATCH_UP_MIN_REACHED_FRACTION * total as f64 {
            median(&mut reached_minutes)
        } else {
            None
        };

    CatchUp {
        modelled_min,
        modelled_censored,
        measured_implied_min,
        measured_implied_censored,
        direct_median_min,
        n_reached,
        n_censored,
    }
}

// --- Hourly-mean drive (rework-2 C1) ----------------------------------------------------------------

/// Drive the model at the fine (900 s) lattice, holding each hour's inputs constant across its 4
/// fine steps (ZOH — the SAME physical inputs `drive_with`'s 3600 s step would use for that hour,
/// just resolved finer: `build_input` is still called once per hour), and return the HOURLY-MEAN
/// trajectory: hour `i`'s value (`i = 1..=n_hours`) is the trapezoid mean of the fine states spanning
/// it (fine indices `4i-4..=4i`, see [`hour_mean_trapezoid`]); index `0` is the seed state itself
/// (the "grid seed", not a measured hour). This matches how the measured series is actually read — InfluxDB
/// `aggregateWindow(mean)`, an HOURLY MEAN, not a point sample — so the model side must be one too:
/// comparing `drive_with`'s end-of-hour POINT against a measured mean gave a perfect-model fit
/// k̂ 1.3–1.5 with a tight CI and no flag (rework-2 C1).
fn drive_hourly_mean(
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    x0: &DVector<f64>,
    data: &DriveData,
) -> Vec<DVector<f64>> {
    let disc = ss.discretize(KERNEL_DT_S);
    let n_hours = data.grid_times.len().saturating_sub(1);
    let mut fine: Vec<DVector<f64>> = Vec::with_capacity(4 * n_hours + 1);
    fine.push(x0.clone());
    for h in 0..n_hours {
        let u = build_input(net, ss, latitude, longitude, data, h);
        for _ in 0..4 {
            let next = ss.step(&disc, fine.last().unwrap(), &u);
            fine.push(next);
        }
    }
    let mut mean_traj: Vec<DVector<f64>> = Vec::with_capacity(n_hours + 1);
    mean_traj.push(fine[0].clone());
    for i in 1..=n_hours {
        let base = 4 * (i - 1);
        let sum = &fine[base] * 0.5
            + &fine[base + 1]
            + &fine[base + 2]
            + &fine[base + 3]
            + &fine[base + 4] * 0.5;
        mean_traj.push(sum / 4.0);
    }
    mean_traj
}

// --- CLI -------------------------------------------------------------------------------------------

struct Args {
    start: DateTime<Utc>,
    days: i64,
    warmup_h: i64,
    model_path: String,
    config_path: String,
    out: Option<String>,
    dump: Option<String>,
    from: Option<String>,
    legacy_duty: bool,
    on_duty: f64,
    min_on_h: i64,
    off_h: i64,
    off_duty: f64,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut start = None;
    let mut days = None;
    let mut warmup_h = 48i64;
    let mut model_path = "model.json5".to_string();
    let mut config_path = "config.json5".to_string();
    let mut out = None;
    let mut dump = None;
    let mut from = None;
    let mut legacy_duty = false;
    let mut on_duty = 0.7;
    let mut min_on_h = 2i64;
    let mut off_h = 4i64;
    let mut off_duty = 0.1;

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
            "--days" => {
                days = Some(val!("--days").parse::<i64>().context("parsing --days")?);
            }
            "--warmup-h" => {
                warmup_h = val!("--warmup-h")
                    .parse::<i64>()
                    .context("parsing --warmup-h")?;
            }
            "--model" => model_path = val!("--model").clone(),
            "--config" => config_path = val!("--config").clone(),
            "--out" => out = Some(val!("--out").clone()),
            "--dump" => dump = Some(val!("--dump").clone()),
            "--from" => from = Some(val!("--from").clone()),
            "--legacy-duty" => {
                legacy_duty = true;
                i += 1;
            }
            "--on-duty" => {
                on_duty = val!("--on-duty")
                    .parse::<f64>()
                    .context("parsing --on-duty")?
            }
            "--min-on-h" => {
                min_on_h = val!("--min-on-h")
                    .parse::<i64>()
                    .context("parsing --min-on-h")?
            }
            "--off-h" => off_h = val!("--off-h").parse::<i64>().context("parsing --off-h")?,
            "--off-duty" => {
                off_duty = val!("--off-duty")
                    .parse::<f64>()
                    .context("parsing --off-duty")?
            }
            other => bail!("backtest-heating: unrecognized argument {other:?}"),
        }
    }

    let start = start.ok_or_else(|| anyhow::anyhow!("backtest-heating needs --start"))?;
    ensure!(
        start.minute() == 0 && start.second() == 0 && start.nanosecond() == 0,
        "--start must be hour-aligned (minute/second 0), got {start}"
    );
    let days = days.ok_or_else(|| anyhow::anyhow!("backtest-heating needs --days"))?;
    ensure!(
        (1..=7).contains(&days),
        "--days must be in 1..=7 (got {days})"
    );
    // Capped at 7 days: the relay `relay_state_before` lookback is exactly 7 days, so a longer
    // warm-up would start from an already-unknown (assumed-OFF) relay state anyway; it also keeps
    // every bounded read a small, predictable number of ≤7-day chunks.
    ensure!(
        (0..=168).contains(&warmup_h),
        "--warmup-h must be in 0..=168"
    );

    Ok(Args {
        start,
        days,
        warmup_h,
        model_path,
        config_path,
        out,
        dump,
        from,
        legacy_duty,
        on_duty,
        min_on_h,
        off_h,
        off_duty,
    })
}

// --- Report (item 7) --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ZoneReport {
    coverage_skipped: bool,
    pre_fit: Option<BTreeMap<&'static str, Stats>>,
    post_fit: Option<BTreeMap<&'static str, Stats>>,
    gain_w: f64,
    kwh_true: f64,
    kwh_legacy: f64,
    n_episodes: usize,
    kernel_fit: KernelFit,
    episodes: BTreeMap<i64, LeadStats>,
    lag: LagResult,
    catch_up: CatchUp,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    start: String,
    stop: String,
    days: i64,
    warmup_h: i64,
    legacy_duty: bool,
    model_path: String,
    config_path: String,
    radiation_source: String,
    /// Hours of memory the kernel was built with (`run()`'s `4 × hours.len()`, floored at
    /// [`MIN_KERNEL_N`]'s 48 h) — "not reached" in the catch-up figures means not reached within
    /// this many hours, not a hardcoded 48 (rework-1 C3).
    kernel_horizon_h: f64,
    state_before_assumed_off: Vec<String>,
    zones: BTreeMap<String, ZoneReport>,
}

fn print_report(r: &Report) {
    println!(
        "backtest-heating {} .. {} ({} day(s), {} h warm-up{}) — model {} config {}",
        r.start,
        r.stop,
        r.days,
        r.warmup_h,
        if r.legacy_duty {
            ", LEGACY duty semantics"
        } else {
            ""
        },
        r.model_path,
        r.config_path,
    );
    println!("  solar driving input: {}", r.radiation_source);
    println!("  kernel horizon: {:.0} h", r.kernel_horizon_h);
    if !r.state_before_assumed_off.is_empty() {
        println!(
            "  no relay state found before the window for: {} (assumed OFF)",
            r.state_before_assumed_off.join(", ")
        );
    }
    println!();
    println!("-- Active backtest: RMSE / bias (K), pre-fit (config gains) -> post-fit (NNLS) --");
    for (zone, z) in &r.zones {
        if z.coverage_skipped {
            println!("  {zone:<16} SKIPPED (coverage < 50%)");
            continue;
        }
        for cat in ["all", "heating-on", "after-pulse", "other"] {
            let pre = z.pre_fit.as_ref().and_then(|m| m.get(cat));
            let post = z.post_fit.as_ref().and_then(|m| m.get(cat));
            if pre.is_none() && post.is_none() {
                continue;
            }
            let (pr, pb) = pre
                .map(|s| (s.rmse(), s.bias()))
                .unwrap_or((f64::NAN, f64::NAN));
            let (qr, qb, qmax) = post.map(|s| (s.rmse(), s.bias(), s.max_abs())).unwrap_or((
                f64::NAN,
                f64::NAN,
                f64::NAN,
            ));
            println!(
                "  {zone:<16}{cat:<12} n={:<4} rmse {pr:.2}->{qr:.2} bias {pb:+.2}->{qb:+.2} max|err| {qmax:.2} gain={:.0}W",
                post.map(|s| s.n).unwrap_or(0),
                z.gain_w,
            );
        }
        println!(
            "  {zone:<16}kWh delivered: true {:.1} vs legacy-duty {:.1} (undercount {:+.0}%)",
            z.kwh_true,
            z.kwh_legacy,
            if z.kwh_true > 1e-6 {
                100.0 * (z.kwh_legacy - z.kwh_true) / z.kwh_true
            } else {
                0.0
            }
        );
    }
    println!();
    println!("-- Kernel check (whole-window least squares): k_hat, s_hat, 95% CI --");
    for (zone, z) in &r.zones {
        let kf = &z.kernel_fit;
        match kf.k_hat {
            Some(k) => println!(
                "  {zone:<16} k_hat={k:.2} (diurnal-ctrl {:?}{}) s_hat={:.2} CI95={:?} own_kWh={:.1} VIF={:.1} n={}{}",
                kf.k_hat_diurnal_ctrl,
                if kf.diurnal_sensitive {
                    ", DIURNAL-SENSITIVE"
                } else {
                    ""
                },
                kf.s_hat.unwrap_or(f64::NAN),
                kf.k_ci95,
                kf.own_kwh,
                kf.vif,
                kf.n_hours,
                if kf.identified {
                    String::new()
                } else {
                    format!(" NOT IDENTIFIED ({})", kf.reason.as_deref().unwrap_or("?"))
                },
            ),
            None => println!(
                "  {zone:<16} not identifiable ({})",
                kf.reason.as_deref().unwrap_or("?")
            ),
        }
    }
    println!();
    println!("-- Episode table: median measured (raw / drift-corr. / pretrend-corr.) vs modelled K/kWh --");
    for (zone, z) in &r.zones {
        for (&lead, ls) in &z.episodes {
            if ls.n == 0 {
                continue;
            }
            println!(
                "  {zone:<16}{lead:>3}h n={:<3} raw={:?} drift={:?} pretrend={:?} model={:?} ratio={:?} solar-flagged={}",
                ls.n,
                ls.raw_per_kwh,
                ls.drift_corrected_per_kwh,
                ls.pretrend_corrected_per_kwh,
                ls.modelled_per_kwh,
                ls.ratio_drift_vs_modelled,
                ls.n_solar_flagged,
            );
        }
    }
    println!();
    println!("-- Lag: time to peak / 63% of peak (h), measured vs modelled*s_hat --");
    for (zone, z) in &r.zones {
        let l = &z.lag;
        if l.n_episodes == 0 {
            continue;
        }
        println!(
            "  {zone:<16} n={} t_peak {:?} vs {:?} (model {:?}) t63 {:?} vs {:?} (model {:?})",
            l.n_episodes,
            l.t_peak_measured_h,
            l.t_peak_model_scaled_h,
            l.t_peak_model_h,
            l.t63_measured_h,
            l.t63_model_scaled_h,
            l.t63_model_h,
        );
    }
    println!();
    println!("-- Catch-up: minutes to +1K at max_heat_kw --");
    let fmt_catchup = |v: Option<f64>, censored: bool, horizon_h: f64| -> String {
        match v {
            Some(m) => format!("{m:.0}"),
            None if censored => format!("> {horizon_h:.0} h"),
            None => "None".to_string(),
        }
    };
    for (zone, z) in &r.zones {
        let c = &z.catch_up;
        println!(
            "  {zone:<16} modelled={} measured-implied={} direct_median={:?} (n reached {} / censored {})",
            fmt_catchup(c.modelled_min, c.modelled_censored, r.kernel_horizon_h),
            fmt_catchup(c.measured_implied_min, c.measured_implied_censored, r.kernel_horizon_h),
            c.direct_median_min,
            c.n_reached,
            c.n_censored,
        );
    }
}

/// Entry point — called from `main.rs` for `backtest-heating`, with `--model`/`--config` already
/// parsed out so the model/config load can use the candidate paths instead of the default ones.
pub async fn run(args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    let stop = parsed.start + Duration::hours(parsed.days * 24);
    let read_start = parsed.start - Duration::hours(parsed.warmup_h);

    let model = Model::load(&parsed.model_path)
        .with_context(|| format!("loading --model {}", parsed.model_path))?;
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load(&parsed.config_path)
        .with_context(|| format!("loading --config {}", parsed.config_path))?;

    let mut heated_zone_rooms: Vec<(String, String)> = Vec::new();
    for zone in config.heating.zones.keys() {
        if net
            .marker_indices
            .contains_key(&(zone.clone(), "heating".to_string()))
        {
            heated_zone_rooms.push((zone.clone(), zone.clone())); // room resolved below if `db` exists
        }
    }
    heated_zone_rooms.sort();

    let dump = if let Some(path) = &parsed.from {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading --from {path}"))?;
        serde_json::from_str::<ReplayDump>(&text)
            .with_context(|| format!("parsing --from {path}"))?
    } else {
        let db = SourceClients::with_signals(
            InfluxDB::from_config(&parsed.config_path)?,
            config.data_sources.clone(),
        );
        let resolved: Vec<(String, String)> = heated_zone_rooms
            .iter()
            .filter_map(|(zone, _)| {
                db.zone_room(zone)
                    .map(|room| (zone.clone(), room.to_string()))
            })
            .collect();
        if resolved.len() != heated_zone_rooms.len() {
            eprintln!(
                "backtest-heating: {} heated zone(s) have no room mapping — recorded heating unavailable for them",
                heated_zone_rooms.len() - resolved.len()
            );
        }
        read_window(&db, &resolved, read_start, stop).await?
    };
    if let Some(path) = &parsed.dump {
        std::fs::write(path, serde_json::to_string_pretty(&dump)?)
            .with_context(|| format!("writing --dump {path}"))?;
        println!("backtest-heating: dumped raw series to {path}");
    }

    let parts = parts_from_dump(&dump);
    anyhow::ensure!(
        !parts.zone_series.is_empty(),
        "no measured zone series available (check --from file contents or InfluxDB zone mappings)"
    );
    let radiation_source = if !parts.drive.direct.is_empty() && !parts.drive.diffuse.is_empty() {
        "radiation (direct+diffuse)".to_string()
    } else if !parts.drive.shortwave.is_empty() {
        "GHI (shortwave_radiation)".to_string()
    } else if !parts.drive.cloud.is_empty() {
        "cloud cover only".to_string()
    } else {
        "cloud fallback (0.5, no weather data in window)".to_string()
    };

    let ground_c = config.site.ground_temperature_c;
    let mut full = assemble_drive_data(&parts.drive, ground_c, 0.5)?;

    let mut heating_true: HashMap<String, Vec<f64>> = HashMap::new();
    let mut heating_legacy: HashMap<String, Vec<f64>> = HashMap::new();
    let mut state_before_missing = Vec::new();
    for (zone, _) in &heated_zone_rooms {
        let Some(spec) = config.heating.zones.get(zone) else {
            continue;
        };
        let events: Vec<(DateTime<Utc>, f64)> = parts
            .relay_events
            .get(zone)
            .map(|v| v.iter().map(|s| (s.time, s.value)).collect())
            .unwrap_or_default();
        let state_before = parts
            .relay_state_before
            .get(zone)
            .map(|s| (s.time, s.value));
        if state_before.is_none() {
            state_before_missing.push(zone.clone());
        }
        let duty_true = relay_duty_hourly(&events, state_before, &full.hours);
        let duty_legacy = legacy_duty_hourly(&events, &full.hours);
        heating_true.insert(
            zone.clone(),
            duty_true.iter().map(|d| d * spec.max_heat_kw).collect(),
        );
        heating_legacy.insert(
            zone.clone(),
            duty_legacy.iter().map(|d| d * spec.max_heat_kw).collect(),
        );
    }
    // The SAME duty the model is driven with also drives the kernel/episode checks under
    // `--legacy-duty` (rework-1 L1) — one consistent arm, not a model driven by the legacy duty
    // scored against episodes/kernels built from the true one. The true-vs-legacy kWh TOTALS are
    // still both always reported, regardless of which arm is active.
    let heating_chosen = if parsed.legacy_duty {
        &heating_legacy
    } else {
        &heating_true
    };
    full.heating_kw = heating_chosen.clone();
    full.scheduled_loads = config.scheduled_loads.clone();
    full.scheduled_w = config
        .scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
        .collect();
    full.sensor_power_w = vec![None; full.scheduled_loads.len()];
    full.local_offset = config.site.offset_at(read_start);

    let latitude = Angle::new::<degree>(config.site.latitude);
    let longitude = Angle::new::<degree>(config.site.longitude);
    let x0 = seed_from_series(&net, &ss, &parts.zone_series, full.hours[0]);

    // Pre-fit: score with the static CONFIG gains (no NNLS).
    let mut pre_data = full.clone();
    pre_data.internal_gain_w = config.heating.internal_gains();
    let traj_pre = drive_hourly_mean(&net, &ss, latitude, longitude, &x0, &pre_data);

    // Post-fit: the per-window NNLS fit (the SAME baseline `fit_gains` expects: no internal gains,
    // fixed scheduled loads already applied).
    let gain_zones = config.heating.gain_zones();
    let gain_caps_w = config.heating.gain_caps_w();
    // Scored hours are those ENDING in (start, stop] (rework-1 L2): the hour ending AT `start`
    // (index `hour_key(start)`) is the last WARM-UP hour, covering (start-1h, start] — the first
    // SCORED hour is the one ending at `start + 1h`.
    let scored_from = full
        .hours
        .iter()
        .position(|&h| h == hour_key(parsed.start) + 1)
        .context("scored window start not found in the read data (gap at the window boundary?)")?;
    let fit_window = full.hours.len().saturating_sub(scored_from);
    let fit = fit_gains(
        &net,
        &ss,
        latitude,
        longitude,
        &x0,
        &full,
        &parts.zone_series,
        &config.scheduled_loads,
        &config.heating.gain_groups,
        &gain_zones,
        &gain_caps_w,
        fit_window,
        full.local_offset,
    );
    let mut post_data = full.clone();
    post_data.internal_gain_w = fit.gains.clone();
    post_data.scheduled_w = fit.scheduled_w.clone();
    let traj_post = drive_hourly_mean(&net, &ss, latitude, longitude, &x0, &post_data);
    let full_post_fit_c: HashMap<String, Vec<f64>> = net
        .zone_indices
        .iter()
        .filter_map(|(zone, &node)| {
            ss.state_index(node).map(|row| {
                (
                    zone.clone(),
                    traj_post
                        .iter()
                        .map(|x| crate::tools::k_to_c(x[row]))
                        .collect(),
                )
            })
        })
        .collect();
    let traj_pre_c: HashMap<String, Vec<f64>> = net
        .zone_indices
        .iter()
        .filter_map(|(zone, &node)| {
            ss.state_index(node).map(|row| {
                (
                    zone.clone(),
                    traj_pre
                        .iter()
                        .map(|x| crate::tools::k_to_c(x[row]))
                        .collect(),
                )
            })
        })
        .collect();

    let scored_to = (scored_from + (parsed.days * 24) as usize).min(full.hours.len());
    // Build the kernel to ≥ 2× the read window (rework-1 C3, widened by rework-2 C2): cheap (an
    // `Ad·g` chain) relative to a real slab kernel's memory, and the 2× margin is what lets
    // `rescale_kernel` profile the fastest grid speed (s=0.5, needing `g` evaluated out to
    // `window / 0.5 = 2·window`) without `interp_kernel` ever plateauing in the range actually used.
    let kernel_n = (8 * full.hours.len()).max(MIN_KERNEL_N);
    let kernel_horizon_h = kernel_n as f64 * KERNEL_DT_S / 3600.0;
    let ks = build_kernels(&ss, &net, KERNEL_DT_S, kernel_n, &[], &[]);

    let mut zones: Vec<String> = parts.zone_series.keys().cloned().collect();
    zones.sort();
    let mut report_zones: BTreeMap<String, ZoneReport> = BTreeMap::new();
    for zone in &zones {
        let Some(predicted_pre) = traj_pre_c.get(zone) else {
            continue;
        };
        let Some(predicted_post) = full_post_fit_c.get(zone) else {
            continue;
        };
        let measured_series = &parts.zone_series[zone];
        let measured: Vec<Option<f64>> = {
            let mut by_hour: HashMap<i64, f64> = HashMap::new();
            for s in measured_series {
                by_hour.entry(hour_key(s.time)).or_insert(s.value);
            }
            full.hours.iter().map(|h| by_hour.get(h).copied()).collect()
        };
        let duty_kw = heating_chosen.get(zone);
        let max_heat_kw = config
            .heating
            .zones
            .get(zone)
            .map(|z| z.max_heat_kw)
            .unwrap_or(0.0);
        let classes = match (duty_kw, max_heat_kw > 0.0) {
            (Some(v), true) => {
                classify_hours(&v.iter().map(|kw| kw / max_heat_kw).collect::<Vec<_>>())
            }
            _ => vec![HourClass::Other; full.hours.len()],
        };

        let pre_fit = score_categories(predicted_pre, &measured, &classes, scored_from, scored_to);
        let post_fit =
            score_categories(predicted_post, &measured, &classes, scored_from, scored_to);
        let coverage_skipped = post_fit.is_none();

        let kwh_true: f64 = heating_true
            .get(zone)
            .map(|v| v[scored_from..scored_to].iter().sum())
            .unwrap_or(0.0);
        let kwh_legacy: f64 = heating_legacy
            .get(zone)
            .map(|v| v[scored_from..scored_to].iter().sum())
            .unwrap_or(0.0);
        let gain_w = fit
            .gains
            .get(zone)
            .map(|p| p.night.max(p.day).max(p.evening))
            .unwrap_or(0.0);

        let (kernel_fit, episodes_table, lag, catch_up_result, n_episodes) =
            if let (Some(g), Some(kw), Some(max_kw)) = (
                ks.kernels.get(&(zone.clone(), zone.clone())),
                heating_chosen.get(zone),
                config.heating.zones.get(zone).map(|z| z.max_heat_kw),
            ) {
                let kf = fit_kernel_gain(
                    g,
                    kw,
                    &measured
                        .iter()
                        .zip(predicted_post)
                        .map(|(m, &p)| m.map(|m| m - p))
                        .collect::<Vec<_>>(),
                    &full.grid_times,
                    full.local_offset,
                    scored_from,
                    scored_to,
                );
                let duty: Vec<f64> = kw.iter().map(|p| p / max_kw.max(1e-9)).collect();
                let episodes = detect_episodes(
                    &duty,
                    parsed.on_duty,
                    parsed.min_on_h,
                    parsed.off_h,
                    parsed.off_duty,
                    scored_from,
                );
                let et = episode_table(
                    &episodes,
                    g,
                    kw,
                    &measured,
                    predicted_post,
                    latitude,
                    longitude,
                    &full.grid_times,
                    &full,
                );
                // Derived figures (ŝ-scaled lag, k̂·g_ŝ catch-up) only from an identified fit — an
                // edge ŝ or k̂ ≤ 0 would otherwise print as a plain, plausible-looking number.
                let (k_id, s_id) = if kf.identified {
                    (kf.k_hat, kf.s_hat)
                } else {
                    (None, None)
                };
                let lr = lag_result(&episodes, g, kw, &measured, predicted_post, s_id);
                let cu = catch_up(
                    &episodes,
                    g,
                    kw,
                    &duty,
                    &measured,
                    predicted_post,
                    max_kw,
                    k_id,
                    s_id,
                );
                (kf, et, lr, cu, episodes.len())
            } else {
                (
                    KernelFit {
                        n_hours: 0,
                        own_kwh: 0.0,
                        k_hat: None,
                        s_hat: None,
                        k_ci95: None,
                        vif: f64::NAN,
                        reason: Some(
                            "zone has no own kernel (no heating marker/state row)".to_string(),
                        ),
                        k_hat_diurnal_ctrl: None,
                        diurnal_sensitive: false,
                        identified: false,
                    },
                    BTreeMap::new(),
                    LagResult::default(),
                    CatchUp::default(),
                    0,
                )
            };

        report_zones.insert(
            zone.clone(),
            ZoneReport {
                coverage_skipped,
                pre_fit,
                post_fit,
                gain_w,
                kwh_true,
                kwh_legacy,
                n_episodes,
                kernel_fit,
                episodes: episodes_table,
                lag,
                catch_up: catch_up_result,
            },
        );
    }

    let report = Report {
        start: parsed.start.to_rfc3339(),
        stop: stop.to_rfc3339(),
        days: parsed.days,
        warmup_h: parsed.warmup_h,
        legacy_duty: parsed.legacy_duty,
        model_path: parsed.model_path.clone(),
        config_path: parsed.config_path.clone(),
        radiation_source,
        kernel_horizon_h,
        state_before_assumed_off: state_before_missing,
        zones: report_zones,
    };
    print_report(&report);
    if let Some(path) = &parsed.out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing --out {path}"))?;
        println!("backtest-heating: wrote {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ---------- score split ----------

    #[test]
    fn classify_hours_marks_on_after_and_other() {
        let duty = vec![0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let classes = classify_hours(&duty);
        assert_eq!(classes[0], HourClass::Other);
        assert_eq!(classes[2], HourClass::On);
        assert_eq!(classes[3], HourClass::On);
        assert_eq!(classes[4], HourClass::After); // 1h after last ON
        assert_eq!(classes[9], HourClass::After); // 6h after last ON (index 3)
                                                  // index 3 + 7 = 10 is out of range here; extend to check "other" past 6h:
        let mut duty2 = duty.clone();
        duty2.push(0.0); // index 10, 7h after
        let classes2 = classify_hours(&duty2);
        assert_eq!(classes2[10], HourClass::Other);
    }

    #[test]
    fn score_categories_skips_low_coverage_zone() {
        let predicted = vec![20.0; 10];
        let measured = vec![
            Some(20.0),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ];
        let classes = vec![HourClass::Other; 10];
        assert!(score_categories(&predicted, &measured, &classes, 0, 10).is_none());
    }

    #[test]
    fn score_categories_splits_all_on_after_other() {
        let predicted = vec![21.0, 21.0, 21.0, 21.0];
        let measured = vec![Some(20.0), Some(20.0), Some(20.0), Some(20.0)];
        let classes = vec![
            HourClass::On,
            HourClass::After,
            HourClass::Other,
            HourClass::Other,
        ];
        let scored = score_categories(&predicted, &measured, &classes, 0, 4).unwrap();
        assert_eq!(scored["all"].n, 4);
        assert_eq!(scored["heating-on"].n, 1);
        assert_eq!(scored["after-pulse"].n, 1);
        assert_eq!(scored["other"].n, 2);
        assert!((scored["all"].bias() - 1.0).abs() < 1e-9);
    }

    // ---------- episode detection ----------

    #[test]
    fn detect_episodes_finds_a_qualifying_streak() {
        // index: 0..=4 off, 5..=7 on (3h >= min_on 2h), preceded by 4h off (1..=4); t0=4 needs
        // >=4 hours of history before it (the detector's edge-of-window guard).
        let mut duty = vec![0.0; 30];
        for d in duty.iter_mut().take(8).skip(5) {
            *d = 1.0;
        }
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].t0, 4);
        assert_eq!(episodes[0].run_start, 5);
        assert_eq!(episodes[0].run_end, 8);
    }

    #[test]
    fn detect_episodes_rejects_a_streak_at_the_start_of_window() {
        // ON streak starting at index 0 has no preceding off hours to check -> rejected.
        let mut duty = vec![0.0; 20];
        for d in duty.iter_mut().take(3) {
            *d = 1.0;
        }
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert!(episodes.is_empty());
    }

    #[test]
    fn detect_episodes_rejects_too_short_a_streak() {
        let mut duty = vec![0.0; 20];
        duty[5] = 1.0; // only 1h, min_on_h = 2
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert!(episodes.is_empty());
    }

    #[test]
    fn detect_episodes_rejects_insufficient_off_time_before() {
        let mut duty = vec![0.0; 20];
        duty[3] = 0.5; // above off_duty right before the streak
        for d in duty.iter_mut().take(8).skip(4) {
            *d = 1.0;
        }
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert!(episodes.is_empty());
    }

    #[test]
    fn detect_episodes_rejects_a_streak_inside_the_warm_up() {
        // Same qualifying streak as `detect_episodes_finds_a_qualifying_streak` (t0=4), but with
        // min_t0=5 (the scored window starts one hour later) — the warm-up masses are still
        // settling there, so no episode should be detected (rework-1 M2).
        let mut duty = vec![0.0; 30];
        for d in duty.iter_mut().take(8).skip(5) {
            *d = 1.0;
        }
        assert_eq!(detect_episodes(&duty, 0.7, 2, 4, 0.1, 0).len(), 1);
        assert!(detect_episodes(&duty, 0.7, 2, 4, 0.1, 5).is_empty());
    }

    // ---------- estimator B (kernel fit) ----------

    /// A tiny single-zone house with an underfloor `"heating"` marker and an exterior wall — a
    /// REAL (if small) RC network, so its kernel actually decays with physical time constants
    /// (rework-1 C4: the old synthetic test used a hand-built kernel that monotonically RISES to
    /// a plateau, which the real slab kernel does not).
    fn build_one_zone_with_heating() -> (RcNetwork, StateSpace) {
        let model = Model::from_json(
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
                        { material: "concrete", thickness: 0.1 },
                    ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { lr: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["lr", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["lr", "outside"], area: 30 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        (net, ss)
    }

    /// A thermostat-like heating schedule: nightly bursts whose start/length/intensity vary by
    /// day — a day-invariant pulse leaves the response SPEED only weakly identified (the SSE(s)
    /// profile goes nearly flat near the true s, collinear with the per-day nuisance terms), so
    /// the varying shape is what gives the profile a sharp minimum, as real cycling heat does.
    fn cycling_kw(n_hours: usize, max_kw: f64) -> Vec<f64> {
        (0..n_hours)
            .map(|i| {
                let d = i / 24;
                let h = i % 24;
                let on_start = 1 + (d * 7) % 5;
                let len = 2 + (d * 3) % 4;
                if h >= on_start && h < on_start + len {
                    max_kw * (0.6 + 0.1 * ((d + h) % 4) as f64)
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// Drive `net`/`ss` (constant −2 °C outside, no solar) from a flat 18 °C seed with `kw`
    /// (kW) at the `lr` zone's `"heating"` slab marker, over `n_hours` hourly steps, at the
    /// HOURLY-MEAN convention (`drive_hourly_mean` — rework-2 C1: the measured series this is
    /// compared against is itself an hourly mean, not `drive_with`'s end-of-hour point). Returns
    /// `(grid_times, predicted zone-air °C, local offset)`.
    fn drive_lr(
        net: &RcNetwork,
        ss: &StateSpace,
        kw: &[f64],
        n_hours: usize,
    ) -> (Vec<DateTime<Utc>>, Vec<f64>, FixedOffset) {
        let offset = FixedOffset::east_opt(0).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let outside: Vec<TimeSample> = (0..=n_hours)
            .map(|h| TimeSample {
                time: start + Duration::hours(h as i64),
                value: -2.0,
            })
            .collect();
        let mut data = assemble_drive_data(
            &DriveSeries {
                outside,
                ..Default::default()
            },
            10.0,
            1.0,
        )
        .unwrap();
        data.heating_kw = HashMap::from([("lr".to_string(), kw.to_vec())]);
        data.local_offset = offset;
        let lat = Angle::new::<degree>(50.0);
        let lon = Angle::new::<degree>(14.0);
        let x0 = DVector::from_element(ss.n_states(), crate::tools::c_to_k(18.0));
        let traj = drive_hourly_mean(net, ss, lat, lon, &x0, &data);
        let row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let predicted: Vec<f64> = traj.iter().map(|x| crate::tools::k_to_c(x[row])).collect();
        (data.grid_times, predicted, offset)
    }

    /// The "house" for a perfect-model test: the SAME tiny model driven at 60 s (not the 900 s the
    /// tool uses), each hour reduced to its continuous time mean (61 samples, trapezoid) — what
    /// `aggregateWindow(mean)` of a densely logged sensor returns. `heat_scale` multiplies the
    /// heating flux (0.7 ⇒ the house really gets 0.7× the kernel's heat). Independent of
    /// `drive_hourly_mean`'s own averaging, so a skewed hour mean there shows up as k̂/ŝ ≠ 1.
    fn measured_lr_fine_mean(
        net: &RcNetwork,
        ss: &StateSpace,
        kw: &[f64],
        n_hours: usize,
        heat_scale: f64,
    ) -> Vec<Option<f64>> {
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let outside: Vec<TimeSample> = (0..=n_hours)
            .map(|h| TimeSample {
                time: start + Duration::hours(h as i64),
                value: -2.0,
            })
            .collect();
        let mut data = assemble_drive_data(
            &DriveSeries {
                outside,
                ..Default::default()
            },
            10.0,
            1.0,
        )
        .unwrap();
        let scaled: Vec<f64> = kw.iter().map(|v| v * heat_scale).collect();
        data.heating_kw = HashMap::from([("lr".to_string(), scaled)]);
        let lat = Angle::new::<degree>(50.0);
        let lon = Angle::new::<degree>(14.0);
        let disc = ss.discretize(60.0);
        let row = ss.state_index(net.zone_indices["lr"]).unwrap();
        let mut x = DVector::from_element(ss.n_states(), crate::tools::c_to_k(18.0));
        let mut out = vec![Some(crate::tools::k_to_c(x[row]))];
        for h in 0..n_hours {
            let u = build_input(net, ss, lat, lon, &data, h);
            let mut acc = 0.5 * x[row];
            for m in 0..60 {
                x = ss.step(&disc, &x, &u);
                acc += if m == 59 { 0.5 * x[row] } else { x[row] };
            }
            out.push(Some(crate::tools::k_to_c(acc / 60.0)));
        }
        out
    }

    /// C4(a): measured := the drive's own trajectory (truth k=1, s=1) — the residual is then
    /// exactly zero everywhere, so the fit should recover k=1, s=1 exactly (and NOT flag a
    /// diurnal confound that isn't there).
    #[test]
    fn fit_kernel_gain_perfect_model_recovers_k1_s1() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 6;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * (n_hours + 1), &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = cycling_kw(n_hours + 1, 3.0);
        let (grid_times, predicted, offset) = drive_lr(&net, &ss, &kw, n_hours);
        // "Measured" = the same physics at 60 s, hour-averaged continuously — NOT the tool's own
        // 900 s hour mean, so the two averaging conventions are tested against each other.
        let measured = measured_lr_fine_mean(&net, &ss, &kw, n_hours, 1.0);
        let residual: Vec<Option<f64>> = measured
            .iter()
            .zip(&predicted)
            .map(|(m, p)| m.map(|m| m - p))
            .collect();

        let from = 48usize;
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, from, n_hours + 1);
        let k_hat = fit.k_hat.expect("should be identifiable");
        let s_hat = fit.s_hat.expect("should be identifiable");
        assert!((0.99..=1.01).contains(&k_hat), "k_hat={k_hat}");
        assert!((s_hat - 1.0).abs() < 1e-9, "s_hat={s_hat}");
        assert!(!fit.diurnal_sensitive, "fit={fit:?}");
    }

    /// C4(b): measured := a SEPARATE drive with the heating input physically scaled ×0.7 (real
    /// state-space superposition — `rescale_kernel` never runs on the "truth" side), while the
    /// model's own prediction (what `residual` is built against) still assumes the full recorded
    /// `kw`. k_true = 0.7 and s_true = 1 exactly by construction (scaling the INPUT, not the
    /// kernel, cannot change response SPEED).
    #[test]
    fn fit_kernel_gain_physical_k07_recovers_within_tolerance() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 6;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * (n_hours + 1), &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw_recorded = cycling_kw(n_hours + 1, 3.0);

        let (grid_times, model_predicted, offset) = drive_lr(&net, &ss, &kw_recorded, n_hours);
        let measured = measured_lr_fine_mean(&net, &ss, &kw_recorded, n_hours, 0.7);
        let residual: Vec<Option<f64>> = measured
            .iter()
            .zip(&model_predicted)
            .map(|(m, p)| m.map(|m| m - p))
            .collect();

        let from = 48usize;
        let fit = fit_kernel_gain(
            &g,
            &kw_recorded,
            &residual,
            &grid_times,
            offset,
            from,
            n_hours + 1,
        );
        let k_hat = fit.k_hat.expect("should be identifiable");
        let s_hat = fit.s_hat.expect("should be identifiable");
        assert!((0.68..=0.72).contains(&k_hat), "k_hat={k_hat}");
        let s_step = S_GRID[1] - S_GRID[0];
        assert!((s_hat - 1.0).abs() <= s_step + 1e-9, "s_hat={s_hat}");
    }

    /// C4(c): a synthetic k/s recovery test (the regression form the estimator actually fits,
    /// `target = k·x_s`, on the real kernel — not the physics round-trip (a)/(b) are), with AR(1)
    /// noise σ ≈ 0.05 K as research.md specifies, plus a per-day trend and a diurnal sine.
    #[test]
    fn fit_kernel_gain_synthetic_noise_recovers_k_and_s() {
        let (net, ss) = build_one_zone_with_heating();
        let true_k = 0.7;
        let true_s = 1.5;
        let n_hours = 24 * 10;
        let kernel_n = 8 * (n_hours + 1);
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, kernel_n, &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = cycling_kw(n_hours + 1, 3.0);
        let (grid_times, _predicted, offset) = drive_lr(&net, &ss, &kw, n_hours);

        let g_s_true = rescale_kernel(&g, true_s);
        let x1 = fine_convolution_hourly(&g, &kw);
        let x_true = fine_convolution_hourly(&g_s_true, &kw);
        let mut residual: Vec<Option<f64>> = Vec::with_capacity(n_hours + 1);
        let mut noise_state = 0.0_f64;
        // AR(1): n_i = phi·n_{i-1} + w_i, w_i ~ Uniform(-A, A); stationary std ≈ 0.05 K at
        // phi=0.5, A=0.075 (std(w)=A/√3, std(n)=std(w)/√(1-phi²)).
        let phi = 0.5;
        let amp = 0.075;
        for i in 0..=n_hours {
            let day = (i / 24) as f64;
            let hour = (i % 24) as f64;
            let trend = 0.02 * day - 0.0005 * day * hour;
            let diurnal = 0.01 * (hour / 24.0 * std::f64::consts::TAU).sin();
            noise_state =
                phi * noise_state + amp * (((i * 7919 + 104729) % 10007) as f64 / 10007.0 - 0.5);
            let target = true_k * x_true.get(i).copied().unwrap_or(0.0);
            let r = target - x1.get(i).copied().unwrap_or(0.0) + trend + diurnal + noise_state;
            residual.push(Some(r));
        }

        let from = 48usize;
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, from, n_hours + 1);
        let k_hat = fit.k_hat.expect("should be identifiable");
        let s_hat = fit.s_hat.expect("should be identifiable");
        assert!((k_hat - true_k).abs() < 0.08, "k_hat={k_hat}");
        let s_step = S_GRID[1] - S_GRID[0];
        assert!((s_hat - true_s).abs() <= s_step + 1e-9, "s_hat={s_hat}");
    }

    #[test]
    fn fit_kernel_gain_zero_heating_is_not_identifiable() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 3;
        let kernel_n = 8 * n_hours;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, kernel_n, &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = vec![0.0; n_hours];
        let offset = FixedOffset::east_opt(0).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let grid_times: Vec<DateTime<Utc>> = (0..n_hours)
            .map(|h| start + Duration::hours(h as i64))
            .collect();
        let residual: Vec<Option<f64>> = vec![Some(0.1); n_hours];
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, 24, n_hours);
        assert!(fit.k_hat.is_none());
        assert!(fit.reason.is_some());
    }

    /// M1 (rework-2): a target perfectly explained by `k = -1` (constructed so the house gets
    /// COLDER when heated, the opposite of physical) must still be REPORTED — `k_hat` is not
    /// hidden — but flagged `identified: false` with an explanatory reason, so it can never
    /// silently drive a proposal.
    #[test]
    fn fit_kernel_gain_negative_k_hat_is_not_identified() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 6;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * (n_hours + 1), &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = cycling_kw(n_hours + 1, 3.0);
        let x1 = fine_convolution_hourly(&g, &kw);
        // target = residual + x1 = -x1  =>  k = -1 at s = 1 (a perfect, if impossible, fit).
        let residual: Vec<Option<f64>> = (0..=n_hours).map(|i| Some(-2.0 * x1[i])).collect();
        let offset = FixedOffset::east_opt(0).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let grid_times: Vec<DateTime<Utc>> = (0..=n_hours)
            .map(|h| start + Duration::hours(h as i64))
            .collect();
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, 48, n_hours + 1);
        let k_hat = fit
            .k_hat
            .expect("the fit itself succeeds — just an impossible answer");
        assert!(k_hat <= 0.0, "k_hat={k_hat}");
        assert!(!fit.identified, "fit={fit:?}");
        assert_eq!(fit.reason.as_deref(), Some("k_hat <= 0"));
    }

    /// M1 (rework-2): a target perfectly explained at the FASTEST grid speed (`s = 0.5`, the grid
    /// edge) must also be flagged `identified: false` — the profile's minimum there is as likely a
    /// boundary artifact (the true speed lies outside 0.5..2.0) as a real optimum, and the grid
    /// can't tell the two apart.
    #[test]
    fn fit_kernel_gain_s_hat_at_grid_edge_is_not_identified() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 6;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * (n_hours + 1), &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = cycling_kw(n_hours + 1, 3.0);
        let x1 = fine_convolution_hourly(&g, &kw);
        let g_s05 = rescale_kernel(&g, S_GRID[0]);
        let x_s05 = fine_convolution_hourly(&g_s05, &kw);
        // target = residual + x1 = x_s05  =>  k = 1 at s = 0.5 exactly.
        let residual: Vec<Option<f64>> = (0..=n_hours).map(|i| Some(x_s05[i] - x1[i])).collect();
        let offset = FixedOffset::east_opt(0).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let grid_times: Vec<DateTime<Utc>> = (0..=n_hours)
            .map(|h| start + Duration::hours(h as i64))
            .collect();
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, 48, n_hours + 1);
        let s_hat = fit.s_hat.expect("the fit itself succeeds");
        assert_eq!(s_hat, S_GRID[0], "s_hat={s_hat}");
        assert!(!fit.identified, "fit={fit:?}");
        assert_eq!(fit.reason.as_deref(), Some("s_hat at grid edge"));
    }

    /// C2 (rework-2): `rescale_kernel`'s dropped renormalisation only stays safe if `interp_kernel`
    /// never plateaus inside the range the regressor actually uses. With `g` built to the required
    /// ≥ 2× the window (here: `8 * hours` fine steps for `hours`-long `kw_hourly`), the fastest
    /// grid speed (`s = 0.5`) needs lags up to exactly `g`'s own length — still real interpolation,
    /// not the flat `g.last()` fallback. With the OLD (rework-1) `4 * hours` sizing, the same lag
    /// WOULD plateau.
    #[test]
    fn interp_kernel_does_not_plateau_within_the_regressor_used_range() {
        let hours = 40;
        // Strictly increasing ramp so a plateau is numerically distinguishable from real
        // interpolation.
        let g: Vec<f64> = (1..=8 * hours).map(|j| j as f64).collect();
        let last_used_lag = (4 * hours) as f64 / S_GRID[0]; // = 8*hours: the boundary the regressor needs
        let v = interp_kernel(&g, last_used_lag);
        assert!((v - (8 * hours) as f64).abs() < 1e-9, "v={v}");
        let v_inside = interp_kernel(&g, last_used_lag - 1.0);
        assert!(
            (v_inside - (8 * hours - 1) as f64).abs() < 1e-9,
            "v_inside={v_inside}"
        );
        // The OLD (rework-1) sizing — g truncated to 4*hours — WOULD plateau at this same lag,
        // confirming the longer kernel is what avoids it.
        let g_short = &g[..4 * hours];
        let v_short = interp_kernel(g_short, last_used_lag);
        assert_eq!(v_short, *g_short.last().unwrap());
    }

    /// H1 (rework-2): the Refuter's p5/c1_q3 scenario — truth k=1, but the MODEL has a 0.15 K
    /// diurnal error and heating is night-clustered — must raise `diurnal_sensitive` instead of
    /// silently reporting a confident wrong `k_hat` (on the real house kernel: k̂=0.37, CI
    /// (0.31,0.42), VIF 2.3, no guard firing; with rework-1's control, which reused the plain
    /// fit's confounded `s_hat`, this amplitude did not raise the flag on this tiny test kernel
    /// either — re-profiling `s` independently inside the control, as rework-2 H1 requires, is
    /// what makes the SAME 0.15 K amplitude the Refuter used actually catch it here too).
    #[test]
    fn fit_kernel_gain_diurnal_confound_raises_the_flag() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 24 * 9;
        let kernel_n = 8 * n_hours;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, kernel_n, &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let kw = cycling_kw(n_hours, 3.0); // night-clustered (on_start 1..=5h local)
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let grid_times: Vec<DateTime<Utc>> = (0..n_hours)
            .map(|h| start + Duration::hours(h as i64))
            .collect();
        let offset = FixedOffset::east_opt(0).unwrap();
        let residual: Vec<Option<f64>> = (0..n_hours)
            .map(|i| Some(0.15 * ((i % 24) as f64 / 24.0 * std::f64::consts::TAU).cos()))
            .collect();
        let fit = fit_kernel_gain(&g, &kw, &residual, &grid_times, offset, 48, n_hours);
        assert!(fit.diurnal_sensitive, "fit={fit:?}");
    }

    // ---------- episode table (timing) ----------

    /// C1 (rework-2): perfect model, HOURLY-MEAN measured against the same hourly-mean model
    /// trajectory — every lead's ratio must be ≈ 1.00 (the Refuter's c1_q2, comparing an
    /// end-of-hour POINT "measured" against the same point model instead, got 0.40 at 1 h, 0.83
    /// at 3 h, 0.97 at 12 h even though the model was "perfect").
    #[test]
    fn episode_table_perfect_model_mean_vs_mean_gives_unit_ratio() {
        let (net, ss) = build_one_zone_with_heating();
        let n_hours = 150; // drive_lr builds n_hours+1 = 151 grid points
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * (n_hours + 1), &[], &[]);
        let g = ks.kernels[&("lr".to_string(), "lr".to_string())].clone();
        let max_kw = 3.0;
        let mut kw = vec![0.0; n_hours + 1];
        for v in kw.iter_mut().take(113).skip(100) {
            *v = max_kw; // ON 100..113 (13 h), t0 = 99
        }
        let (grid_times, predicted, offset) = drive_lr(&net, &ss, &kw, n_hours);
        let measured = measured_lr_fine_mean(&net, &ss, &kw, n_hours, 1.0);
        let duty: Vec<f64> = kw.iter().map(|v| v / max_kw).collect();
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert_eq!(episodes.len(), 1);
        let outside: Vec<TimeSample> = (0..=n_hours)
            .map(|h| TimeSample {
                time: grid_times[0] + Duration::hours(h as i64),
                value: -2.0,
            })
            .collect();
        let data = assemble_drive_data(
            &DriveSeries {
                outside,
                ..Default::default()
            },
            10.0,
            1.0,
        )
        .unwrap();
        let lat = Angle::new::<degree>(50.0);
        let lon = Angle::new::<degree>(14.0);
        let et = episode_table(
            &episodes,
            &g,
            &kw,
            &measured,
            &predicted,
            lat,
            lon,
            &grid_times,
            &data,
        );
        for (lead, ls) in &et {
            let ratio = ls.ratio_drift_vs_modelled.expect("has a ratio");
            assert!((ratio - 1.0).abs() < 0.02, "lead={lead} ratio={ratio}");
        }
        let _ = offset;
    }

    // ---------- episode table (pre-trend) ----------

    /// H2 (rework-1): with a drift the house and model SHARE (0.1 K/h cooling, probes2's p3), the
    /// old pre-trend formula (slope of the RESIDUAL, which is ~0 here since measured == model)
    /// left the drift's own contribution to `raw` uncorrected, so `pretrend_corrected` was biased
    /// away from `modelled` (the Refuter: 1 h gave 0.019 against 0.052 modelled). The new formula
    /// (slope of the MEASURED temperature) should cancel the shared drift, leaving
    /// `pretrend_corrected ≈ modelled` — ratio ≈ 1.0 at every lead.
    #[test]
    fn episode_table_pretrend_matches_modelled_under_shared_drift() {
        let (net, ss) = build_one_zone_with_heating();
        let zone = "lr".to_string();
        let n_hours = 80;
        let ks = build_kernels(&ss, &net, KERNEL_DT_S, 8 * n_hours, &[], &[]);
        let g = ks.kernels[&(zone.clone(), zone.clone())].clone();
        let max_kw = 3.0;
        let mut kw = vec![0.0; n_hours];
        for v in kw.iter_mut().take(43).skip(30) {
            *v = max_kw; // ON 30..43 (13 h), t0 = 29
        }
        let drift = |i: usize| -0.1 * i as f64; // K/h, shared by "house" and "model"
        let x = fine_convolution_hourly(&g, &kw);
        let model: Vec<f64> = (0..n_hours).map(|i| 20.0 + drift(i) + x[i]).collect();
        let measured: Vec<Option<f64>> = model.iter().copied().map(Some).collect();

        let start = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let outside: Vec<TimeSample> = (0..n_hours)
            .map(|h| TimeSample {
                time: start + Duration::hours(h as i64),
                value: -2.0,
            })
            .collect();
        let data = assemble_drive_data(
            &DriveSeries {
                outside,
                ..Default::default()
            },
            10.0,
            1.0,
        )
        .unwrap();
        let lat = Angle::new::<degree>(50.0);
        let lon = Angle::new::<degree>(14.0);
        let duty: Vec<f64> = kw.iter().map(|v| v / max_kw).collect();
        let episodes = detect_episodes(&duty, 0.7, 2, 4, 0.1, 0);
        assert_eq!(episodes.len(), 1);
        let et = episode_table(
            &episodes,
            &g,
            &kw,
            &measured,
            &model,
            lat,
            lon,
            &data.grid_times,
            &data,
        );
        for (lead, ls) in &et {
            let pretrend = ls
                .pretrend_corrected_per_kwh
                .expect("has a pretrend reading");
            let modelled = ls.modelled_per_kwh.expect("has a modelled reading");
            let ratio = pretrend / modelled;
            assert!((ratio - 1.0).abs() < 0.1, "lead={lead} ratio={ratio}");
        }
    }

    // ---------- lag (t63 recovery) ----------

    /// A curve with a KNOWN analytic peak/t63 (`x·e^(−x/5)`: peak at x=5, 63 %-of-peak crossing
    /// at x≈1.594 by numeric root-find) — M4: assert the discrete parabolic/linear estimate is
    /// within 15 min (0.25 h) of the TRUE value, not just internally self-consistent.
    fn known_peak_curve(n: usize) -> Vec<f64> {
        (0..=n)
            .map(|i| (i as f64) * (-(i as f64) / 5.0).exp())
            .collect()
    }

    #[test]
    fn peak_and_t63_recovers_known_curve() {
        let curve = known_peak_curve(20);
        let (t_peak, t63) = peak_and_t63(&curve);
        let t_peak = t_peak.expect("curve has a real interior peak, not censored");
        let t63 = t63.expect("curve rises through 63% before the peak");
        assert!((t_peak - 5.0).abs() < 0.25, "t_peak={t_peak}");
        assert!((t63 - 1.594).abs() < 0.25, "t63={t63}");
    }

    #[test]
    fn peak_and_t63_a_one_hour_stamp_shift_moves_t63_by_one_hour() {
        let n = 20;
        let base = known_peak_curve(n);
        let (_, t63_a) = peak_and_t63(&base);
        let mut shifted = vec![0.0];
        shifted.extend_from_slice(&base[..base.len() - 1]);
        let (_, t63_b) = peak_and_t63(&shifted);
        // M4: a stamp shift must produce two REAL readings, not pass vacuously because one side
        // censored.
        let a = t63_a.expect("base curve must not censor");
        let b = t63_b.expect("shifted curve must not censor");
        assert!((b - a - 1.0).abs() < 0.26, "a={a} b={b}");
    }

    #[test]
    fn peak_and_t63_still_rising_at_the_horizon_is_censored() {
        // Monotonically increasing all the way to the last index (rework-1 L3): the true peak is
        // at or past the horizon, so both must be `None`, not a false peak near the end.
        let n = 20;
        let curve: Vec<f64> = (0..=n).map(|i| 1.0 - (-(i as f64) / 8.0).exp()).collect();
        assert_eq!(peak_and_t63(&curve), (None, None));
    }

    // ---------- catch-up (interpolation) ----------

    /// L1 (rework-2): no 15-min ceiling — the crossing time is linearly interpolated between the
    /// bracketing fine steps. `g = [0.6, 0.6, …]`, `max_heat_kw = 1.0`: cumulative response is 0.6
    /// at 15 min and 1.2 at 30 min, crossing +1 K at `15 + (1.0-0.6)/(1.2-0.6)*15 = 25` min exactly
    /// — a ceiling would instead report 30.
    #[test]
    fn step_response_minutes_interpolates_between_fine_steps() {
        let g = vec![0.6; 4];
        let minutes = step_response_minutes(&g, 1.0).expect("reaches +1K within the kernel");
        assert!((minutes - 25.0).abs() < 1e-9, "minutes={minutes}");
    }

    #[test]
    fn step_response_minutes_censored_when_never_reached() {
        let g = vec![0.01; 4]; // cumulative only reaches 0.04, never +1K
        assert_eq!(step_response_minutes(&g, 1.0), None);
    }

    // ---------- fixture round-trip ----------

    #[test]
    fn dump_round_trips_through_json() {
        let parts = DumpParts {
            drive: DriveSeries {
                outside: vec![TimeSample {
                    time: t("2026-01-10T05:00:00Z"),
                    value: -2.5,
                }],
                cloud: vec![],
                direct: vec![],
                diffuse: vec![],
                shortwave: vec![],
            },
            zone_series: HashMap::from([(
                "livingroom".to_string(),
                vec![TimeSample {
                    time: t("2026-01-10T05:00:00Z"),
                    value: 21.0,
                }],
            )]),
            relay_events: HashMap::from([(
                "livingroom".to_string(),
                vec![TimeSample {
                    time: t("2026-01-10T04:15:00Z"),
                    value: 1.0,
                }],
            )]),
            relay_state_before: HashMap::from([(
                "livingroom".to_string(),
                TimeSample {
                    time: t("2026-01-09T00:00:00Z"),
                    value: 0.0,
                },
            )]),
        };
        let dump = dump_from_parts(&parts);
        assert_eq!(dump.schema, SCHEMA);
        let json = serde_json::to_string(&dump).unwrap();
        let back: ReplayDump = serde_json::from_str(&json).unwrap();
        let round = parts_from_dump(&back);
        assert_eq!(round.drive.outside.len(), 1);
        assert_eq!(round.zone_series["livingroom"].len(), 1);
        assert_eq!(round.relay_events["livingroom"].len(), 1);
        assert_eq!(round.relay_state_before["livingroom"].value, 0.0);
    }

    // ---------- CLI parsing ----------

    #[test]
    fn parse_args_rejects_non_hour_aligned_start() {
        let args: Vec<String> = ["--start", "2026-01-10T05:30:00Z", "--days", "3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_rejects_days_out_of_range() {
        let args: Vec<String> = ["--start", "2026-01-10T05:00:00Z", "--days", "8"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_defaults() {
        let args: Vec<String> = ["--start", "2026-01-10T05:00:00Z", "--days", "3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_args(&args).unwrap();
        assert_eq!(a.warmup_h, 48);
        assert_eq!(a.model_path, "model.json5");
        assert_eq!(a.config_path, "config.json5");
        assert!(!a.legacy_duty);
        assert!((a.on_duty - 0.7).abs() < 1e-9);
        assert_eq!(a.min_on_h, 2);
        assert_eq!(a.off_h, 4);
        assert!((a.off_duty - 0.1).abs() < 1e-9);
    }
}
