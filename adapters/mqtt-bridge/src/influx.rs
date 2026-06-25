//! The InfluxDB write target for the bridge: line-protocol encoding + a blocking `ureq` POST to the
//! v2 `/api/v2/write` endpoint. Writing is gated by `armed` — in dry-run the writer encodes the line
//! but performs no network call.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Utc;

use crate::config::InfluxConfig;

pub struct InfluxWriter {
    cfg: InfluxConfig,
    token: Option<String>,
    armed: bool,
}

impl InfluxWriter {
    pub fn new(cfg: InfluxConfig, token: Option<String>, armed: bool) -> Self {
        Self { cfg, token, armed }
    }

    pub fn default_bucket(&self) -> &str {
        &self.cfg.bucket
    }

    /// Write one line of protocol to `bucket` (or the default). Returns `Ok(true)` when a point was
    /// actually sent, `Ok(false)` in dry-run, and `Err` on a transport / non-2xx response.
    pub fn write(&self, bucket: Option<&str>, line: &str) -> Result<bool> {
        if !self.armed {
            return Ok(false);
        }
        let bucket = bucket.unwrap_or(&self.cfg.bucket);
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow!("no write token"))?;
        let url = format!(
            "{}/api/v2/write?org={}&bucket={}&precision=ms",
            self.cfg.url.trim_end_matches('/'),
            urlencode(&self.cfg.org),
            urlencode(bucket),
        );
        let resp = ureq::post(&url)
            .timeout(Duration::from_secs(10))
            .set("Authorization", &format!("Token {token}"))
            .set("Content-Type", "text/plain; charset=utf-8")
            .send_string(line);
        match resp {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(code, r)) => {
                // Cap the echoed body so a verbose error can't flood the log.
                let body: String = r
                    .into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                Err(anyhow!("influx {code}: {body}"))
            }
            Err(e) => Err(anyhow!("{e}")),
        }
    }
}

/// Encode one point as InfluxDB line protocol with a millisecond timestamp. Returns `None` for a
/// non-finite value: line protocol has no NaN/inf field, so the point is dropped here (in both debug
/// and release builds) rather than emitted as `field=nan`/`inf` that influx would reject. This is the
/// single finitude guard — the caller logs and skips on `None`.
pub fn line_protocol(
    measurement: &str,
    tags: &BTreeMap<String, String>,
    field: &str,
    value: f64,
) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let mut head = escape_meas(measurement);
    for (k, v) in tags {
        head.push(',');
        head.push_str(&escape_key(k));
        head.push('=');
        head.push_str(&escape_key(v));
    }
    let ts = Utc::now().timestamp_millis();
    Some(format!("{head} {}={} {ts}", escape_key(field), value))
}

fn escape_meas(s: &str) -> String {
    // Backslash first (so it doesn't double-escape the `\` we insert below); then strip CR/LF and
    // escape the line-protocol specials.
    s.replace('\\', "\\\\")
        .replace(['\n', '\r'], "")
        .replace(',', "\\,")
        .replace(' ', "\\ ")
}

fn escape_key(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(['\n', '\r'], "")
        .replace(',', "\\,")
        .replace('=', "\\=")
        .replace(' ', "\\ ")
}

/// Minimal percent-encoding for the org/bucket query params (names are simple, but be safe).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_line_with_tags() {
        let mut tags = BTreeMap::new();
        tags.insert("car".to_string(), "1".to_string());
        let line = line_protocol("teslamate", &tags, "battery_level", 80.0).unwrap();
        assert!(line.starts_with("teslamate,car=1 battery_level=80 "));
        // a trailing millisecond timestamp
        assert!(line.rsplit(' ').next().unwrap().parse::<i64>().unwrap() > 0);
        // a non-finite value is dropped (no NaN/inf line)
        assert!(line_protocol("m", &tags, "f", f64::NAN).is_none());
        assert!(line_protocol("m", &tags, "f", f64::INFINITY).is_none());
    }

    #[test]
    fn escapes_spaces_and_commas() {
        let mut tags = BTreeMap::new();
        tags.insert("room name".to_string(), "a,b".to_string());
        let line = line_protocol("my meas", &tags, "v", 1.0).unwrap();
        assert!(line.starts_with("my\\ meas,room\\ name=a\\,b v=1 "));
    }
}
