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
/// `deny_unknown_fields`: the POST body is user input and every field is optional, so a typo
/// (`{"target": 90}` for `target_pct`) otherwise deserialised to an all-None preference that
/// validated, merged nothing, and returned `{"ok": true}` — reporting success for a setting that
/// was silently discarded.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

/// Load the persisted preferences for READING (empty on a missing / unreadable file). A parse
/// failure is logged rather than swallowed — silence here looked identical to "no overrides set".
pub fn load() -> EvPrefs {
    match load_strict() {
        Ok(prefs) => prefs,
        Err(e) => {
            eprintln!("[ev] preference store unreadable ({e:#}); treating as empty for this read");
            EvPrefs::default()
        }
    }
}

/// Load for WRITING: distinguishes "no file yet" (an empty map — the normal first run) from
/// "file present but unparseable" (an error).
///
/// The write paths must not use the lenient [`load`]: they load-modify-SAVE, so a store that failed
/// to parse would be replaced by a map containing only the charger being touched — silently
/// destroying every other charger's stored deadline/target/rate. This is reachable without disk
/// corruption (any schema change to `EvPreference`, e.g. renaming an `EvStrategy` variant), so the
/// safe response is to refuse the write and keep the file for inspection.
pub fn load_strict() -> anyhow::Result<EvPrefs> {
    let raw = match std::fs::read_to_string(store_path()) {
        Ok(s) => s,
        // Absent is a legitimate empty history; anything else (permissions, IO) is a real error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(EvPrefs::default()),
        Err(e) => return Err(anyhow::Error::from(e).context("reading ev preference store")),
    };
    serde_json::from_str(&raw).context("parsing ev preference store (refusing to overwrite it)")
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
    // Strict: never overwrite a store we couldn't read (see `load_strict`).
    let mut prefs = load_strict()?;
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
    let mut prefs = load_strict()?;
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

    /// REGRESSION: an unparseable store must NOT be silently replaced. `update`/`clear` load-modify-
    /// save, so swallowing the parse error wiped every other charger's overrides on the next POST.
    #[test]
    fn write_paths_refuse_to_overwrite_an_unparseable_store() {
        let _env = crate::tools::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ev_prefs.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        std::env::set_var("MPC_EV_PREF_STORE", &path);

        assert!(load_strict().is_err(), "a corrupt store must be an error");
        assert!(update("tesla".into(), EvPreference::default()).is_err());
        assert!(clear("tesla").is_err());
        // The file is left intact for inspection, not truncated.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json"
        );
        // Reads still degrade gracefully to "no overrides".
        assert!(load().is_empty());

        // A MISSING file is not an error — the normal first run.
        std::env::set_var("MPC_EV_PREF_STORE", dir.path().join("absent.json"));
        assert!(load_strict().unwrap().is_empty());
        std::env::remove_var("MPC_EV_PREF_STORE");
    }

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
