//! A minimal deserializable mirror of the MPC's `/api/plan/latest` response — only the fields the
//! publisher needs. Reading the public JSON API (rather than depending on the `mpc_home_control`
//! crate) keeps the two decoupled.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

/// The `{ computed_at, age_seconds, data }` envelope every API endpoint returns. `computed_at` is the
/// envelope timestamp (sibling of `data`); `data` is the plan report itself.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestResponse {
    pub computed_at: DateTime<Utc>,
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
}

/// One charger's plan, trimmed to what the EV controller needs: its name, whether it's controllable
/// on our wallbox right now, the first block's planned charge power, and the effective target SoC.
#[derive(Debug, Clone, Deserialize)]
pub struct EvChannel {
    pub name: String,
    #[serde(default)]
    pub controllable_now: bool,
    #[serde(default)]
    pub charge_kw: Vec<f64>,
    #[serde(default)]
    pub target_pct: f64,
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
