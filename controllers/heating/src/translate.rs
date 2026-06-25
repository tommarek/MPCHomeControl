//! The pure translation from a protocol heating payload into a Loxone UDP "virtual input" datagram —
//! IO-free and unit-tested.
//!
//! Loxone ingests `key=value;key=value` UDP datagrams as virtual inputs (the same format it already
//! uses for sensors). This controller sends one datagram per command carrying every zone's desired
//! relay state (`<key>=1` on, `<key>=0` off). The matching virtual inputs + relay logic are added on
//! the Loxone side (see docs/controllers.md) — this is a brand-new control path.

use std::collections::HashMap;

use controller_protocol::{PlannedAction, ZoneHeat};

/// Settings for building the datagram.
#[derive(Debug, Clone)]
pub struct TranslateCfg {
    /// Prefix for a zone's virtual-input key when not overridden, e.g. `"mpc_heat_"` → `mpc_heat_livingroom`.
    pub key_prefix: String,
    /// Optional per-zone key overrides (zone name → exact Loxone virtual-input name).
    pub zone_map: HashMap<String, String>,
}

/// The Loxone virtual-input key for a zone (an override, else `<prefix><zone>`).
fn zone_key(zone: &str, cfg: &TranslateCfg) -> String {
    cfg.zone_map
        .get(zone)
        .cloned()
        .unwrap_or_else(|| format!("{}{}", cfg.key_prefix, zone))
}

/// Build the single UDP datagram for all zones (keys sorted for determinism, joined by `;`). `target`
/// is the Loxone Miniserver `host:port`. Returns `None` when there are no zones.
pub fn translate(zones: &[ZoneHeat], cfg: &TranslateCfg, target: &str) -> Option<PlannedAction> {
    if zones.is_empty() {
        return None;
    }
    // Skip a key that would corrupt the `key=value;…` datagram.
    let mut pairs: Vec<(String, u8)> = zones
        .iter()
        .map(|z| (zone_key(&z.zone, cfg), u8::from(z.on)))
        .filter(|(k, _)| !k.is_empty() && !k.contains(';') && !k.contains('='))
        .collect();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort();
    let on_count = pairs.iter().filter(|(_, v)| *v == 1).count();
    let message = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    Some(PlannedAction {
        target: format!("udp://{target}"),
        message,
        published: false, // the runtime flips this after an armed send
        reason: format!("{on_count}/{} zone(s) on", pairs.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranslateCfg {
        TranslateCfg {
            key_prefix: "mpc_heat_".to_string(),
            zone_map: HashMap::new(),
        }
    }

    fn zone(name: &str, on: bool) -> ZoneHeat {
        ZoneHeat {
            zone: name.to_string(),
            power_kw: if on { 2.0 } else { 0.0 },
            on,
        }
    }

    #[test]
    fn builds_sorted_datagram() {
        let a = translate(
            &[zone("livingroom", true), zone("kitchen", false)],
            &cfg(),
            "192.168.1.10:4000",
        )
        .unwrap();
        assert_eq!(a.target, "udp://192.168.1.10:4000");
        assert_eq!(a.message, "mpc_heat_kitchen=0;mpc_heat_livingroom=1"); // sorted by key
        assert_eq!(a.reason, "1/2 zone(s) on");
        assert!(!a.published);
    }

    #[test]
    fn zone_map_overrides_the_key() {
        let mut c = cfg();
        c.zone_map
            .insert("livingroom".to_string(), "VI_obyvak_heat".to_string());
        let a = translate(&[zone("livingroom", true)], &c, "h:1").unwrap();
        assert_eq!(a.message, "VI_obyvak_heat=1");
    }

    #[test]
    fn no_zones_is_none() {
        assert!(translate(&[], &cfg(), "h:1").is_none());
    }

    #[test]
    fn keys_with_delimiters_are_dropped() {
        let mut c = cfg();
        c.zone_map.insert("bad".to_string(), "k;evil=1".to_string());
        // The malformed key is dropped; only the clean zone survives.
        let a = translate(&[zone("bad", true), zone("livingroom", true)], &c, "h:1").unwrap();
        assert_eq!(a.message, "mpc_heat_livingroom=1");
        // If every key is malformed, there's nothing to send.
        assert!(translate(&[zone("bad", true)], &c, "h:1").is_none());
    }
}
