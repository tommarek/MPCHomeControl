//! `audit-export [--log <file|->] [--plan <file.json>]...` — read-only proof tool for the
//! pv-gated-export item: how much "fiction" (battery→grid export the OLD, ungated planner booked
//! in blocks where the Growatt physically refuses to export — its PV input reads ~0 W) shows up in
//! real house data, and what the NEW gate changes for the CURRENT instant.
//!
//! **A. Historical audit** (only when a `--log` and/or `--plan` source is given): parse each into
//! [`PlannedBlock`]s, join them against MEASURED Growatt telemetry and the real tariff, and total
//! the fiction — planned vs measured export kWh, and the EUR the plan booked for it — in blocks
//! where measured PV was (near) 0, against a PV-present control row. `--log` reads
//! `[mpc] ...` decision lines (`src/mpc_loop.rs::log_decision`'s format — the loop's own per-tick
//! `docker logs mpc-brain` output); `--plan` reads a saved `/api/plan/latest` JSON envelope (the
//! `docker logs` route is currently unusable on the server — it hangs even for `--tail 5`). Either
//! or both may be given; every source is expanded to 15-min quarters before merging
//! ([`merge_expanded`]), so a wider plan block and a log's own quarters over the same window can't
//! double-count — a later source's quarter overwrites an earlier source's quarter at that timestamp.
//!
//! **B. Live comparison** (always): solve the CURRENT on-demand plan twice — once with
//! `battery.export_needs_pv` forced OFF (the OLD/ungated behaviour), once as configured (NEW) —
//! and report the block-level differences, the plan aggregates, and the realized battery/grid cost
//! each would actually incur under the real device constraint
//! ([`crate::optimize::replay::replay_actuated`]).
//!
//! Read-only throughout: writes nothing, actuates nothing.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use uom::si::f64::Angle;

use crate::app::{current_plan, tariff_prices, PlanExtras};
use crate::live_inputs::block_prices;
use crate::optimize::config::ControlConfig;
use crate::optimize::replay::replay_actuated;
use crate::optimize::unified::PV_PRESENT_KW;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::what_if::{align_15min, BLOCKS_PER_DAY, BLOCK_SECONDS};

/// How long to pause between successive per-day InfluxDB windows — the historical audit's own
/// version of `what_if.rs`'s "never run unbounded/rapid-fire queries on the live server" rule.
/// `pub(crate)`: `terminal_backtest` reuses the same pacing for its own chunked reads.
pub(crate) const INTER_DAY_PAUSE_S: u64 = 2;
/// Measured power (W) below which a block counts as PV-dark — a bare noise floor, not
/// [`PV_PRESENT_KW`] (that threshold is on the FORECAST the LP gates on; this is on the MEASURED
/// sample the audit scores against).
const DARK_PV_W: f64 = 1.0;

/// One planned block, from either the decision log or a saved plan-JSON envelope — the common
/// shape [`audit_planned_vs_measured`] joins against measured telemetry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedBlock {
    pub(crate) block_start: DateTime<Utc>,
    pub(crate) dt_hours: f64,
    /// Forecast PV (kW) the plan itself saw, when the source carries it (the plan-JSON source
    /// does; the decision log does not — `None`). Informational only ("plan said PV=0"); the audit
    /// gates on MEASURED PV, joined in separately.
    pub(crate) pv_kw: Option<f64>,
    /// Total planned grid export this block (solar + battery), kW.
    pub(crate) grid_export_kw: f64,
    /// The battery's share of `grid_export_kw` (kW) — informational.
    pub(crate) battery_export_kw: f64,
    /// Export price this block (price-units/kWh); `None` until filled in from the real tariff (the
    /// log source doesn't carry it — the plan-JSON source does).
    pub(crate) export_price: Option<f64>,
}

/// One parsed `[mpc] ...` decision-log line (`src/mpc_loop.rs::log_decision`'s format).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogDecision {
    pub(crate) block_start: DateTime<Utc>,
    pub(crate) mode: String,
    pub(crate) export_enabled: bool,
    pub(crate) inverter_on: bool,
    pub(crate) heat_kw: f64,
    pub(crate) battery_kw: f64,
    pub(crate) grid_import_kw: f64,
    pub(crate) grid_export_kw: f64,
}

impl LogDecision {
    /// No forecast PV or export price in the log line — `pv_kw: None`, `export_price: None`, the
    /// caller fills the price in from the real tariff. `battery_export_kw` approximates the
    /// battery's share of `grid_export_kw` from the net battery power (+ = discharging).
    fn into_planned_block(self) -> PlannedBlock {
        let grid_export_kw = self.grid_export_kw.max(0.0);
        PlannedBlock {
            block_start: self.block_start,
            dt_hours: 0.25, // the logged block is always block 0 of a 15-minute-grid plan
            pv_kw: None,
            grid_export_kw,
            battery_export_kw: self.battery_kw.max(0.0).min(grid_export_kw),
            export_price: None,
        }
    }
}

/// Parse one `log_decision` line, e.g.:
/// `[mpc] 2026-09-30 14:30 UTC: mode regular (export on, inverter on), heat 0.0 kW, battery +0.0 kW,
/// grid import 0.0 / export 2.7 kW (36h cost -5.34 EUR / -133 CZK)  [fallbacks: ...]`
/// `None` for a non-matching line (blank lines, other `[mpc] ...` log lines, truncated output) —
/// the caller skips those rather than erroring the whole file.
pub(crate) fn parse_decision_line(line: &str) -> Option<LogDecision> {
    let rest = line.strip_prefix("[mpc] ")?;
    let (ts_str, rest) = rest.split_once(": mode ")?;
    let block_start = Utc.from_utc_datetime(
        &chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M UTC").ok()?,
    );
    let (mode, rest) = rest.split_once(" (")?;
    let (gates, rest) = rest.split_once("), ")?;
    let mut export_enabled = None;
    let mut inverter_on = None;
    for part in gates.split(", ") {
        if let Some(v) = part.strip_prefix("export ") {
            export_enabled = Some(v == "on");
        } else if let Some(v) = part.strip_prefix("inverter ") {
            inverter_on = Some(v == "on");
        }
    }
    let (heat_part, rest) = rest.split_once(", battery ")?;
    let heat_kw = heat_part
        .strip_prefix("heat ")?
        .strip_suffix(" kW")?
        .parse()
        .ok()?;
    let (battery_part, rest) = rest.split_once(", grid import ")?;
    let battery_kw = battery_part.strip_suffix(" kW")?.parse().ok()?;
    let (import_part, rest) = rest.split_once(" / export ")?;
    let grid_import_kw = import_part.parse().ok()?;
    let (export_part, _rest) = rest.split_once(" kW (")?;
    let grid_export_kw = export_part.parse().ok()?;
    Some(LogDecision {
        block_start,
        mode: mode.to_string(),
        export_enabled: export_enabled?,
        inverter_on: inverter_on?,
        heat_kw,
        battery_kw,
        grid_import_kw,
        grid_export_kw,
    })
}

#[derive(Debug, Deserialize)]
struct PlanEnvelope {
    data: PlanEnvelopeData,
}
#[derive(Debug, Deserialize)]
struct PlanEnvelopeData {
    timeline: Vec<TimelineRow>,
}
#[derive(Debug, Deserialize)]
struct TimelineRow {
    t: DateTime<Utc>,
    dt_minutes: u32,
    pv_kw: f64,
    grid_export_kw: f64,
    discharge_kw: f64,
    export_price: f64,
}

/// Parse a saved `/api/plan/latest` `{computed_at, age_seconds, data: {..., timeline: [...]}}`
/// envelope into [`PlannedBlock`]s (one per `timeline` row). Unknown fields are ignored.
pub(crate) fn parse_plan_envelope(json_str: &str) -> Result<Vec<PlannedBlock>> {
    let envelope: PlanEnvelope =
        serde_json::from_str(json_str).context("parsing plan envelope JSON")?;
    Ok(envelope
        .data
        .timeline
        .into_iter()
        .map(|row| {
            let grid_export_kw = row.grid_export_kw.max(0.0);
            PlannedBlock {
                block_start: row.t,
                dt_hours: row.dt_minutes as f64 / 60.0,
                pv_kw: Some(row.pv_kw),
                grid_export_kw,
                battery_export_kw: row.discharge_kw.max(0.0).min(grid_export_kw),
                export_price: Some(row.export_price),
            }
        })
        .collect())
}

/// Totals from joining planned blocks against measured telemetry over some window.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AuditTotals {
    /// Blocks where the plan booked export while measured PV was (near) 0 — the fiction.
    pub(crate) dark_blocks: usize,
    pub(crate) dark_planned_export_kwh: f64,
    pub(crate) dark_measured_export_kwh: f64,
    /// EUR (or whatever price-units the joined prices carry) the plan booked for the dark-block
    /// export — `planned_kwh × export_price`, summed.
    pub(crate) dark_eur_booked: f64,
    /// The control row: blocks where the plan booked export and measured PV was present.
    pub(crate) lit_blocks: usize,
    pub(crate) lit_planned_export_kwh: f64,
    pub(crate) lit_measured_export_kwh: f64,
    /// Blocks the plan booked export in but a measured PV/export sample or a price was missing —
    /// excluded from every other total rather than zero-filled (a missing sample is unknown, not
    /// zero export).
    pub(crate) skipped_missing_data: usize,
}

impl AuditTotals {
    fn merge(&mut self, other: AuditTotals) {
        self.dark_blocks += other.dark_blocks;
        self.dark_planned_export_kwh += other.dark_planned_export_kwh;
        self.dark_measured_export_kwh += other.dark_measured_export_kwh;
        self.dark_eur_booked += other.dark_eur_booked;
        self.lit_blocks += other.lit_blocks;
        self.lit_planned_export_kwh += other.lit_planned_export_kwh;
        self.lit_measured_export_kwh += other.lit_measured_export_kwh;
        self.skipped_missing_data += other.skipped_missing_data;
    }
}

/// Split a planned block into consecutive 15-min sub-blocks (same kW/price) so it can be scored
/// against ONE measured sample per quarter — a wider (e.g. hourly) block's single measured sample
/// at its START would otherwise stand in for the whole hour and hide a partially-dark one (a 17:00
/// hourly block reading 5 W PV at the top of the hour is scored "lit" even when 45 of its 60
/// minutes were actually dark). `dt_hours <= 0.25` (already 15 min or shorter) is returned as-is.
/// The single expansion point — called once, at merge time ([`merge_expanded`]), so a block that
/// already came in as quarters is never expanded (and so never double-counted) again.
fn expand_to_15min(block: &PlannedBlock) -> Vec<PlannedBlock> {
    const QUARTER_HOURS: f64 = 0.25;
    let quarters = (block.dt_hours / QUARTER_HOURS).round().max(1.0) as i64;
    if quarters <= 1 {
        return vec![block.clone()];
    }
    (0..quarters)
        .map(|q| PlannedBlock {
            block_start: block.block_start + Duration::minutes(15 * q),
            dt_hours: QUARTER_HOURS,
            ..block.clone()
        })
        .collect()
}

/// Expand every block in `source` to 15-min quarters ([`expand_to_15min`]) and merge into `blocks`,
/// keyed by quarter start. A quarter already present (from an earlier call / earlier source) is
/// OVERWRITTEN — callers merge sources in increasing trust order (e.g. `--log` then `--plan`) so
/// the last-merged source wins on any overlap. This is the ONLY place expansion happens: merging
/// pre-expansion is what stops an hourly plan block and a log's own 15-min blocks over the same
/// window from both contributing the same quarter to [`audit_planned_vs_measured`].
fn merge_expanded(blocks: &mut BTreeMap<DateTime<Utc>, PlannedBlock>, source: Vec<PlannedBlock>) {
    for block in source {
        for q in expand_to_15min(&block) {
            blocks.insert(q.block_start, q);
        }
    }
}

/// Join already-expanded 15-min `planned` quarters ([`merge_expanded`]) against measured telemetry
/// (W, keyed by block start) and total up the fiction: quarters where the plan booked export
/// (`grid_export_kw > 0`) while measured PV was below [`DARK_PV_W`], vs the PV-present control row.
/// Pure — a quarter whose measured PV/export sample or export price is missing is counted
/// `skipped_missing_data`, never silently treated as 0.
pub(crate) fn audit_planned_vs_measured(
    planned: &[PlannedBlock],
    measured_pv_w: &HashMap<DateTime<Utc>, f64>,
    measured_export_w: &HashMap<DateTime<Utc>, f64>,
) -> AuditTotals {
    let mut totals = AuditTotals::default();
    for b in planned {
        if b.grid_export_kw <= 0.0 {
            continue; // only blocks the plan booked export in are interesting
        }
        let (Some(&pv_w), Some(&exp_w), Some(price)) = (
            measured_pv_w.get(&b.block_start),
            measured_export_w.get(&b.block_start),
            b.export_price,
        ) else {
            totals.skipped_missing_data += 1;
            continue;
        };
        let planned_kwh = b.grid_export_kw * b.dt_hours;
        let measured_kwh = (exp_w / 1000.0).max(0.0) * b.dt_hours;
        if pv_w < DARK_PV_W {
            totals.dark_blocks += 1;
            totals.dark_planned_export_kwh += planned_kwh;
            totals.dark_measured_export_kwh += measured_kwh;
            totals.dark_eur_booked += planned_kwh * price;
        } else {
            totals.lit_blocks += 1;
            totals.lit_planned_export_kwh += planned_kwh;
            totals.lit_measured_export_kwh += measured_kwh;
        }
    }
    totals
}

/// Section A: the historical fiction-vs-reality audit over every elapsed block in `blocks`.
async fn run_historical_audit(
    db: &SourceClients,
    config: &ControlConfig,
    blocks: BTreeMap<DateTime<Utc>, PlannedBlock>,
) -> Result<()> {
    let now = Utc::now();
    // `blocks` is already quarter-expanded and deduplicated by `merge_expanded`; `BTreeMap::
    // into_values` yields them in key (block_start) order already, so no separate sort is needed.
    let elapsed: Vec<PlannedBlock> = blocks
        .into_values()
        .filter(|b| b.block_start + Duration::seconds((b.dt_hours * 3600.0).round() as i64) < now)
        .collect();
    if elapsed.is_empty() {
        println!("historical audit: no elapsed planned blocks to score");
        return Ok(());
    }

    let mut by_day: BTreeMap<NaiveDate, Vec<PlannedBlock>> = BTreeMap::new();
    for b in elapsed {
        by_day
            .entry(b.block_start.date_naive())
            .or_default()
            .push(b);
    }

    println!(
        "historical audit: {} day(s), dark = measured PV < {DARK_PV_W:.0} W",
        by_day.len()
    );
    let mut grand = AuditTotals::default();
    let days = by_day.len();
    for (n, (day, mut day_blocks)) in by_day.into_iter().enumerate() {
        let day_start = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap());
        let start_s = day_start.to_rfc3339();
        let stop_s =
            (day_start + Duration::seconds(BLOCK_SECONDS * BLOCKS_PER_DAY as i64)).to_rfc3339();
        let pv = db
            .growatt_series("InputPower", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let exp = db
            .growatt_series("ACPowerToGrid", &start_s, &stop_s, "15m")
            .await
            .unwrap_or_default();
        let pv_b = align_15min(&pv, day_start, BLOCKS_PER_DAY);
        let exp_b = align_15min(&exp, day_start, BLOCKS_PER_DAY);
        let block_ts = |i: usize| day_start + Duration::seconds(BLOCK_SECONDS * i as i64);
        let measured_pv_w: HashMap<DateTime<Utc>, f64> = pv_b
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (block_ts(i), v)))
            .collect();
        let measured_export_w: HashMap<DateTime<Utc>, f64> = exp_b
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (block_ts(i), v)))
            .collect();

        if day_blocks.iter().any(|b| b.export_price.is_none()) {
            if let Ok(Some(prices)) = block_prices(db, day_start, BLOCKS_PER_DAY).await {
                if let Some(spot) = prices.current.into_iter().collect::<Option<Vec<f64>>>() {
                    let (_import, export) =
                        tariff_prices(&config.tariff, &config.site, &spot, day_start);
                    let price_by_block: HashMap<DateTime<Utc>, f64> = (0..BLOCKS_PER_DAY)
                        .map(|i| (block_ts(i), export[i]))
                        .collect();
                    for b in day_blocks.iter_mut().filter(|b| b.export_price.is_none()) {
                        b.export_price = price_by_block.get(&b.block_start).copied();
                    }
                }
            }
        }

        let totals = audit_planned_vs_measured(&day_blocks, &measured_pv_w, &measured_export_w);
        println!(
            "  {day}: dark {} blocks, planned {:.2} kWh / measured {:.2} kWh / booked {:.2} \
             EUR  |  lit (control) {} blocks, planned {:.2} kWh / measured {:.2} kWh  |  skipped {}",
            totals.dark_blocks,
            totals.dark_planned_export_kwh,
            totals.dark_measured_export_kwh,
            totals.dark_eur_booked,
            totals.lit_blocks,
            totals.lit_planned_export_kwh,
            totals.lit_measured_export_kwh,
            totals.skipped_missing_data,
        );
        grand.merge(totals);
        if n + 1 < days {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    println!(
        "TOTAL: dark {} blocks, planned {:.2} kWh / measured {:.2} kWh / booked {:.2} EUR  |  \
         lit (control) {} blocks, planned {:.2} kWh / measured {:.2} kWh  |  skipped {}",
        grand.dark_blocks,
        grand.dark_planned_export_kwh,
        grand.dark_measured_export_kwh,
        grand.dark_eur_booked,
        grand.lit_blocks,
        grand.lit_planned_export_kwh,
        grand.lit_measured_export_kwh,
        grand.skipped_missing_data,
    );
    Ok(())
}

/// Build the slow plan inputs — a 7-day PV-calibration backtest plus the configured
/// `consumption_history_days` windowed-mean consumption training (`app::build_cache`), and the
/// kernel cache — ONCE, so two or more `current_plan` solves over the same tick see identical slow
/// inputs (apples-to-apples) instead of each independently re-running ~2.3 s of training queries.
/// Shared by `export_audit`'s OLD/NEW export-gate comparison and `terminal_backtest`'s OLD/NEW
/// terminal-value comparison.
pub(crate) async fn shared_plan_cache(
    db: &SourceClients,
    net: &RcNetwork,
    config: &ControlConfig,
    ss: &StateSpace,
) -> (
    crate::app::PlanCache,
    std::sync::Arc<crate::optimize::thermal::KernelSet>,
) {
    let cache = crate::app::build_cache(db, net, config, None).await;
    let kernels = std::sync::Arc::new(crate::app::build_kernel_cache(config, net, ss));
    (cache, kernels)
}

/// Section B: solve the current on-demand plan twice (`old_cfg` / `new_cfg`) and compare — the
/// live OLD/NEW machinery shared by `audit-export` (export gate off/on, replay floor `0.0`,
/// `print_timing: false` — output unchanged from before this was generalized) and
/// `audit-dispatch-floor` (`battery.min_dispatch_kw` 0 vs configured, replayed at the CONFIGURED
/// floor, `print_timing: true`). The slow inputs ([`shared_plan_cache`]) are built once and shared
/// by both solves; the per-plan live reads inside `current_plan` (prices, battery SoC, zone
/// temperatures, the thermal-state estimate) still run once per solve, same as any two separate
/// `/api/plan` requests. Returns both solved reports so a caller can run its own additional
/// analysis on them (`audit-dispatch-floor`'s floor-specific counts/table).
#[allow(clippy::too_many_arguments)]
async fn run_live_comparison(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    old_cfg: &ControlConfig,
    new_cfg: &ControlConfig,
    old_label: &str,
    new_label: &str,
    replay_floor: f64,
    print_timing: bool,
) -> Result<(crate::app::PlanReport, crate::app::PlanReport)> {
    let (cache, kernels) = shared_plan_cache(db, net, config, ss).await;
    let extras = || PlanExtras {
        cache: Some(&cache),
        kernels: Some(kernels.clone()),
        replay_inputs: true,
        ..Default::default()
    };

    let old_start = std::time::Instant::now();
    let old = current_plan(db, net, ss, old_cfg, latitude, longitude, extras())
        .await
        .with_context(|| format!("solving {old_label} plan"))?;
    let old_elapsed = old_start.elapsed();
    let new_start = std::time::Instant::now();
    let new = current_plan(db, net, ss, new_cfg, latitude, longitude, extras())
        .await
        .with_context(|| format!("solving {new_label} plan"))?;
    let new_elapsed = new_start.elapsed();
    if print_timing {
        println!(
            "per-solve wall-clock: {old_label} {:.2}s  |  {new_label} {:.2}s",
            old_elapsed.as_secs_f64(),
            new_elapsed.as_secs_f64(),
        );
    }

    println!(
        "live comparison: {old_label} cost {:.2} EUR / {:.2} CZK, export_pv_gated_blocks {}  |  \
         {new_label} cost {:.2} EUR / {:.2} CZK, export_pv_gated_blocks {}",
        old.total_cost_eur,
        old.total_cost_czk,
        old.export_pv_gated_blocks,
        new.total_cost_eur,
        new.total_cost_czk,
        new.export_pv_gated_blocks,
    );
    println!(
        "grid_export_kwh: {old_label} {:.2} {new_label} {:.2}  |  grid_import_kwh: {old_label} \
         {:.2} {new_label} {:.2}  |  battery_discharge_kwh: {old_label} {:.2} {new_label} {:.2}",
        old.grid_export_kwh,
        new.grid_export_kwh,
        old.grid_import_kwh,
        new.grid_import_kwh,
        old.battery_discharge_kwh,
        new.battery_discharge_kwh,
    );

    match (&old.replay_inputs, &new.replay_inputs) {
        (Some(old_ri), Some(new_ri)) => {
            let n = old_ri
                .plan
                .batt_to_grid_kw
                .len()
                .min(new_ri.plan.batt_to_grid_kw.len());
            println!("block-level differences (batt_to_grid or mode changed):");
            for i in 0..n {
                let ob = old_ri.plan.batt_to_grid_kw[i];
                let nb = new_ri.plan.batt_to_grid_kw[i];
                let o_slot = old.timeline.get(i).map(|t| t.slot.as_str()).unwrap_or("?");
                let n_slot = new.timeline.get(i).map(|t| t.slot.as_str()).unwrap_or("?");
                if (ob - nb).abs() > 1e-6 || o_slot != n_slot {
                    let t = new.timeline.get(i).map(|t| t.t);
                    println!(
                        "  block {i} t={t:?} dt_h={:.2} pv_kw={:.3} export_price={:.4}  |  \
                         {old_label} batt_to_grid={ob:.3}kW mode={o_slot}  |  {new_label} \
                         batt_to_grid={nb:.3}kW mode={n_slot}",
                        new_ri.dt_hours.get(i).copied().unwrap_or(0.0),
                        new_ri.inputs.pv_kw.get(i).copied().unwrap_or(0.0),
                        new_ri.inputs.export_price.get(i).copied().unwrap_or(0.0),
                    );
                }
            }

            let bad_new = (0..new_ri.inputs.pv_kw.len())
                .filter(|&i| {
                    new_ri.inputs.pv_kw[i] <= PV_PRESENT_KW && new_ri.plan.batt_to_grid_kw[i] > 1e-6
                })
                .count();
            println!(
                "{new_label} blocks with pv_kw <= {PV_PRESENT_KW} and batt_to_grid > 0 (must be \
                 0): {bad_new}"
            );

            let old_replay = replay_actuated(
                &old_ri.plan,
                &old_ri.inputs,
                &old_ri.flow,
                &old_ri.battery,
                &old_ri.dt_hours,
                replay_floor,
            );
            let new_replay = replay_actuated(
                &new_ri.plan,
                &new_ri.inputs,
                &new_ri.flow,
                &new_ri.battery,
                &new_ri.dt_hours,
                replay_floor,
            );
            let delta_eur = new_replay.realized_grid_cost - old_replay.realized_grid_cost;
            println!(
                "realized battery/grid cost under the true device constraint: {old_label} {:.4} \
                 EUR  {new_label} {:.4} EUR  delta ({new_label} − {old_label}) {:.4} EUR / {:.2} \
                 CZK",
                old_replay.realized_grid_cost,
                new_replay.realized_grid_cost,
                delta_eur,
                delta_eur * config.tariff.eur_czk_rate,
            );
            println!(
                "{old_label} plan's blocked export: {:.2} kWh, {:.2} booked revenue that never \
                 materialises",
                old_replay.blocked_export_kwh, old_replay.blocked_revenue,
            );
        }
        _ => println!("replay comparison unavailable (replay_inputs missing on one of the plans)"),
    }
    Ok((old, new))
}

/// `audit-export [--log <file|->] [--plan <file.json>]...` entry point.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    args: &[String],
) -> Result<()> {
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

    // Merged at QUARTER granularity (`merge_expanded` expands each source to 15-min sub-blocks
    // before inserting), so an hourly plan block and a log's own 15-min blocks over the same window
    // can never both contribute the same quarter. Sources merge in increasing trust order: `--log`
    // first, then each `--plan` file in argument order — a later source's quarter overwrites an
    // earlier source's quarter at the same timestamp.
    let mut blocks: BTreeMap<DateTime<Utc>, PlannedBlock> = BTreeMap::new();
    if let Some(path) = &log_path {
        let text = if path == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(path).with_context(|| format!("reading log file {path}"))?
        };
        let log_blocks: Vec<PlannedBlock> = text
            .lines()
            .filter_map(parse_decision_line)
            .map(LogDecision::into_planned_block)
            .collect();
        println!(
            "parsed {} decision-log blocks from {path}",
            log_blocks.len()
        );
        merge_expanded(&mut blocks, log_blocks);
    }
    for path in &plan_paths {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading plan file {path}"))?;
        let rows = parse_plan_envelope(&text)?;
        println!("parsed {} timeline blocks from {path}", rows.len());
        merge_expanded(&mut blocks, rows);
    }

    if blocks.is_empty() {
        println!("historical audit: no --log/--plan source given, skipping section A");
    } else {
        run_historical_audit(db, config, blocks).await?;
    }

    let mut old_cfg = config.clone();
    old_cfg.battery.export_needs_pv = false;
    let mut new_cfg = config.clone();
    new_cfg.battery.export_needs_pv = true;
    run_live_comparison(
        db, config, net, ss, latitude, longitude, &old_cfg, &new_cfg, "OLD", "NEW", 0.0, false,
    )
    .await?;
    Ok(())
}

/// `audit-dispatch-floor` — read-only proof tool for the demoted-discharge-floor item: does the
/// dispatch-floor LP (`battery.min_dispatch_kw` plumbed into `optimize_unified`'s fix-and-round —
/// see `optimize::unified::round_dispatch_legs`) actually keep every `batt_to_grid`/
/// `batt_grid_charge` leg at `0` or `>= min_dispatch_kw`, and at what cost? OLD = the current
/// config with the floor forced to `0.0` (today's pre-item behaviour); NEW = as configured. Both
/// replayed via [`replay_actuated`] at the CONFIGURED floor (not OLD's `0.0`): the question is "what
/// would actually be actuated", which is the same real Growatt floor regardless of which plan
/// produced the leg. Read-only throughout: writes nothing, actuates nothing.
pub async fn run_dispatch_floor(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
) -> Result<()> {
    let mut old_cfg = config.clone();
    old_cfg.battery.min_dispatch_kw = 0.0;
    let new_cfg = config.clone();
    let floor = config.battery.min_dispatch_kw;

    let (old, new) = run_live_comparison(
        db,
        config,
        net,
        ss,
        latitude,
        longitude,
        &old_cfg,
        &new_cfg,
        "OLD(floor=0)",
        "NEW(floor)",
        floor,
        true,
    )
    .await?;

    let sub_floor_counts = |ri: &crate::optimize::replay::ReplayInputs| -> (usize, usize) {
        let sub_floor = |v: f64| v > 1e-9 && v < floor;
        (
            ri.plan
                .batt_to_grid_kw
                .iter()
                .filter(|&&v| sub_floor(v))
                .count(),
            ri.plan
                .batt_grid_charge_kw
                .iter()
                .filter(|&&v| sub_floor(v))
                .count(),
        )
    };
    let regular_slot_dispatch =
        |report: &crate::app::PlanReport, ri: &crate::optimize::replay::ReplayInputs| -> usize {
            report
                .timeline
                .iter()
                .enumerate()
                .filter(|(i, t)| {
                    t.slot == "regular"
                        && (ri.plan.batt_to_grid_kw.get(*i).copied().unwrap_or(0.0) > 1e-9
                            || ri.plan.batt_grid_charge_kw.get(*i).copied().unwrap_or(0.0) > 1e-9)
                })
                .count()
        };

    // A LAUNDER block: no commanded grid leg (sub-floor), yet the battery moves more energy than
    // the house's real deficit/surplus allows — `batt_to_load + Σ ev_batt` beyond the deficit
    // (a pinned-off export re-routed into the load while solar exports the same kWh) or
    // `solar_to_batt` beyond the surplus (a pinned-off grid charge re-routed through solar while
    // the grid serves the load). Must be 0 for a NEW plan whose pinned re-solve carried the caps.
    let launder_blocks = |ri: &crate::optimize::replay::ReplayInputs| -> (usize, f64) {
        let n = ri.plan.batt_to_grid_kw.len();
        let mut blocks = 0;
        let mut kwh = 0.0;
        for i in 0..n {
            let (deficit, surplus) = crate::optimize::unified::deficit_surplus(&ri.plan, i);
            // `discharge_kw − batt_to_grid_kw` is `batt_to_load + Σ ev_batt` and `charge_kw −
            // batt_grid_charge_kw` is `solar_to_batt`, by `UnifiedPlan`'s construction.
            let mut laundered = 0.0;
            if ri.plan.batt_to_grid_kw[i] < floor {
                laundered +=
                    (ri.plan.discharge_kw[i] - ri.plan.batt_to_grid_kw[i] - deficit).max(0.0);
            }
            if ri.plan.batt_grid_charge_kw[i] < floor {
                laundered +=
                    (ri.plan.charge_kw[i] - ri.plan.batt_grid_charge_kw[i] - surplus).max(0.0);
            }
            if laundered > 1e-9 {
                blocks += 1;
                kwh += laundered * ri.dt_hours.get(i).copied().unwrap_or(0.0);
            }
        }
        (blocks, kwh)
    };

    match (&old.replay_inputs, &new.replay_inputs) {
        (Some(old_ri), Some(new_ri)) => {
            let (old_b, old_g) = sub_floor_counts(old_ri);
            let (new_b, new_g) = sub_floor_counts(new_ri);
            println!(
                "sub-floor legs (0 < v < {floor:.2} kW): OLD(floor=0) batt_to_grid {old_b} \
                 grid_charge {old_g}  |  NEW(floor) batt_to_grid {new_b} grid_charge {new_g}"
            );
            let old_reg = regular_slot_dispatch(&old, old_ri);
            let new_reg = regular_slot_dispatch(&new, new_ri);
            println!(
                "regular-slot blocks with nonzero batt_to_grid/batt_grid_charge: OLD(floor=0) \
                 {old_reg}  |  NEW(floor) {new_reg} (must be 0 for a NEW Rounded plan; \
                 new.rounded={})",
                new.rounded,
            );
            let (old_launder_blocks, old_launder_kwh) = launder_blocks(old_ri);
            let (new_launder_blocks, new_launder_kwh) = launder_blocks(new_ri);
            println!(
                "launder blocks (battery moving more than the real deficit/surplus with no commanded leg): \
                 OLD(floor=0) {old_launder_blocks} ({old_launder_kwh:.2} kWh)  |  NEW(floor) \
                 {new_launder_blocks} ({new_launder_kwh:.2} kWh) (must be 0 for NEW)",
            );

            let n = old_ri
                .plan
                .batt_to_grid_kw
                .len()
                .min(new_ri.plan.batt_to_grid_kw.len());
            println!("per-block OLD/NEW leg table (blocks where either grid leg differs):");
            for i in 0..n {
                let (ob, nb) = (
                    old_ri.plan.batt_to_grid_kw[i],
                    new_ri.plan.batt_to_grid_kw[i],
                );
                let (og, ng) = (
                    old_ri.plan.batt_grid_charge_kw[i],
                    new_ri.plan.batt_grid_charge_kw[i],
                );
                if (ob - nb).abs() > 1e-6 || (og - ng).abs() > 1e-6 {
                    let t = new.timeline.get(i).map(|t| t.t);
                    let dt_h = new_ri.dt_hours.get(i).copied().unwrap_or(0.0);
                    let price = new_ri.inputs.import_price.get(i).copied().unwrap_or(0.0);
                    let o_slot = old.timeline.get(i).map(|t| t.slot.as_str()).unwrap_or("?");
                    let n_slot = new.timeline.get(i).map(|t| t.slot.as_str()).unwrap_or("?");
                    println!(
                        "  block {i} t={t:?} dt_h={dt_h:.2} import_price={price:.4}  |  \
                         OLD(floor=0) batt_to_grid={ob:.3}kW grid_charge={og:.3}kW slot={o_slot}  \
                         |  NEW(floor) batt_to_grid={nb:.3}kW grid_charge={ng:.3}kW slot={n_slot}",
                    );
                }
            }
        }
        _ => println!("dispatch-floor comparison unavailable (replay_inputs missing)"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example_line() {
        let line = "[mpc] 2026-09-30 14:30 UTC: mode regular (export on, inverter on), heat 0.0 kW, \
                     battery +0.0 kW, grid import 0.0 / export 2.7 kW (36h cost -5.34 EUR / -133 CZK)  \
                     [fallbacks: day-ahead prices (18/144 blocks unpublished; day-type median)]";
        let d = parse_decision_line(line).expect("should parse");
        assert_eq!(
            d.block_start,
            Utc.with_ymd_and_hms(2026, 9, 30, 14, 30, 0).unwrap()
        );
        assert_eq!(d.mode, "regular");
        assert!(d.export_enabled);
        assert!(d.inverter_on);
        assert_eq!(d.heat_kw, 0.0);
        assert_eq!(d.battery_kw, 0.0);
        assert_eq!(d.grid_import_kw, 0.0);
        assert_eq!(d.grid_export_kw, 2.7);
    }

    #[test]
    fn parses_negative_signed_battery_power_and_gates_off() {
        let line = "[mpc] 2026-10-01 05:00 UTC: mode charge_from_grid (export off, inverter on), \
                     heat 1.2 kW, battery -3.5 kW, grid import 4.0 / export 0.0 kW \
                     (36h cost 1.10 EUR / 27 CZK)";
        let d = parse_decision_line(line).expect("should parse");
        assert_eq!(d.mode, "charge_from_grid");
        assert!(!d.export_enabled);
        assert!(d.inverter_on);
        assert_eq!(d.battery_kw, -3.5);
        assert_eq!(d.grid_import_kw, 4.0);
    }

    #[test]
    fn non_matching_lines_return_none() {
        assert!(parse_decision_line("").is_none());
        assert!(parse_decision_line("[mpc] internal-gain re-fit: no extra gain needed").is_none());
        assert!(parse_decision_line("some unrelated log line").is_none());
    }

    #[test]
    fn plan_envelope_parses_timeline_rows() {
        let json = r#"{
            "computed_at": "2026-09-30T14:35:28.135074230+00:00",
            "age_seconds": 1.2,
            "data": {
                "total_cost_eur": -5.34,
                "timeline": [
                    {
                        "t": "2026-09-30T14:30:00Z",
                        "dt_minutes": 15,
                        "pv_kw": 4.09,
                        "grid_export_kw": 2.707,
                        "discharge_kw": 0.0,
                        "export_price": 0.15017,
                        "slot": "regular"
                    },
                    {
                        "t": "2026-09-30T17:15:00Z",
                        "dt_minutes": 15,
                        "pv_kw": 0.0,
                        "grid_export_kw": 1.5,
                        "discharge_kw": 1.5,
                        "export_price": 0.28,
                        "slot": "discharge_to_grid"
                    }
                ]
            }
        }"#;
        let blocks = parse_plan_envelope(json).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].dt_hours, 0.25);
        assert_eq!(blocks[0].pv_kw, Some(4.09));
        assert_eq!(blocks[0].grid_export_kw, 2.707);
        assert_eq!(blocks[0].battery_export_kw, 0.0);
        assert_eq!(blocks[1].battery_export_kw, 1.5);
        assert_eq!(blocks[1].export_price, Some(0.28));
    }

    fn block(ts: DateTime<Utc>, grid_export_kw: f64, price: f64) -> PlannedBlock {
        PlannedBlock {
            block_start: ts,
            dt_hours: 0.25,
            pv_kw: None,
            grid_export_kw,
            battery_export_kw: grid_export_kw,
            export_price: Some(price),
        }
    }

    /// A dark block (4 kW planned, 0 W measured PV, 0 W measured export — the fiction) and a
    /// PV-present control block where the export materialised.
    #[test]
    fn audit_totals_a_dark_and_a_lit_block() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 30, 17, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 9, 30, 14, 30, 0).unwrap();
        let planned = vec![block(t0, 4.0, 0.28), block(t1, 2.7, 0.15)];
        let measured_pv_w = HashMap::from([(t0, 0.0), (t1, 4090.0)]);
        let measured_export_w = HashMap::from([(t0, 0.0), (t1, 2700.0)]);

        let totals = audit_planned_vs_measured(&planned, &measured_pv_w, &measured_export_w);
        assert_eq!(totals.dark_blocks, 1);
        assert!((totals.dark_planned_export_kwh - 1.0).abs() < 1e-9); // 4 kW * 0.25 h
        assert_eq!(totals.dark_measured_export_kwh, 0.0);
        assert!((totals.dark_eur_booked - 0.28).abs() < 1e-9); // 1.0 kWh * 0.28
        assert_eq!(totals.lit_blocks, 1);
        assert!((totals.lit_planned_export_kwh - 0.675).abs() < 1e-9); // 2.7 kW * 0.25 h
        assert!((totals.lit_measured_export_kwh - 0.675).abs() < 1e-9); // 2700 W -> 2.7 kW * 0.25 h
        assert_eq!(totals.skipped_missing_data, 0);
    }

    /// An hourly block expands into 4 consecutive 15-min quarters, same kW/price, consecutive
    /// starts; a block already 15 min or shorter is returned unchanged.
    #[test]
    fn expand_to_15min_splits_an_hourly_block_into_four_quarters() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 30, 17, 0, 0).unwrap();
        let mut hourly = block(t0, 4.0, 0.28);
        hourly.dt_hours = 1.0;
        let quarters = expand_to_15min(&hourly);
        assert_eq!(quarters.len(), 4);
        for (i, q) in quarters.iter().enumerate() {
            assert_eq!(q.block_start, t0 + Duration::minutes(15 * i as i64));
            assert_eq!(q.dt_hours, 0.25);
            assert_eq!(q.grid_export_kw, 4.0);
            assert_eq!(q.export_price, Some(0.28));
        }

        let fine = block(t0, 1.0, 0.28);
        assert_eq!(expand_to_15min(&fine), vec![fine]);
    }

    /// The merge-time fix: an hourly plan block (23:00, covering 23:00–00:00) and an overlapping
    /// 15-min block from a LATER source at 23:30 must not both contribute the 23:30 quarter — the
    /// later source wins on that one quarter, the hourly block's other 3 quarters are untouched.
    #[test]
    fn later_source_overwrites_only_the_overlapping_quarter() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 30, 23, 0, 0).unwrap();
        let mut blocks: BTreeMap<DateTime<Utc>, PlannedBlock> = BTreeMap::new();

        let mut hourly = block(t0, 4.0, 0.28);
        hourly.dt_hours = 1.0;
        merge_expanded(&mut blocks, vec![hourly]);
        assert_eq!(blocks.len(), 4); // 23:00, 23:15, 23:30, 23:45

        let later = block(t0 + Duration::minutes(30), 1.5, 0.19); // a different plan for 23:30
        merge_expanded(&mut blocks, vec![later.clone()]);

        assert_eq!(blocks.len(), 4, "still exactly 4 quarters, no duplicate");
        assert_eq!(blocks[&(t0 + Duration::minutes(30))], later);
        // The other 3 quarters still carry the hourly block's values, untouched.
        assert_eq!(blocks[&t0].grid_export_kw, 4.0);
        assert_eq!(blocks[&(t0 + Duration::minutes(15))].grid_export_kw, 4.0);
        assert_eq!(blocks[&(t0 + Duration::minutes(45))].grid_export_kw, 4.0);

        // Scoring the merged (already-expanded) quarters counts the 23:30 quarter exactly ONCE,
        // with the later source's export (1.5 kW), not double-counted with the hourly block's 4 kW.
        let planned: Vec<PlannedBlock> = blocks.into_values().collect();
        let measured_pv_w = HashMap::from([
            (t0, 0.0),
            (t0 + Duration::minutes(15), 0.0),
            (t0 + Duration::minutes(30), 0.0),
            (t0 + Duration::minutes(45), 0.0),
        ]);
        let measured_export_w = HashMap::from([
            (t0, 0.0),
            (t0 + Duration::minutes(15), 0.0),
            (t0 + Duration::minutes(30), 0.0),
            (t0 + Duration::minutes(45), 0.0),
        ]);
        let totals = audit_planned_vs_measured(&planned, &measured_pv_w, &measured_export_w);
        assert_eq!(totals.dark_blocks, 4);
        // 3 quarters at 4 kW + 1 quarter at 1.5 kW, all * 0.25 h — not 4 quarters at 4 kW (which
        // would mean the 23:30 quarter was double-counted/overwritten the wrong way).
        assert!((totals.dark_planned_export_kwh - (3.0 * 4.0 + 1.5) * 0.25).abs() < 1e-9);
    }

    #[test]
    fn audit_totals_skips_blocks_with_missing_measurement_or_price() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 30, 17, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 9, 30, 17, 15, 0).unwrap();
        let mut b1 = block(t1, 1.0, 0.28);
        b1.export_price = None;
        let planned = vec![block(t0, 1.0, 0.28), b1];
        let measured_pv_w = HashMap::from([(t0, 0.0)]); // t1 missing entirely
        let measured_export_w = HashMap::new(); // both missing
        let totals = audit_planned_vs_measured(&planned, &measured_pv_w, &measured_export_w);
        assert_eq!(totals.skipped_missing_data, 2);
        assert_eq!(totals.dark_blocks, 0);
        assert_eq!(totals.lit_blocks, 0);
    }

    #[test]
    fn blocks_with_no_planned_export_are_ignored() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 30, 17, 0, 0).unwrap();
        let planned = vec![block(t0, 0.0, 0.28)];
        let measured_pv_w = HashMap::from([(t0, 0.0)]);
        let measured_export_w = HashMap::from([(t0, 0.0)]);
        let totals = audit_planned_vs_measured(&planned, &measured_pv_w, &measured_export_w);
        assert_eq!(totals.dark_blocks, 0);
        assert_eq!(totals.skipped_missing_data, 0);
    }
}
