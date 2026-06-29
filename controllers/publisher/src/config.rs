//! The plan-publisher configuration (JSON5, mirroring the parent crate's convention).

use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// How the publisher reads the read-only MPC plan and republishes commands to MQTT.
#[derive(Debug, Clone, Deserialize)]
pub struct PublisherConfig {
    /// The MPC's read-only plan endpoint to poll.
    #[serde(default = "default_mpc_url")]
    pub mpc_url: String,
    /// Poll cadence (seconds).
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Deadman window (seconds): each command is valid for `now + this`. If the publisher stalls,
    /// the command expires and controllers hand control back. Keep it a small multiple of the poll.
    #[serde(default = "default_deadman_seconds")]
    pub deadman_seconds: i64,
    /// `true` = publish to MQTT; `false` = dry-run, log only. Struct default is `false`; the production
    /// `publisher.json5` sets `armed: true`. (Publishing only touches the inert `mpc/control/...`
    /// namespace; hardware actuation is a separate two-key arm on the per-domain controllers.)
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mqtt: MqttConfig,
    /// Emit a battery command (for the Growatt controller) when present.
    #[serde(default)]
    pub battery: Option<BatteryPub>,
    /// Emit a heating command (for the heating controller) when present.
    #[serde(default)]
    pub heating: Option<HeatingPub>,
    /// Emit an EV-charger command (for the EV controller) when present.
    #[serde(default)]
    pub ev: Option<EvPub>,
    /// Emit a controllable-load command (for the boiler controller) when present.
    #[serde(default)]
    pub boiler: Option<BoilerPub>,
    /// Emit a unified Loxone command (for the loxone controller) when present — supersedes the
    /// `heating`/`ev` blocks for Loxone-bound actuation (configure this OR those, not both).
    #[serde(default)]
    pub loxone: Option<LoxonePub>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    #[serde(default = "controller_common::default_mqtt_host")]
    pub host: String,
    #[serde(default = "controller_common::default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_publisher_client_id")]
    pub client_id: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: controller_common::default_mqtt_host(),
            port: controller_common::default_mqtt_port(),
            client_id: default_publisher_client_id(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatteryPub {
    #[serde(default = "default_growatt_id")]
    pub controller_id: String,
    /// SoC band (kWh) the controller pins stop-SoC against.
    pub min_soc_kwh: f64,
    pub max_soc_kwh: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeatingPub {
    #[serde(default = "default_heating_id")]
    pub controller_id: String,
    /// A zone is "on" when its planned power exceeds this (kW) — mirrors the MPC's relay threshold.
    #[serde(default = "default_on_threshold_kw")]
    pub on_threshold_kw: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvPub {
    #[serde(default = "default_ev_id")]
    pub controller_id: String,
    /// A charger is "on" when its planned first-block power exceeds this (kW).
    #[serde(default = "default_on_threshold_kw")]
    pub on_threshold_kw: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoilerPub {
    #[serde(default = "default_boiler_id")]
    pub controller_id: String,
    /// A controllable load is "on" when its planned first-block power exceeds this (kW).
    #[serde(default = "default_on_threshold_kw")]
    pub on_threshold_kw: f64,
}

/// The unified Loxone command: maps plan fields to exact Loxone virtual-input keys. Each sub-section
/// is optional, so the wired domains grow without touching the controller (which is a generic writer).
#[derive(Debug, Clone, Deserialize)]
pub struct LoxonePub {
    #[serde(default = "default_loxone_id")]
    pub controller_id: String,
    /// Per-zone heating relays.
    #[serde(default)]
    pub heating: Option<LoxoneHeatingMap>,
    /// EV charge power.
    #[serde(default)]
    pub ev: Option<LoxoneEvMap>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoxoneHeatingMap {
    /// A zone is "on" (`1`) when its planned power exceeds this (kW).
    #[serde(default = "default_on_threshold_kw")]
    pub on_threshold_kw: f64,
    /// MPC zone name → exact Loxone virtual-input key (e.g. `ground_hall` → `MPCHeatChodbaDole`). A
    /// zone with no entry is simply not written.
    #[serde(default)]
    pub zone_keys: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoxoneEvMap {
    /// The virtual-input key for the controllable charger's power (kW), e.g. `EvChargePower`. Applies
    /// to the first controllable charger with a non-empty plan (the house has a single wallbox).
    pub power_key: String,
}

fn default_mpc_url() -> String {
    "http://127.0.0.1:3000/api/plan/latest".to_string()
}
fn default_poll_seconds() -> u64 {
    30
}
fn default_deadman_seconds() -> i64 {
    120
}
fn default_publisher_client_id() -> String {
    "mpc-plan-publisher".to_string()
}
fn default_growatt_id() -> String {
    "growatt".to_string()
}
fn default_heating_id() -> String {
    "heating".to_string()
}
fn default_ev_id() -> String {
    "ev".to_string()
}
fn default_boiler_id() -> String {
    "boiler".to_string()
}
fn default_loxone_id() -> String {
    "loxone".to_string()
}
fn default_on_threshold_kw() -> f64 {
    0.05
}

impl PublisherConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = json5::from_str(&std::fs::read_to_string(path)?)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject contradictory configs. The unified `loxone` block supersedes the per-domain
    /// `heating`/`ev` blocks for Loxone-bound actuation, so configuring both would publish two
    /// conflicting commands for the same hardware.
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !(self.loxone.is_some() && (self.heating.is_some() || self.ev.is_some())),
            "publisher config sets both a `loxone` block and a `heating`/`ev` block — the unified \
             loxone controller supersedes them; configure one or the other, not both (they would \
             double-actuate the same Loxone outputs)"
        );
        // Every Loxone VI key must be non-empty, delimiter-free (else `translate` silently drops it),
        // and distinct (two outputs sharing a key would collide in the one datagram).
        if let Some(lx) = &self.loxone {
            fn bad_key(k: &str) -> bool {
                k.is_empty() || k.contains([';', '=', '\n', '\r', '\0'])
            }
            let mut seen: HashSet<&str> = HashSet::new();
            if let Some(h) = &lx.heating {
                for key in h.zone_keys.values() {
                    anyhow::ensure!(
                        !bad_key(key),
                        "loxone `zone_keys` value {key:?} is empty or contains a datagram delimiter (;=/newline/NUL)"
                    );
                    anyhow::ensure!(
                        seen.insert(key.as_str()),
                        "loxone maps two outputs to the same virtual input {key:?} — each must be distinct"
                    );
                }
            }
            if let Some(e) = &lx.ev {
                anyhow::ensure!(
                    !bad_key(&e.power_key),
                    "loxone ev `power_key` {:?} is empty or contains a datagram delimiter (;=/newline/NUL)",
                    e.power_key
                );
                anyhow::ensure!(
                    seen.insert(e.power_key.as_str()),
                    "loxone maps two outputs to the same virtual input {:?} — each must be distinct",
                    e.power_key
                );
            }
        }
        Ok(())
    }
}
