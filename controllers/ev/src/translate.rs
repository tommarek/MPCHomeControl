//! The pure translation from a protocol [`Payload::Load`](controller_protocol::Payload::Load) into a
//! Loxone UDP "virtual input" datagram — IO-free and unit-tested.
//!
//! Each charger contributes up to three virtual inputs the loxone wallbox logic reads: `<stem>_kw`
//! (the modulating power setpoint), `<stem>_on` (the relay/enable flag), and `<stem>_target` (the
//! target SoC %, when the plan supplies it). A modulating wallbox uses `_kw`; an on/off one uses
//! `_on`. The matching virtual inputs are added on the Loxone side (see docs/controllers.md).

use std::collections::HashMap;

use controller_protocol::{LoadChannel, PlannedAction};

/// Settings for building the datagram.
#[derive(Debug, Clone)]
pub struct TranslateCfg {
    /// Prefix for a charger's key stem when not overridden, e.g. `"mpc_ev_"` → `mpc_ev_garage_kw`.
    pub key_prefix: String,
    /// Optional per-charger stem overrides (channel name → exact Loxone stem, before the suffix).
    pub channel_map: HashMap<String, String>,
}

/// The Loxone key stem for a charger (an override, else `<prefix><channel>`).
fn channel_stem(channel: &str, cfg: &TranslateCfg) -> String {
    cfg.channel_map
        .get(channel)
        .cloned()
        .unwrap_or_else(|| format!("{}{}", cfg.key_prefix, channel))
}

/// Round any float to 3 decimals so the datagram is stable and free of float noise — used for both a
/// kW power (→ W resolution) and a SoC % target. A non-finite input (which the plan should never
/// produce) is coerced to 0 so we never emit `NaN`/`inf` into a Loxone virtual input.
fn round3(value: f64) -> f64 {
    if value.is_finite() {
        (value * 1000.0).round() / 1000.0
    } else {
        0.0
    }
}

/// Build the single UDP datagram for all channels (keys sorted for determinism, joined by `;`).
/// `target` is the Loxone Miniserver `host:port`. Returns `None` when there are no usable channels.
pub fn translate(
    channels: &[LoadChannel],
    cfg: &TranslateCfg,
    target: &str,
) -> Option<PlannedAction> {
    if channels.is_empty() {
        return None;
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut on_count = 0usize;
    for ch in channels {
        let stem = channel_stem(&ch.channel, cfg);
        // Skip a stem that would corrupt the `key=value;…` datagram.
        if stem.is_empty() || stem.contains([';', '=', '\n', '\r', '\0']) {
            continue;
        }
        if ch.enabled {
            on_count += 1;
        }
        pairs.push((format!("{stem}_kw"), format!("{}", round3(ch.power_kw))));
        pairs.push((format!("{stem}_on"), u8::from(ch.enabled).to_string()));
        if let Some(t) = ch.target_soc {
            pairs.push((format!("{stem}_target"), format!("{}", round3(t))));
        }
    }
    if pairs.is_empty() {
        return None;
    }
    pairs.sort();
    let n_channels = pairs.iter().filter(|(k, _)| k.ends_with("_on")).count();
    let message = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    Some(PlannedAction {
        target: format!("udp://{target}"),
        message,
        published: false, // the runtime flips this after an armed send
        reason: format!("{on_count}/{n_channels} charger(s) on"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranslateCfg {
        TranslateCfg {
            key_prefix: "mpc_ev_".to_string(),
            channel_map: HashMap::new(),
        }
    }

    fn ch(name: &str, power: f64, on: bool, target: Option<f64>) -> LoadChannel {
        LoadChannel {
            channel: name.to_string(),
            power_kw: power,
            enabled: on,
            target_c: None,
            target_soc: target,
        }
    }

    #[test]
    fn builds_sorted_datagram_with_power_on_and_target() {
        let a = translate(
            &[ch("garage", 3.6, true, Some(80.0))],
            &cfg(),
            "192.168.1.10:4000",
        )
        .unwrap();
        assert_eq!(a.target, "udp://192.168.1.10:4000");
        // sorted: _kw, _on, _target
        assert_eq!(
            a.message,
            "mpc_ev_garage_kw=3.6;mpc_ev_garage_on=1;mpc_ev_garage_target=80"
        );
        assert_eq!(a.reason, "1/1 charger(s) on");
        assert!(!a.published);
    }

    #[test]
    fn off_charger_emits_zero_and_no_target_when_absent() {
        let a = translate(&[ch("street", 0.0, false, None)], &cfg(), "h:1").unwrap();
        assert_eq!(a.message, "mpc_ev_street_kw=0;mpc_ev_street_on=0");
        assert_eq!(a.reason, "0/1 charger(s) on");
    }

    #[test]
    fn channel_map_overrides_the_stem() {
        let mut c = cfg();
        c.channel_map
            .insert("garage".to_string(), "VI_wallbox".to_string());
        let a = translate(&[ch("garage", 7.0, true, None)], &c, "h:1").unwrap();
        assert_eq!(a.message, "VI_wallbox_kw=7;VI_wallbox_on=1");
    }

    #[test]
    fn no_channels_is_none() {
        assert!(translate(&[], &cfg(), "h:1").is_none());
    }

    #[test]
    fn malformed_stems_are_dropped() {
        let mut c = cfg();
        c.channel_map
            .insert("bad".to_string(), "k;evil=1".to_string());
        let a = translate(
            &[ch("bad", 1.0, true, None), ch("garage", 2.0, true, None)],
            &c,
            "h:1",
        )
        .unwrap();
        assert_eq!(a.message, "mpc_ev_garage_kw=2;mpc_ev_garage_on=1");
        assert!(translate(&[ch("bad", 1.0, true, None)], &c, "h:1").is_none());
    }
}
