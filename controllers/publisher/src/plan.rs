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
    // item 1 (rework cycle 2, finding 1): `first_step`/`FirstStep`/`ModeStep` used to be the sole
    // source `commands()` read for the CURRENT command; that's exactly the bug (right after a mark,
    // and before the brain's own re-plan, `first_step` is still the block that just ENDED). The
    // publisher now sources the current command from `timeline[0..2]`'s COVERING block instead (see
    // `build::covering_block`), so this field is deliberately no longer mapped here — the brain still
    // emits it (other consumers, e.g. the dashboard, still read it from the brain's own API), and this
    // struct has no `deny_unknown_fields`, so dropping it here only stops mapping JSON we no longer use.
    #[serde(default)]
    pub timeline: Vec<TimelineBlock>,
    /// item 3 (rework cycle 2, findings 5/2): block 1, with its `heat_kw`/`cool_kw`/`hvac_heat_kw`
    /// FROZEN (and `frozen: true`) from `mark − 120 s` onward — see `TimelineBlock::frozen`'s doc on
    /// the brain side. [`next_commands`](crate::build::next_commands) builds the NEXT command from
    /// THIS field, not `timeline[1]`, and emits nothing unless `frozen` is `true`: that is what makes
    /// what the controllers apply at the mark always equal what the brain itself latches at rollover.
    /// `#[serde(default)]`: absent (an older brain, before this field existed, or before block 1 even
    /// exists) reads as `None` — no next command is built, the fail-safe direction.
    #[serde(default)]
    pub next_step: Option<TimelineBlock>,
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
pub struct TimelineBlock {
    /// Block start (RFC 3339). item G: `timeline[1].t` is the NEXT command's `apply_at`.
    pub t: DateTime<Utc>,
    /// item F: 15 for a near-term fine block, 60 for an hourly one further out. Block 1 is always a
    /// fine block (design §6), so this is only read to size the next command's own validity window
    /// (`apply_at + one block`) — never assumed to be 15 elsewhere.
    pub dt_minutes: u32,
    pub soc_kwh: f64,
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
    pub charge_kw: f64,
    pub discharge_kw: f64,
    #[serde(default)]
    pub heat_kw: HashMap<String, f64>,
    /// Mirrors `FirstStep::controllable_load_kw`; empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    /// item 3: `true` only on [`PlanReport::next_step`], once the brain's pre-mark freeze window has
    /// pinned it — see that field's doc. Absent (an older brain) or on an ordinary `timeline` row ⇒
    /// `false`, the fail-safe default (`next_commands` emits nothing unless this is `true`).
    #[serde(default)]
    pub frozen: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// item F (the multi-rate planning grid) + item G (`timeline[1]` now feeds the next command):
    /// every field this publisher reads off a block (`t`, `dt_minutes`, `soc_kwh`, `slot`,
    /// `export_enabled`, `inverter_on`, `charge_kw`, `discharge_kw`, `heat_kw`) parses correctly
    /// alongside brain-side fields it never reads (`import_price`, `cool_kw`, `temp_c`, …), which
    /// must parse as ignored extras, not a deserialization error (no `deny_unknown_fields` on
    /// `TimelineBlock`; this test is the guard against that assumption silently breaking).
    /// `controllable_load_kw` is deliberately absent from the fixture to prove its `#[serde(default)]`
    /// (an older brain / a block with no controllable load omits it).
    #[test]
    fn timeline_block_parses_every_field_this_publisher_reads() {
        let json = r#"{
            "t": "2026-09-22T13:00:00Z",
            "dt_minutes": 60,
            "import_price": 0.12,
            "export_price": 0.05,
            "price_is_placeholder": false,
            "pv_kw": 0.0,
            "load_kw": 0.4,
            "soc_kwh": 6.2,
            "charge_kw": 1.5,
            "discharge_kw": 0.0,
            "grid_import_kw": 0.4,
            "grid_export_kw": 0.0,
            "curtail_kw": 0.0,
            "heat_kw": {"livingroom": 2.1},
            "cool_kw": {},
            "hvac_heat_kw": {},
            "temp_c": {},
            "slot": "charge_from_grid",
            "export_enabled": true,
            "inverter_on": true
        }"#;
        let block: TimelineBlock = serde_json::from_str(json).expect("unknown fields are ignored");
        assert_eq!(
            block.t,
            DateTime::parse_from_rfc3339("2026-09-22T13:00:00Z").unwrap()
        );
        assert_eq!(block.dt_minutes, 60);
        assert!((block.soc_kwh - 6.2).abs() < 1e-9);
        assert!((block.charge_kw - 1.5).abs() < 1e-9);
        assert_eq!(block.slot, "charge_from_grid");
        assert!(block.export_enabled && block.inverter_on);
        assert_eq!(block.heat_kw.get("livingroom"), Some(&2.1));
        assert!(
            block.controllable_load_kw.is_empty(),
            "absent field defaults to {{}}"
        );
    }
}
