//! A minimal deserializable mirror of the MPC's `/api/plan/latest` response — only the fields the
//! publisher needs. Reading the public JSON API (rather than depending on the `mpc_home_control`
//! crate) keeps the two decoupled.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

/// The `{ computed_at, age_seconds, data }` envelope every API endpoint returns. `computed_at` is the
/// envelope timestamp (sibling of `data`); `data` is the plan report itself. `age_seconds` is
/// **server-computed** — the staleness gate uses it instead of comparing `computed_at` against this
/// host's clock, so cross-host skew can't fake (or hide) a stale plan.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestResponse {
    pub computed_at: DateTime<Utc>,
    #[serde(default)]
    pub age_seconds: u64,
    pub data: PlanReport,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanReport {
    pub first_step: FirstStep,
    #[serde(default)]
    pub timeline: Vec<TimelineBlock>,
    /// Per-charger EV plan (absent when no EV is configured).
    #[serde(default)]
    pub ev: Vec<EvChannel>,
    /// Server-set: a safety-critical input (thermal seed / outside temperature) fell back to a
    /// placeholder. A degraded plan is published for inspection but must NOT be actuated — the
    /// publisher skips all commands so the controllers deadman-revert to their failsafe.
    #[serde(default)]
    pub degraded: bool,
}

/// One charger's plan, trimmed to what the EV controller needs: its name, whether it's controllable
/// on our wallbox right now, the first block's planned charge power, the effective target SoC, and
/// the live SoC (`None` when the car's telemetry is stale/untracked — the omit-vs-zero distinction).
#[derive(Debug, Clone, Deserialize)]
pub struct EvChannel {
    pub name: String,
    #[serde(default)]
    pub controllable_now: bool,
    #[serde(default)]
    pub charge_kw: Vec<f64>,
    #[serde(default)]
    pub target_pct: f64,
    #[serde(default)]
    pub soc_pct: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirstStep {
    pub hour_start: DateTime<Utc>,
    #[serde(default)]
    pub heat_kw: HashMap<String, f64>,
    /// Controllable scheduled-load draw (kW) per load for the coming block (`on · rated_kw`) — the
    /// boiler controller's setpoint. Empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    pub mode: ModeStep,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeStep {
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
    pub charge_kw: f64,
    pub discharge_kw: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineBlock {
    pub soc_kwh: f64,
}
