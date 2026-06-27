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
    #[serde(default = "controller_common::default_mqtt_host")]
    pub host: String,
    #[serde(default = "controller_common::default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_client_id")]
    pub client_id: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: controller_common::default_mqtt_host(),
            port: controller_common::default_mqtt_port(),
            client_id: default_client_id(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoxoneConfig {
    #[serde(default = "controller_common::default_loxone_host")]
    pub host: String,
    #[serde(default = "controller_common::default_loxone_port")]
    pub port: u16,
}

impl Default for LoxoneConfig {
    fn default() -> Self {
        Self {
            host: controller_common::default_loxone_host(),
            port: controller_common::default_loxone_port(),
        }
    }
}

impl HeatingConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = json5::from_str(&std::fs::read_to_string(path)?)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject a config that would silently misbehave at runtime — `failsafe` is compared `== "all_off"`,
    /// so a typo would fall through to `hold` unnoticed (matches the ev/boiler controllers).
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.failsafe.as_str(), "hold" | "all_off"),
            "failsafe must be \"hold\" or \"all_off\", got {:?}",
            self.failsafe
        );
        Ok(())
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
fn default_client_id() -> String {
    "mpc-controller-heating".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(failsafe: &str) -> Result<HeatingConfig> {
        let cfg: HeatingConfig = json5::from_str(&format!(r#"{{ failsafe: "{failsafe}" }}"#))?;
        cfg.validate().map(|()| cfg)
    }

    #[test]
    fn failsafe_must_be_a_known_mode() {
        assert!(parse("hold").is_ok());
        assert!(parse("all_off").is_ok());
        // a typo must be rejected, not silently treated as "hold"
        assert!(parse("all-off").is_err());
    }
}
