//! The pure translation from a protocol [`Payload::Load`](controller_protocol::Payload::Load) into a
//! device command — IO-free and unit-tested.
//!
//! **Stub.** The real boiler hardware (a Modbus relay / smart socket) isn't wired yet, so this does
//! not speak any device protocol. It produces a single logged **would-send** [`PlannedAction`] that
//! records the per-channel on/off setpoint the controller *would* drive — the audit record the
//! runtime prints in dry-run. When the Modbus boiler arrives, replace the `target`/`message` here
//! with the real datagram (the controller scaffold around it stays the same).

use controller_protocol::{LoadChannel, PlannedAction};

/// Settings for building the (stub) command.
#[derive(Debug, Clone)]
pub struct TranslateCfg {
    /// A label for the device target in the would-send record (e.g. `"boiler-modbus"`). No real
    /// endpoint is contacted yet — it only annotates the audit log.
    pub target_label: String,
}

/// Round a kW power to 3 decimals (→ W resolution) for a stable, noise-free record. A non-finite
/// input (which the plan should never produce) is coerced to 0.
fn round3(value: f64) -> f64 {
    if value.is_finite() {
        (value * 1000.0).round() / 1000.0
    } else {
        0.0
    }
}

/// Build the single (stub) would-send action for all controllable-load channels — keys sorted for a
/// deterministic record, joined by `;`. Returns `None` when there are no valid channels (the list is
/// empty, or every name was dropped as malformed). **No hardware is touched**; `published` stays
/// `false` and the runtime only logs it.
pub fn translate(channels: &[LoadChannel], cfg: &TranslateCfg) -> Option<PlannedAction> {
    if channels.is_empty() {
        return None;
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut on_count = 0usize;
    for ch in channels {
        // Skip a channel name that would corrupt the `key=value;…` record.
        if ch.channel.is_empty() || ch.channel.contains([';', '=', '\n', '\r']) {
            continue;
        }
        if ch.enabled {
            on_count += 1;
        }
        pairs.push((
            format!("{}_kw", ch.channel),
            format!("{}", round3(ch.power_kw)),
        ));
        pairs.push((
            format!("{}_on", ch.channel),
            u8::from(ch.enabled).to_string(),
        ));
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
        target: format!("stub://{}", cfg.target_label),
        message,
        published: false, // stub: nothing is ever actually sent
        reason: format!("{on_count}/{n_channels} load(s) on (stub — no device protocol yet)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranslateCfg {
        TranslateCfg {
            target_label: "boiler-modbus".to_string(),
        }
    }

    fn ch(name: &str, power: f64, on: bool) -> LoadChannel {
        LoadChannel {
            channel: name.to_string(),
            power_kw: power,
            enabled: on,
            target_c: None,
            target_soc: None,
        }
    }

    #[test]
    fn builds_sorted_stub_record_with_power_and_on() {
        let a = translate(&[ch("boiler", 2.0, true)], &cfg()).unwrap();
        assert_eq!(a.target, "stub://boiler-modbus");
        // sorted: _kw before _on
        assert_eq!(a.message, "boiler_kw=2;boiler_on=1");
        assert_eq!(a.reason, "1/1 load(s) on (stub — no device protocol yet)");
        assert!(!a.published, "the stub never actually sends");
    }

    #[test]
    fn off_load_emits_zero() {
        let a = translate(&[ch("boiler", 0.0, false)], &cfg()).unwrap();
        assert_eq!(a.message, "boiler_kw=0;boiler_on=0");
        assert_eq!(a.reason, "0/1 load(s) on (stub — no device protocol yet)");
    }

    #[test]
    fn no_channels_is_none() {
        assert!(translate(&[], &cfg()).is_none());
    }

    #[test]
    fn malformed_channel_names_are_dropped() {
        let a = translate(
            &[ch("k;evil=1", 1.0, true), ch("boiler", 2.0, true)],
            &cfg(),
        )
        .unwrap();
        assert_eq!(a.message, "boiler_kw=2;boiler_on=1");
        assert!(translate(&[ch("k;evil=1", 1.0, true)], &cfg()).is_none());
    }
}
