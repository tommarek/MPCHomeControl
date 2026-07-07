//! The pure mapping from the MPC plan to per-controller [`ControlCommand`]s — IO-free and unit-tested.

use chrono::{DateTime, Duration, Utc};
use controller_protocol::{
    BatteryPayload, BatterySlot, ControlCommand, LoadChannel, LoxoneWrite, Payload, ZoneHeat,
    SCHEMA_VERSION,
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

/// A battery command whose plan block starts this far in the past is refused. Unlike the heating /
/// loxone writes (which describe "state now"), a battery command programs an explicit inverter
/// `slot_window` at the block's local HH:MM — re-issuing one for a long-past block would leave a
/// stale timeslot armed in the inverter. One block (900 s) + generous solve/poll/skew slack.
const MAX_BLOCK_AGE_SECONDS: i64 = 1200;

/// True when the plan's first block is too old to safely program the battery timeslot.
fn battery_block_stale(block_start: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - block_start).num_seconds() > MAX_BLOCK_AGE_SECONDS
}

/// Build the commands for the configured controllers from one plan poll. `seq` is the producer's
/// monotonic counter; `now` is the publish instant (the deadman is `now + deadman_seconds`).
pub fn commands(
    api: &LatestResponse,
    cfg: &PublisherConfig,
    seq: u64,
    now: DateTime<Utc>,
) -> Vec<(String, ControlCommand)> {
    let fs = &api.data.first_step;
    let valid_until = now + Duration::seconds(cfg.deadman_seconds.max(0));
    let plan_id = api.computed_at.to_rfc3339();

    let envelope = |controller_id: &str, payload: Payload| ControlCommand {
        schema_version: SCHEMA_VERSION.to_string(),
        controller_id: controller_id.to_string(),
        issued_at: api.computed_at,
        block_start: fs.hour_start,
        valid_until,
        plan_id: plan_id.clone(),
        command_seq: seq,
        payload,
    };

    let mut out = Vec::new();

    if let Some(b) = &cfg.battery {
        if battery_block_stale(fs.hour_start, now) {
            eprintln!(
                "[publisher] battery block_start {} is >{}s old — skipping the battery command \
                 (stale timeslot); the controller will deadman-revert",
                fs.hour_start, MAX_BLOCK_AGE_SECONDS
            );
        } else {
            let soc_kwh = api.data.timeline.first().map(|t| t.soc_kwh);
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
        // One channel per charger controllable on our wallbox right now that is scheduled (a
        // non-empty plan) OR has known SoC — the latter gets an explicit **0 kW** when its plan is
        // empty (target reached / nothing scheduled), because loxone virtual inputs hold their last
        // value and an omitted write would keep the wallbox charging at the previous setpoint
        // indefinitely. Omission is reserved for the SoC-unknown/untracked case, leaving loxone's
        // own control in place rather than forcing it to 0 kW.
        let mut channels: Vec<LoadChannel> = api
            .data
            .ev
            .iter()
            .filter(|c| c.controllable_now && (!c.charge_kw.is_empty() || c.soc_pct.is_some()))
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

    if let Some(b) = &cfg.boiler {
        // One channel per controllable load, with the coming block's planned draw as the setpoint and
        // an `enabled` flag from the on-threshold (the load-shift on/off decision). A generic
        // `Payload::Load`, like the EV path — the boiler controller reads it.
        let mut channels: Vec<LoadChannel> = fs
            .controllable_load_kw
            .iter()
            .map(|(name, &power_kw)| LoadChannel {
                channel: name.clone(),
                power_kw,
                enabled: power_kw > b.on_threshold_kw,
                target_c: None,
                target_soc: None,
            })
            .collect();
        channels.sort_by(|a, b| a.channel.cmp(&b.channel)); // deterministic order
        out.push((
            b.controller_id.clone(),
            envelope(&b.controller_id, Payload::Load { channels }),
        ));
    }

    if let Some(lx) = &cfg.loxone {
        // The unified Loxone datagram: map each wired plan field to its exact virtual-input key. The
        // controller is a generic writer, so adding a domain is a config row here, not a code change.
        let mut writes: Vec<LoxoneWrite> = Vec::new();
        if let Some(h) = &lx.heating {
            for (zone, &power_kw) in &fs.heat_kw {
                if let Some(key) = h.zone_keys.get(zone) {
                    writes.push(LoxoneWrite {
                        key: key.clone(),
                        value: f64::from(power_kw > h.on_threshold_kw), // relay 1/0
                    });
                }
            }
        }
        if let Some(e) = &lx.ev {
            // The controllable charger that's scheduled OR has known SoC; first-block power. A
            // known-SoC charger with an empty plan writes an explicit **0** — the loxone VI holds
            // its last value, so omitting the write would keep charging at the stale setpoint past
            // the target. Omission is reserved for the SoC-unknown/untracked case.
            if let Some(c) =
                api.data.ev.iter().find(|c| {
                    c.controllable_now && (!c.charge_kw.is_empty() || c.soc_pct.is_some())
                })
            {
                writes.push(LoxoneWrite {
                    key: e.power_key.clone(),
                    value: c.charge_kw.first().copied().unwrap_or(0.0),
                });
            }
        }
        writes.sort_by(|a, b| a.key.cmp(&b.key)); // deterministic order
        out.push((
            lx.controller_id.clone(),
            envelope(&lx.controller_id, Payload::Loxone { writes }),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BatteryPub, BoilerPub, EvPub, HeatingPub, LoxoneEvMap, LoxoneHeatingMap, LoxonePub,
        MqttConfig, PublisherConfig,
    };
    use std::collections::HashMap;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn api_json() -> LatestResponse {
        // A realistic /api/plan/latest envelope (extra fields present to prove they're ignored).
        let json = r#"{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {
                "total_cost_eur": 1.23,
                "first_step": {
                    "hour_start": "2026-06-23T12:00:00Z",
                    "heat_kw": { "livingroom": 2.4, "office": 0.0 },
                    "cool_kw": {},
                    "controllable_load_kw": { "water heat-pump": 2.0 },
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
        }"#;
        serde_json::from_str(json).unwrap()
    }

    fn cfg() -> PublisherConfig {
        PublisherConfig {
            mpc_url: "http://x/api/plan/latest".into(),
            poll_seconds: 30,
            deadman_seconds: 120,
            max_plan_age_seconds: 900,
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
            boiler: None,
            loxone: None,
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
    fn builds_boiler_load_command_from_controllable_loads() {
        let mut c = cfg();
        c.boiler = Some(BoilerPub {
            controller_id: "boiler".into(),
            on_threshold_kw: 0.05,
        });
        let cmds = commands(&api_json(), &c, 5, utc("2026-06-23T12:00:05Z"));
        let boiler = &cmds.iter().find(|(id, _)| id == "boiler").unwrap().1;
        match &boiler.payload {
            Payload::Load { channels } => {
                assert_eq!(channels.len(), 1);
                assert_eq!(channels[0].channel, "water heat-pump");
                assert_eq!(channels[0].power_kw, 2.0); // first block's planned draw
                assert!(channels[0].enabled); // 2.0 > 0.05
                assert_eq!(channels[0].target_soc, None);
            }
            _ => panic!("expected a load payload"),
        }
    }

    #[test]
    fn builds_unified_loxone_command_from_heating_and_ev() {
        let mut c = cfg();
        c.heating = None; // the loxone block supersedes the per-domain heating/ev blocks
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([
                    ("livingroom".to_string(), "MPCHeatObyvak".to_string()),
                    ("office".to_string(), "MPCHeatPracovna".to_string()),
                    // a zone with no key is simply not written
                ]),
            }),
            ev: Some(LoxoneEvMap {
                power_key: "EvChargePower".into(),
            }),
        });
        let cmds = commands(&api_json(), &c, 9, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                // Sorted by key: EvChargePower=3.6 (garage first block), MPCHeatObyvak=1
                // (livingroom 2.4 > 0.05), MPCHeatPracovna=0 (office 0.0).
                assert_eq!(writes.len(), 3);
                assert_eq!(writes[0].key, "EvChargePower");
                assert_eq!(writes[0].value, 3.6);
                assert_eq!(writes[1].key, "MPCHeatObyvak");
                assert_eq!(writes[1].value, 1.0);
                assert_eq!(writes[2].key, "MPCHeatPracovna");
                assert_eq!(writes[2].value, 0.0);
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    #[test]
    fn rejects_loxone_alongside_heating_or_ev() {
        let mut c = cfg(); // heating: Some, ev: None, loxone: None → valid
        assert!(c.validate().is_ok());
        // Adding the unified loxone block while heating is still configured is contradictory.
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: None,
            ev: None,
        });
        assert!(
            c.validate().is_err(),
            "loxone + heating/ev must be rejected (double-actuation)"
        );
        // Loxone alone is fine.
        c.heating = None;
        assert!(c.validate().is_ok());
        // Loxone + ev is also rejected.
        c.ev = Some(EvPub {
            controller_id: "ev".into(),
            on_threshold_kw: 0.05,
        });
        assert!(c.validate().is_err(), "loxone + ev must be rejected");
    }

    #[test]
    fn rejects_malformed_or_colliding_loxone_keys() {
        let base = |keys: Vec<(&str, &str)>, ev: Option<&str>| {
            let mut c = cfg();
            c.heating = None;
            c.loxone = Some(LoxonePub {
                controller_id: "loxone".into(),
                heating: Some(LoxoneHeatingMap {
                    on_threshold_kw: 0.05,
                    zone_keys: keys
                        .into_iter()
                        .map(|(z, k)| (z.to_string(), k.to_string()))
                        .collect(),
                }),
                ev: ev.map(|p| LoxoneEvMap {
                    power_key: p.to_string(),
                }),
            });
            c
        };
        // a delimiter in a zone key would be silently dropped by translate
        assert!(base(vec![("livingroom", "MPC;bad")], None)
            .validate()
            .is_err());
        // two zones mapped to the same virtual input collide in the datagram
        assert!(base(
            vec![("livingroom", "MPCHeatX"), ("office", "MPCHeatX")],
            None
        )
        .validate()
        .is_err());
        // an empty ev power_key would vanish
        assert!(base(vec![("livingroom", "MPCHeatObyvak")], Some(""))
            .validate()
            .is_err());
        // a clean config passes
        assert!(
            base(vec![("livingroom", "MPCHeatObyvak")], Some("EvChargePower"))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn loxone_omits_zones_without_a_key() {
        let mut c = cfg();
        c.heating = None;
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            // only livingroom is mapped; office (also in the plan's heat_kw) is intentionally absent
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([("livingroom".to_string(), "MPCHeatObyvak".to_string())]),
            }),
            ev: None,
        });
        let cmds = commands(&api_json(), &c, 1, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(writes.len(), 1);
                assert_eq!(writes[0].key, "MPCHeatObyvak");
            }
            _ => panic!("expected a loxone payload"),
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

    #[test]
    fn battery_command_skipped_when_block_start_is_stale() {
        // Plan block starts 12:00. At +1100 s the battery command still builds; at +1300 s it is
        // refused (a stale inverter timeslot) while the heating command is unaffected.
        let fresh = commands(&api_json(), &cfg(), 7, utc("2026-06-23T12:18:20Z")); // +1100 s
        assert!(fresh.iter().any(|(id, _)| id == "growatt"));
        let stale = commands(&api_json(), &cfg(), 7, utc("2026-06-23T12:21:40Z")); // +1300 s
        assert!(
            !stale.iter().any(|(id, _)| id == "growatt"),
            "battery command must be skipped for a >1200 s old block"
        );
        assert!(
            stale.iter().any(|(id, _)| id == "heating"),
            "heating (state-now) must still be emitted"
        );
    }

    /// The EV write matrix: scheduled → setpoint; done-but-tracked → explicit 0; SoC-unknown → omitted.
    fn ev_api(chargers: &str) -> LatestResponse {
        let json = format!(
            r#"{{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {{
                "first_step": {{
                    "hour_start": "2026-06-23T12:00:00Z",
                    "heat_kw": {{}},
                    "controllable_load_kw": {{}},
                    "mode": {{ "slot": "regular", "export_enabled": true, "inverter_on": true,
                              "charge_kw": 0.0, "discharge_kw": 0.0 }}
                }},
                "timeline": [],
                "ev": [{chargers}]
            }}
        }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn ev_write_matrix_zero_vs_omit() {
        let mut c = cfg();
        c.battery = None;
        c.heating = None;
        c.ev = Some(EvPub {
            controller_id: "ev".into(),
            on_threshold_kw: 0.05,
        });
        // Scheduled charger → first-block setpoint.
        let scheduled = r#"{ "name": "a", "controllable_now": true, "charge_kw": [3.6], "target_pct": 80.0, "soc_pct": 55.0 }"#;
        // Target reached (empty plan) but SoC known → explicit 0 kW, disabled.
        let done = r#"{ "name": "b", "controllable_now": true, "charge_kw": [], "target_pct": 80.0, "soc_pct": 80.0 }"#;
        // Controllable but SoC unknown (empty plan, no soc) → omitted, native control stays.
        let unknown =
            r#"{ "name": "c", "controllable_now": true, "charge_kw": [], "target_pct": 80.0 }"#;
        let api = ev_api(&format!("{scheduled},{done},{unknown}"));
        let cmds = commands(&api, &c, 1, utc("2026-06-23T12:00:05Z"));
        let ev = &cmds.iter().find(|(id, _)| id == "ev").unwrap().1;
        match &ev.payload {
            Payload::Load { channels } => {
                assert_eq!(channels.len(), 2, "SoC-unknown charger must be omitted");
                assert_eq!(
                    (channels[0].channel.as_str(), channels[0].power_kw),
                    ("a", 3.6)
                );
                assert!(channels[0].enabled);
                assert_eq!(
                    (channels[1].channel.as_str(), channels[1].power_kw),
                    ("b", 0.0),
                    "done-but-tracked charger must get an explicit 0"
                );
                assert!(!channels[1].enabled);
            }
            _ => panic!("expected a load payload"),
        }

        // Same matrix through the unified loxone path: the first match is the scheduled charger;
        // with only the done charger present, an explicit 0 write is emitted.
        c.ev = None;
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: None,
            ev: Some(LoxoneEvMap {
                power_key: "EvChargePower".into(),
            }),
        });
        let done_only = ev_api(done);
        let cmds = commands(&done_only, &c, 2, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(writes.len(), 1);
                assert_eq!(
                    (writes[0].key.as_str(), writes[0].value),
                    ("EvChargePower", 0.0)
                );
            }
            _ => panic!("expected a loxone payload"),
        }
        let unknown_only = ev_api(unknown);
        let cmds = commands(&unknown_only, &c, 3, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert!(writes.is_empty(), "SoC-unknown charger must be omitted");
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    #[test]
    fn validate_rejects_bad_cadence_ids_and_soc_band() {
        // deadman <= poll oscillates into failsafe every cycle.
        let mut c = cfg();
        c.deadman_seconds = 30;
        assert!(c.validate().is_err(), "deadman <= poll must be rejected");
        // max_plan_age below 3x poll gates fresh plans as stale.
        let mut c = cfg();
        c.max_plan_age_seconds = 60;
        assert!(
            c.validate().is_err(),
            "max_plan_age < 3x poll must be rejected"
        );
        // Duplicate controller ids race on one topic.
        let mut c = cfg();
        c.ev = Some(EvPub {
            controller_id: "growatt".into(), // collides with the battery block
            on_threshold_kw: 0.05,
        });
        assert!(
            c.validate().is_err(),
            "duplicate controller_id must be rejected"
        );
        // Inverted SoC band.
        let mut c = cfg();
        c.battery = Some(BatteryPub {
            controller_id: "growatt".into(),
            min_soc_kwh: 11.0,
            max_soc_kwh: 10.0,
        });
        assert!(c.validate().is_err(), "min_soc > max_soc must be rejected");
    }
}
