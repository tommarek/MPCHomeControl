//! Loxone controller configuration (JSON5).

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct LoxoneControllerConfig {
    /// Intends to actuate; the UDP send also requires the `MPC_CONTROLLER_ARM` env token. Struct default
    /// is `false` (dry-run); the production `loxone.json5` sets `armed: true`.
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
    /// The heartbeat/gate virtual input, sent `=1` with every datagram and `=0` on the deadman
    /// release, so Loxone can `AND` every MPC output with it. Empty disables the heartbeat.
    #[serde(default = "default_heartbeat_key")]
    pub heartbeat_key: String,
    /// On deadman expiry: `hold` (default — go quiet; the recommended digital-input/Off-Delay wiring
    /// times the gate out) or `release` (send `<heartbeat_key>=0`, for an analog-*value* gate). With a
    /// pulse-mode digital input, `release` is counter-productive: the `=0` is just another pulse that
    /// retriggers the Off-Delay and *delays* the fallback — use `hold` there.
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

impl LoxoneControllerConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_json5(&std::fs::read_to_string(path)?)
    }

    /// Parse + validate (shared by [`Self::load`] and the tests).
    fn from_json5(s: &str) -> Result<Self> {
        let cfg: Self = json5::from_str(s)?;
        anyhow::ensure!(
            matches!(cfg.failsafe.as_str(), "release" | "hold"),
            "loxone controller `failsafe` must be \"release\" or \"hold\", got {:?} \
             (a typo would silently disable the MPCActive release)",
            cfg.failsafe
        );
        // A delimiter in the heartbeat key would be silently dropped by `translate`, disabling the
        // gate — reject it rather than fail open.
        anyhow::ensure!(
            !cfg.heartbeat_key.contains([';', '=', '\n', '\r', '\0']),
            "loxone controller `heartbeat_key` must not contain a datagram delimiter (;=/newline/NUL), \
             got {:?}",
            cfg.heartbeat_key
        );
        // `release` sends `<heartbeat_key>=0`; with no key there is no gate to release.
        anyhow::ensure!(
            cfg.failsafe != "release" || !cfg.heartbeat_key.is_empty(),
            "loxone controller `failsafe: \"release\"` needs a non-empty `heartbeat_key`; \
             set one or use `failsafe: \"hold\"`"
        );
        Ok(cfg)
    }

    /// The Loxone UDP target `host:port`.
    pub fn loxone_target(&self) -> String {
        format!("{}:{}", self.loxone.host, self.loxone.port)
    }
}

fn default_controller_id() -> String {
    "loxone".to_string()
}
fn default_control_topic() -> String {
    "mpc/control/loxone".to_string()
}
fn default_heartbeat_key() -> String {
    "MPCActive".to_string()
}
fn default_failsafe() -> String {
    // "hold" suits the recommended Loxone wiring (MPCActive as a digital-input pulse → Off-Delay
    // watchdog): on the deadman the controller simply goes quiet and the Off-Delay times out. Use
    // "release" only if Loxone reads MPCActive as an analog *value* (then `=0` gates off directly).
    "hold".to_string()
}
fn default_client_id() -> String {
    "mpc-controller-loxone".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = LoxoneControllerConfig::from_json5("{}").unwrap();
        assert_eq!(cfg.failsafe, "hold");
        assert_eq!(cfg.heartbeat_key, "MPCActive");
        assert_eq!(cfg.controller_id, "loxone");
        assert_eq!(cfg.control_topic, "mpc/control/loxone");
    }

    #[test]
    fn rejects_a_malformed_or_missing_gate() {
        // A delimiter in the heartbeat key would be silently dropped → reject it.
        assert!(LoxoneControllerConfig::from_json5(r#"{ heartbeat_key: "MPC;Active" }"#).is_err());
        // `release` with no gate to release is incoherent.
        assert!(LoxoneControllerConfig::from_json5(
            r#"{ failsafe: "release", heartbeat_key: "" }"#
        )
        .is_err());
        // `hold` with an empty (disabled) gate is fine.
        assert!(
            LoxoneControllerConfig::from_json5(r#"{ failsafe: "hold", heartbeat_key: "" }"#)
                .is_ok()
        );
    }

    #[test]
    fn rejects_unknown_failsafe() {
        assert!(LoxoneControllerConfig::from_json5(r#"{ failsafe: "hold" }"#).is_ok());
        // A typo must be rejected, not silently treated as "hold".
        assert!(LoxoneControllerConfig::from_json5(r#"{ failsafe: "relase" }"#).is_err());
    }
}
