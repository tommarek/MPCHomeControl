//! The plan-publisher configuration (JSON5, mirroring the parent crate's convention).

use anyhow::Result;
use serde::Deserialize;
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
    /// `true` = publish to MQTT; `false` (default) = dry-run, log only. (Publishing only touches the
    /// inert `mpc/control/...` namespace; hardware actuation is a separate arm on the controllers.)
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_publisher_client_id")]
    pub client_id: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: default_mqtt_host(),
            port: default_mqtt_port(),
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
    /// A zone is "on" when its planned power exceeds this (kW) — mirrors the shadow's relay threshold.
    #[serde(default = "default_on_threshold_kw")]
    pub on_threshold_kw: f64,
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
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
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
fn default_on_threshold_kw() -> f64 {
    0.05
}

impl PublisherConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(json5::from_str(&std::fs::read_to_string(path)?)?)
    }
}
