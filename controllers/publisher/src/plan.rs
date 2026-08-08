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
    /// REQUIRED (no serde default): the staleness gate keys on this, and a missing field
    /// defaulting to 0 would read as "always fresh" — silently disabling the wedged-loop
    /// failsafe on any schema skew. A deserialize error publishes nothing → deadman → failsafe,
    /// the fail-safe direction.
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
    ///
    /// Defaults to **true** when absent: these two booleans are the ONLY gate between a plan and
    /// the hardware, and the brain always serializes them. An absent field therefore means we are
    /// talking to something we don't understand (renamed field, mismatched build) — the safe
    /// reading of "I can't tell" is "don't actuate", not "go ahead".
    #[serde(default = "unsafe_until_proven")]
    pub degraded: bool,
    /// Server-set: the plan came from the binary-relaxed fallback LP (solver timeout/busy). Its
    /// on/off decisions may be fractional — actuating would round them up to full power and latch
    /// that; skip commands until a strict solve lands (normally the next tick). Absent ⇒ `true`,
    /// for the same fail-safe reason as `degraded`.
    #[serde(default = "unsafe_until_proven")]
    pub relaxed: bool,
}

/// Fail-safe default for the actuation gates (see `degraded`/`relaxed`).
fn unsafe_until_proven() -> bool {
    true
}

/// One charger's plan, trimmed to what the unified loxone EV write needs: whether it's
/// controllable on our wallbox right now and the first block's planned charge power.
#[derive(Debug, Clone, Deserialize)]
pub struct EvChannel {
    #[serde(default)]
    pub controllable_now: bool,
    #[serde(default)]
    pub charge_kw: Vec<f64>,
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
