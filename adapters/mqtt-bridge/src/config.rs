//! MQTT-bridge configuration (JSON5).
//!
//! A `signals` list maps each subscribed MQTT topic to a destination in the InfluxDB pull store
//! (measurement + field + static tags). The bridge is dry-run by default; the write token is read
//! from the environment (`token_env`) and never stored in the file.

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    /// Intends to write to the store; the write also requires the `MPC_ADAPTER_ARM` env token.
    /// Default dry-run (logs the line protocol it *would* write).
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub influx: InfluxConfig,
    /// Topic → store-locator map. Each entry normalises one MQTT signal into the pull store.
    #[serde(default)]
    pub signals: Vec<SignalMap>,
}

// Deliberately not shared with mqtt-source via mqtt-common: each adapter needs its *own* default
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
pub struct InfluxConfig {
    #[serde(default = "default_influx_url")]
    pub url: String,
    #[serde(default = "default_influx_org")]
    pub org: String,
    #[serde(default = "default_influx_bucket")]
    pub bucket: String,
    /// Env var holding the write token (never stored in the file). Tried in order with a fallback.
    #[serde(default = "default_token_env")]
    pub token_env: String,
}

impl Default for InfluxConfig {
    fn default() -> Self {
        Self {
            url: default_influx_url(),
            org: default_influx_org(),
            bucket: default_influx_bucket(),
            token_env: default_token_env(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignalMap {
    /// MQTT topic filter to subscribe (exact, or with `+`/`#` wildcards).
    pub topic: String,
    /// Destination InfluxDB measurement.
    pub measurement: String,
    /// Destination field key (a float).
    pub field: String,
    /// Static tags written with every point (e.g. `car=1`).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Multiply the parsed value before writing (unit conversion; default 1).
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Optional JSON pointer if the payload is a JSON object (e.g. `/battery_level`); absent ⇒ the
    /// whole payload is the scalar.
    #[serde(default)]
    pub pointer: Option<String>,
    /// Optional destination bucket override (defaults to `influx.bucket`).
    #[serde(default)]
    pub bucket: Option<String>,
}

impl BridgeConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_json5(&std::fs::read_to_string(path)?)
    }

    /// Parse + validate from a JSON5 string (the file-free core of [`load`], so it is unit-testable).
    fn from_json5(s: &str) -> Result<Self> {
        let cfg: Self = json5::from_str(s)?;
        for s in &cfg.signals {
            let what = format!("signal {:?} → {}/{}", s.topic, s.measurement, s.field);
            mqtt_common::validate_scale(s.scale, &what).map_err(|e| anyhow::anyhow!(e))?;
            // Make the production-safety guarantee STRUCTURAL: the bridge must never write a live
            // house bucket (the MPC reads these read-only). A bucket typo could otherwise corrupt
            // real data when armed, so reject it at load instead of trusting the documented intent.
            let bucket = s.bucket.as_deref().unwrap_or(&cfg.influx.bucket);
            if PROTECTED_BUCKETS.contains(&bucket) {
                anyhow::bail!(
                    "signal {:?} → {}/{}: bucket {bucket:?} is a protected live-data bucket — the \
                     bridge must never write it (use a dedicated bucket such as {:?})",
                    s.topic,
                    s.measurement,
                    s.field,
                    default_influx_bucket()
                );
            }
            mqtt_common::validate_pointer(s.pointer.as_deref(), &format!("signal {:?}", s.topic))
                .map_err(|e| anyhow::anyhow!(e))?;
            mqtt_common::validate_filter(&s.topic).map_err(|e| anyhow::anyhow!(e))?;
            // Line protocol has no escape for a newline/CR; `line_protocol` strips them, which could
            // silently collide two distinct names ("a\nb" and "ab" → "ab"). Reject them at load — these
            // names are operator config, so a control char is a mistake, not data.
            let names = std::iter::once(("measurement", s.measurement.as_str()))
                .chain(std::iter::once(("field", s.field.as_str())))
                .chain(
                    s.tags
                        .iter()
                        .flat_map(|(k, v)| [("tag key", k.as_str()), ("tag value", v.as_str())]),
                );
            for (what, value) in names {
                anyhow::ensure!(
                    !value.contains(['\n', '\r']),
                    "signal {:?}: {what} {value:?} must not contain a newline or carriage return",
                    s.topic
                );
            }
        }
        Ok(cfg)
    }

    /// The write token, resolved from `token_env` then the standard influx fallbacks. The MPC's own
    /// read path uses the same names — one secret in the environment serves both.
    pub fn resolve_token(&self) -> Option<String> {
        [
            self.influx.token_env.as_str(),
            "INFLUX_TOKEN",
            "INFLUXDB_TOKEN",
        ]
        .into_iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }
}

/// The live house buckets the MPC reads read-only — the bridge must never write any of these (see
/// the production-safety note). A signal targeting one is rejected at config load.
const PROTECTED_BUCKETS: &[&str] = &["loxone", "solar", "weather_forecast", "ote_prices"];

fn default_scale() -> f64 {
    1.0
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "mpc-adapter-mqtt-bridge".to_string()
}
fn default_influx_url() -> String {
    "http://127.0.0.1:8086".to_string()
}
fn default_influx_org() -> String {
    "loxone".to_string()
}
fn default_influx_bucket() -> String {
    "ev".to_string()
}
fn default_token_env() -> String {
    "INFLUXDB_TOKEN".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_positive_or_non_finite_scale() {
        let good = r#"{ signals: [{ topic: "a", measurement: "m", field: "f", scale: 0.001 }] }"#;
        assert!(BridgeConfig::from_json5(good).is_ok());
        // `Infinity`/`NaN` are JSON5 literals that parse to a non-finite f64, so the *validation* (not
        // the number parser) must be what rejects them.
        for bad_scale in ["0", "-1", "Infinity", "NaN"] {
            let cfg = format!(
                r#"{{ signals: [{{ topic: "a", measurement: "m", field: "f", scale: {bad_scale} }}] }}"#
            );
            assert!(
                BridgeConfig::from_json5(&cfg).is_err(),
                "scale {bad_scale} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_signals_targeting_protected_live_buckets() {
        // An explicit bucket override onto a live bucket is rejected …
        for bucket in ["loxone", "solar", "weather_forecast", "ote_prices"] {
            let cfg = format!(
                r#"{{ signals: [{{ topic: "a", measurement: "m", field: "f", bucket: "{bucket}" }}] }}"#
            );
            assert!(
                BridgeConfig::from_json5(&cfg).is_err(),
                "bucket {bucket} should be rejected"
            );
        }
        // … as is inheriting one through the default `influx.bucket`.
        let inherited = r#"{ influx: { bucket: "loxone" }, signals: [{ topic: "a", measurement: "m", field: "f" }] }"#;
        assert!(BridgeConfig::from_json5(inherited).is_err());
        // A dedicated bucket is fine.
        let ok = r#"{ signals: [{ topic: "a", measurement: "m", field: "f", bucket: "ev" }] }"#;
        assert!(BridgeConfig::from_json5(ok).is_ok());
    }

    #[test]
    fn rejects_control_chars_in_line_protocol_names() {
        // `\n`/`\r` would be stripped by line_protocol and could collide two distinct names.
        let bad_meas =
            r#"{ signals: [{ topic: "t", measurement: "a\nb", field: "f", bucket: "ev" }] }"#;
        assert!(BridgeConfig::from_json5(bad_meas).is_err());
        let bad_tag = r#"{ signals: [{ topic: "t", measurement: "m", field: "f", bucket: "ev", tags: { car: "1\r2" } }] }"#;
        assert!(BridgeConfig::from_json5(bad_tag).is_err());
    }
}
