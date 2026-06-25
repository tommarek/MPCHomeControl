//! mqtt-source configuration (JSON5).
//!
//! A `topics` list maps each subscribed MQTT topic to a stable URL `name` the MPC pulls
//! (`GET /v1/value/<name>`). Read-only: the adapter only subscribes and serves `GET`s — it never
//! publishes and never writes any store.

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub mqtt: MqttConfig,
    /// Address the HTTP value endpoint binds (the MPC reaches it by service name on the docker net).
    #[serde(default = "default_bind")]
    pub bind: String,
    /// The topics to subscribe + expose.
    #[serde(default)]
    pub topics: Vec<TopicMap>,
}

// Deliberately not shared with mqtt-bridge via mqtt-common: each adapter needs its *own* default
// `client_id` (MQTT ids must be unique per broker, or the broker drops the older connection), and a
// serde `default` on a shared struct field can't be parameterised per crate.
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
pub struct TopicMap {
    /// URL alias — the MPC pulls `GET /v1/value/<name>`.
    pub name: String,
    /// MQTT topic filter to subscribe (exact, or with `+`/`#` wildcards).
    pub topic: String,
    /// Optional JSON pointer if the payload is a JSON object; absent ⇒ the whole payload is the scalar.
    #[serde(default)]
    pub pointer: Option<String>,
    /// Multiplicative factor applied to the parsed value before serving (unit conversion).
    #[serde(default = "unit_scale")]
    pub scale: f64,
    /// Serve `404` (→ the MPC reads `None`) once the cached value is older than this many seconds.
    /// Absent ⇒ always serve the last-known value (right for retained / rarely-changing signals like
    /// a charge limit).
    #[serde(default)]
    pub max_age_seconds: Option<u64>,
}

impl SourceConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_json5(&std::fs::read_to_string(path)?)
    }

    /// Parse + validate from a JSON5 string (the file-free core of [`load`], so it is unit-testable).
    fn from_json5(s: &str) -> Result<Self> {
        let cfg: Self = json5::from_str(s)?;
        for t in &cfg.topics {
            let what = format!("topic {:?} ({})", t.name, t.topic);
            mqtt_common::validate_scale(t.scale, &what).map_err(|e| anyhow::anyhow!(e))?;
            mqtt_common::validate_pointer(t.pointer.as_deref(), &format!("topic {:?}", t.name))
                .map_err(|e| anyhow::anyhow!(e))?;
            mqtt_common::validate_filter(&t.topic).map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(cfg)
    }
}

fn unit_scale() -> f64 {
    1.0
}
fn default_bind() -> String {
    "0.0.0.0:8088".to_string()
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "mpc-adapter-mqtt-source".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_scale_and_malformed_pointer() {
        let ok =
            r#"{ topics: [{ name: "a", topic: "t", scale: 0.01, pointer: "/charge_limit_soc" }] }"#;
        assert!(SourceConfig::from_json5(ok).is_ok());
        let bad_scale = r#"{ topics: [{ name: "a", topic: "t", scale: NaN }] }"#;
        assert!(SourceConfig::from_json5(bad_scale).is_err());
        let bad_pointer = r#"{ topics: [{ name: "a", topic: "t", pointer: "charge_limit_soc" }] }"#;
        assert!(SourceConfig::from_json5(bad_pointer).is_err()); // missing leading '/'
    }
}
