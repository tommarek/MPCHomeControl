//! `mpc-controller-growatt` — a **reference** controller (dry-run draft, never armed in practice:
//! loxone_smart_home owns the real inverter).
//!
//! Subscribes the north command topic, translates the battery intent into the Growatt MQTT command
//! vocabulary ([`translate`]), and — only when *both* the config `armed` flag and the
//! `MPC_CONTROLLER_ARM` env token are set — publishes it. Otherwise it logs the would-send messages.
//! A `valid_until` deadman reverts to `regular` (handing control back) if commands go silent.

mod config;
mod translate;

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use controller_protocol::{
    actions_changed, topics, BatteryPayload, BatterySlot, ControlCommand, ControllerStatus, Mode,
    Payload, PlannedAction, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde_json::json;

use crate::config::GrowattConfig;
use crate::translate::{translate, SlotWindow, TranslateCfg};

/// The exact env token required (alongside `armed: true`) before anything is sent to hardware.
const ARM_TOKEN: &str = "i-understand-this-actuates";

/// Hardware actuation needs BOTH the config flag and the env token — neither alone is enough.
fn resolve_armed(cfg: &GrowattConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// The inverter slot window (local `HH:MM`) covering a command's 15-minute block.
fn slot_window(block_start: DateTime<Utc>, offset_hours: i32) -> SlotWindow {
    let local = block_start + ChronoDuration::hours(offset_hours as i64);
    let stop = local + ChronoDuration::minutes(15);
    SlotWindow {
        start: local.format("%H:%M").to_string(),
        stop: stop.format("%H:%M").to_string(),
    }
}

struct State {
    cfg: GrowattConfig,
    tcfg: TranslateCfg,
    client: AsyncClient,
    armed: bool,
    last_seq: Option<u64>,
    last_actions: Vec<PlannedAction>,
    last_command_at: Option<DateTime<Utc>>,
    soc_pct: Option<f64>,
    reverted: bool,
    valid_until: Option<DateTime<Utc>>,
}

impl State {
    async fn on_command(&mut self, bytes: &[u8]) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[growatt] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, Utc::now()) {
            println!("[growatt] ignoring command: {why}");
            return;
        }
        let Payload::Battery(battery) = &cmd.payload else {
            println!("[growatt] ignoring non-battery payload");
            return;
        };
        let window = slot_window(cmd.block_start, self.cfg.utc_offset_hours);
        let actions = translate(battery, &self.tcfg, &window, self.soc_pct);

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(Utc::now());
        self.valid_until = Some(cmd.valid_until);
        self.reverted = false;

        if !actions_changed(&self.last_actions, &actions) {
            println!(
                "[growatt] command seq {} unchanged — skipping re-publish",
                cmd.command_seq
            );
            return;
        }
        let ctx = format!("command seq {} ({:?})", cmd.command_seq, battery.slot);
        self.apply(actions, &ctx).await;
    }

    async fn apply(&mut self, mut actions: Vec<PlannedAction>, ctx: &str) {
        println!(
            "[growatt] {ctx} — {} action(s) [{}]:",
            actions.len(),
            if self.armed { "ARMED" } else { "dry-run" }
        );
        for act in actions.iter_mut() {
            if self.armed {
                match self
                    .client
                    .publish(
                        &act.target,
                        QoS::AtLeastOnce,
                        false,
                        act.message.clone().into_bytes(),
                    )
                    .await
                {
                    Ok(()) => act.published = true,
                    Err(e) => eprintln!("[growatt] publish {} failed: {e}", act.target),
                }
                tokio::time::sleep(Duration::from_secs(1)).await; // >=1 s between Growatt commands
            }
            println!(
                "    {} {} {}  ({})",
                if act.published {
                    "PUBLISHED"
                } else {
                    "would-send"
                },
                act.target,
                act.message,
                act.reason
            );
        }
        self.last_actions = actions.clone();
        self.publish_status(actions).await;
    }

    async fn check_deadman(&mut self) {
        if self.reverted {
            return;
        }
        let Some(vu) = self.valid_until else { return };
        if Utc::now() < vu {
            return;
        }
        self.reverted = true;
        println!(
            "[growatt] DEADMAN expired (valid_until {vu}) → failsafe '{}'",
            self.cfg.failsafe
        );
        if self.cfg.failsafe == "revert_to_regular" {
            let regular = BatteryPayload {
                slot: BatterySlot::Regular,
                export_enabled: true,
                inverter_on: true,
                charge_kw: 0.0,
                discharge_kw: 0.0,
                min_soc_kwh: self.cfg.battery_capacity_kwh * 0.2,
                max_soc_kwh: self.cfg.battery_capacity_kwh,
                soc_kwh: None,
            };
            let window = slot_window(Utc::now(), self.cfg.utc_offset_hours);
            let actions = translate(&regular, &self.tcfg, &window, self.soc_pct);
            self.apply(actions, "failsafe revert_to_regular").await;
        }
        // "hold" → issue nothing; the inverter keeps its last mode / loxone resumes.
    }

    fn on_telemetry(&mut self, bytes: &[u8]) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
            if let Some(soc) = v.get("SOC").and_then(|x| x.as_f64()) {
                self.soc_pct = Some(soc);
            }
        }
    }

    async fn publish_status(&self, actions: Vec<PlannedAction>) {
        let status = ControllerStatus {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: self.cfg.controller_id.clone(),
            at: Utc::now(),
            mode: if self.armed {
                Mode::Armed
            } else {
                Mode::DryRun
            },
            last_command_at: self.last_command_at,
            deadman_expired: self.reverted,
            telemetry: json!({ "soc_pct": self.soc_pct }),
            actions,
        };
        if let Ok(json) = serde_json::to_string(&status) {
            let _ = self
                .client
                .publish(
                    topics::status(&self.cfg.controller_id),
                    QoS::AtLeastOnce,
                    false,
                    json.into_bytes(),
                )
                .await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "growatt.json5".to_string());
    let cfg = GrowattConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    if armed {
        println!(
            "*** mpc-controller-growatt ARMED — WILL PUBLISH to {} on mqtt://{}:{} ***",
            cfg.command_base, cfg.mqtt.host, cfg.mqtt.port
        );
    } else if cfg.armed {
        println!("--- mpc-controller-growatt: config armed but MPC_CONTROLLER_ARM token absent → DRY-RUN ---");
    } else {
        println!(
            "--- mpc-controller-growatt DRY-RUN — logging only, the inverter is untouched ---"
        );
    }

    let mut opts = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    let health = topics::health(&cfg.controller_id);
    opts.set_last_will(LastWill::new(
        health.clone(),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    client
        .subscribe(&cfg.control_topic, QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(&cfg.telemetry_topic, QoS::AtMostOnce)
        .await?;
    client
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;

    let control_topic = cfg.control_topic.clone();
    let telemetry_topic = cfg.telemetry_topic.clone();
    println!("[growatt] listening on {control_topic} (telemetry {telemetry_topic})");
    let tcfg = cfg.translate_cfg();
    let mut state = State {
        cfg,
        tcfg,
        client,
        armed,
        last_seq: None,
        last_actions: Vec::new(),
        last_command_at: None,
        soc_pct: None,
        reverted: false,
        valid_until: None,
    };

    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                // Re-subscribe on every (re)connect — rumqttc does not replay subscriptions after a
                // reconnect, so without this a single broker blip silently stops command delivery.
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    let id = state.cfg.controller_id.clone();
                    let _ = state.client.subscribe(&control_topic, QoS::AtLeastOnce).await;
                    let _ = state.client.subscribe(&telemetry_topic, QoS::AtMostOnce).await;
                    let _ = state
                        .client
                        .publish(topics::health(&id), QoS::AtLeastOnce, true, "online")
                        .await;
                    println!("[growatt] (re)connected, subscribed to {control_topic}");
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic == control_topic {
                        state.on_command(&p.payload).await;
                    } else if p.topic == telemetry_topic {
                        state.on_telemetry(&p.payload);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[growatt] mqtt connection: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn slot_window_is_local_block() {
        let w = slot_window(utc("2026-06-23T10:00:00Z"), 2);
        assert_eq!(w.start, "12:00");
        assert_eq!(w.stop, "12:15");
    }
}
