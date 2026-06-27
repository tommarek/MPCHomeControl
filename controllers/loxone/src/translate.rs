//! Pure translation of generic Loxone virtual-input writes into one UDP datagram — IO-free and
//! unit-tested.
//!
//! Loxone ingests `key=value;key=value` UDP datagrams as virtual inputs (the same format it uses for
//! sensors). This controller is deliberately generic: it writes whatever `(key, value)` pairs the
//! publisher hands it, so a new Loxone-driven actuation never needs a controller change — only a
//! publisher mapping row. The keys themselves (`MPCHeatChodbaDole`, `EvChargePower`, …) and the
//! `MPCActive` gate are owned upstream.

use controller_protocol::{LoxoneWrite, PlannedAction};

/// Format a value for a Loxone virtual input: crisp integers (relay `1`/`0`, enum codes) with no
/// trailing `.0`, otherwise up to 3 decimals (W resolution for a kW setpoint) with trailing zeros
/// trimmed. A non-finite value (which the plan should never produce) becomes `0`.
fn fmt_value(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    if v.fract() == 0.0 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Build the single UDP datagram for all writes (keys sorted for determinism, joined by `;`).
/// `target` is the Loxone Miniserver `host:port`. A key containing a delimiter (`;`/`=`/newline) is
/// dropped so it can't corrupt the datagram. Returns `None` when there are no valid writes.
pub fn translate(writes: &[LoxoneWrite], target: &str) -> Option<PlannedAction> {
    let mut pairs: Vec<(String, String)> = writes
        .iter()
        .filter(|w| !w.key.is_empty() && !w.key.contains([';', '=', '\n', '\r', '\0']))
        .map(|w| (w.key.clone(), fmt_value(w.value)))
        .collect();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort();
    let message = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    Some(PlannedAction {
        target: format!("udp://{target}"),
        message,
        published: false, // the runtime flips this after an armed send
        reason: format!("{} write(s)", pairs.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(key: &str, value: f64) -> LoxoneWrite {
        LoxoneWrite {
            key: key.to_string(),
            value,
        }
    }

    #[test]
    fn builds_sorted_datagram_with_clean_values() {
        let a = translate(
            &[
                w("MPCActive", 1.0),
                w("EvChargePower", 3.6),
                w("MPCHeatChodbaDole", 1.0),
            ],
            "192.168.0.200:4000",
        )
        .unwrap();
        assert_eq!(a.target, "udp://192.168.0.200:4000");
        // sorted by key; 1.0 → "1", 3.6 → "3.6"
        assert_eq!(
            a.message,
            "EvChargePower=3.6;MPCActive=1;MPCHeatChodbaDole=1"
        );
        assert_eq!(a.reason, "3 write(s)");
        assert!(!a.published);
    }

    #[test]
    fn formats_values_crisply() {
        assert_eq!(fmt_value(1.0), "1");
        assert_eq!(fmt_value(0.0), "0");
        assert_eq!(fmt_value(3.6), "3.6");
        assert_eq!(fmt_value(2.5), "2.5");
        assert_eq!(fmt_value(12.34), "12.34");
        assert_eq!(fmt_value(-1.5), "-1.5");
        assert_eq!(fmt_value(f64::NAN), "0");
        assert_eq!(fmt_value(f64::INFINITY), "0");
    }

    #[test]
    fn drops_keys_with_delimiters_and_empty() {
        let a = translate(
            &[
                w("k;evil=1", 1.0),
                w("bad\nkey", 1.0),
                w("nul\0key", 1.0),
                w("", 1.0),
                w("EvChargePower", 2.0),
            ],
            "h:1",
        )
        .unwrap();
        assert_eq!(a.message, "EvChargePower=2");
        // Every key malformed / no writes → nothing to send.
        assert!(translate(&[w("k;evil", 1.0)], "h:1").is_none());
        assert!(translate(&[], "h:1").is_none());
    }
}
