//! `mpc-controller-ev` — drives the EV-charging path: the MPC's per-charger plan becomes Loxone UDP
//! virtual-input datagrams for the wallbox.
//!
//! Subscribes the north command topic, translates the `Payload::Load` channels into one
//! `key=value;…` UDP datagram ([`translate`]), and — only with *both* the config `armed` flag and the
//! `MPC_CONTROLLER_ARM` env token — sends it to the Loxone Miniserver. Otherwise it logs the
//! would-send datagram. A `valid_until` deadman either holds (loxone resumes) or drives all chargers
//! off. The publisher only ever sends channels for chargers controllable on our wallbox, so a
//! monitored / away car never reaches here.

mod config;
mod translate;

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use controller_common::UdpClient;
use controller_protocol::{
    actions_changed, topics, ControlCommand, ControllerStatus, LoadChannel, Mode, Payload,
    PlannedAction, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};

use crate::config::EvControllerConfig;
use crate::translate::{translate, TranslateCfg};

/// The exact env token required (alongside `armed: true`) before any datagram is sent.
const ARM_TOKEN: &str = "i-understand-this-actuates";

fn resolve_armed(cfg: &EvControllerConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

struct State {
    cfg: EvControllerConfig,
    tcfg: TranslateCfg,
    target: String,
    client: AsyncClient,
    armed: bool,
    sender: Option<UdpClient>,
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
                eprintln!("[ev] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, Utc::now()) {
            println!("[ev] ignoring command: {why}");
            return;
        }
        let Payload::Load { channels } = &cmd.payload else {
            println!("[ev] ignoring non-load payload");
            return;
        };

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(Utc::now());
        self.valid_until = Some(cmd.valid_until);
        self.deadman_at = Some(controller_common::monotonic_deadline(cmd.valid_until));
        self.reverted = false;
        self.last_channels = channels.clone();

        let actions: Vec<PlannedAction> = translate(channels, &self.tcfg, &self.target)
            .into_iter()
            .collect();
        if !actions_changed(&self.last_actions, &actions) {
            println!(
                "[ev] command seq {} unchanged — skipping re-send",
                cmd.command_seq
            );
            return;
        }
        let ctx = format!("command seq {}", cmd.command_seq);
        self.apply(actions, &ctx).await;
    }

    async fn apply(&mut self, mut actions: Vec<PlannedAction>, ctx: &str) {
        println!(
            "[ev] {ctx} — {} datagram(s) [{}]:",
            actions.len(),
            if self.armed { "ARMED" } else { "dry-run" }
        );
        for act in actions.iter_mut() {
            if self.armed {
                if let Some(sender) = &self.sender {
                    match sender.send(&act.message) {
                        Ok(()) => act.published = true,
                        Err(e) => eprintln!("[ev] UDP send to {} failed: {e}", self.target),
                    }
                }
            }
            println!(
                "    {} {} {}  ({})",
                if act.published { "SENT" } else { "would-send" },
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
        let Some(deadman) = self.deadman_at else {
            return;
        };
        if Instant::now() < deadman {
            return;
        }
        self.reverted = true;
        println!(
            "[ev] DEADMAN expired (valid_until {:?}) → failsafe '{}'",
            self.valid_until, self.cfg.failsafe
        );
        if self.cfg.failsafe == "all_off" {
            // Drive every last-known charger off; loxone's own logic takes over from there.
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
            let actions: Vec<PlannedAction> = translate(&off, &self.tcfg, &self.target)
                .into_iter()
                .collect();
            self.apply(actions, "failsafe all_off").await;
        }
        // "hold" → send nothing; loxone's native wallbox control resumes.
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
            telemetry: serde_json::Value::Null, // wallbox state isn't read back on this path
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
        .unwrap_or_else(|| "ev.json5".to_string());
    let cfg = EvControllerConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    let target = cfg.loxone_target();
    if armed {
        println!("*** mpc-controller-ev ARMED — WILL SEND UDP to {target} ***");
    } else if cfg.armed {
        println!(
            "--- mpc-controller-ev: config armed but MPC_CONTROLLER_ARM token absent → DRY-RUN ---"
        );
    } else {
        println!("--- mpc-controller-ev DRY-RUN — logging only, the wallbox is untouched ---");
    }

    let sender = if armed {
        Some(UdpClient::bind(target.clone())?)
    } else {
        None
    };

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
    println!("[ev] listening on {control_topic} → UDP {target}");
    let tcfg = cfg.translate_cfg();
    let mut state = State {
        cfg,
        tcfg,
        target,
        client,
        armed,
        sender,
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
                    println!("[ev] (re)connected, subscribed to {control_topic}");
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic == control_topic {
                        state.on_command(&p.payload).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[ev] mqtt connection: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
        }
    }
}
