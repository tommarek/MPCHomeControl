//! The persisted data behind the "new features" page (`/api/features`, [`crate::features`]).
//!
//! Two small JSON stores, both opened once at startup in [`crate::web::AppState`]:
//! - the **accuracy history** (`MPC_ACCURACY_HISTORY_STORE`): per UTC day, per zone, per forecast
//!   arm and lead bin, the forward-temperature-prediction error — the only long-lived record of how
//!   the thermal model's accuracy moved across a release;
//! - the **feature samples** (`MPC_FEATURE_SAMPLES_STORE`): sampled A/B records the loop cannot
//!   recompute later (terminal-SoC valuations, nowcast outcomes, relay-duty reads, priority-zone
//!   counterfactuals).
//!
//! Neither is ever written on the planning path. The loop only pushes in-memory samples
//! ([`FeatureSamples::push_terminal`] and friends — O(1) under a mutex); every disk write, Influx
//! read and counterfactual solve happens on [`run_collector`], a separately supervised task, like
//! the ledger scorer. Strictly read-only towards InfluxDB.
//!
//! Module shape: the generic store, the two data types with their pure logic, the pure day-scoring
//! and scheduling helpers, then the bounded IO and the collector task.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Timelike, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::app::{current_plan, PlanExtras, PlanReport};
use crate::estimate::{hour_key, keep_first_by_hour};
use crate::forecast_validation::{
    accumulate_by, accumulate_leads, load_snapshots, Arm, ErrAcc, Snapshot,
};
use crate::ledger::LedgerRow;
use crate::optimize::config::{ControlConfig, SiteConfig};
use crate::relay_duty::RelayDuty;
use crate::source::SourceClients;
use crate::validate::read_heating_kw_strict;
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

    /// Run `f` over the value under the lock (keep it short — never across IO).
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.lock())
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
    pub fn n(&self) -> usize {
        self.0 as usize
    }
    pub fn rmse_k(&self) -> f64 {
        self.1
    }
    pub fn bias_k(&self) -> f64 {
        self.2
    }

    fn from_acc(acc: &ErrAcc) -> BinStat {
        let round4 = |x: f64| (x * 1e4).round() / 1e4;
        if acc.n == 0 {
            return BinStat(0, 0.0, 0.0);
        }
        BinStat(
            acc.n as u32,
            round4((acc.sq / acc.n as f64).sqrt()),
            round4(acc.err / acc.n as f64),
        )
    }
}

/// Night targets for the night-bias view: local hour in `[NIGHT_FROM_HOUR, 24) ∪ [0, NIGHT_TO_HOUR)`.
pub const NIGHT_FROM_HOUR: u32 = 22;
pub const NIGHT_TO_HOUR: u32 = 6;
/// A local day whose measured PV energy (the ledger's Growatt `InputPower`, kWh) reaches this counts
/// as "sunny": about half of a clear early-October day on this house's array (55-66 kWh measured
/// on 2026-10-01/02), so a bright day with some cloud still qualifies and an overcast one does not.
pub const SUNNY_PV_KWH: f64 = 30.0;
/// Scored 15-minute-equivalent blocks a local day needs before its PV energy is trusted (of 96).
pub const MIN_SUNNY_DAY_BLOCKS: f64 = 90.0;
/// Distinct anchor hours a UTC day's snapshots must cover before it is scored: a partially covered
/// day (a restart, the store's first day) would bias the daily figure, so it is skipped, not stored.
pub const MIN_ANCHOR_HOURS: usize = 20;
/// Hours after an anchor day's midnight until every target of its snapshots (anchor + 36 h, so up to
/// the end of the next day + 12 h) has elapsed and its measured hour has landed.
pub const ACCURACY_SETTLE_HOURS: i64 = 2 * 24 + 13;

/// Night targets of one arm, split by whether the sun shone the day before: `[n, rmse_k, bias_k]`
/// cells, all leads pooled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NightStats {
    pub sunny: BinStat,
    pub other: BinStat,
}

/// One zone's day: per lead bin ([`HIST_BINS_H`] order) for each forecast arm.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneArms {
    /// The plan's own prediction (solar scale applied), over ALL that day's snapshots.
    pub scaled: Vec<BinStat>,
    /// The same prediction with `s_z = 1`, over the snapshots that carry the arm. Absent before the
    /// arm existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unscaled: Option<Vec<BinStat>>,
    /// The scaled prediction restricted to those same snapshots — the like-for-like baseline for
    /// `unscaled` (on a day the arm started mid-day, `scaled` covers more snapshots than
    /// `unscaled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaled_ab: Option<Vec<BinStat>>,
    /// Night-target error of the `scaled` arm, by sunny / other previous day. Absent when no night
    /// target had a known sunny-day flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night: Option<NightStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night_unscaled: Option<NightStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night_scaled_ab: Option<NightStats>,
}

/// One UTC ANCHOR day: every prediction made by a snapshot anchored in `[day 00:00, day+1 00:00)`,
/// scored against everything it predicted. Keying by when the prediction was MADE (not when its
/// target fell) is what lets a release split the history cleanly: a day is wholly "before" or
/// wholly "after" the model that made its predictions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayAccuracy {
    pub zones: BTreeMap<String, ZoneArms>,
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

/// Which local days were sunny, from the ledger's measured PV energy: `true` when the day's PV
/// reached [`SUNNY_PV_KWH`]. A day with fewer than [`MIN_SUNNY_DAY_BLOCKS`] scored blocks is left
/// out (unknown, not "not sunny"). Keyed by the site-local date. Pure.
pub fn sunny_days(rows: &[LedgerRow], site: &SiteConfig) -> HashMap<NaiveDate, bool> {
    let mut by_day: HashMap<NaiveDate, (f64, f64)> = HashMap::new();
    for row in rows {
        let Some(m) = row.measured.as_ref().filter(|_| row.scored) else {
            continue;
        };
        let day = row.t.with_timezone(&site.offset_at(row.t)).date_naive();
        let e = by_day.entry(day).or_default();
        e.0 += f64::from(row.dt_minutes) / 15.0;
        e.1 += m.pv_kwh;
    }
    by_day
        .into_iter()
        .filter(|(_, (blocks, _))| *blocks >= MIN_SUNNY_DAY_BLOCKS)
        .map(|(day, (_, pv))| (day, pv >= SUNNY_PV_KWH))
        .collect()
}

/// The night-bias cell of a target instant: `Some(1)` for a night target (local hour in
/// `[NIGHT_FROM_HOUR, 24) ∪ [0, NIGHT_TO_HOUR)`) after a sunny day, `Some(0)` after any other known
/// day, `None` for a daytime target or an unknown day. The "day before" of a night target is the
/// local day that was in progress six hours earlier. Pure.
pub fn night_cell(
    site: &SiteConfig,
    sunny: &HashMap<NaiveDate, bool>,
    target: DateTime<Utc>,
) -> Option<usize> {
    let local = target.with_timezone(&site.offset_at(target));
    let hour = local.hour();
    if (NIGHT_TO_HOUR..NIGHT_FROM_HOUR).contains(&hour) {
        return None;
    }
    let day = (local - Duration::hours(NIGHT_TO_HOUR as i64)).date_naive();
    sunny.get(&day).map(|s| usize::from(*s))
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
/// [`hour_key`], one per zone), targets up to `now`: per zone, per arm, per [`HIST_BINS_H`] bin, plus
/// the night targets split by `night` (see [`night_cell`]). A zone with no scoreable point in any
/// arm is omitted; a day with none at all is an empty [`DayAccuracy`]. Pure.
pub fn score_day(
    snapshots: &[Snapshot],
    measured: &HashMap<String, HashMap<i64, f64>>,
    day: NaiveDate,
    now: DateTime<Utc>,
    night: &dyn Fn(DateTime<Utc>) -> Option<usize>,
) -> DayAccuracy {
    let window = (DateTime::<Utc>::MIN_UTC, now);
    let of_day = || {
        snapshots
            .iter()
            .filter(|s| s.anchored_at.date_naive() == day)
    };
    let run = |arm, paired| accumulate_leads(of_day(), measured, window, arm, paired, &HIST_BINS_H);
    let run_night =
        |arm, paired| accumulate_by(of_day(), measured, window, arm, paired, &|_, t| night(t));
    let scaled = run(Arm::Scaled, false);
    let unscaled = run(Arm::Unscaled, false);
    let scaled_ab = run(Arm::Scaled, true);
    let (night_scaled, night_unscaled, night_ab) = (
        run_night(Arm::Scaled, false),
        run_night(Arm::Unscaled, false),
        run_night(Arm::Scaled, true),
    );

    let bins_of = |acc: &HashMap<(usize, String), ErrAcc>, zone: &str| -> Option<Vec<BinStat>> {
        let bins: Vec<BinStat> = (0..HIST_BINS_H.len())
            .map(|b| {
                acc.get(&(b, zone.to_string()))
                    .map_or(BinStat(0, 0.0, 0.0), BinStat::from_acc)
            })
            .collect();
        bins.iter().any(|s| s.0 > 0).then_some(bins)
    };
    let night_of = |acc: &HashMap<(usize, String), ErrAcc>, zone: &str| -> Option<NightStats> {
        let cell = |c: usize| {
            acc.get(&(c, zone.to_string()))
                .map_or(BinStat(0, 0.0, 0.0), BinStat::from_acc)
        };
        let stats = NightStats {
            other: cell(0),
            sunny: cell(1),
        };
        (stats.other.0 + stats.sunny.0 > 0).then_some(stats)
    };
    let mut zone_names: Vec<&String> = scaled
        .keys()
        .chain(unscaled.keys())
        .map(|(_, zone)| zone)
        .collect();
    zone_names.sort();
    zone_names.dedup();
    let mut zones = BTreeMap::new();
    for zone in zone_names {
        let unscaled_bins = bins_of(&unscaled, zone);
        let has_arm = unscaled_bins.is_some();
        let arms = ZoneArms {
            scaled: bins_of(&scaled, zone)
                .unwrap_or_else(|| vec![BinStat(0, 0.0, 0.0); HIST_BINS_H.len()]),
            scaled_ab: unscaled_bins.as_ref().and(bins_of(&scaled_ab, zone)),
            unscaled: unscaled_bins,
            night: night_of(&night_scaled, zone),
            night_unscaled: night_of(&night_unscaled, zone),
            night_scaled_ab: if has_arm {
                night_of(&night_ab, zone)
            } else {
                None
            },
        };
        zones.insert(zone.clone(), arms);
    }
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
/// Days of relay-duty history read on a cold start.
pub const RELAY_BACKFILL_DAYS: i64 = 7;

/// One hourly terminal-SoC sample: the value the LP used vs the legacy in-horizon median.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSample {
    pub t: DateTime<Utc>,
    /// The valuation the plan was solved with (`terminal_soc_value_eur_per_kwh`).
    pub outlook_eur_kwh: f64,
    /// What the pre-2026-10-01 valuation would have been on the same inputs.
    pub legacy_eur_kwh: f64,
    /// `outlook` | `horizon_median` — the latter means the outlook fell back to the legacy value
    /// (the two arms are then identical, not a comparison).
    pub source: String,
    /// The plan's end-of-horizon battery energy (kWh).
    pub end_soc_kwh: f64,
}

/// One hourly PV-nowcast outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowcastSample {
    pub t: DateTime<Utc>,
    pub applied: bool,
    #[serde(default)]
    pub ratio: Option<f64>,
    /// Why it was skipped (truncated), when it was.
    #[serde(default)]
    pub reason: Option<String>,
}

/// One UTC day of heating energy read both ways off the same relays: `zone -> [legacy_kwh,
/// events_kwh]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayDutyDay {
    pub date: String,
    pub zones: BTreeMap<String, [f64; 2]>,
}

/// One hourly priority-zones A/B row: the live plan (warmth values as configured) against a plan
/// solved with every warmth value 0 on the same inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WarmthSample {
    pub t: DateTime<Utc>,
    /// Why no counterfactual was solved (`no heat planned`, `busy`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// The two arms are known identical without solving (no planned heat: the zero-warmth arm can
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
    /// K·h each priority zone sat above its floor in the live plan (the zero-warmth plan does not
    /// compute the figure, so it has no counterpart).
    #[serde(default)]
    pub warmth_kh_with: BTreeMap<String, f64>,
}

/// The sampled A/B records, each kind bounded by [`SAMPLE_RETENTION_DAYS`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureSamples {
    #[serde(default)]
    pub terminal: Vec<TerminalSample>,
    #[serde(default)]
    pub nowcast: Vec<NowcastSample>,
    #[serde(default)]
    pub relay_duty: Vec<RelayDutyDay>,
    #[serde(default)]
    pub warmth: Vec<WarmthSample>,
}

/// Push `sample` unless the newest entry is already in the same UTC hour (or later): at most one
/// sample per hour per kind, O(1).
fn push_hourly<S>(list: &mut Vec<S>, sample: S, time_of: impl Fn(&S) -> DateTime<Utc>) -> bool {
    if list
        .last()
        .is_some_and(|last| hour_key(time_of(last)) >= hour_key(time_of(&sample)))
    {
        return false;
    }
    list.push(sample);
    true
}

impl FeatureSamples {
    pub fn push_terminal(&mut self, s: TerminalSample) -> bool {
        push_hourly(&mut self.terminal, s, |s| s.t)
    }
    pub fn push_nowcast(&mut self, s: NowcastSample) -> bool {
        push_hourly(&mut self.nowcast, s, |s| s.t)
    }
    pub fn push_warmth(&mut self, s: WarmthSample) -> bool {
        push_hourly(&mut self.warmth, s, |s| s.t)
    }

    pub fn has_relay_day(&self, day: NaiveDate) -> bool {
        let key = date_key(day);
        self.relay_duty.iter().any(|d| d.date == key)
    }

    /// Add a day's relay-duty row, keeping date order; a day already present is not rewritten.
    pub fn push_relay_day(&mut self, row: RelayDutyDay) -> bool {
        if self.relay_duty.iter().any(|d| d.date == row.date) {
            return false;
        }
        self.relay_duty.push(row);
        self.relay_duty.sort_by(|a, b| a.date.cmp(&b.date));
        true
    }

    /// Drop entries older than [`SAMPLE_RETENTION_DAYS`]; returns whether anything went.
    pub fn prune(&mut self, now: DateTime<Utc>) -> bool {
        let cutoff = now - Duration::days(SAMPLE_RETENTION_DAYS);
        let cutoff_date = date_key(cutoff.date_naive());
        let before =
            self.terminal.len() + self.nowcast.len() + self.relay_duty.len() + self.warmth.len();
        self.terminal.retain(|s| s.t >= cutoff);
        self.nowcast.retain(|s| s.t >= cutoff);
        self.warmth.retain(|s| s.t >= cutoff);
        self.relay_duty.retain(|d| d.date >= cutoff_date);
        before
            != self.terminal.len() + self.nowcast.len() + self.relay_duty.len() + self.warmth.len()
    }
}

/// The terminal-SoC sample of a published plan; `None` for a degraded/relaxed plan (computed from
/// fallback inputs) or one without a legacy value to compare with.
pub fn terminal_sample(plan: &PlanReport, now: DateTime<Utc>) -> Option<TerminalSample> {
    if plan.degraded || plan.relaxed {
        return None;
    }
    Some(TerminalSample {
        t: now,
        outlook_eur_kwh: plan.terminal_soc_value_eur_per_kwh,
        legacy_eur_kwh: plan.terminal_soc_value_legacy_eur_per_kwh?,
        source: plan.terminal_soc_value_source.clone(),
        end_soc_kwh: plan.final_soc_kwh,
    })
}

/// The PV-nowcast sample of a published plan.
pub fn nowcast_sample(plan: &PlanReport, now: DateTime<Utc>) -> NowcastSample {
    const MAX_REASON_CHARS: usize = 120;
    NowcastSample {
        t: now,
        applied: plan.pv_nowcast.applied,
        ratio: plan.pv_nowcast.ratio,
        reason: plan
            .pv_nowcast
            .reason
            .as_ref()
            .map(|r| r.chars().take(MAX_REASON_CHARS).collect()),
    }
}

/// Record the samples a freshly planned tick offers; O(1). Called by the loop just before it
/// publishes the plan.
pub fn push_plan_samples(store: &JsonStore<FeatureSamples>, plan: &PlanReport, now: DateTime<Utc>) {
    store.update(|samples| {
        let mut changed = false;
        if let Some(sample) = terminal_sample(plan, now) {
            changed |= samples.push_terminal(sample);
        }
        changed |= samples.push_nowcast(nowcast_sample(plan, now));
        changed
    });
}

// ============================================================ Pure scheduling helpers

/// UTC days in the last [`RELAY_BACKFILL_DAYS`] that are complete (see [`accuracy_days_due`]) and
/// have no relay-duty row yet. Ascending. Pure.
pub fn relay_days_due(samples: &FeatureSamples, now: DateTime<Utc>) -> Vec<NaiveDate> {
    let today = now.date_naive();
    (1..=RELAY_BACKFILL_DAYS + 1)
        .rev()
        .map(|back| today - Duration::days(back))
        .filter(|d| now >= utc_midnight(*d) + Duration::days(1) + Duration::hours(1))
        .filter(|d| *d >= today - Duration::days(RELAY_BACKFILL_DAYS))
        .filter(|d| !samples.has_relay_day(*d))
        .collect()
}

/// The stop-stamped unix-hour keys of one UTC day (hour `h` covers `[h-1, h)`), the grid
/// [`read_heating_kw`] takes.
pub fn day_hour_keys(day: NaiveDate) -> Vec<i64> {
    let first = hour_key(utc_midnight(day));
    (1..=24).map(|h| first + h).collect()
}

/// Sum each zone's hourly kW series (one hour per entry) into kWh, rounded to 3 decimals.
fn kwh_by_zone(kw: &HashMap<String, Vec<f64>>) -> BTreeMap<String, f64> {
    kw.iter()
        .map(|(zone, series)| {
            (
                zone.clone(),
                (series.iter().sum::<f64>() * 1e3).round() / 1e3,
            )
        })
        .collect()
}

/// Combine the two reads of one day into the stored row: every zone either read produced, a missing
/// side counting as 0 kWh.
pub fn relay_day_row(
    day: NaiveDate,
    legacy: &HashMap<String, Vec<f64>>,
    events: &HashMap<String, Vec<f64>>,
) -> RelayDutyDay {
    let (legacy, events) = (kwh_by_zone(legacy), kwh_by_zone(events));
    let zones = legacy
        .keys()
        .chain(events.keys())
        .map(|zone| {
            (
                zone.clone(),
                [
                    legacy.get(zone).copied().unwrap_or(0.0),
                    events.get(zone).copied().unwrap_or(0.0),
                ],
            )
        })
        .collect();
    RelayDutyDay {
        date: date_key(day),
        zones,
    }
}

/// The few figures of a plan the priority-zones A/B compares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanFigures {
    pub cost_eur: f64,
    pub heat_kwh: BTreeMap<String, f64>,
    pub warmth_kh: BTreeMap<String, f64>,
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
            warmth_kh: plan
                .warmth_kh
                .iter()
                .map(|(z, v)| (z.clone(), *v))
                .collect(),
        }
    }

    fn total_heat_kwh(&self) -> f64 {
        self.heat_kwh.values().sum()
    }
}

/// Below this total planned heat (kWh over the horizon) a plan counts as "no heat planned".
const NO_HEAT_KWH: f64 = 0.01;

/// What the hourly priority-zones A/B does this hour, decided before any solve.
#[derive(Debug, Clone, PartialEq)]
pub enum WarmthStep {
    /// Every configured warmth value is 0: the feature is off, record nothing.
    Off,
    /// Record a row without solving.
    Record(WarmthSample),
    /// Solve the zero-warmth counterfactual.
    Solve,
}

/// The skip rules, in order: feature off; no usable live plan; no planned heat (the zero-warmth arm
/// can only heat less, so the arms are identical); on-demand solver busy. Pure.
pub fn warmth_step(
    now: DateTime<Utc>,
    warmth_values: impl IntoIterator<Item = f64>,
    live: Option<(&PlanFigures, bool)>,
    solver_idle: bool,
) -> WarmthStep {
    if warmth_values.into_iter().all(|v| v <= 0.0) {
        return WarmthStep::Off;
    }
    let skipped = |why: &str, identical: bool| {
        WarmthStep::Record(WarmthSample {
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
    WarmthStep::Solve
}

// ============================================================ Bounded IO

/// Hours of measured series one anchor day needs: the day itself plus its last anchor's 36 h.
const ANCHOR_DAY_SPAN_H: i64 = 24 + 36;
const READ_TIMEOUT: StdDuration = StdDuration::from_secs(20);
/// Pause between per-zone reads of one scoring day (server-friendly).
const ZONE_READ_PAUSE: StdDuration = StdDuration::from_millis(250);
/// Pause between the two heating reads of one day.
const RELAY_READ_PAUSE: StdDuration = StdDuration::from_secs(3);
/// Days scored (accuracy) / read (relay duty) per pass, so a cold-start backfill trickles in.
const MAX_ACCURACY_DAYS_PER_PASS: usize = 2;
const MAX_RELAY_DAYS_PER_PASS: usize = 2;
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
        let sunny = sunny_days(&state.ledger.rows_snapshot(), &state.config.site);
        let site = &state.config.site;
        let scored_day = score_day(&snapshots, &measured, day, now, &|t| {
            night_cell(site, &sunny, t)
        });
        let n_zones = scored_day.zones.len();
        state
            .accuracy_history
            .update(|h| h.insert_day(day, scored_day, now));
        println!("[features] accuracy history: scored {day} ({n_zones} zones)");
        scored += 1;
    }
}

/// One day's heating kWh both ways off the same relays: the two strict `read_heating_kw` dispatch
/// arms with `heating.relay_duty` forced (the `audit-relay-duty` comparison, on a 1-day window).
/// `None` when either read failed: a day recorded from a failed read would read as "no heating"
/// forever, so it stays due and is retried.
async fn read_relay_day(state: &AppState, day: NaiveDate) -> Option<RelayDutyDay> {
    let start = utc_midnight(day);
    let (start_s, stop_s) = (start.to_rfc3339(), (start + Duration::days(1)).to_rfc3339());
    let hours = day_hour_keys(day);
    let mut legacy = state.config.heating.clone();
    legacy.relay_duty = RelayDuty::Legacy;
    let mut events = state.config.heating.clone();
    events.relay_duty = RelayDuty::Events;
    let legacy_kw =
        read_heating_kw_strict(&state.db, &state.net, &legacy, &hours, &start_s, &stop_s).await;
    tokio::time::sleep(RELAY_READ_PAUSE).await;
    let events_kw =
        read_heating_kw_strict(&state.db, &state.net, &events, &hours, &start_s, &stop_s).await;
    relay_day_from_reads(day, legacy_kw, events_kw)
}

/// The stored row of a day from its two reads; `None` (and a log line) when either failed. Pure.
pub fn relay_day_from_reads(
    day: NaiveDate,
    legacy: Result<HashMap<String, Vec<f64>>>,
    events: Result<HashMap<String, Vec<f64>>>,
) -> Option<RelayDutyDay> {
    match (legacy, events) {
        (Ok(legacy), Ok(events)) => Some(relay_day_row(day, &legacy, &events)),
        (legacy, events) => {
            for (name, res) in [("legacy", legacy), ("events", events)] {
                if let Err(e) = res {
                    eprintln!(
                        "[features] relay duty {day}: {name} read failed ({e:#}); will retry"
                    );
                }
            }
            None
        }
    }
}

async fn relay_pass(
    state: &AppState,
    now: DateTime<Utc>,
    retry_at: &mut HashMap<String, tokio::time::Instant>,
) {
    let due = state.feature_samples.read(|s| relay_days_due(s, now));
    let mut read = 0;
    for day in due {
        if read >= MAX_RELAY_DAYS_PER_PASS {
            break;
        }
        let key = format!("relay:{day}");
        if retry_at
            .get(&key)
            .is_some_and(|at| *at > tokio::time::Instant::now())
        {
            continue;
        }
        if read > 0 {
            tokio::time::sleep(RELAY_READ_PAUSE).await;
        }
        read += 1;
        let Some(row) = read_relay_day(state, day).await else {
            retry_at.insert(key, tokio::time::Instant::now() + RETRY_AFTER);
            continue;
        };
        println!(
            "[features] relay duty: read {day} ({} zones, legacy {:.2} kWh, events {:.2} kWh)",
            row.zones.len(),
            row.zones.values().map(|z| z[0]).sum::<f64>(),
            row.zones.values().map(|z| z[1]).sum::<f64>(),
        );
        state.feature_samples.update(|s| s.push_relay_day(row));
    }
}

/// `config` with every priority-zone warmth value zeroed — the counterfactual arm.
fn without_warmth(config: &ControlConfig) -> ControlConfig {
    let mut off = config.clone();
    for zone in off.heating.zones.values_mut() {
        zone.warmth_value_eur_per_kh = 0.0;
    }
    off
}

/// The hourly priority-zones A/B (see [`warmth_step`]). The counterfactual is ONE on-demand-path
/// plan (never the loop's permit) over the loop's own slow-input cache and kernels, solved only
/// when the on-demand solver is idle; the live plan is the loop's latest published one, so a block
/// rollover between the two (their block 0 would differ) discards the sample.
async fn warmth_pass(state: &AppState, now: DateTime<Utc>) {
    let latest = crate::web::lock_latest(state);
    let live = latest.as_ref().map(|tp| {
        (
            PlanFigures::of(&tp.plan),
            !tp.plan.degraded && !tp.plan.relaxed,
        )
    });
    let step = warmth_step(
        now,
        state
            .config
            .heating
            .zones
            .values()
            .map(|z| z.warmth_value_eur_per_kh),
        live.as_ref().map(|(f, usable)| (f, *usable)),
        crate::app::on_demand_solver_idle(),
    );
    let sample = match step {
        WarmthStep::Off => return,
        WarmthStep::Record(sample) => sample,
        WarmthStep::Solve => {
            let (Some(latest), Some((live_figures, _))) = (&latest, &live) else {
                return;
            };
            let cache = state
                .plan_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let off_config = without_warmth(&state.config);
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
            let skipped = |why: &str| WarmthSample {
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
                    WarmthSample {
                        t: now,
                        skipped: None,
                        identical: false,
                        cost_with_eur: Some(live_figures.cost_eur),
                        cost_without_eur: Some(without.cost_eur),
                        heat_with_kwh: live_figures.heat_kwh.clone(),
                        heat_without_kwh: without.heat_kwh,
                        warmth_kh_with: live_figures.warmth_kh.clone(),
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("[features] warmth counterfactual failed: {e:#}");
                    skipped("counterfactual failed")
                }
                Err(_) => skipped("counterfactual timed out"),
            }
        }
    };
    state.feature_samples.update(|s| s.push_warmth(sample));
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
/// same way): every [`PASS_INTERVAL`] append the accuracy history and the relay-duty rows that have
/// come due, run the hourly priority-zones counterfactual, prune, and persist. Every failure is
/// logged and retried later; a wedged DB degrades this task alone.
pub async fn run_collector(state: Arc<AppState>) {
    tokio::time::sleep(STARTUP_DELAY).await;
    let mut interval = tokio::time::interval(PASS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut retry_at: HashMap<String, tokio::time::Instant> = HashMap::new();
    let mut last_warmth_hour: Option<i64> = None;
    loop {
        interval.tick().await;
        let now = Utc::now();
        accuracy_pass(&state, now, &mut retry_at).await;
        relay_pass(&state, now, &mut retry_at).await;
        if last_warmth_hour != Some(hour_key(now)) {
            last_warmth_hour = Some(hour_key(now));
            warmth_pass(&state, now).await;
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

    fn snapshot(anchor: &str, hours: i64, scaled: f64, unscaled: Option<f64>) -> Snapshot {
        let anchored_at = utc(anchor);
        let n = hours as usize;
        let mut value = serde_json::json!({
            "anchored_at": anchored_at,
            "block_ends": (1..=hours).map(|h| anchored_at + Duration::hours(h)).collect::<Vec<_>>(),
            "zones": { "lr": vec![scaled; n] },
        });
        if let Some(u) = unscaled {
            value["zones_unscaled"] = serde_json::json!({ "lr": vec![u; n] });
        }
        serde_json::from_value(value).unwrap()
    }

    fn measured_flat(from: &str, hours: i64, value: f64) -> HashMap<String, HashMap<i64, f64>> {
        let first = hour_key(utc(from));
        HashMap::from([(
            "lr".to_string(),
            (0..hours).map(|h| (first + h, value)).collect(),
        )])
    }

    // ---- store

    #[test]
    fn store_round_trips_across_a_reopen_and_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("samples.json");
        let store = JsonStore::<FeatureSamples>::open_at(path.clone(), "test");
        assert!(!store.is_dirty());
        store.update(|s| {
            s.push_terminal(TerminalSample {
                t: utc("2026-10-03T10:00:00Z"),
                outlook_eur_kwh: 0.25,
                legacy_eur_kwh: 0.04,
                source: "outlook".into(),
                end_soc_kwh: 7.0,
            })
        });
        assert!(store.is_dirty());
        store.persist().unwrap();
        assert!(!store.is_dirty());
        let reopened = JsonStore::<FeatureSamples>::open_at(path, "test");
        assert_eq!(reopened.read(|s| s.terminal.len()), 1);
        assert_eq!(reopened.read(|s| s.terminal[0].outlook_eur_kwh), 0.25);
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
        assert!(store.read(|h| h.days.is_empty()), "starts empty in memory");
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
        assert!(reopened.read(|h| h.has_day(day("2026-10-02"))));
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
        first.zones.insert("a".into(), ZoneArms::default());
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

    fn no_night(_: DateTime<Utc>) -> Option<usize> {
        None
    }

    fn site() -> SiteConfig {
        serde_json::from_value(
            serde_json::json!({"latitude": 49.5, "longitude": 17.4, "utc_offset_hours": 0}),
        )
        .unwrap()
    }

    #[test]
    fn score_day_scores_both_arms_the_paired_baseline_and_the_night_cells() {
        // Anchor day 2026-10-01: snapshot A (no arm) anchored 18:00, B (with arm) at 20:00; both
        // predict a flat 21.5 (scaled) / 22.0 (unscaled) against a measured 21.0.
        let snaps = [
            snapshot("2026-10-01T18:00:00Z", 30, 22.0, None),
            snapshot("2026-10-01T20:00:00Z", 30, 21.5, Some(22.0)),
        ];
        let measured = measured_flat("2026-10-01T00:00:00Z", 72, 21.0);
        let now = utc("2026-10-04T00:00:00Z");
        // Every target before 06:00 UTC is a night target after a sunny day.
        let d = score_day(&snaps, &measured, day("2026-10-01"), now, &|t| {
            (t.hour() < 6).then_some(1)
        });
        let lr = &d.zones["lr"];
        let n_scaled: usize = lr.scaled.iter().map(BinStat::n).sum();
        let n_ab: usize = lr.scaled_ab.as_ref().unwrap().iter().map(BinStat::n).sum();
        let n_unscaled: usize = lr.unscaled.as_ref().unwrap().iter().map(BinStat::n).sum();
        assert_eq!(n_scaled, 60, "both snapshots, 30 hourly targets each");
        assert_eq!(n_ab, 30, "paired baseline = the snapshot carrying the arm");
        assert_eq!(n_unscaled, 30);
        let ab = lr.scaled_ab.as_ref().unwrap();
        let un = lr.unscaled.as_ref().unwrap();
        for (a, u) in ab.iter().zip(un).filter(|(a, _)| a.n() > 0) {
            assert!((a.rmse_k() - 0.5).abs() < 1e-9 && (a.bias_k() - 0.5).abs() < 1e-9);
            assert!((u.rmse_k() - 1.0).abs() < 1e-9 && (u.bias_k() - 1.0).abs() < 1e-9);
        }
        // Night targets: A ends 10-02 00..05 and 10-03 00 (7); B adds 10-03 01, 02 (9).
        let night = lr.night.unwrap();
        assert_eq!((night.sunny.n(), night.other.n()), (16, 0));
        assert!((night.sunny.bias_k() - ((7.0 * 1.0 + 9.0 * 0.5) / 16.0)).abs() < 1e-3);
        assert_eq!(lr.night_unscaled.unwrap().sunny.n(), 9);
        assert_eq!(lr.night_scaled_ab.unwrap().sunny.n(), 9);
        assert!((lr.night_unscaled.unwrap().sunny.bias_k() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn score_day_scores_only_snapshots_anchored_on_that_day() {
        // One snapshot anchored 2026-10-01 23:00 (its single target ends 10-02 00:00).
        let snaps = [snapshot("2026-10-01T23:00:00Z", 1, 21.5, None)];
        let measured = measured_flat("2026-10-01T00:00:00Z", 48, 21.0);
        let now = utc("2026-10-04T00:00:00Z");
        let d1 = score_day(&snaps, &measured, day("2026-10-01"), now, &no_night);
        let d2 = score_day(&snaps, &measured, day("2026-10-02"), now, &no_night);
        assert_eq!(
            d1.zones["lr"].scaled.iter().map(BinStat::n).sum::<usize>(),
            1,
            "scored on its ANCHOR day even though the target is on the next"
        );
        assert!(d2.zones.is_empty());
    }

    #[test]
    fn score_day_skips_targets_that_have_not_elapsed() {
        let snaps = [snapshot("2026-10-01T20:00:00Z", 30, 21.5, None)];
        let measured = measured_flat("2026-10-01T00:00:00Z", 72, 21.0);
        let d = score_day(
            &snaps,
            &measured,
            day("2026-10-01"),
            utc("2026-10-01T23:00:00Z"),
            &no_night,
        );
        // Targets 21:00, 22:00, 23:00.
        assert_eq!(
            d.zones["lr"].scaled.iter().map(BinStat::n).sum::<usize>(),
            3
        );
    }

    #[test]
    fn score_day_omits_unscaled_and_night_arms_when_absent() {
        let snaps = [snapshot("2026-09-30T20:00:00Z", 30, 21.5, None)];
        let measured = measured_flat("2026-09-30T00:00:00Z", 72, 21.0);
        let d = score_day(
            &snaps,
            &measured,
            day("2026-09-30"),
            utc("2026-10-04T00:00:00Z"),
            &no_night,
        );
        let lr = &d.zones["lr"];
        assert!(lr.unscaled.is_none() && lr.scaled_ab.is_none() && lr.night.is_none());
        assert!(lr.scaled.iter().any(|b| b.n() > 0));
        // The JSON stays compact: no arm keys, cells are 3-element arrays.
        let json = serde_json::to_string(lr).unwrap();
        assert!(
            !json.contains("unscaled") && !json.contains("scaled_ab") && !json.contains("night")
        );
        assert!(json.starts_with("{\"scaled\":[["), "{json}");
    }

    #[test]
    fn score_day_with_no_measurements_is_an_empty_day() {
        let snaps = [snapshot("2026-10-01T20:00:00Z", 30, 21.5, None)];
        let d = score_day(
            &snaps,
            &HashMap::new(),
            day("2026-10-01"),
            utc("2026-10-04T00:00:00Z"),
            &no_night,
        );
        assert!(d.zones.is_empty());
    }

    /// 24 hourly snapshots (3 targets each) anchored through `date`.
    fn full_day_snapshots(date: &str, hours: usize) -> Vec<Snapshot> {
        (0..hours)
            .map(|h| snapshot(&format!("{date}T{h:02}:00:00Z"), 3, 21.0, None))
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

    fn pv_rows(date: &str, n: usize, pv_kwh_each: f64) -> Vec<LedgerRow> {
        let t0 = utc(&format!("{date}T00:00:00Z"));
        (0..n)
            .map(|i| {
                serde_json::from_value(serde_json::json!({
                    "t": t0 + Duration::minutes(15 * i as i64),
                    "dt_minutes": 15,
                    "scored": true,
                    "measured": {"pv_kwh": pv_kwh_each},
                }))
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn sunny_days_uses_measured_pv_energy_and_skips_thin_days() {
        let mut rows = pv_rows("2026-10-01", 96, 0.5); // 48 kWh: sunny
        rows.extend(pv_rows("2026-10-02", 96, 0.1)); // 9.6 kWh: not
        rows.extend(pv_rows("2026-10-03", 50, 5.0)); // too few blocks: unknown
        let map = sunny_days(&rows, &site());
        assert_eq!(map.get(&day("2026-10-01")), Some(&true));
        assert_eq!(map.get(&day("2026-10-02")), Some(&false));
        assert!(!map.contains_key(&day("2026-10-03")));
    }

    #[test]
    fn night_cell_pairs_a_night_target_with_the_day_before_it() {
        let sunny = HashMap::from([(day("2026-10-01"), true), (day("2026-10-02"), false)]);
        let cell = |t: &str| night_cell(&site(), &sunny, utc(t));
        assert_eq!(
            cell("2026-10-01T23:00:00Z"),
            Some(1),
            "evening of the sunny day"
        );
        assert_eq!(
            cell("2026-10-02T02:00:00Z"),
            Some(1),
            "after midnight: still that day"
        );
        assert_eq!(cell("2026-10-02T05:00:00Z"), Some(1));
        assert_eq!(cell("2026-10-02T06:00:00Z"), None, "morning is not night");
        assert_eq!(cell("2026-10-02T21:00:00Z"), None);
        assert_eq!(cell("2026-10-02T22:00:00Z"), Some(0));
        assert_eq!(cell("2026-10-04T02:00:00Z"), None, "unknown previous day");
    }

    #[test]
    fn a_failed_relay_read_on_either_side_records_no_row() {
        let ok = || Ok(HashMap::from([("kitchen".to_string(), vec![1.0; 24])]));
        let err = || Err(anyhow::anyhow!("influx down"));
        let d = day("2026-10-02");
        let row = relay_day_from_reads(d, ok(), ok()).expect("both reads fine");
        assert_eq!(row.zones["kitchen"], [24.0, 24.0]);
        assert!(relay_day_from_reads(d, err(), ok()).is_none());
        assert!(relay_day_from_reads(d, ok(), err()).is_none());
        assert!(relay_day_from_reads(d, err(), err()).is_none());
        // A healthy read with no heating at all is a real (empty) row.
        let empty = relay_day_from_reads(d, Ok(HashMap::new()), Ok(HashMap::new())).unwrap();
        assert!(empty.zones.is_empty());
    }

    // ---- samples

    fn terminal(t: &str) -> TerminalSample {
        TerminalSample {
            t: utc(t),
            outlook_eur_kwh: 0.25,
            legacy_eur_kwh: 0.04,
            source: "outlook".into(),
            end_soc_kwh: 7.0,
        }
    }

    #[test]
    fn samples_keep_at_most_one_per_hour_per_kind() {
        let mut s = FeatureSamples::default();
        assert!(s.push_terminal(terminal("2026-10-03T10:00:20Z")));
        assert!(
            !s.push_terminal(terminal("2026-10-03T10:59:00Z")),
            "same hour"
        );
        assert!(
            !s.push_terminal(terminal("2026-10-03T09:30:00Z")),
            "clock stepped back"
        );
        assert!(s.push_terminal(terminal("2026-10-03T11:00:05Z")));
        assert_eq!(s.terminal.len(), 2);
        let nc = |t: &str| NowcastSample {
            t: utc(t),
            applied: true,
            ratio: Some(1.2),
            reason: None,
        };
        assert!(s.push_nowcast(nc("2026-10-03T10:00:00Z")));
        assert!(!s.push_nowcast(nc("2026-10-03T10:30:00Z")));
        let w = |t: &str| WarmthSample {
            t: utc(t),
            ..Default::default()
        };
        assert!(s.push_warmth(w("2026-10-03T10:00:00Z")));
        assert!(!s.push_warmth(w("2026-10-03T10:10:00Z")));
    }

    #[test]
    fn samples_prune_drops_each_kind_past_retention() {
        let now = utc("2026-12-01T00:00:00Z");
        let mut s = FeatureSamples::default();
        s.push_terminal(terminal("2026-09-01T10:00:00Z")); // 91 days old
        s.push_terminal(terminal("2026-11-30T10:00:00Z"));
        s.push_relay_day(RelayDutyDay {
            date: "2026-09-01".into(),
            zones: BTreeMap::new(),
        });
        s.push_relay_day(RelayDutyDay {
            date: "2026-11-30".into(),
            zones: BTreeMap::new(),
        });
        assert!(s.prune(now));
        assert_eq!(s.terminal.len(), 1);
        assert_eq!(s.relay_duty.len(), 1);
        assert_eq!(s.relay_duty[0].date, "2026-11-30");
        assert!(!s.prune(now), "nothing left to drop");
    }

    #[test]
    fn relay_rows_are_kept_in_date_order_and_never_rewritten() {
        let mut s = FeatureSamples::default();
        let row = |d: &str, v: f64| RelayDutyDay {
            date: d.into(),
            zones: BTreeMap::from([("kitchen".to_string(), [v, v * 2.0])]),
        };
        assert!(s.push_relay_day(row("2026-10-02", 1.0)));
        assert!(s.push_relay_day(row("2026-10-01", 3.0)));
        assert!(!s.push_relay_day(row("2026-10-02", 9.0)));
        assert_eq!(s.relay_duty[0].date, "2026-10-01");
        assert_eq!(s.relay_duty[1].zones["kitchen"], [1.0, 2.0]);
    }

    #[test]
    fn samples_file_with_missing_kinds_still_parses() {
        let parsed: FeatureSamples = serde_json::from_str(r#"{"terminal":[]}"#).unwrap();
        assert!(parsed.nowcast.is_empty() && parsed.warmth.is_empty());
    }

    // ---- scheduling helpers

    #[test]
    fn relay_days_due_covers_the_last_seven_complete_days_once() {
        let mut s = FeatureSamples::default();
        let now = utc("2026-10-03T12:00:00Z");
        let due = relay_days_due(&s, now);
        assert_eq!(due.len(), 7);
        assert_eq!(due[0], day("2026-09-26"));
        assert_eq!(*due.last().unwrap(), day("2026-10-02"));
        // Day boundary: before 01:00 UTC the previous day is not complete yet.
        let early = relay_days_due(&s, utc("2026-10-03T00:30:00Z"));
        assert_eq!(*early.last().unwrap(), day("2026-10-01"));
        // A read day drops out.
        s.push_relay_day(RelayDutyDay {
            date: "2026-10-02".into(),
            zones: BTreeMap::new(),
        });
        assert_eq!(relay_days_due(&s, now).len(), 6);
    }

    #[test]
    fn day_hour_keys_are_the_24_stop_stamped_hours() {
        let keys = day_hour_keys(day("2026-10-02"));
        assert_eq!(keys.len(), 24);
        assert_eq!(keys[0], hour_key(utc("2026-10-02T01:00:00Z")));
        assert_eq!(keys[23], hour_key(utc("2026-10-03T00:00:00Z")));
    }

    #[test]
    fn relay_day_row_pairs_the_two_reads_per_zone() {
        let legacy = HashMap::from([
            ("kitchen".to_string(), vec![0.5; 24]),
            ("only_legacy".to_string(), vec![1.0; 2]),
        ]);
        let events = HashMap::from([
            ("kitchen".to_string(), vec![1.0; 24]),
            ("only_events".to_string(), vec![0.25; 4]),
        ]);
        let row = relay_day_row(day("2026-10-02"), &legacy, &events);
        assert_eq!(row.date, "2026-10-02");
        assert_eq!(row.zones["kitchen"], [12.0, 24.0]);
        assert_eq!(row.zones["only_legacy"], [2.0, 0.0]);
        assert_eq!(row.zones["only_events"], [0.0, 1.0]);
    }

    // ---- priority-zones A/B skip rules

    fn figures(heat: f64) -> PlanFigures {
        PlanFigures {
            cost_eur: 1.0,
            heat_kwh: BTreeMap::from([("livingroom".to_string(), heat)]),
            warmth_kh: BTreeMap::new(),
        }
    }

    #[test]
    fn warmth_step_is_off_when_every_value_is_zero() {
        let now = utc("2026-10-03T10:00:00Z");
        assert_eq!(
            warmth_step(now, [0.0, 0.0], Some((&figures(5.0), true)), true),
            WarmthStep::Off
        );
        assert_eq!(warmth_step(now, [], None, true), WarmthStep::Off);
    }

    #[test]
    fn warmth_step_skips_without_solving_when_no_heat_is_planned() {
        let now = utc("2026-10-03T10:00:00Z");
        let WarmthStep::Record(row) =
            warmth_step(now, [0.0, 0.017], Some((&figures(0.0), true)), true)
        else {
            panic!("expected a skipped row");
        };
        assert_eq!(row.skipped.as_deref(), Some("no heat planned"));
        assert!(row.identical);
        assert!(row.cost_with_eur.is_none());
    }

    #[test]
    fn warmth_step_skips_a_degraded_or_missing_live_plan_and_a_busy_solver() {
        let now = utc("2026-10-03T10:00:00Z");
        let skipped = |step: WarmthStep| match step {
            WarmthStep::Record(r) => (r.skipped.unwrap(), r.identical),
            other => panic!("expected a skip, got {other:?}"),
        };
        assert_eq!(
            skipped(warmth_step(now, [0.01], None, true)),
            ("no live plan".into(), false)
        );
        assert_eq!(
            skipped(warmth_step(now, [0.01], Some((&figures(5.0), false)), true)),
            ("live plan degraded".into(), false)
        );
        assert_eq!(
            skipped(warmth_step(now, [0.01], Some((&figures(5.0), true)), false)),
            ("busy".into(), false)
        );
        assert_eq!(
            warmth_step(now, [0.01], Some((&figures(5.0), true)), true),
            WarmthStep::Solve
        );
    }
}
