//! Growatt controller configuration (JSON5).

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::translate::TranslateCfg;

#[derive(Debug, Clone, Deserialize)]
pub struct GrowattConfig {
    /// `true` only *intends* to actuate; the south-side publish also requires the `MPC_CONTROLLER_ARM`
    /// env token. Both keys are needed — neither alone touches the inverter. Default dry-run.
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default = "default_controller_id")]
    pub controller_id: String,
    /// North topic this controller subscribes to for commands.
    #[serde(default = "default_control_topic")]
    pub control_topic: String,
    /// South topic prefix for the real Growatt commands.
    #[serde(default = "default_command_base")]
    pub command_base: String,
    /// Growatt live-telemetry topic the controller subscribes to (for the live SoC).
    #[serde(default = "default_telemetry_topic")]
    pub telemetry_topic: String,
    #[serde(default = "default_inverter_kw_rating")]
    pub inverter_kw_rating: f64,
    #[serde(default = "default_powerrate_step_pct")]
    pub powerrate_step_pct: f64,
    #[serde(default = "default_battery_capacity_kwh")]
    pub battery_capacity_kwh: f64,
    /// Local civil-time offset from UTC (for the inverter slot's wall-clock window).
    #[serde(default = "default_utc_offset_hours")]
    pub utc_offset_hours: i32,
    /// What to do when a command's deadman expires: `revert_to_regular` (hand back to loxone) or `hold`.
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

impl GrowattConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(json5::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn translate_cfg(&self) -> TranslateCfg {
        TranslateCfg {
            command_base: self.command_base.clone(),
            inverter_kw_rating: self.inverter_kw_rating,
            powerrate_step_pct: self.powerrate_step_pct,
            battery_capacity_kwh: self.battery_capacity_kwh,
        }
    }
}

fn default_controller_id() -> String {
    "growatt".to_string()
}
fn default_control_topic() -> String {
    "mpc/control/growatt".to_string()
}
fn default_command_base() -> String {
    "energy/solar/command".to_string()
}
fn default_telemetry_topic() -> String {
    "energy/solar".to_string()
}
fn default_inverter_kw_rating() -> f64 {
    5.3
}
fn default_powerrate_step_pct() -> f64 {
    1.0
}
fn default_battery_capacity_kwh() -> f64 {
    10.0
}
fn default_utc_offset_hours() -> i32 {
    2
}
fn default_failsafe() -> String {
    "revert_to_regular".to_string()
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "mpc-controller-growatt".to_string()
}
