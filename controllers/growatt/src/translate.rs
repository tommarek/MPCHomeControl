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
    /// Battery max charge/discharge power (kW) at `powerrate=100%` — the reference for turning a kW
    /// setpoint into the integer `powerrate` percent. This is the **battery** power limit (loxone's
    /// `battery_charge_max_kw`, ~9.8 kW), NOT the inverter's AC rating.
    pub battery_power_max_kw: f64,
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

/// Turn a kW setpoint into the inverter's integer `powerrate` percent against `max_power_kw`
/// (battery max charge/discharge power = 100%), quantized to `step_pct`. A **nonzero** setpoint
/// floors at 1% (an active slot must request ≥1%, matching loxone's `max(1, …)`); 0 kW maps to 0.
pub fn powerrate_pct(kw: f64, max_power_kw: f64, step_pct: f64) -> u32 {
    if max_power_kw <= 0.0 || kw <= 0.0 {
        return 0;
    }
    let raw = (kw / max_power_kw * 100.0).clamp(0.0, 100.0);
    let step = step_pct.max(1.0);
    let pct = ((raw / step).round() * step).clamp(0.0, 100.0).round() as u32;
    pct.max(1)
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

/// Disable a mode's slot 1 (an empty, disabled window). Emitted for the **non-selected** modes so the
/// inverter is never left with two slots enabled — mirrors loxone's `ensure_exclusive`, which the
/// SPH inverter requires (with both battery-first and grid-first enabled its behaviour is undefined).
fn disable_slot(base: &str, mode: &str) -> PlannedAction {
    action(
        base,
        &format!("{mode}/set/timeslot"),
        json!({ "start": "00:00", "stop": "00:00", "enabled": false, "slot": 1 }),
        format!("exclusivity: disable {mode} slot"),
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
            json!({ "id": 0, "type": "16b", "registerType": "H", "value": 0 }),
            "inverter_off → modbus holding reg0=0",
        )];
    }

    let min_pct = soc_pct(b.min_soc_kwh, cfg.battery_capacity_kwh);
    let max_pct = soc_pct(b.max_soc_kwh, cfg.battery_capacity_kwh);
    let pct = |kw: f64| powerrate_pct(kw, cfg.battery_power_max_kw, cfg.powerrate_step_pct);

    let mut a = vec![action(
        base,
        "modbus/set",
        json!({ "id": 0, "type": "16b", "registerType": "H", "value": 1 }),
        "inverter on → modbus holding reg0=1",
    )];

    match b.slot {
        BatterySlot::Regular => {
            // Load-first: neither battery-first nor grid-first should be active.
            a.push(disable_slot(base, "batteryfirst"));
            a.push(disable_slot(base, "gridfirst"));
            a.push(action(
                base,
                "loadfirst/set/stopsoc",
                json!({ "value": min_pct }),
                format!("regular → load_first, stop-soc={min_pct}%"),
            ));
        }
        BatterySlot::ChargeFromGrid => {
            a.push(disable_slot(base, "gridfirst"));
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
                    cfg.battery_power_max_kw,
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
            a.push(disable_slot(base, "batteryfirst"));
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
            a.push(disable_slot(base, "batteryfirst"));
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
            a.push(disable_slot(base, "gridfirst"));
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

    // The orthogonal export gate, applied after the slot (inverter_off already returned). The gateway
    // expects `{"value": true}` on the edge-triggered enable/disable topics (loxone parity).
    a.push(if b.export_enabled {
        action(
            base,
            "export/enable",
            json!({ "value": true }),
            "export enabled",
        )
    } else {
        action(
            base,
            "export/disable",
            json!({ "value": true }),
            "export disabled",
        )
    });

    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranslateCfg {
        TranslateCfg {
            command_base: "energy/solar/command".into(),
            battery_power_max_kw: 9.8,
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
        assert_eq!(powerrate_pct(9.8, 9.8, 1.0), 100); // full
        assert_eq!(powerrate_pct(3.0, 9.8, 1.0), 31); // 30.6 → 31
        assert_eq!(powerrate_pct(99.0, 9.8, 1.0), 100); // clamp
        assert_eq!(powerrate_pct(0.0, 9.8, 1.0), 0); // zero stays zero
        assert_eq!(powerrate_pct(0.04, 9.8, 1.0), 1); // nonzero floors at 1% (loxone max(1,…))
        assert_eq!(powerrate_pct(2.45, 9.8, 5.0), 25); // 25% on step 5
    }

    #[test]
    fn inverter_off_short_circuits() {
        let a = translate(&payload(BatterySlot::InverterOff), &cfg(), &window(), None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].target, "energy/solar/command/modbus/set");
        assert_eq!(
            a[0].message,
            r#"{"id":0,"registerType":"H","type":"16b","value":0}"#
        );

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
            r#"{"value":31}"#
        ); // 3/9.8 = 30.6 → 31
        assert_eq!(
            find(&a, "/batteryfirst/set/acchargeenabled").message,
            r#"{"value":1}"#
        );
        // Exclusivity: the opposite (grid-first) slot is explicitly disabled.
        assert_eq!(
            find(&a, "/gridfirst/set/timeslot").message,
            r#"{"enabled":false,"slot":1,"start":"00:00","stop":"00:00"}"#
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
            r#"{"value":20}"#
        ); // 2/9.8 = 20.4 → 20
           // Exclusivity: the opposite (battery-first) slot is explicitly disabled.
        assert_eq!(
            find(&a, "/batteryfirst/set/timeslot").message,
            r#"{"enabled":false,"slot":1,"start":"00:00","stop":"00:00"}"#
        );
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
    fn regular_disables_both_slots_and_export_payload_carries_value() {
        let a = translate(&payload(BatterySlot::Regular), &cfg(), &window(), None);
        // Load-first: both battery-first and grid-first slots are explicitly disabled (exclusivity).
        let disabled = r#"{"enabled":false,"slot":1,"start":"00:00","stop":"00:00"}"#;
        assert_eq!(find(&a, "/batteryfirst/set/timeslot").message, disabled);
        assert_eq!(find(&a, "/gridfirst/set/timeslot").message, disabled);
        // Export enable carries the {"value":true} payload the gateway expects (not an empty body).
        assert_eq!(find(&a, "/export/enable").message, r#"{"value":true}"#);
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
