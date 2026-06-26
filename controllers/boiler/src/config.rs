//! Boiler controller configuration (JSON5).

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::translate::TranslateCfg;

#[derive(Debug, Clone, Deserialize)]
pub struct BoilerControllerConfig {
    /// Intends to actuate; a real send would also require the `MPC_CONTROLLER_ARM` env token. Default
    /// dry-run. (The translate path is a stub today, so even armed it only logs — see `translate.rs`.)
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default = "default_controller_id")]
    pub controller_id: String,
    /// North topic this controller subscribes to for commands.
    #[serde(default = "default_control_topic")]
    pub control_topic: String,
    /// A label for the (not-yet-wired) device target, recorded in the would-send audit log.
    #[serde(default = "default_target_label")]
    pub target_label: String,
    /// On deadman expiry: `hold` (stop sending — the existing system resumes) or `all_off` (drive all
    /// loads off).
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

impl BoilerControllerConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = json5::from_str(&std::fs::read_to_string(path)?)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject a config that would silently misbehave at runtime. `failsafe` is compared `== "all_off"`
    /// with every other value falling through to *hold*, so a typo would silently get `hold` — pin it
    /// to the known set at load. (Mirrors the EV controller.)
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
            target_label: self.target_label.clone(),
        }
    }
}

fn default_controller_id() -> String {
    "boiler".to_string()
}
fn default_control_topic() -> String {
    "mpc/control/boiler".to_string()
}
fn default_target_label() -> String {
    "boiler".to_string()
}
fn default_failsafe() -> String {
    "hold".to_string()
}
fn default_client_id() -> String {
    "mpc-controller-boiler".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_failsafe(failsafe: &str) -> BoilerControllerConfig {
        json5::from_str(&format!(r#"{{ failsafe: "{failsafe}" }}"#)).unwrap()
    }

    #[test]
    fn failsafe_must_be_a_known_mode() {
        assert!(cfg_with_failsafe("hold").validate().is_ok());
        assert!(cfg_with_failsafe("all_off").validate().is_ok());
        assert!(cfg_with_failsafe("all-off").validate().is_err());
    }

    #[test]
    fn defaults_are_sane() {
        let cfg: BoilerControllerConfig = json5::from_str("{}").unwrap();
        assert_eq!(cfg.controller_id, "boiler");
        assert_eq!(cfg.control_topic, "mpc/control/boiler");
        assert!(!cfg.armed);
        assert!(cfg.validate().is_ok());
    }
}
