//! Live EV charging preferences, set from the dashboard and persisted to the MPC's **own** JSON file
//! (`MPC_EV_PREF_STORE`). This is the only write the MPC makes — to its own state, never to the house
//! (InfluxDB/MQTT/loxone are untouched; the wallbox is driven by the controller, gated separately).
//! A preference takes precedence over the config defaults, but the effective target is always
//! capped at the car's own charge limit (see `state::effective_target_pct` — the car stops
//! accepting there regardless).

use std::collections::HashMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::optimize::config::EvStrategy;

/// A per-charger override. Any field `None` ⇒ fall back to config (and, for the target, the car).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvPreference {
    #[serde(default)]
    pub strategy: Option<EvStrategy>,
    /// Cap the charge rate (kW); clamped to the charger's `max_kw`.
    #[serde(default)]
    pub max_rate_kw: Option<f64>,
    /// Target state of charge (%, 0..100).
    #[serde(default)]
    pub target_pct: Option<f64>,
    /// "Charged-by" local time-of-day, `"HH:MM"`.
    #[serde(default)]
    pub deadline: Option<String>,
}

/// All chargers' live preferences, keyed by charger name.
pub type EvPrefs = HashMap<String, EvPreference>;

fn store_path() -> String {
    std::env::var("MPC_EV_PREF_STORE").unwrap_or_else(|_| "ev_prefs.json".to_string())
}

/// Load the persisted preferences (empty on a missing / unreadable file — best-effort).
pub fn load() -> EvPrefs {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the full preference map to the local store. Write-then-rename so a crash mid-write can
/// never truncate the existing file (the same atomic pattern as the forecast snapshots).
pub fn save(prefs: &EvPrefs) -> anyhow::Result<()> {
    let path = store_path();
    // Create the parent directory if a nested `MPC_EV_PREF_STORE` points into one that doesn't exist
    // yet (e.g. a mounted `/data/mpc/ev_prefs.json`) — same as the forecast-snapshot store.
    if let Some(parent) = std::path::Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("creating ev preference directory")?;
    }
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(prefs)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Serializes the load-modify-save in [`update`] so two concurrent dashboard POSTs (the multi-threaded
/// server runs handlers in parallel) can't both load and then clobber each other's write — a lost update.
static UPDATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Merge one charger's preference into the persisted store, atomically w.r.t. other [`update`] calls.
/// **Field-level merge**: only the fields that are `Some` in the POSTed body overwrite the stored
/// ones — the documented "set any subset" contract. (A whole-object replace would silently wipe a
/// stored deadline/rate every time the dashboard saves just the strategy.) Clearing a stored field
/// is [`clear`] — remove the whole preference and re-set what should remain.
/// The whole load-modify-save runs under [`UPDATE_LOCK`]; it is fully synchronous (no `.await`), so
/// holding the lock across the file IO is safe.
pub fn update(name: String, pref: EvPreference) -> anyhow::Result<()> {
    let _guard = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut prefs = load();
    merge_into(prefs.entry(name).or_default(), pref);
    save(&prefs)
}

/// Overwrite only the fields the incoming preference actually sets (`Some`).
fn merge_into(stored: &mut EvPreference, pref: EvPreference) {
    if pref.strategy.is_some() {
        stored.strategy = pref.strategy;
    }
    if pref.max_rate_kw.is_some() {
        stored.max_rate_kw = pref.max_rate_kw;
    }
    if pref.target_pct.is_some() {
        stored.target_pct = pref.target_pct;
    }
    if pref.deadline.is_some() {
        stored.deadline = pref.deadline;
    }
}

/// Remove one charger's stored preference entirely (every field reverts to config / the car) —
/// the documented way to clear overrides. Atomic w.r.t. [`update`] via the same lock.
pub fn clear(name: &str) -> anyhow::Result<()> {
    let _guard = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut prefs = load();
    prefs.remove(name);
    save(&prefs)
}

impl EvPreference {
    /// `HH:MM` deadline as `(hour, minute)`, if set and well-formed.
    pub fn deadline_hm(&self) -> Option<(u32, u32)> {
        let (h, m) = self.deadline.as_ref()?.split_once(':')?;
        let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some((h, m))
    }

    /// Validate an incoming preference from the dashboard.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(t) = self.target_pct {
            anyhow::ensure!((0.0..=100.0).contains(&t), "target_pct must be in 0..100");
        }
        if let Some(r) = self.max_rate_kw {
            // Reject a non-finite rate at the API boundary (`Infinity >= 0` would otherwise pass and
            // only be caught by the downstream clamp); finite ≥ 0 is the contract.
            anyhow::ensure!(
                r.is_finite() && r >= 0.0,
                "max_rate_kw must be finite and ≥ 0"
            );
        }
        anyhow::ensure!(
            self.deadline.is_none() || self.deadline_hm().is_some(),
            "deadline must be local HH:MM"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_fields_the_post_omits() {
        // The dashboard's Save sends only {strategy, target_pct}; a stored deadline must survive
        // (the documented "set any subset" contract — a whole-object replace silently wiped it).
        let mut stored = EvPreference {
            strategy: None,
            max_rate_kw: Some(7.0),
            target_pct: Some(80.0),
            deadline: Some("07:00".into()),
        };
        merge_into(
            &mut stored,
            EvPreference {
                strategy: Some(EvStrategy::SolarPreferred),
                max_rate_kw: None,
                target_pct: Some(90.0),
                deadline: None,
            },
        );
        assert_eq!(stored.strategy, Some(EvStrategy::SolarPreferred)); // set
        assert_eq!(stored.target_pct, Some(90.0)); // overwritten
        assert_eq!(stored.max_rate_kw, Some(7.0)); // kept
        assert_eq!(stored.deadline.as_deref(), Some("07:00")); // kept
    }
}
