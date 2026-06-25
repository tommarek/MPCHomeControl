//! The pure mapping from the MPC plan to per-controller [`ControlCommand`]s — IO-free and unit-tested.

use chrono::{DateTime, Duration, Utc};
use controller_protocol::{
    BatteryPayload, BatterySlot, ControlCommand, LoadChannel, Payload, ZoneHeat, SCHEMA_VERSION,
};

use crate::config::PublisherConfig;
use crate::plan::LatestResponse;

/// Parse the plan's `slot` string into the protocol enum. Unknown strings (and `"regular"`) map to
/// the safe self-consumption default.
pub fn parse_slot(slot: &str) -> BatterySlot {
    match slot {
        "charge_from_grid" => BatterySlot::ChargeFromGrid,
        "discharge_to_grid" => BatterySlot::DischargeToGrid,
        "sell_production" => BatterySlot::SellProduction,
        "battery_hold" => BatterySlot::BatteryHold,
        "inverter_off" => BatterySlot::InverterOff,
        _ => BatterySlot::Regular,
    }
}

/// Build the commands for the configured controllers from one plan poll. `seq` is the producer's
/// monotonic counter; `now` is the publish instant (the deadman is `now + deadman_seconds`).
pub fn commands(
    api: &LatestResponse,
    cfg: &PublisherConfig,
    seq: u64,
    now: DateTime<Utc>,
) -> Vec<(String, ControlCommand)> {
    let fs = &api.data.plan.first_step;
    let valid_until = now + Duration::seconds(cfg.deadman_seconds.max(0));
    let plan_id = api.data.computed_at.to_rfc3339();

    let envelope = |controller_id: &str, payload: Payload| ControlCommand {
        schema_version: SCHEMA_VERSION.to_string(),
        controller_id: controller_id.to_string(),
        issued_at: api.data.computed_at,
        block_start: fs.hour_start,
        valid_until,
        plan_id: plan_id.clone(),
        command_seq: seq,
        payload,
    };

    let mut out = Vec::new();

    if let Some(b) = &cfg.battery {
        let soc_kwh = api.data.plan.timeline.first().map(|t| t.soc_kwh);
        let payload = Payload::Battery(BatteryPayload {
            slot: parse_slot(&fs.mode.slot),
            export_enabled: fs.mode.export_enabled,
            inverter_on: fs.mode.inverter_on,
            charge_kw: fs.mode.charge_kw,
            discharge_kw: fs.mode.discharge_kw,
            min_soc_kwh: b.min_soc_kwh,
            max_soc_kwh: b.max_soc_kwh,
            soc_kwh,
        });
        out.push((b.controller_id.clone(), envelope(&b.controller_id, payload)));
    }

    if let Some(h) = &cfg.heating {
        let mut zones: Vec<ZoneHeat> = fs
            .heat_kw
            .iter()
            .map(|(zone, &power_kw)| ZoneHeat {
                zone: zone.clone(),
                power_kw,
                on: power_kw > h.on_threshold_kw,
            })
            .collect();
        zones.sort_by(|a, b| a.zone.cmp(&b.zone)); // deterministic order
        out.push((
            h.controller_id.clone(),
            envelope(&h.controller_id, Payload::Heating { zones }),
        ));
    }

    if let Some(e) = &cfg.ev {
        // One channel per charger controllable on our wallbox right now AND actually scheduled (a
        // non-empty plan). Monitored / away chargers — and a controllable charger the MPC couldn't
        // schedule (no SoC) so its plan is empty — carry no command, leaving loxone's own control in
        // place rather than forcing it to 0 kW. The first block's planned power is the setpoint.
        let mut channels: Vec<LoadChannel> = api
            .data
            .plan
            .ev
            .iter()
            .filter(|c| c.controllable_now && !c.charge_kw.is_empty())
            .map(|c| {
                let power_kw = c.charge_kw.first().copied().unwrap_or(0.0);
                LoadChannel {
                    channel: c.name.clone(),
                    power_kw,
                    enabled: power_kw > e.on_threshold_kw,
                    target_c: None,
                    target_soc: Some(c.target_pct),
                }
            })
            .collect();
        channels.sort_by(|a, b| a.channel.cmp(&b.channel)); // deterministic order
        out.push((
            e.controller_id.clone(),
            envelope(&e.controller_id, Payload::Load { channels }),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BatteryPub, EvPub, HeatingPub, MqttConfig, PublisherConfig};

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn api_json() -> LatestResponse {
        // A realistic /api/plan/latest envelope (extra fields present to prove they're ignored).
        let json = r#"{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {
                "computed_at": "2026-06-23T12:00:00Z",
                "plan": {
                    "total_cost_eur": 1.23,
                    "first_step": {
                        "hour_start": "2026-06-23T12:00:00Z",
                        "heat_kw": { "livingroom": 2.4, "office": 0.0 },
                        "cool_kw": {},
                        "battery_charge_kw": 3.0,
                        "battery_discharge_kw": 0.0,
                        "grid_import_kw": 3.0,
                        "grid_export_kw": 0.0,
                        "mode": {
                            "slot": "charge_from_grid",
                            "export_enabled": false,
                            "inverter_on": true,
                            "charge_kw": 3.0,
                            "discharge_kw": 0.0
                        }
                    },
                    "timeline": [ { "soc_kwh": 6.1, "slot": "charge_from_grid" } ],
                    "ev": [
                        { "name": "garage", "controllable_now": true, "charge_kw": [3.6, 0.0], "target_pct": 80.0 },
                        { "name": "street", "controllable_now": false, "charge_kw": [0.0], "target_pct": 90.0 }
                    ]
                }
            }
        }"#;
        serde_json::from_str(json).unwrap()
    }

    fn cfg() -> PublisherConfig {
        PublisherConfig {
            mpc_url: "http://x/api/plan/latest".into(),
            poll_seconds: 30,
            deadman_seconds: 120,
            armed: false,
            mqtt: MqttConfig::default(),
            battery: Some(BatteryPub {
                controller_id: "growatt".into(),
                min_soc_kwh: 2.0,
                max_soc_kwh: 10.0,
            }),
            heating: Some(HeatingPub {
                controller_id: "heating".into(),
                on_threshold_kw: 0.05,
            }),
            ev: None,
        }
    }

    #[test]
    fn builds_battery_and_heating_commands() {
        let now = utc("2026-06-23T12:00:05Z");
        let cmds = commands(&api_json(), &cfg(), 7, now);
        assert_eq!(cmds.len(), 2);

        let battery = &cmds.iter().find(|(id, _)| id == "growatt").unwrap().1;
        assert_eq!(battery.command_seq, 7);
        assert_eq!(battery.plan_id, "2026-06-23T12:00:00+00:00");
        assert_eq!(battery.valid_until, utc("2026-06-23T12:02:05Z")); // now + 120 s
        match &battery.payload {
            Payload::Battery(b) => {
                assert_eq!(b.slot, BatterySlot::ChargeFromGrid);
                assert_eq!(b.charge_kw, 3.0);
                assert!(!b.export_enabled && b.inverter_on);
                assert_eq!(b.min_soc_kwh, 2.0);
                assert_eq!(b.soc_kwh, Some(6.1)); // from timeline[0]
            }
            _ => panic!("expected a battery payload"),
        }

        let heating = &cmds.iter().find(|(id, _)| id == "heating").unwrap().1;
        match &heating.payload {
            Payload::Heating { zones } => {
                assert_eq!(zones.len(), 2);
                // Sorted; livingroom is on (2.4 > 0.05), office off (0.0).
                assert_eq!(zones[0].zone, "livingroom");
                assert!(zones[0].on);
                assert_eq!(zones[1].zone, "office");
                assert!(!zones[1].on);
            }
            _ => panic!("expected a heating payload"),
        }
    }

    #[test]
    fn builds_ev_load_command_for_controllable_chargers_only() {
        let mut c = cfg();
        c.ev = Some(EvPub {
            controller_id: "ev".into(),
            on_threshold_kw: 0.05,
        });
        let cmds = commands(&api_json(), &c, 3, utc("2026-06-23T12:00:05Z"));
        let ev = &cmds.iter().find(|(id, _)| id == "ev").unwrap().1;
        match &ev.payload {
            Payload::Load { channels } => {
                // The away "street" charger is filtered out; only the controllable "garage" remains.
                assert_eq!(channels.len(), 1);
                assert_eq!(channels[0].channel, "garage");
                assert_eq!(channels[0].power_kw, 3.6); // first block's planned power
                assert!(channels[0].enabled); // 3.6 > 0.05
                assert_eq!(channels[0].target_soc, Some(80.0));
            }
            _ => panic!("expected a load payload"),
        }
    }

    #[test]
    fn omits_a_controller_when_unconfigured() {
        let mut c = cfg();
        c.heating = None;
        let cmds = commands(&api_json(), &c, 1, utc("2026-06-23T12:00:05Z"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, "growatt");
    }

    #[test]
    fn slot_parsing_defaults_to_regular() {
        assert_eq!(parse_slot("regular"), BatterySlot::Regular);
        assert_eq!(parse_slot("inverter_off"), BatterySlot::InverterOff);
        assert_eq!(parse_slot("nonsense"), BatterySlot::Regular);
    }
}
