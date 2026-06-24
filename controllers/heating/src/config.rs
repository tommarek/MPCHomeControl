//! Heating controller configuration (JSON5).

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::translate::TranslateCfg;

#[derive(Debug, Clone, Deserialize)]
pub struct HeatingConfig {
    /// Intends to actuate; the UDP send also requires the `MPC_CONTROLLER_ARM` env token. Default dry-run.
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default = "default_controller_id")]
    pub controller_id: String,
    /// North topic this controller subscribes to for commands.
    #[serde(default = "default_control_topic")]
    pub control_topic: String,
    /// The Loxone Miniserver UDP virtual-input endpoint.
    #[serde(default)]
    pub loxone: LoxoneConfig,
    /// Default prefix for a zone's virtual-input key (`<prefix><zone>`).
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// Optional per-zone virtual-input key overrides.
    #[serde(default)]
    pub zone_map: HashMap<String, String>,
    /// On deadman expiry: `hold` (stop sending — loxone resumes) or `all_off` (drive all zones off).
    #[serde(default = "default_failsafe")]
    pub failsafe: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_client_id")]
    pub client_id: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: default_mqtt_host(),
            port: default_mqtt_port(),
            client_id: default_client_id(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoxoneConfig {
    #[serde(default = "default_loxone_host")]
    pub host: String,
    #[serde(default = "default_loxone_port")]
    pub port: u16,
}

impl Default for LoxoneConfig {
    fn default() -> Self {
        Self {
            host: default_loxone_host(),
            port: default_loxone_port(),
        }
    }
}

impl HeatingConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(json5::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn translate_cfg(&self) -> TranslateCfg {
        TranslateCfg {
            key_prefix: self.key_prefix.clone(),
            zone_map: self.zone_map.clone(),
        }
    }

    /// The Loxone UDP target `host:port`.
    pub fn loxone_target(&self) -> String {
        format!("{}:{}", self.loxone.host, self.loxone.port)
    }
}

fn default_controller_id() -> String {
    "heating".to_string()
}
fn default_control_topic() -> String {
    "mpc/control/heating".to_string()
}
fn default_key_prefix() -> String {
    "mpc_heat_".to_string()
}
fn default_failsafe() -> String {
    "hold".to_string()
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "mpc-controller-heating".to_string()
}
fn default_loxone_host() -> String {
    "192.168.1.10".to_string()
}
fn default_loxone_port() -> u16 {
    4000
}
