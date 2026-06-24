//! The pure translation from a protocol [`BatteryPayload`] into the Growatt MQTT command sequence —
//! IO-free and exhaustively unit-tested. This is the ground-truth mapping; the runtime only logs it
//! (dry-run) or publishes it (armed).
//!
//! It mirrors the `loxone_smart_home` Growatt vocabulary: the 6 `slot` modes map to the inverter's
//! load-first / battery-first / grid-first modes via the `energy/solar/command/...` topics, plus the
//! orthogonal export gate and the modbus inverter on/off.

use controller_protocol::{BatteryPayload, BatterySlot, PlannedAction};
use serde_json::json;

/// The settings the translation needs (the per-block time window is supplied separately).
#[derive(Debug, Clone)]
pub struct TranslateCfg {
    /// MQTT topic prefix for commands, e.g. `"energy/solar/command"`.
    pub command_base: String,
    /// Inverter AC rating (kW) used to turn a kW setpoint into the integer `powerrate` percent.
    pub inverter_kw_rating: f64,
    /// `powerrate` quantization step (percent); the inverter takes integer percent.
    pub powerrate_step_pct: f64,
    /// Battery usable capacity (kWh) used to turn a kWh SoC into a percent stop-SoC.
    pub battery_capacity_kwh: f64,
}

/// The wall-clock window (local `HH:MM`) the inverter slot 1 should cover this block.
#[derive(Debug, Clone)]
pub struct SlotWindow {
    pub start: String,
    pub stop: String,
}

/// Turn a kW setpoint into the inverter's integer `powerrate` percent (clamped 0..100, quantized).
pub fn powerrate_pct(kw: f64, rating_kw: f64, step_pct: f64) -> u32 {
    if rating_kw <= 0.0 {
        return 0;
    }
    let raw = (kw / rating_kw * 100.0).clamp(0.0, 100.0);
    let step = step_pct.max(1.0);
    ((raw / step).round() * step).clamp(0.0, 100.0).round() as u32
}

/// kWh → integer stop-SoC percent (clamped 0..100).
fn soc_pct(kwh: f64, capacity_kwh: f64) -> u32 {
    if capacity_kwh <= 0.0 {
        return 0;
    }
    (kwh / capacity_kwh * 100.0).clamp(0.0, 100.0).round() as u32
}

fn action(
    base: &str,
    sub: &str,
    payload: serde_json::Value,
    reason: impl Into<String>,
) -> PlannedAction {
    PlannedAction {
        target: format!("{base}/{sub}"),
        message: payload.to_string(),
        published: false, // the runtime flips this to true after an armed send
        reason: reason.into(),
    }
}

fn timeslot(
    base: &str,
    mode: &str,
    window: &SlotWindow,
    reason: impl Into<String>,
) -> PlannedAction {
    action(
        base,
        &format!("{mode}/set/timeslot"),
        json!({ "start": window.start, "stop": window.stop, "enabled": true, "slot": 1 }),
        reason,
    )
}

/// Translate a battery command into the ordered Growatt MQTT messages. `telemetry_soc_pct` is the
/// controller's freshest live SoC (preferred over the command's `soc_kwh` for `battery_hold`).
pub fn translate(
    b: &BatteryPayload,
    cfg: &TranslateCfg,
    window: &SlotWindow,
    telemetry_soc_pct: Option<f64>,
) -> Vec<PlannedAction> {
    let base = &cfg.command_base;

    // Inverter off short-circuits: power it down and issue nothing else.
    if !b.inverter_on || matches!(b.slot, BatterySlot::InverterOff) {
        return vec![action(
            base,
            "modbus/set",
            json!({ "register": 0, "value": 0 }),
            "inverter_off → modbus reg0=0",
        )];
    }

    let min_pct = soc_pct(b.min_soc_kwh, cfg.battery_capacity_kwh);
    let max_pct = soc_pct(b.max_soc_kwh, cfg.battery_capacity_kwh);
    let pct = |kw: f64| powerrate_pct(kw, cfg.inverter_kw_rating, cfg.powerrate_step_pct);

    let mut a = vec![action(
        base,
        "modbus/set",
        json!({ "register": 0, "value": 1 }),
        "inverter on → modbus reg0=1",
    )];

    match b.slot {
        BatterySlot::Regular => {
            a.push(action(
                base,
                "loadfirst/set/stopsoc",
                json!({ "value": min_pct }),
                format!("regular → load_first, stop-soc={min_pct}%"),
            ));
        }
        BatterySlot::ChargeFromGrid => {
            a.push(timeslot(
                base,
                "batteryfirst",
                window,
                "charge_from_grid → battery_first slot",
            ));
            a.push(action(
                base,
                "batteryfirst/set/stopsoc",
                json!({ "value": max_pct }),
                format!("charge to max stop-soc={max_pct}%"),
            ));
            a.push(action(
                base,
                "batteryfirst/set/powerrate",
                json!({ "value": pct(b.charge_kw) }),
                format!(
                    "charge {:.2}kW/{:.2}kW = {}%",
                    b.charge_kw,
                    cfg.inverter_kw_rating,
                    pct(b.charge_kw)
                ),
            ));
            a.push(action(
                base,
                "batteryfirst/set/acchargeenabled",
                json!({ "value": 1 }),
                "AC charge enabled",
            ));
        }
        BatterySlot::DischargeToGrid => {
            a.push(timeslot(
                base,
                "gridfirst",
                window,
                "discharge_to_grid → grid_first slot",
            ));
            a.push(action(
                base,
                "gridfirst/set/stopsoc",
                json!({ "value": min_pct }),
                format!("discharge floor stop-soc={min_pct}%"),
            ));
            a.push(action(
                base,
                "gridfirst/set/powerrate",
                json!({ "value": pct(b.discharge_kw) }),
                format!(
                    "discharge {:.2}kW = {}%",
                    b.discharge_kw,
                    pct(b.discharge_kw)
                ),
            ));
        }
        BatterySlot::SellProduction => {
            a.push(timeslot(
                base,
                "gridfirst",
                window,
                "sell_production → grid_first slot",
            ));
            a.push(action(
                base,
                "gridfirst/set/stopsoc",
                json!({ "value": 100 }),
                "sell PV, keep battery → stop-soc=100%",
            ));
            a.push(action(
                base,
                "gridfirst/set/powerrate",
                json!({ "value": pct(b.discharge_kw) }),
                format!(
                    "export rate {:.2}kW = {}%",
                    b.discharge_kw,
                    pct(b.discharge_kw)
                ),
            ));
        }
        BatterySlot::BatteryHold => {
            // Pin stop-SoC to the live SoC so the battery neither charges nor drains. Prefer the
            // controller's own (fresher) telemetry SoC; fall back to the command's `soc_kwh`, then min.
            let hold_pct = telemetry_soc_pct
                .map(|p| p.clamp(0.0, 100.0).round() as u32)
                .or_else(|| b.soc_kwh.map(|k| soc_pct(k, cfg.battery_capacity_kwh)))
                .unwrap_or(min_pct);
            a.push(timeslot(
                base,
                "batteryfirst",
                window,
                "battery_hold → battery_first slot",
            ));
            a.push(action(
                base,
                "batteryfirst/set/stopsoc",
                json!({ "value": hold_pct }),
                format!("hold: pin stop-soc to live SoC={hold_pct}%"),
            ));
            a.push(action(
                base,
                "batteryfirst/set/acchargeenabled",
                json!({ "value": 0 }),
                "no AC charge while holding",
            ));
        }
        BatterySlot::InverterOff => unreachable!("handled by the short-circuit above"),
    }

    // The orthogonal export gate, applied after the slot (inverter_off already returned).
    a.push(if b.export_enabled {
        action(base, "export/enable", json!({}), "export enabled")
    } else {
        action(base, "export/disable", json!({}), "export disabled")
    });

    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranslateCfg {
        TranslateCfg {
            command_base: "energy/solar/command".into(),
            inverter_kw_rating: 5.3,
            powerrate_step_pct: 1.0,
            battery_capacity_kwh: 10.0,
        }
    }

    fn window() -> SlotWindow {
        SlotWindow {
            start: "12:00".into(),
            stop: "12:15".into(),
        }
    }

    fn payload(slot: BatterySlot) -> BatteryPayload {
        BatteryPayload {
            slot,
            export_enabled: true,
            inverter_on: true,
            charge_kw: 3.0,
            discharge_kw: 2.0,
            min_soc_kwh: 2.0,
            max_soc_kwh: 10.0,
            soc_kwh: Some(6.0),
        }
    }

    /// Find the action whose target ends with `suffix`, asserting it exists.
    fn find<'a>(actions: &'a [PlannedAction], suffix: &str) -> &'a PlannedAction {
        actions
            .iter()
            .find(|a| a.target.ends_with(suffix))
            .unwrap_or_else(|| panic!("no action ending {suffix} in {actions:#?}"))
    }

    #[test]
    fn powerrate_quantizes_and_clamps() {
        assert_eq!(powerrate_pct(3.0, 5.3, 1.0), 57); // 56.6 → 57
        assert_eq!(powerrate_pct(0.1, 5.3, 1.0), 2); // 1.9 → 2
        assert_eq!(powerrate_pct(99.0, 5.3, 1.0), 100); // clamp
        assert_eq!(powerrate_pct(0.0, 5.3, 1.0), 0);
        assert_eq!(powerrate_pct(2.65, 5.3, 5.0), 50); // step 5
    }

    #[test]
    fn inverter_off_short_circuits() {
        let a = translate(&payload(BatterySlot::InverterOff), &cfg(), &window(), None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].target, "energy/solar/command/modbus/set");
        assert_eq!(a[0].message, r#"{"register":0,"value":0}"#);

        // inverter_on=false reaches the same short-circuit regardless of slot.
        let mut p = payload(BatterySlot::Regular);
        p.inverter_on = false;
        let a = translate(&p, &cfg(), &window(), None);
        assert_eq!(a.len(), 1);
        assert!(a[0].message.contains(r#""value":0"#));
    }

    #[test]
    fn charge_from_grid_maps_to_battery_first_with_ac_charge() {
        let a = translate(
            &payload(BatterySlot::ChargeFromGrid),
            &cfg(),
            &window(),
            None,
        );
        assert!(find(&a, "/modbus/set").message.contains(r#""value":1"#)); // inverter on
        assert!(find(&a, "/batteryfirst/set/timeslot")
            .message
            .contains(r#""slot":1"#));
        assert_eq!(
            find(&a, "/batteryfirst/set/stopsoc").message,
            r#"{"value":100}"#
        ); // max 10/10
        assert_eq!(
            find(&a, "/batteryfirst/set/powerrate").message,
            r#"{"value":57}"#
        ); // 3/5.3
        assert_eq!(
            find(&a, "/batteryfirst/set/acchargeenabled").message,
            r#"{"value":1}"#
        );
        assert!(find(&a, "/export/enable").target.ends_with("export/enable"));
    }

    #[test]
    fn discharge_to_grid_maps_to_grid_first() {
        let a = translate(
            &payload(BatterySlot::DischargeToGrid),
            &cfg(),
            &window(),
            None,
        );
        assert!(find(&a, "/gridfirst/set/timeslot")
            .message
            .contains("12:15"));
        assert_eq!(
            find(&a, "/gridfirst/set/stopsoc").message,
            r#"{"value":20}"#
        ); // min 2/10
        assert_eq!(
            find(&a, "/gridfirst/set/powerrate").message,
            r#"{"value":38}"#
        ); // 2/5.3=37.7→38
    }

    #[test]
    fn sell_production_keeps_battery_with_stopsoc_100() {
        let a = translate(
            &payload(BatterySlot::SellProduction),
            &cfg(),
            &window(),
            None,
        );
        assert_eq!(
            find(&a, "/gridfirst/set/stopsoc").message,
            r#"{"value":100}"#
        );
    }

    #[test]
    fn battery_hold_pins_stopsoc_to_live_soc() {
        // Prefer the controller's telemetry SoC (61%) over the command's soc_kwh (6.0/10 = 60%).
        let a = translate(
            &payload(BatterySlot::BatteryHold),
            &cfg(),
            &window(),
            Some(61.0),
        );
        assert_eq!(
            find(&a, "/batteryfirst/set/stopsoc").message,
            r#"{"value":61}"#
        );
        assert_eq!(
            find(&a, "/batteryfirst/set/acchargeenabled").message,
            r#"{"value":0}"#
        );

        // Without telemetry, fall back to the command's soc_kwh.
        let a = translate(&payload(BatterySlot::BatteryHold), &cfg(), &window(), None);
        assert_eq!(
            find(&a, "/batteryfirst/set/stopsoc").message,
            r#"{"value":60}"#
        );
    }

    #[test]
    fn export_toggle_is_orthogonal() {
        let mut p = payload(BatterySlot::Regular);
        p.export_enabled = false;
        let a = translate(&p, &cfg(), &window(), None);
        assert!(find(&a, "/export/disable")
            .target
            .ends_with("export/disable"));
        assert!(!a.iter().any(|x| x.target.ends_with("export/enable")));
    }

    #[test]
    fn nothing_is_marked_published_by_translation() {
        // Translation is pure; the runtime sets `published` only after an armed send.
        for slot in [
            BatterySlot::Regular,
            BatterySlot::ChargeFromGrid,
            BatterySlot::DischargeToGrid,
            BatterySlot::SellProduction,
            BatterySlot::BatteryHold,
            BatterySlot::InverterOff,
        ] {
            let a = translate(&payload(slot), &cfg(), &window(), None);
            assert!(a.iter().all(|x| !x.published), "{slot:?} marked published");
        }
    }

    #[test]
    fn powerrate_step_edge_cases() {
        // On a step boundary after rounding (2.65/5.3 = 50%, step 5).
        assert_eq!(powerrate_pct(2.65, 5.3, 5.0), 50);
        // Low end with a larger step.
        assert_eq!(powerrate_pct(0.265, 5.3, 5.0), 5);
        // step=0 is treated as 1 (the .max(1.0) guard).
        assert_eq!(powerrate_pct(2.65, 5.3, 0.0), 50);
        // Fractional step.
        assert_eq!(powerrate_pct(1.325, 5.3, 0.5), 25);
        // kw exceeding the rating clamps to 100.
        assert_eq!(powerrate_pct(10.0, 5.3, 1.0), 100);
    }
}
