//! Two small JSON stores the `features` collector maintains, both opened once at startup in
//! [`crate::web::AppState`]:
//! - the **feature samples** (`MPC_FEATURE_SAMPLES_STORE`): sampled A/B records nothing can
//!   recompute later (today the comfort-pull counterfactuals) — the data behind the "new features"
//!   page (`/api/features`, [`crate::features`]);
//! - the **accuracy history** (`MPC_ACCURACY_HISTORY_STORE`): per UTC day, per zone and lead bin,
//!   the plan's forward-temperature-prediction error — a write-only, long-lived archive of how the
//!   thermal model's accuracy moved across releases (nothing serves it; it is read off the disk).
//!
//! Neither is ever written on the planning path: every disk write, Influx read and counterfactual
//! solve happens on [`run_collector`], a separately supervised task, like the ledger scorer.
//! Strictly read-only towards InfluxDB.
//!
//! Module shape: the generic store, the two data types with their pure logic, the pure day-scoring
//! and scheduling helpers, then the bounded IO and the collector task.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::app::{current_plan, PlanExtras, PlanReport};
use crate::estimate::{hour_key, keep_first_by_hour};
use crate::forecast_validation::{accumulate_leads, load_snapshots, ErrAcc, Snapshot};
use crate::optimize::config::ControlConfig;
use crate::source::SourceClients;
use crate::tools::{mean, rmse, round_to};
use crate::web::AppState;

// ============================================================ Generic JSON store

/// A mutex-guarded value persisted as one JSON file by temp-file + rename (atomic on one
/// filesystem). A file that exists but cannot be read or parsed is NEVER overwritten: the store
/// starts empty in memory and the first [`Self::persist`] moves the broken file to `<path>.corrupt`
/// first (the ledger's policy — one bad byte must not cost the history, and a read-only caller that
/// never persists can never rename anything).
pub struct JsonStore<T> {
    path: PathBuf,
    label: &'static str,
    value: Mutex<T>,
    write: Mutex<()>,
    dirty: AtomicBool,
    quarantine_on_persist: AtomicBool,
}

impl<T> JsonStore<T>
where
    T: Serialize + DeserializeOwned + Default + Clone,
{
    /// Open the store at `$env_var`, else `default_file`. The path is resolved once, here.
    pub fn open(env_var: &str, default_file: &str, label: &'static str) -> Self {
        let path = std::env::var(env_var).unwrap_or_else(|_| default_file.to_string());
        Self::open_at(PathBuf::from(path), label)
    }

    pub fn open_at(path: PathBuf, label: &'static str) -> Self {
        let (value, quarantine) = Self::load(&path, label);
        Self {
            path,
            label,
            value: Mutex::new(value),
            write: Mutex::new(()),
            dirty: AtomicBool::new(false),
            quarantine_on_persist: AtomicBool::new(quarantine),
        }
    }

    /// The stored value and whether the on-disk file needs quarantining at the next persist. A
    /// missing file is the normal empty case.
    fn load(path: &Path, label: &str) -> (T, bool) {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (T::default(), false),
            Err(e) => {
                eprintln!(
                    "[{label}] ERROR: store at {} could not be read ({e}) — starting EMPTY in \
                     memory; the file is moved to {}.corrupt before the next write",
                    path.display(),
                    path.display()
                );
                return (T::default(), true);
            }
        };
        match serde_json::from_str::<T>(&raw) {
            Ok(value) => (value, false),
            Err(e) => {
                eprintln!(
                    "[{label}] ERROR: store at {} is unparseable ({e}) — starting EMPTY in \
                     memory; the file is moved to {}.corrupt before the next write",
                    path.display(),
                    path.display()
                );
                (T::default(), true)
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.value.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mutate the value in place; `f` returns whether it changed anything (and so needs persisting).
    pub fn update(&self, f: impl FnOnce(&mut T) -> bool) {
        if f(&mut self.lock()) {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> T {
        self.lock().clone()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Write the current value to disk (see the type docs). A failed write leaves the store dirty so
    /// the next collector pass retries.
    pub fn persist(&self) -> Result<()> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if self.quarantine_on_persist.load(Ordering::Relaxed) {
            let aside = format!("{}.corrupt", self.path.display());
            match std::fs::rename(&self.path, &aside) {
                Ok(()) => eprintln!(
                    "[{}] quarantined the broken store to {aside} before writing fresh state",
                    self.label
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("quarantining broken store to {aside}"))
                }
            }
            self.quarantine_on_persist.store(false, Ordering::Relaxed);
        }
        self.dirty.store(false, Ordering::Relaxed);
        if let Err(e) = self.write_atomic() {
            self.dirty.store(true, Ordering::Relaxed);
            return Err(e);
        }
        Ok(())
    }

    fn write_atomic(&self) -> Result<()> {
        let json = serde_json::to_string(&*self.lock())
            .with_context(|| format!("serializing the {} store", self.label))?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating the {} directory", self.label))?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, json).with_context(|| format!("writing the {} store", self.label))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing the {} store", self.label))?;
        Ok(())
    }
}

// ============================================================ Accuracy history

/// Lead bins (hours, half-open; the last includes its upper edge) of the persisted history — the
/// resolved scorecard's five merged to four, so each cell keeps a usable sample count per day.
pub const HIST_BINS_H: [(f64, f64); 4] = [(0.0, 6.0), (6.0, 12.0), (12.0, 24.0), (24.0, 36.0)];
/// Days of accuracy history kept.
pub const ACCURACY_RETENTION_DAYS: i64 = 400;

/// One cell of the history: `[n, rmse_k, mean_bias_k]` (a JSON array — the file holds ~150 cells a
/// day). Bias is predicted − measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BinStat(pub u32, pub f64, pub f64);

impl BinStat {
    fn from_acc(acc: &ErrAcc) -> BinStat {
        if acc.n == 0 {
            return BinStat(0, 0.0, 0.0);
        }
        BinStat(
            acc.n as u32,
            round_to(rmse(acc.sq, acc.n), 4),
            round_to(mean(acc.err, acc.n), 4),
        )
    }
}

/// Distinct anchor hours a UTC day's snapshots must cover before it is scored: a partially covered
/// day (a restart, the store's first day) would bias the daily figure, so it is skipped, not stored.
pub const MIN_ANCHOR_HOURS: usize = 20;
/// Hours after an anchor day's midnight until every target of its snapshots (anchor + 36 h, so up to
/// the end of the next day + 12 h) has elapsed and its measured hour has landed.
pub const ACCURACY_SETTLE_HOURS: i64 = 2 * 24 + 13;

/// One zone's day: the plan's own prediction error per lead bin ([`HIST_BINS_H`] order), over all
/// that day's snapshots. Stored under the key `scaled` so the file stays readable by older binaries
/// (a rollback); the retired arm keys older releases wrote beside it are dropped on the next write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneAccuracy {
    #[serde(rename = "scaled")]
    pub plan: Vec<BinStat>,
}

/// One UTC ANCHOR day: every prediction made by a snapshot anchored in `[day 00:00, day+1 00:00)`,
/// scored against everything it predicted. Keying by when the prediction was MADE (not when its
/// target fell) is what lets a release split the history cleanly: a day is wholly "before" or
/// wholly "after" the model that made its predictions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayAccuracy {
    pub zones: BTreeMap<String, ZoneAccuracy>,
}

/// The daily forecast-accuracy history, keyed by UTC anchor date (`YYYY-MM-DD`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccuracyHistory {
    pub days: BTreeMap<String, DayAccuracy>,
}

pub fn date_key(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

impl AccuracyHistory {
    pub fn has_day(&self, day: NaiveDate) -> bool {
        self.days.contains_key(&date_key(day))
    }

    /// Add a day; a day already present is left as it is (a recorded day is never rewritten).
    /// Drops days past [`ACCURACY_RETENTION_DAYS`]. Returns whether anything changed.
    pub fn insert_day(&mut self, day: NaiveDate, value: DayAccuracy, now: DateTime<Utc>) -> bool {
        let key = date_key(day);
        let added = !self.days.contains_key(&key);
        if added {
            self.days.insert(key, value);
        }
        let before = self.days.len();
        let cutoff = date_key((now - Duration::days(ACCURACY_RETENTION_DAYS)).date_naive());
        self.days.retain(|d, _| *d >= cutoff);
        added || self.days.len() != before
    }
}

fn utc_midnight(day: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight is valid"))
}

/// Distinct anchor hours of the snapshots anchored on UTC `day`.
fn anchor_hours(snapshots: &[Snapshot], day: NaiveDate) -> usize {
    snapshots
        .iter()
        .filter(|s| s.anchored_at.date_naive() == day)
        .map(|s| hour_key(s.anchored_at))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Score every snapshot anchored on UTC `day` against the measured hourly series (keyed by
/// [`hour_key`], one per zone), targets up to `now`, per zone per [`HIST_BINS_H`] bin. A zone with no
/// scoreable point is omitted; a day with none at all is an empty [`DayAccuracy`]. Pure.
pub fn score_day(
    snapshots: &[Snapshot],
    measured: &HashMap<String, HashMap<i64, f64>>,
    day: NaiveDate,
    now: DateTime<Utc>,
) -> DayAccuracy {
    let of_day = snapshots
        .iter()
        .filter(|s| s.anchored_at.date_naive() == day);
    let acc = accumulate_leads(
        of_day,
        measured,
        (DateTime::<Utc>::MIN_UTC, now),
        &HIST_BINS_H,
    );
    let mut zone_names: Vec<&String> = acc.keys().map(|(_, zone)| zone).collect();
    zone_names.sort();
    zone_names.dedup();
    let zones = zone_names
        .into_iter()
        .map(|zone| {
            let plan = (0..HIST_BINS_H.len())
                .map(|b| {
                    acc.get(&(b, zone.clone()))
                        .map_or(BinStat(0, 0.0, 0.0), BinStat::from_acc)
                })
                .collect();
            (zone.clone(), ZoneAccuracy { plan })
        })
        .collect();
    DayAccuracy { zones }
}

/// UTC anchor days not yet in `history` that are settled (every target of the day's snapshots has
/// elapsed and its measured hour landed: `now` is [`ACCURACY_SETTLE_HOURS`] past the day's start) and
/// whose snapshots cover at least [`MIN_ANCHOR_HOURS`] distinct anchor hours. Ascending: the oldest
/// missing day first, so a restart backfills in order. A partially covered day is never due. Pure.
pub fn accuracy_days_due(
    snapshots: &[Snapshot],
    history: &AccuracyHistory,
    now: DateTime<Utc>,
) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = snapshots
        .iter()
        .map(|s| s.anchored_at.date_naive())
        .filter(|d| now >= utc_midnight(*d) + Duration::hours(ACCURACY_SETTLE_HOURS))
        .filter(|d| !history.has_day(*d))
        .collect();
    days.sort();
    days.dedup();
    days.retain(|d| anchor_hours(snapshots, *d) >= MIN_ANCHOR_HOURS);
    days
}

// ============================================================ Feature samples

/// Days of samples kept.
pub const SAMPLE_RETENTION_DAYS: i64 = 60;

/// One hourly comfort-targets A/B row: the live plan (`comfort.pull_eur_per_kh` as configured)
/// against a plan solved with the pull 0 on the same inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PullSample {
    pub t: DateTime<Utc>,
    /// Why no counterfactual was solved (`no heat planned`, `busy`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// The two arms are known identical without solving (no planned heat: the pull-off arm can
    /// only heat less).
    #[serde(default)]
    pub identical: bool,
    #[serde(default)]
    pub cost_with_eur: Option<f64>,
    #[serde(default)]
    pub cost_without_eur: Option<f64>,
    #[serde(default)]
    pub heat_with_kwh: BTreeMap<String, f64>,
    #[serde(default)]
    pub heat_without_kwh: BTreeMap<String, f64>,
    /// K·h each pulled zone sat below its target in the live plan (the pull-off plan does not
    /// compute the figure, so it has no counterpart).
    #[serde(default)]
    pub pull_kh_with: BTreeMap<String, f64>,
}

/// The sampled A/B records, bounded by [`SAMPLE_RETENTION_DAYS`]. Kinds a file from an older
/// release still carries are ignored on load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureSamples {
    #[serde(default)]
    pub pull: Vec<PullSample>,
}

impl FeatureSamples {
    /// Push `s` unless the newest row is already in the same UTC hour (or later): at most one row
    /// per hour, O(1).
    pub fn push_pull(&mut self, s: PullSample) -> bool {
        if self
            .pull
            .last()
            .is_some_and(|last| hour_key(last.t) >= hour_key(s.t))
        {
            return false;
        }
        self.pull.push(s);
        true
    }

    /// Drop rows older than [`SAMPLE_RETENTION_DAYS`]; returns whether anything went.
    pub fn prune(&mut self, now: DateTime<Utc>) -> bool {
        let cutoff = now - Duration::days(SAMPLE_RETENTION_DAYS);
        let before = self.pull.len();
        self.pull.retain(|s| s.t >= cutoff);
        before != self.pull.len()
    }
}

/// The few figures of a plan the comfort-targets A/B compares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanFigures {
    pub cost_eur: f64,
    pub heat_kwh: BTreeMap<String, f64>,
    pub pull_kh: BTreeMap<String, f64>,
}

impl PlanFigures {
    pub fn of(plan: &PlanReport) -> PlanFigures {
        let mut heat_kwh: BTreeMap<String, f64> = BTreeMap::new();
        for block in &plan.timeline {
            let dt_h = f64::from(block.dt_minutes) / 60.0;
            for (zone, kw) in &block.heat_kw {
                *heat_kwh.entry(zone.clone()).or_insert(0.0) += kw * dt_h;
            }
        }
        PlanFigures {
            cost_eur: plan.total_cost_eur,
            heat_kwh,
            pull_kh: plan.pull_kh.iter().map(|(z, v)| (z.clone(), *v)).collect(),
        }
    }

    fn total_heat_kwh(&self) -> f64 {
        self.heat_kwh.values().sum()
    }
}

/// Below this total planned heat (kWh over the horizon) a plan counts as "no heat planned".
const NO_HEAT_KWH: f64 = 0.01;

/// What the hourly comfort-targets A/B does this hour, decided before any solve.
#[derive(Debug, Clone, PartialEq)]
pub enum PullStep {
    /// The pull is 0 everywhere (`comfort.pull_eur_per_kh × priority`): the feature is off, record nothing.
    Off,
    /// Record a row without solving.
    Record(PullSample),
    /// Solve the pull-off counterfactual.
    Solve,
}

/// The skip rules, in order: feature off; no usable live plan; no planned heat (the pull-off arm
/// can only heat less, so the arms are identical); on-demand solver busy. Pure.
pub fn pull_step(
    now: DateTime<Utc>,
    pull_weights: impl IntoIterator<Item = f64>,
    live: Option<(&PlanFigures, bool)>,
    solver_idle: bool,
) -> PullStep {
    if pull_weights.into_iter().all(|v| v <= 0.0) {
        return PullStep::Off;
    }
    let skipped = |why: &str, identical: bool| {
        PullStep::Record(PullSample {
            t: now,
            skipped: Some(why.to_string()),
            identical,
            ..Default::default()
        })
    };
    let Some((figures, usable)) = live else {
        return skipped("no live plan", false);
    };
    if !usable {
        return skipped("live plan degraded", false);
    }
    if figures.total_heat_kwh() < NO_HEAT_KWH {
        return skipped("no heat planned", true);
    }
    if !solver_idle {
        return skipped("busy", false);
    }
    PullStep::Solve
}

// ============================================================ Bounded IO

/// Hours of measured series one anchor day needs: the day itself plus its last anchor's 36 h.
const ANCHOR_DAY_SPAN_H: i64 = 24 + 36;
const READ_TIMEOUT: StdDuration = StdDuration::from_secs(20);
/// Pause between per-zone reads of one scoring day (server-friendly).
const ZONE_READ_PAUSE: StdDuration = StdDuration::from_millis(250);
/// Days scored per pass, so a cold-start backfill trickles in.
const MAX_ACCURACY_DAYS_PER_PASS: usize = 2;
/// Wait this long before retrying a day whose read failed.
const RETRY_AFTER: StdDuration = StdDuration::from_secs(3600);
/// The counterfactual plan's own bound (the strict pipeline's 32 s plus the pre-solve reads).
const COUNTERFACTUAL_TIMEOUT: StdDuration = StdDuration::from_secs(60);

/// One UTC day's measured hourly zone temperatures (≤ 1 day, one zone per query). `None` when any
/// zone's read FAILED — a day scored from a partial read would be recorded as final.
async fn read_day_measured(
    db: &SourceClients,
    zones: &[String],
    day: NaiveDate,
    span: Duration,
) -> Option<HashMap<String, HashMap<i64, f64>>> {
    let start = utc_midnight(day);
    let (start_s, stop_s) = (start.to_rfc3339(), (start + span).to_rfc3339());
    let mut measured = HashMap::new();
    for zone in zones {
        match tokio::time::timeout(
            READ_TIMEOUT,
            db.read_zone_temperature_series(zone, &start_s, &stop_s, "1h"),
        )
        .await
        {
            Ok(Ok(series)) => {
                measured.insert(zone.clone(), keep_first_by_hour(&series));
            }
            Ok(Err(e)) => {
                eprintln!("[features] {day}: zone {zone:?} read failed ({e:#}); will retry");
                return None;
            }
            Err(_) => {
                eprintln!("[features] {day}: zone {zone:?} read timed out; will retry");
                return None;
            }
        }
        tokio::time::sleep(ZONE_READ_PAUSE).await;
    }
    Some(measured)
}

async fn accuracy_pass(
    state: &AppState,
    now: DateTime<Utc>,
    retry_at: &mut HashMap<String, tokio::time::Instant>,
) {
    let Ok(snapshots) = tokio::task::spawn_blocking(load_snapshots).await else {
        return;
    };
    let history = state.accuracy_history.snapshot();
    let mut scored = 0;
    for day in accuracy_days_due(&snapshots, &history, now) {
        if scored >= MAX_ACCURACY_DAYS_PER_PASS {
            break;
        }
        let key = format!("accuracy:{day}");
        if retry_at
            .get(&key)
            .is_some_and(|at| *at > tokio::time::Instant::now())
        {
            continue;
        }
        let mut zones: Vec<String> = snapshots
            .iter()
            .filter(|s| s.anchored_at.date_naive() == day)
            .flat_map(|s| s.zones.keys().cloned())
            .collect();
        zones.sort();
        zones.dedup();
        // Targets reach 36 h past the last anchor of the day: one read of day 00:00 .. +60 h.
        let Some(measured) =
            read_day_measured(&state.db, &zones, day, Duration::hours(ANCHOR_DAY_SPAN_H)).await
        else {
            retry_at.insert(key, tokio::time::Instant::now() + RETRY_AFTER);
            continue;
        };
        let scored_day = score_day(&snapshots, &measured, day, now);
        let n_zones = scored_day.zones.len();
        state
            .accuracy_history
            .update(|h| h.insert_day(day, scored_day, now));
        println!("[features] accuracy history: scored {day} ({n_zones} zones)");
        scored += 1;
    }
}

/// `config` with the comfort pull zeroed — the counterfactual arm.
fn without_pull(config: &ControlConfig) -> ControlConfig {
    let mut off = config.clone();
    off.heating.comfort.pull_eur_per_kh = 0.0;
    off
}

/// The hourly comfort-targets A/B (see [`pull_step`]). The counterfactual is ONE on-demand-path
/// plan (never the loop's permit) over the loop's own slow-input cache and kernels, solved only
/// when the on-demand solver is idle; the live plan is the loop's latest published one, so a block
/// rollover between the two (their block 0 would differ) discards the sample.
async fn pull_pass(state: &AppState, now: DateTime<Utc>) {
    let latest = crate::web::lock_latest(state);
    let live = latest.as_ref().map(|tp| {
        (
            PlanFigures::of(&tp.plan),
            !tp.plan.degraded && !tp.plan.relaxed,
        )
    });
    let step = pull_step(
        now,
        state.config.heating.zones.keys().map(|z| {
            state.config.heating.comfort.pull_eur_per_kh * state.config.heating.comfort.priority(z)
        }),
        live.as_ref().map(|(f, usable)| (f, *usable)),
        crate::app::on_demand_solver_idle(),
    );
    let sample = match step {
        PullStep::Off => return,
        PullStep::Record(sample) => sample,
        PullStep::Solve => {
            let (Some(latest), Some((live_figures, _))) = (&latest, &live) else {
                return;
            };
            let cache = state
                .plan_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let off_config = without_pull(&state.config);
            let pins = state
                .plan_pins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_default();
            let extras = PlanExtras {
                cache: cache.as_deref(),
                committed_heat: pins.committed_heat,
                load_run_hours: pins.load_run_hours,
                room_mode_prior: state.room_modes.snapshot(),
                kernels: Some(state.kernels.clone()),
                kalman: state.kalman.get().cloned(),
                ..Default::default()
            };
            let solved = tokio::time::timeout(
                COUNTERFACTUAL_TIMEOUT,
                current_plan(
                    &state.db,
                    &state.net,
                    &state.ss,
                    &off_config,
                    state.latitude,
                    state.longitude,
                    extras,
                ),
            )
            .await;
            let skipped = |why: &str| PullSample {
                t: now,
                skipped: Some(why.to_string()),
                ..Default::default()
            };
            match solved {
                Ok(Ok(plan)) if plan.degraded || plan.relaxed => skipped("counterfactual relaxed"),
                Ok(Ok(plan))
                    if plan.timeline.first().map(|b| b.t)
                        != latest.plan.timeline.first().map(|b| b.t) =>
                {
                    skipped("block rolled")
                }
                Ok(Ok(plan)) => {
                    let without = PlanFigures::of(&plan);
                    PullSample {
                        t: now,
                        skipped: None,
                        identical: false,
                        cost_with_eur: Some(live_figures.cost_eur),
                        cost_without_eur: Some(without.cost_eur),
                        heat_with_kwh: live_figures.heat_kwh.clone(),
                        heat_without_kwh: without.heat_kwh,
                        pull_kh_with: live_figures.pull_kh.clone(),
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("[features] pull counterfactual failed: {e:#}");
                    skipped("counterfactual failed")
                }
                Err(_) => skipped("counterfactual timed out"),
            }
        }
    };
    state.feature_samples.update(|s| s.push_pull(sample));
}

// ============================================================ The collector task

const STARTUP_DELAY: StdDuration = StdDuration::from_secs(150);
const PASS_INTERVAL: StdDuration = StdDuration::from_secs(10 * 60);

async fn persist_if_dirty<T>(store: &Arc<JsonStore<T>>)
where
    T: Serialize + DeserializeOwned + Default + Clone + Send + 'static,
{
    if !store.is_dirty() {
        return;
    }
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || store.persist()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[features] persist failed: {e:#}"),
        Err(e) => eprintln!("[features] persist task panicked: {e}"),
    }
}

/// The page's own task (spawned by `web::serve` beside the loop and the ledger scorer, supervised the
/// same way): every [`PASS_INTERVAL`] append the accuracy-history days that have come due, run the
/// hourly comfort-targets counterfactual, prune, and persist. Every failure is
/// logged and retried later; a wedged DB degrades this task alone.
pub async fn run_collector(state: Arc<AppState>) {
    tokio::time::sleep(STARTUP_DELAY).await;
    let mut interval = tokio::time::interval(PASS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut retry_at: HashMap<String, tokio::time::Instant> = HashMap::new();
    let mut last_pull_hour: Option<i64> = None;
    loop {
        interval.tick().await;
        let now = Utc::now();
        accuracy_pass(&state, now, &mut retry_at).await;
        if last_pull_hour != Some(hour_key(now)) {
            last_pull_hour = Some(hour_key(now));
            pull_pass(&state, now).await;
        }
        state.feature_samples.update(|s| s.prune(now));
        persist_if_dirty(&state.accuracy_history).await;
        persist_if_dirty(&state.feature_samples).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn snapshot(anchor: &str, hours: i64, predicted: f64) -> Snapshot {
        let anchored_at = utc(anchor);
        serde_json::from_value(serde_json::json!({
            "anchored_at": anchored_at,
            "block_ends": (1..=hours).map(|h| anchored_at + Duration::hours(h)).collect::<Vec<_>>(),
            "zones": { "lr": vec![predicted; hours as usize] },
        }))
        .unwrap()
    }

    fn measured_flat(from: &str, hours: i64, value: f64) -> HashMap<String, HashMap<i64, f64>> {
        let first = hour_key(utc(from));
        HashMap::from([(
            "lr".to_string(),
            (0..hours).map(|h| (first + h, value)).collect(),
        )])
    }

    fn pull_row(t: &str) -> PullSample {
        PullSample {
            t: utc(t),
            cost_with_eur: Some(1.2),
            cost_without_eur: Some(1.0),
            ..Default::default()
        }
    }

    // ---- store

    #[test]
    fn store_round_trips_across_a_reopen_and_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("samples.json");
        let store = JsonStore::<FeatureSamples>::open_at(path.clone(), "test");
        assert!(!store.is_dirty());
        store.update(|s| s.push_pull(pull_row("2026-10-03T10:00:00Z")));
        assert!(store.is_dirty());
        store.persist().unwrap();
        assert!(!store.is_dirty());
        let reopened = JsonStore::<FeatureSamples>::open_at(path, "test");
        let samples = reopened.snapshot();
        assert_eq!(samples.pull.len(), 1);
        assert_eq!(samples.pull[0].cost_with_eur, Some(1.2));
    }

    #[test]
    fn store_update_that_changes_nothing_stays_clean() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::<FeatureSamples>::open_at(dir.path().join("s.json"), "test");
        store.update(|_| false);
        assert!(!store.is_dirty());
    }

    #[test]
    fn a_broken_store_is_quarantined_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let store = JsonStore::<AccuracyHistory>::open_at(path.clone(), "test");
        assert!(store.snapshot().days.is_empty(), "starts empty in memory");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "opening alone never touches the broken file"
        );
        store.update(|h| {
            h.insert_day(
                day("2026-10-02"),
                DayAccuracy::default(),
                utc("2026-10-03T00:00:00Z"),
            )
        });
        store.persist().unwrap();
        let aside = format!("{}.corrupt", path.display());
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ this is not json",
            "the broken bytes are preserved"
        );
        let reopened = JsonStore::<AccuracyHistory>::open_at(path, "test");
        assert!(reopened.snapshot().has_day(day("2026-10-02")));
    }

    #[test]
    fn a_missing_file_is_an_empty_store_without_a_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.json");
        let store = JsonStore::<AccuracyHistory>::open_at(path.clone(), "test");
        store.update(|_| true);
        store.persist().unwrap();
        assert!(path.exists());
        assert!(!std::path::Path::new(&format!("{}.corrupt", path.display())).exists());
    }

    // ---- accuracy history

    #[test]
    fn accuracy_history_prunes_past_retention_and_never_rewrites_a_day() {
        let now = utc("2026-10-03T12:00:00Z");
        let mut h = AccuracyHistory::default();
        let mut first = DayAccuracy::default();
        first.zones.insert("a".into(), ZoneAccuracy::default());
        assert!(h.insert_day(day("2026-10-01"), first.clone(), now));
        // A second insert for the same day keeps the original.
        assert!(!h.insert_day(day("2026-10-01"), DayAccuracy::default(), now));
        assert_eq!(h.days["2026-10-01"], first);
        // A day older than the retention window is dropped on the next insert.
        h.days.insert("2024-01-01".into(), DayAccuracy::default());
        h.insert_day(day("2026-10-02"), DayAccuracy::default(), now);
        assert!(!h.days.contains_key("2024-01-01"));
        assert!(h.days.contains_key("2026-10-01") && h.days.contains_key("2026-10-02"));
    }

    #[test]
    fn a_history_day_stored_with_the_retired_arms_still_loads() {
        let raw = r#"{"days":{"2026-10-05":{"zones":{"entrance":{
            "scaled":[[24,0.31,-0.1],[24,0.4,0.0],[48,0.5,0.2],[12,0.6,0.3]],
            "unscaled":[[24,0.3,-0.1],[24,0.4,0.0],[48,0.5,0.2],[12,0.6,0.3]],
            "scaled_ab":[[24,0.3,-0.1],[24,0.4,0.0],[48,0.5,0.2],[12,0.6,0.3]],
            "night":{"sunny":[5,0.2,-0.06],"other":[3,0.1,0.0]},
            "night_unscaled":{"sunny":[5,0.2,-0.09],"other":[3,0.1,0.0]},
            "night_scaled_ab":{"sunny":[5,0.2,-0.06],"other":[3,0.1,0.0]}}}}}}"#;
        let h: AccuracyHistory = serde_json::from_str(raw).unwrap();
        let entrance = &h.days["2026-10-05"].zones["entrance"];
        assert_eq!(entrance.plan.len(), HIST_BINS_H.len());
        assert_eq!(entrance.plan[0], BinStat(24, 0.31, -0.1));
        // Written back under the key older binaries read, without the retired arms.
        let json = serde_json::to_string(entrance).unwrap();
        assert!(json.starts_with("{\"scaled\":[["), "{json}");
        assert!(
            !json.contains("unscaled") && !json.contains("scaled_ab") && !json.contains("night")
        );
    }

    #[test]
    fn score_day_scores_each_zone_per_lead_bin() {
        // Two snapshots anchored 2026-10-01 18:00 and 20:00 predict a flat 21.5 against 21.0.
        let snaps = [
            snapshot("2026-10-01T18:00:00Z", 30, 21.5),
            snapshot("2026-10-01T20:00:00Z", 30, 21.5),
        ];
        let measured = measured_flat("2026-10-01T00:00:00Z", 72, 21.0);
        let d = score_day(
            &snaps,
            &measured,
            day("2026-10-01"),
            utc("2026-10-04T00:00:00Z"),
        );
        let lr = &d.zones["lr"].plan;
        assert_eq!(lr.len(), HIST_BINS_H.len());
        assert_eq!(
            lr.iter().map(|b| b.0).sum::<u32>(),
            60,
            "both snapshots, 30 hourly targets each"
        );
        assert_eq!(
            lr[0].0, 10,
            "leads 1-5 h of each snapshot (the bin is half-open)"
        );
        for b in lr.iter().filter(|b| b.0 > 0) {
            assert!((b.1 - 0.5).abs() < 1e-9 && (b.2 - 0.5).abs() < 1e-9);
        }
    }

    #[test]
    fn score_day_scores_only_snapshots_anchored_on_that_day() {
        // One snapshot anchored 2026-10-01 23:00 (its single target ends 10-02 00:00).
        let snaps = [snapshot("2026-10-01T23:00:00Z", 1, 21.5)];
        let measured = measured_flat("2026-10-01T00:00:00Z", 48, 21.0);
        let now = utc("2026-10-04T00:00:00Z");
        let d1 = score_day(&snaps, &measured, day("2026-10-01"), now);
        let d2 = score_day(&snaps, &measured, day("2026-10-02"), now);
        assert_eq!(
            d1.zones["lr"].plan.iter().map(|b| b.0).sum::<u32>(),
            1,
            "scored on its ANCHOR day even though the target is on the next"
        );
        assert!(d2.zones.is_empty());
    }

    #[test]
    fn score_day_skips_targets_that_have_not_elapsed() {
        let snaps = [snapshot("2026-10-01T20:00:00Z", 30, 21.5)];
        let measured = measured_flat("2026-10-01T00:00:00Z", 72, 21.0);
        let d = score_day(
            &snaps,
            &measured,
            day("2026-10-01"),
            utc("2026-10-01T23:00:00Z"),
        );
        // Targets 21:00, 22:00, 23:00.
        assert_eq!(d.zones["lr"].plan.iter().map(|b| b.0).sum::<u32>(), 3);
    }

    #[test]
    fn score_day_with_no_measurements_is_an_empty_day() {
        let snaps = [snapshot("2026-10-01T20:00:00Z", 30, 21.5)];
        let d = score_day(
            &snaps,
            &HashMap::new(),
            day("2026-10-01"),
            utc("2026-10-04T00:00:00Z"),
        );
        assert!(d.zones.is_empty());
    }

    /// 24 hourly snapshots (3 targets each) anchored through `date`.
    fn full_day_snapshots(date: &str, hours: usize) -> Vec<Snapshot> {
        (0..hours)
            .map(|h| snapshot(&format!("{date}T{h:02}:00:00Z"), 3, 21.0))
            .collect()
    }

    #[test]
    fn accuracy_days_due_needs_settled_targets_and_adequate_anchor_coverage() {
        let mut snaps = full_day_snapshots("2026-10-01", 24);
        snaps.extend(full_day_snapshots("2026-10-02", 12)); // a half-covered day
        let mut history = AccuracyHistory::default();
        // Anchor day 10-01's last targets are 10-02 ~03:00; the rule waits until 10-03 13:00.
        assert!(accuracy_days_due(&snaps, &history, utc("2026-10-03T12:59:00Z")).is_empty());
        let now = utc("2026-10-03T13:00:00Z");
        assert_eq!(
            accuracy_days_due(&snaps, &history, now),
            vec![day("2026-10-01")],
            "the 12-hour day is skipped, not stored"
        );
        // Even long after, a day with < MIN_ANCHOR_HOURS anchors is never due.
        assert_eq!(
            accuracy_days_due(&snaps, &history, utc("2026-10-09T00:00:00Z")),
            vec![day("2026-10-01")]
        );
        // A recorded day is not due again.
        history.insert_day(day("2026-10-01"), DayAccuracy::default(), now);
        assert!(accuracy_days_due(&snaps, &history, now).is_empty());
        assert!(accuracy_days_due(&[], &history, now).is_empty());
        // Exactly MIN_ANCHOR_HOURS distinct anchor hours is enough; one fewer is not.
        let edge = full_day_snapshots("2026-10-05", MIN_ANCHOR_HOURS);
        let later = utc("2026-10-09T00:00:00Z");
        assert_eq!(
            accuracy_days_due(&edge, &AccuracyHistory::default(), later),
            vec![day("2026-10-05")]
        );
        assert!(accuracy_days_due(
            &full_day_snapshots("2026-10-05", MIN_ANCHOR_HOURS - 1),
            &AccuracyHistory::default(),
            later
        )
        .is_empty());
    }

    // ---- samples

    #[test]
    fn pull_samples_keep_at_most_one_per_hour() {
        let mut s = FeatureSamples::default();
        assert!(s.push_pull(pull_row("2026-10-03T10:00:20Z")));
        assert!(!s.push_pull(pull_row("2026-10-03T10:59:00Z")), "same hour");
        assert!(
            !s.push_pull(pull_row("2026-10-03T09:30:00Z")),
            "clock stepped back"
        );
        assert!(s.push_pull(pull_row("2026-10-03T11:00:05Z")));
        assert_eq!(s.pull.len(), 2);
    }

    #[test]
    fn samples_prune_drops_rows_past_retention() {
        let now = utc("2026-12-01T00:00:00Z");
        let mut s = FeatureSamples::default();
        s.push_pull(pull_row("2026-09-01T10:00:00Z")); // 91 days old
        s.push_pull(pull_row("2026-11-30T10:00:00Z"));
        assert!(s.prune(now));
        assert_eq!(s.pull.len(), 1);
        assert!(!s.prune(now), "nothing left to drop");
    }

    #[test]
    fn a_samples_file_with_the_retired_kinds_still_loads() {
        let raw = r#"{
            "terminal":[{"t":"2026-10-03T10:00:00Z","outlook_eur_kwh":0.25,"legacy_eur_kwh":0.04,"source":"outlook","end_soc_kwh":7.0}],
            "nowcast":[{"t":"2026-10-03T10:00:00Z","applied":true,"ratio":1.2}],
            "relay_duty":[{"date":"2026-10-02","zones":{"kitchen":[1.0,2.0]}}],
            "pull":[{"t":"2026-10-03T10:00:00Z","cost_with_eur":1.2,"cost_without_eur":1.0}]
        }"#;
        let parsed: FeatureSamples = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.pull.len(), 1);
        assert_eq!(parsed.pull[0].cost_without_eur, Some(1.0));
        let json = serde_json::to_string(&parsed).unwrap();
        assert!(!json.contains("terminal") && !json.contains("relay_duty"));
        let empty: FeatureSamples = serde_json::from_str("{}").unwrap();
        assert!(empty.pull.is_empty());
    }

    // ---- comfort-targets A/B skip rules

    fn figures(heat: f64) -> PlanFigures {
        PlanFigures {
            cost_eur: 1.0,
            heat_kwh: BTreeMap::from([("livingroom".to_string(), heat)]),
            pull_kh: BTreeMap::new(),
        }
    }

    #[test]
    fn pull_step_is_off_when_every_value_is_zero() {
        let now = utc("2026-10-03T10:00:00Z");
        assert_eq!(
            pull_step(now, [0.0, 0.0], Some((&figures(5.0), true)), true),
            PullStep::Off
        );
        assert_eq!(pull_step(now, [], None, true), PullStep::Off);
    }

    #[test]
    fn pull_step_skips_without_solving_when_no_heat_is_planned() {
        let now = utc("2026-10-03T10:00:00Z");
        let PullStep::Record(row) = pull_step(now, [0.0, 0.017], Some((&figures(0.0), true)), true)
        else {
            panic!("expected a skipped row");
        };
        assert_eq!(row.skipped.as_deref(), Some("no heat planned"));
        assert!(row.identical);
        assert!(row.cost_with_eur.is_none());
    }

    #[test]
    fn pull_step_skips_a_degraded_or_missing_live_plan_and_a_busy_solver() {
        let now = utc("2026-10-03T10:00:00Z");
        let skipped = |step: PullStep| match step {
            PullStep::Record(r) => (r.skipped.unwrap(), r.identical),
            other => panic!("expected a skip, got {other:?}"),
        };
        assert_eq!(
            skipped(pull_step(now, [0.01], None, true)),
            ("no live plan".into(), false)
        );
        assert_eq!(
            skipped(pull_step(now, [0.01], Some((&figures(5.0), false)), true)),
            ("live plan degraded".into(), false)
        );
        assert_eq!(
            skipped(pull_step(now, [0.01], Some((&figures(5.0), true)), false)),
            ("busy".into(), false)
        );
        assert_eq!(
            pull_step(now, [0.01], Some((&figures(5.0), true)), true),
            PullStep::Solve
        );
    }
}
