//! The decision ledger: planned vs measured per block, realized vs planned cost.
//!
//! Every accuracy endpoint elsewhere scores a *forecast*. This scores *decisions*: for each 15-min
//! (or longer, imported) block the loop actually committed to the controllers, it records the
//! planned dispatch, then — once the block has ended and the measured data has had time to land —
//! reads the measured Growatt/heating/EV telemetry and compares. The scorer is strictly off the
//! planning path: a wedged query here must never delay or fail a planning tick. The ledger is the
//! brain's OWN JSON store (like `MPC_FORECAST_STORE`) — the root crate stays read-only towards
//! InfluxDB.
//!
//! Module shape: pure types + scoring math (unit-testable without I/O) at the top, the [`Ledger`]
//! store, the aggregation for `/api/ledger`, the bounded InfluxDB scoring IO, and the read-only
//! `ledger <import|score|show>` CLI at the bottom.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::app::TimelineBlock;
use crate::optimize::config::{ControlConfig, SiteConfig};
use crate::rc_network::RcNetwork;
use crate::relay_duty::relay_duty;
use crate::source::{SourceClients, SourceLocator};
use crate::what_if::{align_15min, BLOCKS_PER_DAY};

// ============================================================ Types

/// A zero-valued plan entry carries no information (that zone/charger simply wasn't asked for
/// anything this block) and, at 96 rows/day x 30 days retained, dominates the store's size — most
/// zones sit at 0 most blocks. Serialize only the non-zero entries; an entry absent on read-back
/// still deserializes to 0.0 via `#[serde(default)]` on the field, so this is lossless for `Planned`
/// (unlike `Measured`'s per-item maps below, where absence means something else).
fn serialize_nonzero_map<S: serde::Serializer>(
    map: &HashMap<String, f64>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut ser = s.serialize_map(Some(map.values().filter(|v| **v != 0.0).count()))?;
    for (k, v) in map {
        if *v != 0.0 {
            ser.serialize_entry(k, v)?;
        }
    }
    ser.end()
}

/// `Measured`'s per-item maps already distinguish "0 kWh measured" from "unknown" via `Option` (see
/// the struct doc) — `None` is the common case (most blocks heat/charge nothing) and dominates the
/// store's size at scale. Serialize only the `Some` entries rather than writing `null` for each
/// `None` one; an absent entry on read-back still deserializes to the field's `#[serde(default)]`
/// (empty map, i.e. every item unknown), which is the same "unknown" meaning `None` carried.
fn serialize_known_map<S: serde::Serializer>(
    map: &HashMap<String, Option<f64>>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let known: Vec<(&String, f64)> = map
        .iter()
        .filter_map(|(k, v)| v.map(|val| (k, val)))
        .collect();
    let mut ser = s.serialize_map(Some(known.len()))?;
    for (k, v) in &known {
        ser.serialize_entry(k, v)?;
    }
    ser.end()
}

/// The planned side of a block — straight off the [`TimelineBlock`] the controllers were given. The
/// per-zone/charger maps serialize only their non-zero entries (see [`serialize_nonzero_map`]): an
/// absent zone there means "0 kW planned", not "unknown".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Planned {
    #[serde(default)]
    pub pv_kw: f64,
    #[serde(default)]
    pub load_kw: f64,
    #[serde(default)]
    pub charge_kw: f64,
    #[serde(default)]
    pub discharge_kw: f64,
    #[serde(default)]
    pub grid_import_kw: f64,
    #[serde(default)]
    pub grid_export_kw: f64,
    #[serde(default, serialize_with = "serialize_nonzero_map")]
    pub heat_kw: HashMap<String, f64>,
    #[serde(default, serialize_with = "serialize_nonzero_map")]
    pub ev_charge_kw: HashMap<String, f64>,
    #[serde(default, serialize_with = "serialize_nonzero_map")]
    pub controllable_load_kw: HashMap<String, f64>,
}

/// The measured side, once scored. Per-item fields (`heat_kwh`, `ev_kwh`) are `None` for a zone or
/// charger whose measurement couldn't be reconstructed — never zero-filled (a zero would silently
/// claim "measured no heat", which the relay's on-change logging can't actually tell us without a
/// known prior state — see [`crate::relay_duty::relay_duty`]). Serialized with only the known (`Some`) entries (see
/// [`serialize_known_map`]): an absent entry there means "unknown", the same as `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Measured {
    #[serde(default)]
    pub pv_kwh: f64,
    #[serde(default)]
    pub load_kwh: f64,
    #[serde(default)]
    pub charge_kwh: f64,
    #[serde(default)]
    pub discharge_kwh: f64,
    #[serde(default)]
    pub import_kwh: f64,
    #[serde(default)]
    pub export_kwh: f64,
    #[serde(default, serialize_with = "serialize_known_map")]
    pub heat_kwh: HashMap<String, Option<f64>>,
    #[serde(default, serialize_with = "serialize_known_map")]
    pub ev_kwh: HashMap<String, Option<f64>>,
}

/// Which plan-quality flags a block's decision carried, snapshotted into [`LedgerRow`] at record
/// time (`PlanReport` has no `time_limited` field — only these three, per the Researcher amendment).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanFlags {
    pub degraded: bool,
    pub relaxed: bool,
    pub rounded: bool,
}

/// One block's recorded decision, scored once the data has landed. Field names are the public API
/// shape (`/api/ledger`'s `rows`) — keep them stable; every field is `#[serde(default)]` so the
/// store migrates forward when a field is added later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRow {
    #[serde(default = "Utc::now")]
    pub t: DateTime<Utc>,
    #[serde(default)]
    pub dt_minutes: u32,
    #[serde(default = "Utc::now")]
    pub recorded_at: DateTime<Utc>,
    /// `"frozen"` (the freeze-window commitment) | `"block0"` (a fresh clean plan's block 0) |
    /// `"degraded"` (no clean plan covered the block) | `"late"` (a clean block 0 first tracked more
    /// than [`LATE_TRACKING_THRESHOLD`] into the block — the loop just started/recovered mid-block,
    /// so the controllers may have run something else for its first few minutes) | `"plan-snapshot"`
    /// | `"log"` (backfilled via `ledger import`).
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub export_enabled: bool,
    #[serde(default)]
    pub inverter_on: bool,
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub relaxed: bool,
    #[serde(default)]
    pub rounded: bool,
    /// A later CLEAN block 0 for this same block disagreed with the recorded decision (slot /
    /// export_enabled / inverter_on / charge_kw / discharge_kw beyond 0.05 kW) — the controllers
    /// never see that drift (the publisher only promotes the frozen/first commitment), but it tells
    /// the owner the brain changed its mind mid-block.
    #[serde(default)]
    pub drifted: bool,
    #[serde(default)]
    pub price_is_placeholder: bool,
    /// EUR/kWh; `None` for a `"log"`-imported row (the decision log doesn't carry prices).
    #[serde(default)]
    pub import_price: Option<f64>,
    #[serde(default)]
    pub export_price: Option<f64>,
    /// Snapshotted at record time so the row stays self-contained even if the live config changes.
    #[serde(default)]
    pub wear_eur_per_kwh: f64,
    #[serde(default)]
    pub eur_czk_rate: f64,
    #[serde(default)]
    pub planned: Planned,
    #[serde(default)]
    pub measured: Option<Measured>,
    #[serde(default)]
    pub scored: bool,
    /// Why `scored` is false, or a transparency note on a scored row (e.g. the fallback mix).
    #[serde(default)]
    pub score_note: Option<String>,
    #[serde(default)]
    pub scored_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub planned_cost_eur: Option<f64>,
    /// `None` unless prices and measured grid flows both exist.
    #[serde(default)]
    pub realized_cost_eur: Option<f64>,
    #[serde(default)]
    pub misses: Vec<String>,
}

impl LedgerRow {
    /// Build a row from the `TimelineBlock` the controllers were given for it — decision 1/2's
    /// "what is the decision" resolved into the ledger's own shape. `source` is the caller's
    /// resolution (`"frozen"` / `"block0"` / `"degraded"` / `"plan-snapshot"`); `wear_eur_per_kwh`
    /// and `eur_czk_rate` are snapshotted from the config/plan that produced `block`.
    pub fn from_block(
        block: &TimelineBlock,
        plan_flags: PlanFlags,
        source: &str,
        wear_eur_per_kwh: f64,
        eur_czk_rate: f64,
        recorded_at: DateTime<Utc>,
    ) -> LedgerRow {
        let dt_h = f64::from(block.dt_minutes) / 60.0;
        // A non-finite plan number (NaN/±inf — a stale or divide-by-zero upstream reading) must
        // never reach `Planned`'s non-`Option` fields: serde_json serializes NaN/infinite as JSON
        // `null`, which those fields can't deserialize back, corrupting the WHOLE store on next load
        // (see the near-identical guard in `score_one_block`). Clamp to 0.0 instead.
        let pv_kw = finite_or_zero(block.pv_kw);
        let load_kw = finite_or_zero(block.load_kw);
        let charge_kw = finite_or_zero(block.charge_kw);
        let discharge_kw = finite_or_zero(block.discharge_kw);
        let grid_import_kw = finite_or_zero(block.grid_import_kw);
        let grid_export_kw = finite_or_zero(block.grid_export_kw);
        let heat_kw = finite_map(&block.heat_kw);
        let ev_charge_kw = finite_map(&block.ev_charge_kw);
        let controllable_load_kw = finite_map(&block.controllable_load_kw);
        let planned_cost_eur = Some(block_cost_eur(
            block.import_price,
            block.export_price,
            wear_eur_per_kwh,
            grid_import_kw * dt_h,
            grid_export_kw * dt_h,
            discharge_kw * dt_h,
        ));
        LedgerRow {
            t: block.t,
            dt_minutes: block.dt_minutes,
            recorded_at,
            source: source.to_string(),
            slot: block.slot.clone(),
            export_enabled: block.export_enabled,
            inverter_on: block.inverter_on,
            degraded: plan_flags.degraded,
            relaxed: plan_flags.relaxed,
            rounded: plan_flags.rounded,
            drifted: false,
            price_is_placeholder: block.price_is_placeholder,
            import_price: Some(block.import_price),
            export_price: Some(block.export_price),
            wear_eur_per_kwh,
            eur_czk_rate,
            planned: Planned {
                pv_kw,
                load_kw,
                charge_kw,
                discharge_kw,
                grid_import_kw,
                grid_export_kw,
                heat_kw,
                ev_charge_kw,
                controllable_load_kw,
            },
            measured: None,
            scored: false,
            score_note: None,
            scored_at: None,
            planned_cost_eur,
            realized_cost_eur: None,
            misses: Vec::new(),
        }
    }
}

fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Non-finite entries become 0 and zero entries are DROPPED, so a row built here is identical to
/// the same row read back from the store (whose maps never carry zeros) — what lets
/// [`rows_equivalent`] recognise an unchanged re-import after a save/load cycle.
fn finite_map(m: &HashMap<String, f64>) -> HashMap<String, f64> {
    m.iter()
        .map(|(k, v)| (k.clone(), finite_or_zero(*v)))
        .filter(|(_, v)| *v != 0.0)
        .collect()
}

/// Decision 3's cost formula, shared by both the planned and the realized side:
/// `import_price·import_kWh − export_price·export_kWh + wear·discharge_kWh`.
pub fn block_cost_eur(
    import_price: f64,
    export_price: f64,
    wear_eur_per_kwh: f64,
    import_kwh: f64,
    export_kwh: f64,
    discharge_kwh: f64,
) -> f64 {
    import_price * import_kwh - export_price * export_kwh + wear_eur_per_kwh * discharge_kwh
}

/// The measured side of one block, once every required sample is in hand — the six Growatt sums are
/// always complete here (that completeness is exactly what gates whether [`score`] gets called at
/// all); `heat_kwh`/`ev_kwh` may still carry per-item `None`s.
#[derive(Debug, Clone, Default)]
pub struct MeasuredBlock {
    pub pv_kwh: f64,
    pub load_kwh: f64,
    pub charge_kwh: f64,
    pub discharge_kwh: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub heat_kwh: HashMap<String, Option<f64>>,
    pub ev_kwh: HashMap<String, Option<f64>>,
}

/// The smallest planned dispatch `classify_mode` ever commands (`app::classify_mode` demotes
/// anything under `battery.min_dispatch_kw` to `regular`) — below this a "miss" would just be
/// `classify_mode` noise, not a real actuation failure.
pub(crate) const MIN_DISPATCH_KW: f64 = 1.0;
/// Separates "did not happen" from "happened less": a trickle from a stale inverter mode (the
/// dusk-export residual that motivated this item) reads a few percent of plan, never a fifth.
pub(crate) const MISS_RATIO: f64 = 0.2;
/// kWh of PV + discharge over a block that an `inverter_on: false` plan should never see — well
/// above meter noise (≈0.8 kW sustained over a 15-min block), so it signals the inverter actually
/// ran instead of staying off as planned.
const INVERTER_OFF_KWH: f64 = 0.2;
/// The publisher only switches a heating relay ON above this planned power
/// (`controllers/publisher`'s `on_threshold_kw` default, `config.rs::default_on_threshold_kw`) — a
/// planned fraction below it was never actuated as "on" in the first place, so a measured-zero duty
/// there is expected behaviour, not a miss.
const HEAT_ON_KW: f64 = 0.05;

/// `v` unless it's NaN/±inf — a non-finite cost would otherwise serialize as JSON `null` into a
/// plain (non-`Option`) field slot it doesn't exist for, except here both cost fields already ARE
/// `Option<f64>`, so the real hazard is the per-item `heat_kwh`/`ev_kwh` and the six Growatt sums,
/// sanitized the same way at their construction site in `score_one_block`. Kept as one small
/// predicate so every call site reads the same intent.
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// Score one row against its measured block: decision 3's cost both sides, decision 5's misses.
/// `None` measurements never trigger a miss (a `None` heat/EV item is simply skipped); degraded and
/// relaxed rows get no misses at all (the publisher never actuates either kind — see
/// `controllers/publisher/src/main.rs` — so there is no real decision to hold accountable). Returns
/// `(measured, planned_cost_eur, realized_cost_eur, misses)`.
pub fn score(
    row: &LedgerRow,
    m: &MeasuredBlock,
) -> (Measured, Option<f64>, Option<f64>, Vec<String>) {
    let dt_h = f64::from(row.dt_minutes) / 60.0;
    let measured = Measured {
        pv_kwh: m.pv_kwh,
        load_kwh: m.load_kwh,
        charge_kwh: m.charge_kwh,
        discharge_kwh: m.discharge_kwh,
        import_kwh: m.import_kwh,
        export_kwh: m.export_kwh,
        heat_kwh: m.heat_kwh.clone(),
        ev_kwh: m.ev_kwh.clone(),
    };
    let (planned_cost, realized_cost) = match (row.import_price, row.export_price) {
        (Some(ip), Some(ep)) => (
            finite(block_cost_eur(
                ip,
                ep,
                row.wear_eur_per_kwh,
                row.planned.grid_import_kw * dt_h,
                row.planned.grid_export_kw * dt_h,
                row.planned.discharge_kw * dt_h,
            )),
            finite(block_cost_eur(
                ip,
                ep,
                row.wear_eur_per_kwh,
                m.import_kwh,
                m.export_kwh,
                m.discharge_kwh,
            )),
        ),
        _ => (row.planned_cost_eur, None),
    };
    let misses = if row.degraded || row.relaxed {
        Vec::new()
    } else {
        detect_misses(row, m, dt_h)
    };
    (measured, planned_cost, realized_cost, misses)
}

fn detect_misses(row: &LedgerRow, m: &MeasuredBlock, dt_h: f64) -> Vec<String> {
    let mut misses = Vec::new();
    if row.planned.grid_export_kw >= MIN_DISPATCH_KW
        && m.export_kwh < row.planned.grid_export_kw * dt_h * MISS_RATIO
    {
        misses.push(format!(
            "export not actuated: planned {:.2} kW, measured {:.2} kW (PV {:.2} kW)",
            row.planned.grid_export_kw,
            m.export_kwh / dt_h,
            m.pv_kwh / dt_h
        ));
    }
    if row.slot == "charge_from_grid"
        && row.planned.charge_kw >= MIN_DISPATCH_KW
        && m.charge_kwh < row.planned.charge_kw * dt_h * MISS_RATIO
    {
        misses.push(format!(
            "grid charge not actuated: planned {:.2} kW, measured {:.2} kW",
            row.planned.charge_kw,
            m.charge_kwh / dt_h
        ));
    }
    if row.planned.discharge_kw >= MIN_DISPATCH_KW
        && m.discharge_kwh < row.planned.discharge_kw * dt_h * MISS_RATIO
    {
        misses.push(format!(
            "discharge not actuated: planned {:.2} kW, measured {:.2} kW",
            row.planned.discharge_kw,
            m.discharge_kwh / dt_h
        ));
    }
    for (zone, &planned_kw) in &row.planned.heat_kw {
        if planned_kw <= HEAT_ON_KW {
            continue;
        }
        if let Some(Some(kwh)) = m.heat_kwh.get(zone) {
            if *kwh <= 0.0 {
                misses.push(format!(
                    "heat:{zone}: planned {planned_kw:.2} kW, measured 0 (relay off)"
                ));
            }
        }
    }
    if !row.inverter_on && (m.pv_kwh + m.discharge_kwh) >= INVERTER_OFF_KWH {
        misses.push(format!(
            "inverter_off: planned off, measured PV+discharge {:.2} kWh",
            m.pv_kwh + m.discharge_kwh
        ));
    }
    misses
}

// ============================================================ Aggregation (the endpoint)

#[derive(Debug, Clone, Serialize)]
pub struct ByMode {
    pub mode: String,
    pub n: usize,
    pub planned_charge_kwh: f64,
    pub measured_charge_kwh: f64,
    pub charge_efficacy: Option<f64>,
    pub planned_discharge_kwh: f64,
    pub measured_discharge_kwh: f64,
    pub discharge_efficacy: Option<f64>,
    pub planned_export_kwh: f64,
    pub measured_export_kwh: f64,
    pub export_efficacy: Option<f64>,
    pub planned_import_kwh: f64,
    pub measured_import_kwh: f64,
    pub import_efficacy: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct ByModeAcc {
    n: usize,
    planned_charge_kwh: f64,
    measured_charge_kwh: f64,
    planned_discharge_kwh: f64,
    measured_discharge_kwh: f64,
    planned_export_kwh: f64,
    measured_export_kwh: f64,
    planned_import_kwh: f64,
    measured_import_kwh: f64,
}

/// `measured/planned`, `None` when `planned` is below 0.01 kWh (decision 7) — a near-zero plan makes
/// the ratio meaningless (and often wildly > 1 from measurement noise alone).
fn efficacy(measured: f64, planned: f64) -> Option<f64> {
    (planned >= 0.01).then_some(measured / planned)
}

impl ByModeAcc {
    fn into_report(self, mode: String) -> ByMode {
        ByMode {
            mode,
            n: self.n,
            planned_charge_kwh: self.planned_charge_kwh,
            measured_charge_kwh: self.measured_charge_kwh,
            charge_efficacy: efficacy(self.measured_charge_kwh, self.planned_charge_kwh),
            planned_discharge_kwh: self.planned_discharge_kwh,
            measured_discharge_kwh: self.measured_discharge_kwh,
            discharge_efficacy: efficacy(self.measured_discharge_kwh, self.planned_discharge_kwh),
            planned_export_kwh: self.planned_export_kwh,
            measured_export_kwh: self.measured_export_kwh,
            export_efficacy: efficacy(self.measured_export_kwh, self.planned_export_kwh),
            planned_import_kwh: self.planned_import_kwh,
            measured_import_kwh: self.measured_import_kwh,
            import_efficacy: efficacy(self.measured_import_kwh, self.planned_import_kwh),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ByDay {
    /// Site-local calendar date, `YYYY-MM-DD`.
    pub date: String,
    /// Rows this day that fed the totals below (scored, non-degraded, non-relaxed).
    pub n_scored: usize,
    /// Rows this day that did NOT (unscored, degraded, or relaxed) — excluded from every total so a
    /// day with few scored blocks reads as thin data, not as "little was planned".
    pub n_unscored: usize,
    /// Of `n_scored`, how many carried `price_is_placeholder` (a backfilled/imported row with no
    /// real price at decision time) — these are EXCLUDED from the cost sums below on both sides, so
    /// a placeholder-priced row's made-up price can never masquerade as real realized economics.
    pub n_placeholder: usize,
    pub planned_cost_eur: f64,
    pub realized_cost_eur: Option<f64>,
    pub planned_cost_czk: f64,
    pub realized_cost_czk: Option<f64>,
    pub planned_heating_kwh: f64,
    pub measured_heating_kwh: Option<f64>,
    pub planned_import_kwh: f64,
    pub measured_import_kwh: Option<f64>,
    pub planned_export_kwh: f64,
    pub measured_export_kwh: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct ByDayAcc {
    n_scored: usize,
    n_unscored: usize,
    n_placeholder: usize,
    planned_cost_eur: f64,
    realized_cost_eur: Option<f64>,
    planned_cost_czk: f64,
    realized_cost_czk: Option<f64>,
    planned_heating_kwh: f64,
    measured_heating_kwh: Option<f64>,
    planned_import_kwh: f64,
    measured_import_kwh: Option<f64>,
    planned_export_kwh: f64,
    measured_export_kwh: Option<f64>,
}

impl ByDayAcc {
    fn into_report(self, date: String) -> ByDay {
        ByDay {
            date,
            n_scored: self.n_scored,
            n_unscored: self.n_unscored,
            n_placeholder: self.n_placeholder,
            planned_cost_eur: self.planned_cost_eur,
            realized_cost_eur: self.realized_cost_eur,
            planned_cost_czk: self.planned_cost_czk,
            realized_cost_czk: self.realized_cost_czk,
            planned_heating_kwh: self.planned_heating_kwh,
            measured_heating_kwh: self.measured_heating_kwh,
            planned_import_kwh: self.planned_import_kwh,
            measured_import_kwh: self.measured_import_kwh,
            planned_export_kwh: self.planned_export_kwh,
            measured_export_kwh: self.measured_export_kwh,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Miss {
    pub t: DateTime<Utc>,
    pub slot: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerReport {
    pub days: i64,
    pub rows: Vec<LedgerRow>,
    pub by_mode: Vec<ByMode>,
    pub by_day: Vec<ByDay>,
    pub misses: Vec<Miss>,
    pub unscored: usize,
}

/// Aggregate the trailing `days` (clamped by the caller to 1..30) ending at `now` into the
/// `/api/ledger` report. Pure (no I/O) — `now` is a parameter so it's directly unit-testable.
///
/// `by_mode`/`by_day` planned-vs-measured totals are built from scored, non-degraded, non-relaxed
/// rows ONLY — on both sides. An unscored row has a planned side but no measured one; mixing it into
/// the planned total while a late-arriving row's measured side is absent would bias every efficacy
/// ratio and cost gap toward "the decision wasn't carried out" when the real reason is just "not
/// scored yet". `by_day` reports `n_scored`/`n_unscored` so that distinction stays visible; the
/// top-level `unscored` count (below) is unaffected and still counts every unscored row in the
/// window regardless of mode/day.
pub fn summarize(
    rows: &[LedgerRow],
    days: i64,
    site: &SiteConfig,
    now: DateTime<Utc>,
) -> LedgerReport {
    let window_start = now - Duration::days(days);
    let in_window: Vec<&LedgerRow> = rows
        .iter()
        .filter(|r| r.t >= window_start && r.t <= now)
        .collect();

    let mut modes: BTreeMap<String, ByModeAcc> = BTreeMap::new();
    let mut day_accs: BTreeMap<String, ByDayAcc> = BTreeMap::new();
    let mut misses: Vec<Miss> = Vec::new();
    let mut unscored = 0usize;

    for r in &in_window {
        let dt_h = f64::from(r.dt_minutes) / 60.0;
        if !r.scored {
            unscored += 1;
        }

        for reason in &r.misses {
            misses.push(Miss {
                t: r.t,
                slot: r.slot.clone(),
                reason: reason.clone(),
            });
        }

        let date =
            r.t.with_timezone(&site.offset_at(r.t))
                .format("%Y-%m-%d")
                .to_string();
        let day_acc = day_accs.entry(date).or_default();

        let eligible = r.scored && !r.degraded && !r.relaxed;
        if !eligible {
            day_acc.n_unscored += 1;
            continue;
        }
        day_acc.n_scored += 1;

        let mode_acc = modes.entry(r.slot.clone()).or_default();
        mode_acc.n += 1;
        mode_acc.planned_charge_kwh += r.planned.charge_kw * dt_h;
        mode_acc.planned_discharge_kwh += r.planned.discharge_kw * dt_h;
        mode_acc.planned_export_kwh += r.planned.grid_export_kw * dt_h;
        mode_acc.planned_import_kwh += r.planned.grid_import_kw * dt_h;

        day_acc.planned_heating_kwh += r.planned.heat_kw.values().sum::<f64>() * dt_h;
        day_acc.planned_import_kwh += r.planned.grid_import_kw * dt_h;
        day_acc.planned_export_kwh += r.planned.grid_export_kw * dt_h;
        // A placeholder-priced row (backfilled/imported with no real price at decision time) has a
        // made-up `planned_cost_eur`/`realized_cost_eur` — excluded from both cost sums so it can
        // never masquerade as real realized economics; kWh totals above are price-independent and
        // stay included. Counted separately so the day's thin cost coverage is still visible.
        if r.price_is_placeholder {
            day_acc.n_placeholder += 1;
        } else {
            let planned_cost = r.planned_cost_eur.unwrap_or(0.0);
            day_acc.planned_cost_eur += planned_cost;
            day_acc.planned_cost_czk += planned_cost * r.eur_czk_rate;
            if let Some(c) = r.realized_cost_eur {
                *day_acc.realized_cost_eur.get_or_insert(0.0) += c;
                *day_acc.realized_cost_czk.get_or_insert(0.0) += c * r.eur_czk_rate;
            }
        }
        // `eligible` implies `r.scored`, which only ever sets `measured: Some(..)` (see
        // `score_one_block`) — the `if let` stays defensive rather than unwrapping.
        if let Some(m) = &r.measured {
            mode_acc.measured_charge_kwh += m.charge_kwh;
            mode_acc.measured_discharge_kwh += m.discharge_kwh;
            mode_acc.measured_export_kwh += m.export_kwh;
            mode_acc.measured_import_kwh += m.import_kwh;

            if m.heat_kwh.values().any(Option::is_some) {
                let known: f64 = m.heat_kwh.values().filter_map(|v| *v).sum();
                *day_acc.measured_heating_kwh.get_or_insert(0.0) += known;
            }
            *day_acc.measured_import_kwh.get_or_insert(0.0) += m.import_kwh;
            *day_acc.measured_export_kwh.get_or_insert(0.0) += m.export_kwh;
        }
    }

    misses.sort_by_key(|m| std::cmp::Reverse(m.t));
    let rows_out: Vec<LedgerRow> = in_window.into_iter().cloned().collect();

    LedgerReport {
        days,
        rows: rows_out,
        by_mode: modes.into_iter().map(|(m, a)| a.into_report(m)).collect(),
        by_day: day_accs
            .into_iter()
            .map(|(d, a)| a.into_report(d))
            .collect(),
        misses,
        unscored,
    }
}

// ============================================================ Store

/// How long a scored or unscoreable row stays in the store — also the endpoint's max `?days=` (see
/// `web::get_ledger`'s clamp, which reuses this constant so the two can never drift apart).
pub(crate) const RETENTION_DAYS: i64 = 30;
/// A block becomes due once this long after it ends (data-landing lag).
const SCORING_LAG: Duration = Duration::minutes(5);
/// Stop retrying an unscoreable block after this long and record it as given up.
const MAX_AGE: Duration = Duration::hours(48);

fn is_due(row: &LedgerRow, now: DateTime<Utc>) -> bool {
    now >= row.t + Duration::minutes(i64::from(row.dt_minutes)) + SCORING_LAG
}

fn is_aged_out(row: &LedgerRow, now: DateTime<Utc>) -> bool {
    now >= row.t + Duration::minutes(i64::from(row.dt_minutes)) + MAX_AGE
}

/// A pending day's scoring IO result for one row, applied back onto the store by `t`.
#[derive(Debug, Clone)]
pub struct ScoreResult {
    pub measured: Option<Measured>,
    pub planned_cost_eur: Option<f64>,
    pub realized_cost_eur: Option<f64>,
    pub misses: Vec<String>,
    pub scored: bool,
    pub score_note: Option<String>,
}

fn apply_result(row: &mut LedgerRow, result: ScoreResult, now: DateTime<Utc>) {
    row.measured = result.measured;
    if result.planned_cost_eur.is_some() {
        row.planned_cost_eur = result.planned_cost_eur;
    }
    row.realized_cost_eur = result.realized_cost_eur;
    row.misses = result.misses;
    row.scored = result.scored;
    row.score_note = result.score_note;
    row.scored_at = Some(now);
}

/// Whether `source` is a live, loop-recorded decision (`"frozen"`/`"block0"`/`"degraded"`/`"late"`)
/// rather than a backfilled one (`"plan-snapshot"`/`"log"`) — ground truth from the running loop
/// always outranks a backfill.
fn is_loop_source(source: &str) -> bool {
    !matches!(source, "plan-snapshot" | "log")
}

/// Whether `a` and `b` record the identical decision for the identical block — the fields that make
/// up "what was decided", not scoring state (which is recorded later and would never match on a
/// fresh import anyway). Used by [`Ledger::record`] to recognize a re-import of unchanged data as a
/// no-op rather than a "replace".
fn rows_equivalent(a: &LedgerRow, b: &LedgerRow) -> bool {
    a.t == b.t
        && a.dt_minutes == b.dt_minutes
        && a.slot == b.slot
        && a.export_enabled == b.export_enabled
        && a.inverter_on == b.inverter_on
        && a.planned == b.planned
        && a.import_price == b.import_price
        && a.export_price == b.export_price
}

/// What [`Ledger::record`] actually did — so a caller (the `ledger import` CLI) can report counts
/// honestly instead of assuming every call added a row (it may have replaced one, or been a no-op).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// No existing row overlapped `[t, t+dt)`; the row was inserted as new.
    Inserted,
    /// One or more overlapping rows were removed and the new row inserted in their place.
    Replaced,
    /// An overlapping row is loop-recorded or already scored — ground truth outranks the incoming
    /// row, which was dropped.
    Skipped,
    /// The sole overlapping row already records the identical decision (same `t`/`dt_minutes`/
    /// `slot`/`planned`/prices) — nothing was written, so re-importing the same file repeatedly
    /// never reports "replaced" for a no-op.
    Deduped,
}

/// The decision ledger's JSON store: an in-memory, mutex-guarded `Vec<LedgerRow>` kept sorted by
/// `t`, persisted to `MPC_LEDGER_STORE` by temp-file + rename. `write` serializes concurrent
/// persists (so the last writer always lands the newest state onto the shared `.tmp` path); `rows`
/// is only ever held for a quick in-memory mutation or clone, never across I/O or an `.await`.
pub struct Ledger {
    path: PathBuf,
    rows: StdMutex<Vec<LedgerRow>>,
    write: StdMutex<()>,
    /// Set by every mutation that actually changed `rows`, cleared by [`Self::persist`] (just
    /// before it snapshots `rows`, so a mutation racing the snapshot is never lost — it simply
    /// re-dirties and is caught by the next persist). Lets the periodic scorer skip writing the
    /// store to disk on a tick where nothing changed.
    dirty: AtomicBool,
    /// Set at [`Self::load`] when the on-disk file existed but failed to parse. The broken file is
    /// left untouched until the FIRST [`Self::persist`] — which quarantines it to `.corrupt` right
    /// before overwriting it — so a read-only `ledger show` (which never persists) can never rename
    /// anything.
    quarantine_on_persist: AtomicBool,
}

impl Ledger {
    fn store_path() -> PathBuf {
        PathBuf::from(
            std::env::var("MPC_LEDGER_STORE")
                .unwrap_or_else(|_| "decision_ledger.json".to_string()),
        )
    }

    /// Returns the loaded rows and whether the on-disk file needs quarantining at the next persist
    /// (it existed but could not be read OR could not be parsed). A missing file is the normal
    /// empty-store case, not an error. An unreadable-but-present file (permissions, non-UTF-8) is
    /// logged as an error AND quarantined like a parse failure — we never read its bytes, so we
    /// can't tell it apart from a genuinely corrupt store, and either way overwriting it with `[]`
    /// at the next persist would destroy it for good.
    fn load(path: &Path) -> (Vec<LedgerRow>, bool) {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), false),
            Err(e) => {
                eprintln!(
                    "[ledger] ERROR: store at {} could not be read ({e}) — starting with an EMPTY \
                     in-memory history; the broken file stays on disk until the next write, which \
                     will move it aside to {}.corrupt first",
                    path.display(),
                    path.display()
                );
                return (Vec::new(), true);
            }
        };
        match serde_json::from_str::<Vec<LedgerRow>>(&raw) {
            Ok(mut rows) => {
                rows.sort_by_key(|r| r.t);
                (rows, false)
            }
            Err(e) => {
                eprintln!(
                    "[ledger] store at {} was unparseable ({e}) — starting an EMPTY in-memory \
                     history; the broken file stays on disk until the next write, which will move \
                     it aside to {}.corrupt first",
                    path.display(),
                    path.display()
                );
                (Vec::new(), true)
            }
        }
    }

    fn from_loaded(path: PathBuf, rows: Vec<LedgerRow>, quarantine: bool) -> Ledger {
        Ledger {
            path,
            rows: StdMutex::new(rows),
            write: StdMutex::new(()),
            dirty: AtomicBool::new(false),
            quarantine_on_persist: AtomicBool::new(quarantine),
        }
    }

    /// Open (or create) the store, resolving `MPC_LEDGER_STORE` ONCE here — later env mutation
    /// (tests aside) never moves the file a live process writes to.
    pub fn open() -> Ledger {
        let path = Self::store_path();
        let (rows, quarantine) = Self::load(&path);
        Self::from_loaded(path, rows, quarantine)
    }

    #[cfg(test)]
    fn open_at(path: PathBuf) -> Ledger {
        let (rows, quarantine) = Self::load(&path);
        Self::from_loaded(path, rows, quarantine)
    }

    fn lock_rows(&self) -> std::sync::MutexGuard<'_, Vec<LedgerRow>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Insert `row`, keeping `t` order, after resolving overlap with whatever is already there.
    /// `row`'s interval is `[row.t, row.t + row.dt_minutes)`; every existing row whose own interval
    /// overlaps it is a candidate to replace (handles a coarser import later superseded by finer
    /// ones, or vice versa — see spec.md's overlap rule). If ANY overlapping row is loop-recorded or
    /// already scored, that's ground truth: `row` is dropped untouched. Otherwise every overlapping
    /// row is removed and `row` is inserted in their place — a later, nearer-term/finer import always
    /// wins over an earlier, coarser one.
    pub fn record(&self, row: LedgerRow) -> RecordOutcome {
        let new_end = row.t + Duration::minutes(i64::from(row.dt_minutes));
        let mut rows = self.lock_rows();
        let overlapping: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let r_end = r.t + Duration::minutes(i64::from(r.dt_minutes));
                r.t < new_end && row.t < r_end
            })
            .map(|(i, _)| i)
            .collect();

        if overlapping.is_empty() {
            let idx = rows.partition_point(|r| r.t < row.t);
            rows.insert(idx, row);
            self.dirty.store(true, Ordering::Relaxed);
            return RecordOutcome::Inserted;
        }

        // Re-recording the exact same decision (e.g. re-running `ledger import` on an unchanged
        // file) is pure churn, not an update — remove-then-reinsert would report "replaced" and
        // dirty the store for a byte-identical result. Only meaningful for a single overlapping row:
        // a new row spanning several existing ones is a real shape change even if, pathologically,
        // their union happened to match it.
        if let [i] = overlapping[..] {
            if !is_loop_source(&row.source) && rows_equivalent(&rows[i], &row) {
                return RecordOutcome::Deduped;
            }
        }

        let authoritative = overlapping
            .iter()
            .any(|&i| is_loop_source(&rows[i].source) || rows[i].scored);
        if authoritative {
            return RecordOutcome::Skipped;
        }

        for &i in overlapping.iter().rev() {
            rows.remove(i);
        }
        let idx = rows.partition_point(|r| r.t < row.t);
        rows.insert(idx, row);
        self.dirty.store(true, Ordering::Relaxed);
        RecordOutcome::Replaced
    }

    pub fn due_for_scoring(&self, now: DateTime<Utc>) -> Vec<LedgerRow> {
        self.lock_rows()
            .iter()
            .filter(|r| !r.scored && is_due(r, now) && !is_aged_out(r, now))
            .cloned()
            .collect()
    }

    /// Give up on any still-unscored row that has crossed [`MAX_AGE`] — idempotent (only touches a
    /// row once, by checking the note it leaves behind).
    pub fn finalize_aged(&self, now: DateTime<Utc>) {
        let mut rows = self.lock_rows();
        let mut changed = false;
        for r in rows.iter_mut() {
            if !r.scored && is_aged_out(r, now) && r.score_note.as_deref() != Some("aged out") {
                r.score_note = Some("aged out".to_string());
                r.scored_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Apply scoring results by `t`, onto rows still unscored (a row scored by a concurrent pass —
    /// or by `ledger import` seeing it already scored — is left alone).
    pub fn apply_scores(&self, scores: Vec<(DateTime<Utc>, ScoreResult)>, now: DateTime<Utc>) {
        let mut rows = self.lock_rows();
        let mut changed = false;
        for (t, result) in scores {
            if let Ok(idx) = rows.binary_search_by_key(&t, |r| r.t) {
                let row = &mut rows[idx];
                if row.scored {
                    continue;
                }
                // A retry that STILL can't score the block, with the identical reason (e.g. the same
                // Growatt field missing), isn't a real change to the row — only `scored_at` would
                // move, and nothing reads that off the persisted file (the API serves the in-memory
                // copy). Without this check, one chronically-failing row would dirty — and so rewrite
                // — the whole multi-MB store on every scorer tick for as long as it kept failing.
                if !result.scored && row.score_note == result.score_note {
                    row.scored_at = Some(now);
                    continue;
                }
                apply_result(row, result, now);
                changed = true;
            }
        }
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Drop rows older than [`RETENTION_DAYS`].
    pub fn prune(&self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(RETENTION_DAYS);
        let mut rows = self.lock_rows();
        let before = rows.len();
        rows.retain(|r| r.t >= cutoff);
        if rows.len() != before {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// A clone of every row currently held — for `/api/ledger` (summarized in-memory, no DB) and the
    /// `ledger show` CLI.
    pub fn rows_snapshot(&self) -> Vec<LedgerRow> {
        self.lock_rows().clone()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Move a store we could not read or parse aside to `.corrupt`, BEFORE [`Self::persist`] writes
    /// fresh state over its path. A separate function (rather than inlined rename-and-ignore) so the
    /// failure path is unit-testable on its own and so `persist` can propagate its error cleanly: if
    /// the rename itself fails, we must NOT fall through to overwriting a file we never actually
    /// examined — that would destroy data we couldn't even confirm was unrecoverable.
    fn quarantine_broken_store(&self) -> Result<()> {
        let aside = format!("{}.corrupt", self.path.display());
        match std::fs::rename(&self.path, &aside) {
            Ok(()) => eprintln!(
                "[ledger] quarantined the broken store to {aside} before writing fresh state"
            ),
            // Removed by hand since it failed to load: nothing left to preserve.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("quarantining broken ledger store to {aside}"))
            }
        }
        Ok(())
    }

    /// Serialize under a short `rows` lock, then write a temp file + rename — atomic on the same
    /// filesystem, so a crash mid-write can't corrupt the store. `write` spans the whole function so
    /// two concurrent persists (the loop's scorer and a `ledger score` CLI run, say) don't share one
    /// `.tmp` path and race each other's rename; the last writer through always lands the newest
    /// in-memory state, never a stale one. The temp filename includes this process's pid so two
    /// DIFFERENT processes pointed at the same store (the server plus a concurrent `ledger` CLI run
    /// against it — don't do that, see docs/api.md) can't collide on one shared `.tmp` path either.
    pub fn persist(&self) -> Result<()> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if self.quarantine_on_persist.load(Ordering::Relaxed) {
            // Propagate the error WITHOUT writing: a failed quarantine leaves `quarantine_on_persist`
            // set (and `dirty` untouched) so the next persist attempt tries again instead of silently
            // overwriting the broken file.
            self.quarantine_broken_store()?;
            self.quarantine_on_persist.store(false, Ordering::Relaxed);
        }
        // Cleared before the snapshot below: a mutation landing between this line and the clone is
        // included in THIS write anyway; one landing after re-dirties and is caught by the next persist.
        self.dirty.store(false, Ordering::Relaxed);
        if let Err(e) = self.write_atomic() {
            // A failed write must be retried, not silently dropped on the floor.
            self.dirty.store(true, Ordering::Relaxed);
            return Err(e);
        }
        Ok(())
    }

    fn write_atomic(&self) -> Result<()> {
        let json = {
            let rows = self.lock_rows();
            serde_json::to_string(&*rows).context("serializing decision ledger")?
        };
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("creating decision ledger directory")?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, json).context("writing decision ledger store")?;
        std::fs::rename(&tmp, &self.path).context("replacing decision ledger store")?;
        Ok(())
    }
}

// ============================================================ Loop hook (pure)

/// The mpc loop's rollover tracker (called from `mpc_loop::run`): given the row currently being
/// tracked for the block in progress (`pending`), this tick's rollover-adjusted current block
/// (`block`), the freeze-window commitment carried INTO this tick (before this tick's own
/// reassignment targets the NEXT mark), and this tick's own block 0, decides what `pending` becomes
/// and whether a just-ended row should be emitted to the store. See decision 1 and the Researcher's
/// amendment in `spec.md` for the exact source precedence and the drift rule.
///
/// How long after a block's start a FIRST tracking (no pending row, no frozen commitment) is still
/// trusted as "block0" rather than labelled `"late"` — the loop's own tick cadence (≤1 min) means a
/// normal rollover reaches this code within seconds of the block starting; a gap this wide means the
/// loop just started or recovered mid-block, so the controllers may have been running something else
/// (their own failsafe, a stale prior command) for the first few minutes of it.
const LATE_TRACKING_THRESHOLD: Duration = Duration::minutes(3);

/// Pure — no I/O, no `Ledger` access — so it's directly unit-testable like `freeze_committed_next`.
#[allow(clippy::too_many_arguments)]
pub fn track_ledger_block(
    pending: Option<LedgerRow>,
    block: DateTime<Utc>,
    committed_mark: Option<DateTime<Utc>>,
    committed_block: Option<&TimelineBlock>,
    block0: &TimelineBlock,
    flags: PlanFlags,
    wear_eur_per_kwh: f64,
    eur_czk_rate: f64,
    now: DateTime<Utc>,
) -> (Option<LedgerRow>, Option<LedgerRow>) {
    if let Some(p) = &pending {
        if block < p.t {
            return (pending, None); // a backward step is never acted on
        }
    }
    let ended = pending.as_ref().filter(|p| block > p.t).cloned();
    let block0_is_clean = !flags.degraded && !flags.relaxed && block0.t == block;

    let current = match &pending {
        Some(p) if p.t == block => {
            let mut p = p.clone();
            if p.source == "degraded" && block0_is_clean {
                // Same rule as the fresh-tracking arm below: a clean plan only reaching this block
                // more than `LATE_TRACKING_THRESHOLD` after it started means the controllers ran
                // something else (the degraded fallback) for the first few minutes — that's "late",
                // not an on-time "block0", even though it's arriving via the upgrade path rather than
                // first tracking.
                let source = if now - block > LATE_TRACKING_THRESHOLD {
                    "late"
                } else {
                    "block0"
                };
                p = LedgerRow::from_block(
                    block0,
                    flags,
                    source,
                    wear_eur_per_kwh,
                    eur_czk_rate,
                    p.recorded_at,
                );
            } else if block0_is_clean && decision_drifted(&p, block0) {
                p.drifted = true;
            }
            Some(p)
        }
        _ => {
            let frozen = committed_mark.filter(|m| *m == block).and(committed_block);
            let (source, decision, row_flags) = match frozen {
                Some(b) => (
                    "frozen",
                    b,
                    PlanFlags {
                        degraded: false,
                        relaxed: false,
                        rounded: flags.rounded,
                    },
                ),
                None if block0_is_clean && now - block > LATE_TRACKING_THRESHOLD => {
                    ("late", block0, flags)
                }
                None if block0_is_clean => ("block0", block0, flags),
                None => ("degraded", block0, flags),
            };
            Some(LedgerRow::from_block(
                decision,
                row_flags,
                source,
                wear_eur_per_kwh,
                eur_czk_rate,
                now,
            ))
        }
    };
    (current, ended)
}

/// decision 1's drift rule: a later clean block 0 for the SAME block disagreeing on any actuated
/// field (slot, the two gates, or either battery rate beyond 0.05 kW).
fn decision_drifted(row: &LedgerRow, block0: &TimelineBlock) -> bool {
    const KW_EPS: f64 = 0.05;
    row.slot != block0.slot
        || row.export_enabled != block0.export_enabled
        || row.inverter_on != block0.inverter_on
        || (row.planned.charge_kw - block0.charge_kw).abs() > KW_EPS
        || (row.planned.discharge_kw - block0.discharge_kw).abs() > KW_EPS
}

// ============================================================ Scoring IO

const GROWATT_FIELDS: [&str; 6] = [
    "InputPower",
    "INVPowerToLocalLoad",
    "ChargePower",
    "DischargePower",
    "ACPowerToUser",
    "ACPowerToGrid",
];
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const INTER_DAY_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_DAYS_PER_PASS: usize = 3;
/// Heating-relay lookback: a day's blocks need the last known relay state before the day starts, in
/// case there was no transition that day at all (decision: bounded to 3 days, per spec.md).
const RELAY_LOOKBACK_DAYS: i64 = 3;

/// Score every row due for scoring, grouped into ≤[`MAX_DAYS_PER_PASS`] per-UTC-day InfluxDB
/// windows (one field per query, bounded by [`QUERY_TIMEOUT`], paused [`INTER_DAY_PAUSE`] between
/// days). Applies the results onto `ledger` itself (by `t`, only onto rows still unscored) and
/// returns the rows it just touched, for the `ledger score` CLI to print. Never panics or
/// propagates — every read failure degrades that row to "stays unscored" with a note.
pub async fn score_due(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ledger: &Ledger,
) -> Vec<LedgerRow> {
    let now = Utc::now();
    ledger.finalize_aged(now);
    let due = ledger.due_for_scoring(now);
    if due.is_empty() {
        return Vec::new();
    }

    let mut by_day: BTreeMap<NaiveDate, Vec<LedgerRow>> = BTreeMap::new();
    for row in due {
        by_day.entry(row.t.date_naive()).or_default().push(row);
    }

    let mut touched = Vec::new();
    for (day_idx, (date, rows)) in by_day.into_iter().take(MAX_DAYS_PER_PASS).enumerate() {
        if day_idx > 0 {
            tokio::time::sleep(INTER_DAY_PAUSE).await;
        }
        let day_scores = score_day(db, config, net, date, &rows).await;
        for (row, (_, result)) in rows.iter().zip(day_scores.iter()) {
            let mut updated = row.clone();
            apply_result(&mut updated, result.clone(), now);
            touched.push(updated);
        }
        ledger.apply_scores(day_scores, now);
    }
    touched
}

async fn score_day(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    date: NaiveDate,
    rows: &[LedgerRow],
) -> Vec<(DateTime<Utc>, ScoreResult)> {
    let day_start = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
    let day_end = day_start + Duration::days(1);
    let start_s = day_start.to_rfc3339();
    let stop_s = day_end.to_rfc3339();

    let mut field_samples: HashMap<&str, Vec<Option<f64>>> = HashMap::new();
    for field in GROWATT_FIELDS {
        let aligned = match tokio::time::timeout(
            QUERY_TIMEOUT,
            db.growatt_series(field, &start_s, &stop_s, "15m"),
        )
        .await
        {
            Ok(Ok(samples)) => align_15min(&samples, day_start, BLOCKS_PER_DAY),
            Ok(Err(e)) => {
                eprintln!("[ledger] growatt field {field} read failed for {date}: {e:#}");
                vec![None; BLOCKS_PER_DAY]
            }
            Err(_) => {
                eprintln!("[ledger] growatt field {field} read timed out for {date}");
                vec![None; BLOCKS_PER_DAY]
            }
        };
        field_samples.insert(field, aligned);
    }

    let heat_start = (day_start - Duration::days(RELAY_LOOKBACK_DAYS)).to_rfc3339();
    let heat_events: HashMap<String, Vec<(DateTime<Utc>, f64)>> =
        match tokio::time::timeout(QUERY_TIMEOUT, db.heating_relay_events(&heat_start, &stop_s))
            .await
        {
            Ok(Ok(by_room)) => by_room
                .into_iter()
                .map(|(room, samples)| {
                    (
                        room,
                        samples.into_iter().map(|s| (s.time, s.value)).collect(),
                    )
                })
                .collect(),
            Ok(Err(e)) => {
                eprintln!("[ledger] heating relay events read failed for {date}: {e:#}");
                HashMap::new()
            }
            Err(_) => {
                eprintln!("[ledger] heating relay events read timed out for {date}");
                HashMap::new()
            }
        };

    let mut ev_series: HashMap<String, Vec<Option<f64>>> = HashMap::new();
    for charger in &config.chargers {
        let Some(loc @ SourceLocator::Influx { .. }) = charger.sources.get("power") else {
            continue; // postgres/teslamate/http sources: this charger stays `null` for the ledger
        };
        match tokio::time::timeout(
            QUERY_TIMEOUT,
            db.read_locator_series(loc, &start_s, &stop_s, "15m"),
        )
        .await
        {
            Ok(Ok(samples)) => {
                ev_series.insert(
                    charger.name.clone(),
                    align_15min(&samples, day_start, BLOCKS_PER_DAY),
                );
            }
            Ok(Err(e)) => eprintln!(
                "[ledger] EV charger {} power read failed for {date}: {e:#}",
                charger.name
            ),
            Err(_) => eprintln!(
                "[ledger] EV charger {} power read timed out for {date}",
                charger.name
            ),
        }
    }

    let heated_zones: Vec<(String, f64)> = config
        .heating
        .zones
        .iter()
        .filter(|(zone, _)| {
            net.marker_indices
                .contains_key(&((*zone).clone(), "heating".to_string()))
        })
        .map(|(zone, spec)| (zone.clone(), spec.max_heat_kw))
        .collect();

    rows.iter()
        .map(|row| {
            let result = score_one_block(
                row,
                day_start,
                &field_samples,
                &heat_events,
                &ev_series,
                &heated_zones,
                db,
                config,
            );
            (row.t, result)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn score_one_block(
    row: &LedgerRow,
    day_start: DateTime<Utc>,
    field_samples: &HashMap<&str, Vec<Option<f64>>>,
    heat_events: &HashMap<String, Vec<(DateTime<Utc>, f64)>>,
    ev_series: &HashMap<String, Vec<Option<f64>>>,
    heated_zones: &[(String, f64)],
    db: &SourceClients,
    config: &ControlConfig,
) -> ScoreResult {
    let idx0 = (row.t - day_start).num_minutes() / 15;
    let n = i64::from(row.dt_minutes) / 15;
    let dt_h = f64::from(row.dt_minutes) / 60.0;

    let mut sums = [0.0f64; GROWATT_FIELDS.len()];
    let mut missing: Option<String> = None;
    'quarters: for q in 0..n {
        let idx = idx0 + q;
        if idx < 0 || idx as usize >= BLOCKS_PER_DAY {
            missing = Some(format!("block falls outside its UTC day at quarter {q}"));
            break 'quarters;
        }
        for (i, field) in GROWATT_FIELDS.iter().enumerate() {
            match field_samples[field].get(idx as usize).copied().flatten() {
                Some(w) if w.is_finite() => sums[i] += w * 0.001 * 0.25, // W -> kW -> kWh / quarter
                _ => {
                    // Treated the same as a missing sample: a non-finite reading must never reach
                    // `score()`'s `Measured` fields, which are plain `f64` — serializing NaN/±inf
                    // there would write JSON `null` into a non-`Option` slot and make the WHOLE
                    // store unparseable at the next load.
                    missing = Some(format!("{field} missing for quarter starting at {q}"));
                    break 'quarters;
                }
            }
        }
    }

    let Some(note) = missing else {
        let mut heat_kwh = HashMap::new();
        for (zone, max_heat_kw) in heated_zones {
            let value = db
                .zone_room(zone)
                .and_then(|room| {
                    heat_events
                        .get(room)
                        .and_then(|events| {
                            relay_duty(events, row.t, Duration::minutes(i64::from(row.dt_minutes)))
                        })
                        .map(|duty| duty * max_heat_kw * dt_h)
                })
                .filter(|v| v.is_finite()); // a non-finite per-item value degrades to "unknown",
                                            // never a stored NaN/inf (see the quarter-sum comment).
            heat_kwh.insert(zone.clone(), value);
        }
        let mut ev_kwh = HashMap::new();
        for charger in &config.chargers {
            let value = ev_series
                .get(&charger.name)
                .and_then(|samples| {
                    let start = usize::try_from(idx0).ok()?;
                    let end = start + n as usize;
                    let window = samples.get(start..end)?;
                    let kw_sum: Option<f64> = window.iter().copied().sum::<Option<f64>>();
                    kw_sum.map(|kw| kw * 0.25)
                })
                .filter(|v| v.is_finite());
            ev_kwh.insert(charger.name.clone(), value);
        }
        let measured_block = MeasuredBlock {
            pv_kwh: sums[0],
            load_kwh: sums[1],
            charge_kwh: sums[2],
            discharge_kwh: sums[3],
            import_kwh: sums[4],
            export_kwh: sums[5],
            heat_kwh,
            ev_kwh,
        };
        let (measured, planned_cost_eur, realized_cost_eur, misses) = score(row, &measured_block);
        return ScoreResult {
            measured: Some(measured),
            planned_cost_eur,
            realized_cost_eur,
            misses,
            scored: true,
            score_note: None,
        };
    };

    ScoreResult {
        measured: None,
        planned_cost_eur: None,
        realized_cost_eur: None,
        misses: Vec::new(),
        scored: false,
        score_note: Some(note),
    }
}

/// How long after startup the scorer first runs — offset from the loop's own startup `build_cache`
/// so the two don't contend for the DB at the same instant.
const SCORER_STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(90);
const SCORER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The ledger's own task (spawned in `web::serve`, beside the MPC loop supervisor — never inside the
/// loop task): score what's due, prune, persist, every [`SCORER_INTERVAL`]. Every failure is logged
/// and never propagates — a wedged DB must degrade this task alone, not the planning loop.
pub async fn run_scorer(state: std::sync::Arc<crate::web::AppState>) {
    tokio::time::sleep(SCORER_STARTUP_DELAY).await;
    let mut interval = tokio::time::interval(SCORER_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let scored = score_due(&state.db, &state.config, &state.net, &state.ledger).await;
        if !scored.is_empty() {
            println!("[ledger] scored {} block(s)", scored.len());
        }
        state.ledger.prune(Utc::now());
        // Only actually write when something changed: a tick with nothing due/pruned would
        // otherwise serialize + rewrite the whole store on the async runtime every 5 min for no
        // reason. The write itself still goes through `spawn_blocking` — synchronous file I/O on
        // the runtime thread would stall every other task sharing it.
        if state.ledger.is_dirty() {
            let ledger = std::sync::Arc::clone(&state.ledger);
            match tokio::task::spawn_blocking(move || ledger.persist()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("[ledger] persist failed: {e:#}"),
                Err(e) => eprintln!("[ledger] persist task panicked: {e}"),
            }
        }
    }
}

// ============================================================ CLI

#[derive(Debug, Deserialize)]
struct LedgerPlanEnvelopeData {
    timeline: Vec<TimelineBlock>,
    #[serde(default)]
    degraded: bool,
    #[serde(default)]
    relaxed: bool,
    #[serde(default)]
    rounded: bool,
    #[serde(default)]
    eur_czk_rate: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LedgerPlanEnvelope {
    computed_at: DateTime<Utc>,
    data: LedgerPlanEnvelopeData,
}

/// A `"log"`-sourced row from one parsed decision-log line — mode/gates/battery/grid only (the log
/// carries no prices, so costs stay `null`, and no per-zone heat breakdown).
fn ledger_row_from_log(d: &crate::export_audit::LogDecision) -> LedgerRow {
    let discharge_kw = d.battery_kw.max(0.0);
    let charge_kw = (-d.battery_kw).max(0.0);
    LedgerRow {
        t: d.block_start,
        dt_minutes: 15,
        recorded_at: d.block_start,
        source: "log".to_string(),
        slot: d.mode.clone(),
        export_enabled: d.export_enabled,
        inverter_on: d.inverter_on,
        degraded: false,
        relaxed: false,
        rounded: false,
        drifted: false,
        price_is_placeholder: false,
        import_price: None,
        export_price: None,
        wear_eur_per_kwh: 0.0,
        eur_czk_rate: 0.0,
        planned: Planned {
            pv_kw: 0.0,
            load_kw: 0.0,
            charge_kw,
            discharge_kw,
            grid_import_kw: d.grid_import_kw,
            grid_export_kw: d.grid_export_kw,
            heat_kw: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
        },
        measured: None,
        scored: false,
        score_note: None,
        scored_at: None,
        planned_cost_eur: None,
        realized_cost_eur: None,
        misses: Vec::new(),
    }
}

fn cmd_import(config: &ControlConfig, ledger: &Ledger, args: &[String]) -> Result<()> {
    let mut log_path: Option<String> = None;
    let mut plan_paths: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--log" => {
                log_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--plan" => {
                if let Some(p) = args.get(i + 1) {
                    plan_paths.push(p.clone());
                }
                i += 2;
            }
            _ => i += 1,
        }
    }

    let now = Utc::now();
    let wear_eur_per_kwh = config
        .tariff
        .czk_to_eur(config.tariff.battery_amortisation_czk);
    let mut added = 0usize;
    let mut replaced = 0usize;
    let mut deduped = 0usize;
    let mut not_ended = 0usize;
    let mut tally = |outcome: RecordOutcome| match outcome {
        RecordOutcome::Inserted => added += 1,
        RecordOutcome::Replaced => replaced += 1,
        RecordOutcome::Skipped | RecordOutcome::Deduped => deduped += 1,
    };

    if let Some(path) = &log_path {
        let text = if path == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(path).with_context(|| format!("reading log file {path}"))?
        };
        for line in text.lines() {
            let Some(d) = crate::export_audit::parse_decision_line(line) else {
                continue;
            };
            if d.block_start + Duration::minutes(15) > now {
                not_ended += 1;
                continue;
            }
            tally(ledger.record(ledger_row_from_log(&d)));
        }
    }

    for path in &plan_paths {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading plan file {path}"))?;
        let envelope: LedgerPlanEnvelope =
            serde_json::from_str(&text).with_context(|| format!("parsing plan envelope {path}"))?;
        let flags = PlanFlags {
            degraded: envelope.data.degraded,
            relaxed: envelope.data.relaxed,
            rounded: envelope.data.rounded,
        };
        let eur_czk_rate = envelope
            .data
            .eur_czk_rate
            .unwrap_or(config.tariff.eur_czk_rate);
        for block in &envelope.data.timeline {
            if block.t + Duration::minutes(i64::from(block.dt_minutes)) > now {
                not_ended += 1;
                continue;
            }
            let row = LedgerRow::from_block(
                block,
                flags,
                "plan-snapshot",
                wear_eur_per_kwh,
                eur_czk_rate,
                envelope.computed_at,
            );
            tally(ledger.record(row));
        }
    }

    ledger.persist()?;
    println!(
        "ledger import: added {added} row(s), replaced {replaced}, deduped {deduped} \
         (ground truth or an identical decision already there), skipped {not_ended} (block hadn't \
         ended yet)"
    );
    Ok(())
}

async fn cmd_score(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ledger: &Ledger,
) -> Result<()> {
    let scored = score_due(db, config, net, ledger).await;
    ledger.prune(Utc::now());
    ledger.persist()?;
    for row in &scored {
        let measured = row.measured.as_ref();
        println!(
            "{} {} planned charge/discharge {:.2}/{:.2} kW measured {:?}/{:?} kWh planned_cost {:?} EUR realized_cost {:?} EUR misses {:?}",
            row.t,
            row.slot,
            row.planned.charge_kw,
            row.planned.discharge_kw,
            measured.map(|m| m.charge_kwh),
            measured.map(|m| m.discharge_kwh),
            row.planned_cost_eur,
            row.realized_cost_eur,
            row.misses,
        );
    }
    let newly_scored = scored.iter().filter(|r| r.scored).count();
    println!(
        "ledger score: processed {} block(s) ({newly_scored} scored, {} still pending)",
        scored.len(),
        scored.len() - newly_scored
    );
    Ok(())
}

fn cmd_show(config: &ControlConfig, ledger: &Ledger, args: &[String]) -> Result<()> {
    let days = args
        .iter()
        .position(|a| a == "--days")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(7)
        .clamp(1, RETENTION_DAYS);
    let rows = ledger.rows_snapshot();
    let report = summarize(&rows, days, &config.site, Utc::now());
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// `ledger <import|score|show> ...` — read-only towards InfluxDB; writes only `MPC_LEDGER_STORE`.
/// `db` is a closure rather than an already-built client: `import`/`show` need no DB connection at
/// all (and so must not fail for lack of `INFLUX_TOKEN`) — it's built lazily, only for `score`.
///
/// The server holds its own store in memory: run this CLI only against a store the server is NOT
/// using (point `MPC_LEDGER_STORE` at a different file) — a concurrent write here would be clobbered
/// by the server's next persist, which never re-reads the file.
pub async fn run(
    db: impl FnOnce() -> Result<SourceClients>,
    config: &ControlConfig,
    net: &RcNetwork,
    args: &[String],
) -> Result<()> {
    let ledger = Ledger::open();
    match args.first().map(String::as_str) {
        Some("import") => cmd_import(config, &ledger, &args[1..]),
        Some("score") => {
            let db = db()?;
            cmd_score(&db, config, net, &ledger).await
        }
        Some("show") => cmd_show(config, &ledger, &args[1..]),
        other => anyhow::bail!(
            "usage: ledger <import|score|show> [...] (got {:?}) — run only against a store the \
             server is NOT using (point MPC_LEDGER_STORE at your own file): the server holds its \
             store in memory and a concurrent CLI write here would be lost at its next persist",
            other.unwrap_or("<nothing>")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ENV_LOCK;
    use std::sync::Mutex;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample_block(start: &str, dt_minutes: u32) -> TimelineBlock {
        TimelineBlock {
            t: t(start),
            dt_minutes,
            import_price: 0.10,
            export_price: 0.05,
            price_is_placeholder: false,
            pv_kw: 1.0,
            load_kw: 0.5,
            soc_kwh: 5.0,
            charge_kw: 0.0,
            discharge_kw: 2.0,
            grid_import_kw: 0.0,
            grid_export_kw: 1.5,
            curtail_kw: 0.0,
            heat_kw: HashMap::from([("livingroom".to_string(), 2.0)]),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            temp_c: HashMap::new(),
            slot: "discharge_to_grid".to_string(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    // ---------- compact map serialization ----------

    #[test]
    fn planned_map_serializes_only_nonzero_entries() {
        let mut heat_kw = HashMap::new();
        heat_kw.insert("livingroom".to_string(), 1.5);
        heat_kw.insert("attic".to_string(), 0.0); // must not appear in the JSON at all
        let planned = Planned {
            heat_kw,
            ..Default::default()
        };
        let json = serde_json::to_value(&planned).unwrap();
        let obj = json["heat_kw"].as_object().unwrap();
        assert_eq!(
            obj.len(),
            1,
            "only the non-zero entry should serialize: {obj:?}"
        );
        assert_eq!(obj["livingroom"], 1.5);

        // Deserializing back still defaults a never-written zone to 0.0 (lossless for `Planned`).
        let round_tripped: Planned = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.heat_kw.get("livingroom"), Some(&1.5));
        assert_eq!(round_tripped.heat_kw.get("attic"), None);
    }

    #[test]
    fn measured_map_serializes_only_known_entries() {
        let mut heat_kwh = HashMap::new();
        heat_kwh.insert("livingroom".to_string(), Some(0.3));
        heat_kwh.insert("attic".to_string(), None); // unknown — must not appear as `null`
        let measured = Measured {
            heat_kwh,
            ..Default::default()
        };
        let json = serde_json::to_value(&measured).unwrap();
        let obj = json["heat_kwh"].as_object().unwrap();
        assert_eq!(
            obj.len(),
            1,
            "only the known entry should serialize: {obj:?}"
        );
        assert_eq!(obj["livingroom"], 0.3);

        // Deserializing back: an absent zone defaults to an empty map, i.e. "unknown" — the same
        // meaning `None` carried before serialization.
        let round_tripped: Measured = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.heat_kwh.get("livingroom"), Some(&Some(0.3)));
        assert_eq!(round_tripped.heat_kwh.get("attic"), None);
    }

    // ---------- block_cost_eur ----------

    #[test]
    fn block_cost_matches_decision_3_formula() {
        let cost = block_cost_eur(0.10, 0.05, 0.02, 10.0, 4.0, 3.0);
        assert!((cost - (0.10 * 10.0 - 0.05 * 4.0 + 0.02 * 3.0)).abs() < 1e-9);
    }

    // ---------- from_block sanitizes non-finite numbers ----------

    #[test]
    fn from_block_sanitizes_non_finite_plan_numbers_to_zero() {
        let mut block = sample_block("2026-09-30T12:00:00Z", 15);
        block.pv_kw = f64::NAN;
        block.discharge_kw = f64::INFINITY;
        block.grid_export_kw = f64::NEG_INFINITY;
        block.heat_kw.insert("attic".to_string(), f64::NAN);

        let row =
            LedgerRow::from_block(&block, PlanFlags::default(), "block0", 0.02, 25.0, block.t);
        assert_eq!(row.planned.pv_kw, 0.0);
        assert_eq!(row.planned.discharge_kw, 0.0);
        assert_eq!(row.planned.grid_export_kw, 0.0);
        // Sanitized to 0 and, like every zero entry, dropped from the map (see `finite_map`).
        assert_eq!(row.planned.heat_kw.get("attic"), None);
        // A finite value elsewhere in the same map must survive untouched.
        assert_eq!(row.planned.heat_kw.get("livingroom"), Some(&2.0));

        // The sanitized row must actually be serializable (the whole point — NaN/inf would otherwise
        // write JSON `null` into these non-`Option` fields and break reload).
        serde_json::to_string(&row).expect("a sanitized row must always serialize");
    }

    // ---------- score / misses ----------

    fn scored_row(block: TimelineBlock) -> LedgerRow {
        LedgerRow::from_block(&block, PlanFlags::default(), "block0", 0.02, 25.0, block.t)
    }

    #[test]
    fn export_miss_detected_below_20_percent() {
        let row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        let m = MeasuredBlock {
            export_kwh: 0.01,
            pv_kwh: 0.0,
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.iter().any(|s| s.starts_with("export not actuated")));
    }

    #[test]
    fn export_efficacy_above_threshold_is_not_a_miss() {
        let row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        // planned export 1.5 kW * 0.25 h = 0.375 kWh; measured at 96% of that clears the 20% floor
        // easily. The block also plans a 2.0 kW discharge, so measured discharge must clear its own
        // 20% floor too, or an unrelated discharge miss would mask the thing under test.
        let m = MeasuredBlock {
            export_kwh: 0.36,
            discharge_kwh: 0.49, // planned 2.0 kW * 0.25 h = 0.5 kWh
            pv_kwh: 0.0,
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.is_empty(), "unexpected misses: {misses:?}");
    }

    #[test]
    fn heat_miss_only_when_duty_known_zero() {
        let row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        let mut heat_kwh = HashMap::new();
        heat_kwh.insert("livingroom".to_string(), Some(0.0));
        let m = MeasuredBlock {
            export_kwh: 1.0,
            heat_kwh,
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.iter().any(|s| s.starts_with("heat:livingroom")));

        // Unknown duty (None) must never trigger the miss.
        let mut heat_kwh_unknown = HashMap::new();
        heat_kwh_unknown.insert("livingroom".to_string(), None);
        let m2 = MeasuredBlock {
            export_kwh: 1.0,
            heat_kwh: heat_kwh_unknown,
            ..Default::default()
        };
        let (_, _, _, misses2) = score(&row, &m2);
        assert!(!misses2.iter().any(|s| s.starts_with("heat:")));
    }

    #[test]
    fn heat_miss_ignored_below_on_threshold() {
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.planned.heat_kw.insert("livingroom".to_string(), 0.03); // below HEAT_ON_KW (0.05)
        let mut heat_kwh = HashMap::new();
        heat_kwh.insert("livingroom".to_string(), Some(0.0));
        let m = MeasuredBlock {
            export_kwh: 1.0,
            discharge_kwh: 1.0,
            heat_kwh,
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(
            !misses.iter().any(|s| s.starts_with("heat:")),
            "a sub-threshold planned trickle was never actuated as 'on' in the first place: {misses:?}"
        );
    }

    #[test]
    fn grid_charge_miss_detected_below_20_percent() {
        let mut block = sample_block("2026-09-29T17:15:00Z", 15);
        block.slot = "charge_from_grid".to_string();
        block.charge_kw = 3.0;
        block.discharge_kw = 0.0;
        block.grid_export_kw = 0.0;
        let row = scored_row(block);
        let m = MeasuredBlock {
            charge_kwh: 0.05, // planned 3.0 kW * 0.25 h = 0.75 kWh; well under the 20% floor
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses
            .iter()
            .any(|s| s.starts_with("grid charge not actuated")));
    }

    #[test]
    fn grid_charge_miss_only_fires_in_the_charge_from_grid_slot() {
        let mut block = sample_block("2026-09-29T17:15:00Z", 15);
        block.slot = "regular".to_string();
        block.charge_kw = 3.0;
        block.discharge_kw = 0.0;
        block.grid_export_kw = 0.0;
        let row = scored_row(block);
        let m = MeasuredBlock::default();
        let (_, _, _, misses) = score(&row, &m);
        assert!(!misses
            .iter()
            .any(|s| s.starts_with("grid charge not actuated")));
    }

    #[test]
    fn discharge_miss_detected_below_20_percent() {
        let mut block = sample_block("2026-09-29T17:15:00Z", 15);
        block.grid_export_kw = 0.0; // isolate from the export miss
        let row = scored_row(block);
        let m = MeasuredBlock {
            discharge_kwh: 0.02, // planned 2.0 kW * 0.25 h = 0.5 kWh; well under the 20% floor
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses
            .iter()
            .any(|s| s.starts_with("discharge not actuated")));
    }

    #[test]
    fn inverter_off_miss_detected_when_pv_plus_discharge_flows() {
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.inverter_on = false;
        row.planned.grid_export_kw = 0.0;
        row.planned.discharge_kw = 0.0;
        row.planned.charge_kw = 0.0;
        let m = MeasuredBlock {
            pv_kwh: 0.15,
            discharge_kwh: 0.1, // sums to 0.25 kWh, above INVERTER_OFF_KWH (0.2)
            ..Default::default()
        };
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.iter().any(|s| s.starts_with("inverter_off")));
    }

    #[test]
    fn degraded_rows_get_no_misses() {
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.degraded = true;
        let m = MeasuredBlock::default(); // everything 0 — would otherwise miss on export/discharge
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.is_empty());
    }

    #[test]
    fn relaxed_rows_get_no_misses() {
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.relaxed = true;
        let m = MeasuredBlock::default(); // same rationale as degraded: the publisher never actuates it
        let (_, _, _, misses) = score(&row, &m);
        assert!(misses.is_empty());
    }

    #[test]
    fn realized_cost_null_without_prices() {
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.import_price = None;
        row.export_price = None;
        let m = MeasuredBlock::default();
        let (_, _, realized_cost, _) = score(&row, &m);
        assert_eq!(realized_cost, None);
    }

    #[test]
    fn non_finite_cost_never_leaks_into_the_row() {
        // A NaN price (a broken upstream feed) must never surface as a stored cost — serializing a
        // non-finite f64 would write JSON `null` into a slot that round-trips fine here (both cost
        // fields are already `Option<f64>`), so the regression this guards is `finite()` being
        // skipped, not a store-format break; still worth pinning down directly.
        let mut row = scored_row(sample_block("2026-09-29T17:15:00Z", 15));
        row.import_price = Some(f64::NAN);
        let m = MeasuredBlock::default();
        let (_, planned_cost, realized_cost, _) = score(&row, &m);
        assert_eq!(planned_cost, None);
        assert_eq!(realized_cost, None);
    }

    // ---------- score_one_block (scoring IO's pure core) ----------

    fn test_control_config() -> ControlConfig {
        json5::from_str(
            r#"{ site: { latitude: 50.0, longitude: 14.0, utc_offset_hours: 1 },
                 heating: { cop: 1.0, comfort_penalty: 10.0, zones: {} } }"#,
        )
        .unwrap()
    }

    /// `from_parts` makes no network call — fine for a unit test, since `score_one_block` only ever
    /// calls `zone_room` (a pure lookup) on it, never a query.
    fn test_source_clients() -> SourceClients {
        SourceClients::with_signals(
            crate::influxdb::InfluxDB::from_parts("http://localhost:0", "org", "token").unwrap(),
            crate::source::DataSources::default(),
        )
    }

    #[test]
    fn score_one_block_sums_w_to_kwh_for_a_single_quarter() {
        let day_start = t("2026-09-30T00:00:00Z");
        let row = scored_row(sample_block("2026-09-30T00:00:00Z", 15));
        let watts = [1000.0, 2000.0, 3000.0, 4000.0, 5000.0, 6000.0];
        let mut field_samples: HashMap<&str, Vec<Option<f64>>> = HashMap::new();
        for (field, w) in GROWATT_FIELDS.iter().zip(watts) {
            let mut v = vec![None; BLOCKS_PER_DAY];
            v[0] = Some(w);
            field_samples.insert(*field, v);
        }
        let config = test_control_config();
        let db = test_source_clients();
        let result = score_one_block(
            &row,
            day_start,
            &field_samples,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &db,
            &config,
        );
        assert!(result.scored, "{:?}", result.score_note);
        let m = result.measured.unwrap();
        // W -> kW -> kWh over one 15-min quarter: w * 0.001 * 0.25.
        assert!((m.pv_kwh - 0.25).abs() < 1e-9, "pv_kwh was {}", m.pv_kwh);
        assert!((m.load_kwh - 0.5).abs() < 1e-9);
        assert!((m.charge_kwh - 0.75).abs() < 1e-9);
        assert!((m.discharge_kwh - 1.0).abs() < 1e-9);
        assert!((m.import_kwh - 1.25).abs() < 1e-9);
        assert!((m.export_kwh - 1.5).abs() < 1e-9);
    }

    #[test]
    fn score_one_block_hourly_row_sums_all_four_quarters() {
        let day_start = t("2026-09-30T00:00:00Z");
        let row = scored_row(sample_block("2026-09-30T00:00:00Z", 60));
        let mut field_samples: HashMap<&str, Vec<Option<f64>>> = HashMap::new();
        for field in GROWATT_FIELDS {
            let mut v = vec![None; BLOCKS_PER_DAY];
            for q in &mut v[0..4] {
                *q = Some(1000.0); // a flat 1 kW across the whole hour, on every field
            }
            field_samples.insert(field, v);
        }
        let config = test_control_config();
        let db = test_source_clients();
        let result = score_one_block(
            &row,
            day_start,
            &field_samples,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &db,
            &config,
        );
        assert!(result.scored, "{:?}", result.score_note);
        let m = result.measured.unwrap();
        // 1 kW held for a full hour = 1.0 kWh, on every field.
        assert!((m.pv_kwh - 1.0).abs() < 1e-9, "pv_kwh was {}", m.pv_kwh);
        assert!((m.export_kwh - 1.0).abs() < 1e-9);
    }

    #[test]
    fn score_one_block_missing_quarter_names_the_field_in_score_note() {
        let day_start = t("2026-09-30T00:00:00Z");
        let row = scored_row(sample_block("2026-09-30T00:00:00Z", 15));
        let mut field_samples: HashMap<&str, Vec<Option<f64>>> = HashMap::new();
        for field in GROWATT_FIELDS {
            field_samples.insert(field, vec![None; BLOCKS_PER_DAY]); // nothing has landed yet
        }
        let config = test_control_config();
        let db = test_source_clients();
        let result = score_one_block(
            &row,
            day_start,
            &field_samples,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &db,
            &config,
        );
        assert!(!result.scored);
        let note = result
            .score_note
            .expect("a missing quarter must leave a note");
        assert!(note.contains("InputPower"), "note was {note:?}");
    }

    // ---------- summarize ----------

    #[test]
    fn efficacy_null_below_001_kwh_planned() {
        assert_eq!(efficacy(0.5, 0.0), None);
        assert_eq!(efficacy(0.5, 1.0), Some(0.5));
    }

    #[test]
    fn summarize_filters_to_the_window_and_counts_unscored() {
        let site = test_site(1);
        let now = t("2026-10-01T00:00:00Z");
        let mut inside = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        inside.measured = Some(Measured {
            charge_kwh: 0.1,
            ..Default::default()
        });
        inside.scored = true;
        let outside = scored_row(sample_block("2026-09-01T12:00:00Z", 15));
        let report = summarize(&[inside, outside], 7, &site, now);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.unscored, 0);
        assert_eq!(report.by_mode.len(), 1);
        assert_eq!(report.by_mode[0].n, 1);
    }

    fn test_site(utc_offset_hours: i32) -> SiteConfig {
        SiteConfig {
            latitude: 50.0,
            longitude: 14.0,
            utc_offset_hours,
            timezone: None,
            ground_temperature_c: 16.0,
            public_holidays: Vec::new(),
            easter_holidays: false,
        }
    }

    #[test]
    fn by_mode_and_by_day_aggregate_scored_rows_only() {
        // Regression for finding 3/12: an unscored row's huge planned import must not leak into the
        // totals a scored row's measured side is compared against — it would otherwise both inflate
        // `planned_import_kwh` and drag `import_efficacy` toward zero for a reason that isn't a real
        // actuation failure, just "not scored yet".
        let site = test_site(1);
        let now = t("2026-10-01T00:00:00Z");

        let mut scored = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        scored.planned.grid_import_kw = 0.2; // 0.2 kW * 0.25 h = 0.05 kWh planned
        scored.scored = true;
        scored.measured = Some(Measured {
            import_kwh: 0.05,
            ..Default::default()
        });

        let mut unscored = scored_row(sample_block("2026-09-30T12:15:00Z", 15));
        unscored.planned.grid_import_kw = 400.0; // huge — must not leak into any total

        let report = summarize(&[scored, unscored], 7, &site, now);
        assert_eq!(report.unscored, 1);
        assert_eq!(report.by_mode.len(), 1);
        let mode = &report.by_mode[0];
        assert_eq!(mode.n, 1);
        assert!(
            (mode.planned_import_kwh - 0.05).abs() < 1e-9,
            "planned import leaked from the unscored row: {}",
            mode.planned_import_kwh
        );
        assert!(
            mode.import_efficacy.unwrap() > 0.9,
            "import_efficacy was dragged down by the unscored row: {:?}",
            mode.import_efficacy
        );

        assert_eq!(report.by_day.len(), 1);
        let day = &report.by_day[0];
        assert_eq!(day.n_scored, 1);
        assert_eq!(day.n_unscored, 1);
        assert!((day.planned_import_kwh - 0.05).abs() < 1e-9);
    }

    #[test]
    fn by_day_degraded_and_relaxed_rows_count_as_unscored_even_if_scored() {
        let site = test_site(1);
        let now = t("2026-10-01T00:00:00Z");
        let mut degraded = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        degraded.scored = true;
        degraded.degraded = true;
        degraded.measured = Some(Measured::default());
        let report = summarize(&[degraded], 7, &site, now);
        assert_eq!(report.by_mode.len(), 0);
        assert_eq!(report.by_day[0].n_scored, 0);
        assert_eq!(report.by_day[0].n_unscored, 1);
    }

    #[test]
    fn placeholder_priced_rows_are_excluded_from_cost_sums_but_counted() {
        let site = test_site(1);
        let now = t("2026-10-01T00:00:00Z");

        let mut real = scored_row(sample_block("2026-09-30T11:00:00Z", 15));
        real.scored = true;
        real.measured = Some(Measured::default());
        real.planned_cost_eur = Some(1.0);
        real.realized_cost_eur = Some(0.5);

        let mut placeholder = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        placeholder.scored = true;
        placeholder.price_is_placeholder = true;
        placeholder.measured = Some(Measured::default());
        // A made-up cost that must NOT reach the day's totals.
        placeholder.planned_cost_eur = Some(1000.0);
        placeholder.realized_cost_eur = Some(1000.0);

        let report = summarize(&[real, placeholder], 7, &site, now);
        assert_eq!(report.by_day.len(), 1);
        let day = &report.by_day[0];
        assert_eq!(
            day.n_scored, 2,
            "both rows are scored, non-degraded, non-relaxed"
        );
        assert_eq!(day.n_placeholder, 1);
        assert!(
            (day.planned_cost_eur - 1.0).abs() < 1e-9,
            "placeholder's cost must not inflate the sum: {}",
            day.planned_cost_eur
        );
        assert!((day.realized_cost_eur.unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn by_day_groups_by_site_local_date_and_converts_cost_to_czk() {
        // 2026-09-30T20:00Z at a +5h site offset is 2026-10-01T01:00 LOCAL — a block that must land
        // in the NEXT site-local calendar day, not the UTC one.
        let site = test_site(5);
        let now = t("2026-10-02T00:00:00Z");
        let mut row = scored_row(sample_block("2026-09-30T20:00:00Z", 15));
        row.scored = true;
        row.measured = Some(Measured::default());
        row.eur_czk_rate = 24.0;
        row.planned_cost_eur = Some(2.0);

        let report = summarize(&[row], 2, &site, now);
        assert_eq!(report.by_day.len(), 1);
        let day = &report.by_day[0];
        assert_eq!(day.date, "2026-10-01");
        assert!(
            (day.planned_cost_czk - 48.0).abs() < 1e-9,
            "planned_cost_czk was {}",
            day.planned_cost_czk
        );
    }

    // ---------- Ledger store ----------

    fn temp_store_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mpc_ledger_test_{name}_{}.json",
            std::process::id()
        ))
    }

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn record_and_persist_round_trips_across_restart() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let ledger = Ledger::open_at(path.clone());
        let row = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        assert_eq!(ledger.record(row.clone()), RecordOutcome::Inserted);
        ledger.persist().unwrap();
        assert!(path.exists());
        assert!(!PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id())).exists());

        let reopened = Ledger::open_at(path.clone());
        let rows = reopened.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].t, row.t);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_t_first_recorded_wins_unless_import_replaced_by_loop() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("same_t");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());

        let mut imported = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        imported.source = "plan-snapshot".to_string();
        assert_eq!(ledger.record(imported), RecordOutcome::Inserted);

        let mut from_loop = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        from_loop.source = "block0".to_string();
        from_loop.slot = "charge_from_grid".to_string();
        assert_eq!(ledger.record(from_loop.clone()), RecordOutcome::Replaced);

        let rows = ledger.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "block0");
        assert_eq!(rows[0].slot, "charge_from_grid");

        // A second loop-recorded row at the same `t` does NOT replace the first loop-recorded one.
        let mut another_loop = from_loop;
        another_loop.slot = "regular".to_string();
        assert_eq!(ledger.record(another_loop), RecordOutcome::Skipped);
        let rows2 = ledger.rows_snapshot();
        assert_eq!(rows2[0].slot, "charge_from_grid");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reimporting_an_identical_row_is_deduped_not_replaced() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("reimport_identical");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());

        let mut first = scored_row(sample_block("2026-09-30T21:00:00Z", 15));
        first.source = "plan-snapshot".to_string();
        assert_eq!(ledger.record(first), RecordOutcome::Inserted);

        // Re-importing the SAME file produces a byte-identical row (possibly a different
        // `recorded_at`/`source` label from a re-run, but the same t/dt/slot/planned/prices) — this
        // must be a no-op, not a "replaced".
        let mut reimported = scored_row(sample_block("2026-09-30T21:00:00Z", 15));
        reimported.source = "plan-snapshot".to_string();
        reimported.recorded_at += Duration::seconds(5);
        assert_eq!(ledger.record(reimported), RecordOutcome::Deduped);
        assert_eq!(ledger.rows_snapshot().len(), 1);

        // Still a no-op after a save/load cycle: the store drops zero map entries, and so does
        // `from_block` (see `finite_map`), so the reloaded row and a fresh import stay equal.
        ledger.persist().unwrap();
        let reloaded = Ledger::open_at(path.clone());
        let mut again = scored_row(sample_block("2026-09-30T21:00:00Z", 15));
        again.source = "plan-snapshot".to_string();
        assert_eq!(reloaded.record(again), RecordOutcome::Deduped);

        // A loop-recorded row for the same block is ground truth and takes over the import's slot
        // even when the decision matches — the label must say where the row really came from.
        let loop_row = scored_row(sample_block("2026-09-30T21:00:00Z", 15));
        assert_eq!(reloaded.record(loop_row), RecordOutcome::Replaced);
        assert_eq!(reloaded.rows_snapshot()[0].source, "block0");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlap_hourly_import_then_finer_quarter_replaces_it() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("overlap_hourly_then_quarter");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());

        let mut hourly = scored_row(sample_block("2026-09-30T21:00:00Z", 60));
        hourly.source = "log".to_string();
        assert_eq!(ledger.record(hourly), RecordOutcome::Inserted);

        let mut quarter = scored_row(sample_block("2026-09-30T21:15:00Z", 15));
        quarter.source = "log".to_string();
        assert_eq!(ledger.record(quarter), RecordOutcome::Replaced);

        let rows = ledger.rows_snapshot();
        assert_eq!(
            rows.len(),
            1,
            "the overlapping hourly row must be removed, not kept alongside"
        );
        assert_eq!(rows[0].t, t("2026-09-30T21:15:00Z"));
        assert_eq!(rows[0].dt_minutes, 15);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlap_quarter_import_then_coarser_hourly_replaces_it() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("overlap_quarter_then_hourly");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());

        let mut quarter = scored_row(sample_block("2026-09-30T21:00:00Z", 15));
        quarter.source = "log".to_string();
        assert_eq!(ledger.record(quarter), RecordOutcome::Inserted);

        let mut hourly = scored_row(sample_block("2026-09-30T21:00:00Z", 60));
        hourly.source = "log".to_string();
        assert_eq!(ledger.record(hourly), RecordOutcome::Replaced);

        let rows = ledger.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].dt_minutes, 60);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlap_loop_recorded_row_always_beats_an_overlapping_import() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("overlap_loop_vs_import");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());

        let mut loop_row = scored_row(sample_block("2026-09-30T21:15:00Z", 15));
        loop_row.source = "block0".to_string();
        assert_eq!(ledger.record(loop_row), RecordOutcome::Inserted);

        // An hourly backfill import covering the same window must not displace the loop's own row.
        let mut hourly_import = scored_row(sample_block("2026-09-30T21:00:00Z", 60));
        hourly_import.source = "log".to_string();
        assert_eq!(ledger.record(hourly_import), RecordOutcome::Skipped);

        let rows = ledger.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "block0");
        assert_eq!(rows[0].dt_minutes, 15);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_drops_rows_older_than_retention() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("prune");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());
        ledger.record(scored_row(sample_block("2026-01-01T00:00:00Z", 15)));
        ledger.record(scored_row(sample_block("2026-09-30T00:00:00Z", 15)));
        ledger.prune(t("2026-10-01T00:00:00Z"));
        let rows = ledger.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].t, t("2026-09-30T00:00:00Z"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rows_age_out_after_48h_unscored() {
        let row = scored_row(sample_block("2026-09-29T00:00:00Z", 15));
        let now = row.t + Duration::hours(49);
        assert!(is_aged_out(&row, now));
        assert!(!is_due(&row, now) || is_aged_out(&row, now)); // aged-out rows are never "due" again
    }

    #[test]
    fn apply_scores_repeated_failure_with_same_note_does_not_dirty_the_store() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("apply_scores_no_change");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::open_at(path.clone());
        let row = scored_row(sample_block("2026-09-30T12:00:00Z", 15));
        ledger.record(row.clone());
        ledger.persist().unwrap();
        assert!(!ledger.is_dirty());

        let still_missing = ScoreResult {
            measured: None,
            planned_cost_eur: None,
            realized_cost_eur: None,
            misses: Vec::new(),
            scored: false,
            score_note: Some("InputPower missing for quarter starting at 0".to_string()),
        };
        // The FIRST failure is a real transition (no note -> a note) and must dirty the store.
        ledger.apply_scores(
            vec![(row.t, still_missing.clone())],
            row.t + Duration::minutes(5),
        );
        assert!(ledger.is_dirty(), "the first failure note is a real change");
        ledger.persist().unwrap();

        // A RETRY with the identical note is not a change.
        ledger.apply_scores(
            vec![(row.t, still_missing.clone())],
            row.t + Duration::minutes(10),
        );
        assert!(
            !ledger.is_dirty(),
            "an identical repeated failure must not dirty the store"
        );

        let different_note = ScoreResult {
            score_note: Some("ChargePower missing for quarter starting at 0".to_string()),
            ..still_missing
        };
        ledger.apply_scores(vec![(row.t, different_note)], row.t + Duration::minutes(10));
        assert!(
            ledger.is_dirty(),
            "a genuinely different note is a real change and must dirty the store"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_store_starts_empty_and_quarantines_only_at_the_first_persist() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("corrupt");
        let aside = PathBuf::from(format!("{}.corrupt", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&aside);
        std::fs::write(&path, b"not valid json").unwrap();

        let ledger = Ledger::open_at(path.clone());
        assert!(ledger.rows_snapshot().is_empty());

        // A read-only access (what `ledger show` does) must never rename anything.
        let _ = ledger.rows_snapshot();
        assert!(
            path.exists(),
            "the broken file must still be at its original path"
        );
        assert!(
            !aside.exists(),
            "nothing should be quarantined before the first persist"
        );

        // The first persist (what `ledger import`/`ledger score`/the loop do) quarantines the
        // broken file, THEN writes the fresh (empty-plus-this-row) state.
        ledger.record(scored_row(sample_block("2026-09-30T12:00:00Z", 15)));
        ledger.persist().unwrap();
        assert!(
            aside.exists(),
            "the broken file should be preserved at .corrupt"
        );
        let reread: Vec<LedgerRow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread.len(), 1);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&aside);
    }

    #[test]
    fn unreadable_non_utf8_store_is_quarantined_not_overwritten() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("non_utf8");
        let aside = PathBuf::from(format!("{}.corrupt", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&aside);
        // Invalid UTF-8 (a lone continuation byte) makes `read_to_string` fail with an `InvalidData`
        // error distinct from `NotFound` — the exact case finding 1 covers: any load failure other
        // than "file doesn't exist" must still quarantine.
        std::fs::write(&path, [0xFFu8, 0xFE, 0xFD]).unwrap();

        let ledger = Ledger::open_at(path.clone());
        assert!(ledger.rows_snapshot().is_empty());
        assert!(
            !aside.exists(),
            "nothing quarantined before the first persist"
        );

        ledger.record(scored_row(sample_block("2026-09-30T12:00:00Z", 15)));
        ledger.persist().unwrap();
        assert!(
            aside.exists(),
            "the unreadable original must be preserved at .corrupt, not silently overwritten"
        );
        let reread: Vec<LedgerRow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread.len(), 1);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&aside);
    }

    #[test]
    fn quarantine_rename_failure_aborts_persist_without_writing() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_store_path("quarantine_rename_failure");
        let _ = std::fs::remove_file(&path);
        // A broken store whose `.corrupt` destination is blocked by a non-empty directory: the
        // quarantine rename fails for real (a missing source, by contrast, is nothing to preserve).
        std::fs::write(&path, b"{not json").unwrap();
        let aside = PathBuf::from(format!("{}.corrupt", path.display()));
        let _ = std::fs::remove_dir_all(&aside);
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("blocker"), b"").unwrap();
        let ledger = Ledger::open_at(path.clone());
        assert!(ledger.quarantine_on_persist.load(Ordering::Relaxed));
        ledger.record(scored_row(sample_block("2026-09-30T12:00:00Z", 15)));

        let err = ledger.persist().unwrap_err();
        assert!(
            format!("{err:#}").contains("quarantining"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{not json",
            "persist must not have overwritten the broken store when the quarantine step failed"
        );
        let _ = std::fs::remove_dir_all(&aside);
        assert!(
            ledger.is_dirty(),
            "the row is still unpersisted, so dirty must stay set for the next retry"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ---------- store size (finding 3d: 30-day retention) ----------

    /// Not a correctness assertion — a measurement, reported for the rework brief: serializes a
    /// synthetic but realistic 30-day/96-blocks-per-day store (the full `RETENTION_DAYS` window) and
    /// prints its size, so the compact-row serialization's actual payoff is visible rather than
    /// asserted blind. Run with `cargo test --nocapture thirty_day_synthetic_store_size_is_reported`
    /// to see the printed number.
    #[test]
    fn thirty_day_synthetic_store_size_is_reported() {
        let zones = [
            "livingroom",
            "kitchen",
            "bedroom",
            "guestroom",
            "bathroom",
            "attic",
            "garage",
            "hall",
        ];
        let start = t("2026-09-01T00:00:00Z");
        let n = (RETENTION_DAYS * 96) as usize;
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            let active_zone = zones[i % zones.len()];
            let mut heat_kw = HashMap::new();
            heat_kw.insert(active_zone.to_string(), 1.5); // every other zone stays at the implicit 0.0

            // A typical day: most zones' relay state is already known from an earlier transition
            // (`None` here, like `Planned`'s zero entries, is compacted out of the real payload), one
            // or two report a real duty this block.
            let mut heat_kwh = HashMap::new();
            heat_kwh.insert(active_zone.to_string(), Some(0.35));

            let row = LedgerRow {
                t: start + Duration::minutes(15 * i as i64),
                dt_minutes: 15,
                recorded_at: start,
                source: if i % 20 == 0 {
                    "frozen".to_string()
                } else {
                    "block0".to_string()
                },
                slot: "regular".to_string(),
                export_enabled: true,
                inverter_on: true,
                degraded: false,
                relaxed: false,
                rounded: true,
                drifted: i % 50 == 0,
                price_is_placeholder: false,
                import_price: Some(0.18),
                export_price: Some(0.05),
                wear_eur_per_kwh: 0.04,
                eur_czk_rate: 25.0,
                planned: Planned {
                    pv_kw: 1.2,
                    load_kw: 0.6,
                    charge_kw: 0.0,
                    discharge_kw: 0.4,
                    grid_import_kw: 0.1,
                    grid_export_kw: 0.0,
                    heat_kw,
                    ev_charge_kw: HashMap::new(),
                    controllable_load_kw: HashMap::new(),
                },
                measured: Some(Measured {
                    pv_kwh: 0.3,
                    load_kwh: 0.15,
                    charge_kwh: 0.0,
                    discharge_kwh: 0.1,
                    import_kwh: 0.025,
                    export_kwh: 0.0,
                    heat_kwh,
                    ev_kwh: HashMap::new(),
                }),
                scored: true,
                score_note: None,
                scored_at: Some(start),
                planned_cost_eur: Some(-0.02),
                realized_cost_eur: Some(0.004),
                misses: if i % 30 == 0 {
                    vec!["export not actuated: planned 1.50 kW, measured 0.10 kW".to_string()]
                } else {
                    Vec::new()
                },
            };
            rows.push(row);
        }

        let json = serde_json::to_vec(&rows).unwrap();
        println!(
            "[ledger] {}-day synthetic store ({} rows): {} bytes ({:.2} MB)",
            RETENTION_DAYS,
            rows.len(),
            json.len(),
            json.len() as f64 / 1_000_000.0
        );
    }

    // ---------- track_ledger_block ----------

    #[test]
    fn rollover_emits_the_ended_row_and_starts_a_new_one() {
        let block0 = sample_block("2026-09-30T17:30:00Z", 15);
        let pending = scored_row(sample_block("2026-09-30T17:15:00Z", 15));
        let (current, ended) = track_ledger_block(
            Some(pending.clone()),
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            block0.t,
        );
        let ended = ended.expect("the previous block should be emitted");
        assert_eq!(ended.t, pending.t);
        let current = current.expect("a new pending row should start");
        assert_eq!(current.t, block0.t);
        assert_eq!(current.source, "block0");
    }

    #[test]
    fn same_block_tick_does_not_emit() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let pending = scored_row(block0.clone());
        let (current, ended) = track_ledger_block(
            Some(pending.clone()),
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            block0.t,
        );
        assert!(ended.is_none());
        assert_eq!(current.unwrap().t, block0.t);
    }

    #[test]
    fn degraded_pending_is_upgraded_by_a_later_clean_plan() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let mut pending = scored_row(block0.clone());
        pending.source = "degraded".to_string();
        pending.degraded = true;
        let (current, ended) = track_ledger_block(
            Some(pending),
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            block0.t, // prompt: upgraded within LATE_TRACKING_THRESHOLD of the block start
        );
        assert!(ended.is_none());
        let current = current.unwrap();
        assert_eq!(current.source, "block0");
        assert!(!current.degraded);
    }

    #[test]
    fn degraded_pending_upgraded_well_after_block_start_is_labelled_late_not_block0() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let mut pending = scored_row(block0.clone());
        pending.source = "degraded".to_string();
        pending.degraded = true;
        let now = block0.t + Duration::minutes(4); // past LATE_TRACKING_THRESHOLD (3 min)
        let (current, ended) = track_ledger_block(
            Some(pending),
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            now,
        );
        assert!(ended.is_none());
        let current = current.unwrap();
        assert_eq!(current.source, "late");
        assert!(!current.degraded);
    }

    #[test]
    fn drift_flag_set_when_later_clean_block0_disagrees() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let pending = scored_row(block0.clone());
        let mut later_block0 = block0.clone();
        later_block0.discharge_kw = 0.0; // was 2.0 — well past the 0.05 kW drift threshold
        later_block0.charge_kw = 1.0;
        let (current, _) = track_ledger_block(
            Some(pending),
            block0.t,
            None,
            None,
            &later_block0,
            PlanFlags::default(),
            0.02,
            25.0,
            block0.t,
        );
        assert!(current.unwrap().drifted);
    }

    #[test]
    fn backward_step_is_ignored() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let pending = scored_row(block0.clone());
        let earlier = pending.t - Duration::minutes(15);
        let (current, ended) = track_ledger_block(
            Some(pending.clone()),
            earlier,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            block0.t,
        );
        assert!(ended.is_none());
        assert_eq!(current.unwrap().t, pending.t);
    }

    #[test]
    fn frozen_commitment_wins_over_block0_at_rollover() {
        let frozen_block = sample_block("2026-09-30T17:30:00Z", 15);
        let mut fresh_block0 = frozen_block.clone();
        fresh_block0.slot = "regular".to_string(); // what this tick's own LP would otherwise decide
        let pending = scored_row(sample_block("2026-09-30T17:15:00Z", 15));
        let (current, _) = track_ledger_block(
            Some(pending),
            frozen_block.t,
            Some(frozen_block.t),
            Some(&frozen_block),
            &fresh_block0,
            PlanFlags::default(),
            0.02,
            25.0,
            frozen_block.t,
        );
        let current = current.unwrap();
        assert_eq!(current.source, "frozen");
        assert_eq!(current.slot, "discharge_to_grid");
    }

    #[test]
    fn late_source_when_first_tracked_well_after_block_start_with_no_frozen_commitment() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let now = block0.t + Duration::minutes(4); // past LATE_TRACKING_THRESHOLD (3 min)
        let (current, ended) = track_ledger_block(
            None, // nothing was being tracked yet — the loop just started/recovered
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            now,
        );
        assert!(ended.is_none());
        assert_eq!(current.unwrap().source, "late");
    }

    #[test]
    fn block0_source_when_first_tracked_promptly() {
        let block0 = sample_block("2026-09-30T17:15:00Z", 15);
        let now = block0.t + Duration::seconds(30); // within LATE_TRACKING_THRESHOLD
        let (current, _) = track_ledger_block(
            None,
            block0.t,
            None,
            None,
            &block0,
            PlanFlags::default(),
            0.02,
            25.0,
            now,
        );
        assert_eq!(current.unwrap().source, "block0");
    }
}
