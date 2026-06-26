//! `mpc-controller-boiler` — the controllable-load (boiler / hot-water tank) path: the MPC's
//! per-load on/off plan becomes a device command.
//!
//! **Stub / dry-run draft.** Subscribes the north command topic, translates the `Payload::Load`
//! channels into a logged would-send record ([`translate`]), and prints it. No real device protocol
//! is wired yet (the Modbus boiler hasn't arrived), so it never actuates — even the `armed` path only
//! logs, until [`translate`] grows a real datagram. A `valid_until` deadman either holds (the existing
//! system resumes) or drives all loads off. The publisher only sends channels for configured
//! controllable loads, so nothing else reaches here.

mod config;
mod translate;

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use controller_protocol::{
    actions_changed, topics, ControlCommand, ControllerStatus, LoadChannel, Mode, Payload,
    PlannedAction, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};

use crate::config::BoilerControllerConfig;
use crate::translate::{translate, TranslateCfg};

/// The exact env token that *would* be required (alongside `armed: true`) before any real send. The
/// translate path is a stub today, so this only governs the would-be-armed annotation in the log.
const ARM_TOKEN: &str = "i-understand-this-actuates";

fn resolve_armed(cfg: &BoilerControllerConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

struct State {
    cfg: BoilerControllerConfig,
    tcfg: TranslateCfg,
    client: AsyncClient,
    armed: bool,
    last_seq: Option<u64>,
    last_actions: Vec<PlannedAction>,
    last_channels: Vec<LoadChannel>,
    last_command_at: Option<DateTime<Utc>>,
    reverted: bool,
    valid_until: Option<DateTime<Utc>>,
    /// Monotonic deadline for the deadman — immune to wall-clock steps.
    deadman_at: Option<Instant>,
}

impl State {
    async fn on_command(&mut self, bytes: &[u8]) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[boiler] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, Utc::now()) {
            println!("[boiler] ignoring command: {why}");
            return;
        }
        let Payload::Load { channels } = &cmd.payload else {
            println!("[boiler] ignoring non-load payload");
            return;
        };

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(Utc::now());
        self.valid_until = Some(cmd.valid_until);
        self.deadman_at = Some(controller_common::monotonic_deadline(cmd.valid_until));
        self.reverted = false;
        self.last_channels = channels.clone();

        let actions: Vec<PlannedAction> = translate(channels, &self.tcfg).into_iter().collect();
        if !actions_changed(&self.last_actions, &actions) {
            println!(
                "[boiler] command seq {} unchanged — skipping re-log",
                cmd.command_seq
            );
            return;
        }
        let ctx = format!("command seq {}", cmd.command_seq);
        self.apply(actions, &ctx).await;
    }

    async fn apply(&mut self, actions: Vec<PlannedAction>, ctx: &str) {
        println!(
            "[boiler] {ctx} — {} record(s) [{}, stub: no device protocol yet]:",
            actions.len(),
            if self.armed { "would-arm" } else { "dry-run" }
        );
        for act in &actions {
            // The stub never sends, so `published` stays false in both modes.
            println!(
                "    would-send {} {}  ({})",
                act.target, act.message, act.reason
            );
        }
        self.last_actions = actions.clone();
        self.publish_status(actions).await;
    }

    async fn check_deadman(&mut self) {
        if self.reverted {
            return;
        }
        let Some(deadman) = self.deadman_at else {
            return;
        };
        if Instant::now() < deadman {
            return;
        }
        self.reverted = true;
        println!(
            "[boiler] DEADMAN expired (valid_until {:?}) → failsafe '{}'",
            self.valid_until, self.cfg.failsafe
        );
        if self.cfg.failsafe == "all_off" {
            // Drive every last-known load off; the existing system takes over from there.
            let off: Vec<LoadChannel> = self
                .last_channels
                .iter()
                .map(|c| LoadChannel {
                    channel: c.channel.clone(),
                    power_kw: 0.0,
                    enabled: false,
                    target_c: None,
                    target_soc: c.target_soc,
                })
                .collect();
            let actions: Vec<PlannedAction> = translate(&off, &self.tcfg).into_iter().collect();
            self.apply(actions, "failsafe all_off").await;
        }
        // "hold" → log nothing; the existing boiler control resumes.
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
            telemetry: serde_json::Value::Null, // the boiler isn't read back on this stub path
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
        .unwrap_or_else(|| "boiler.json5".to_string());
    let cfg = BoilerControllerConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    println!(
        "--- mpc-controller-boiler {} — STUB: logging would-send records only, no device is driven ---",
        if armed { "(config armed)" } else { "DRY-RUN" }
    );

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
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;

    let control_topic = cfg.control_topic.clone();
    println!("[boiler] listening on {control_topic} (stub)");
    let tcfg = cfg.translate_cfg();
    let mut state = State {
        cfg,
        tcfg,
        client,
        armed,
        last_seq: None,
        last_actions: Vec::new(),
        last_channels: Vec::new(),
        last_command_at: None,
        reverted: false,
        valid_until: None,
        deadman_at: None,
    };

    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                // rumqttc doesn't replay subscriptions after a reconnect — re-subscribe on every ConnAck.
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    let id = state.cfg.controller_id.clone();
                    let _ = state.client.subscribe(&control_topic, QoS::AtLeastOnce).await;
                    let _ = state
                        .client
                        .publish(topics::health(&id), QoS::AtLeastOnce, true, "online")
                        .await;
                    println!("[boiler] (re)connected, subscribed to {control_topic}");
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic == control_topic {
                        state.on_command(&p.payload).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[boiler] mqtt connection: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
        }
    }
}
