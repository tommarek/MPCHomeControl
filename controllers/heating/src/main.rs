//! `mpc-controller-heating` — drives a brand-new heating path: per-zone commands become Loxone UDP
//! virtual-input datagrams.
//!
//! Subscribes the north command topic, translates the per-zone heating intent into one
//! `key=value;…` UDP datagram ([`translate`]), and — only with *both* the config `armed` flag and the
//! `MPC_CONTROLLER_ARM` env token — sends it to the Loxone Miniserver. Otherwise it logs the
//! would-send datagram. A `valid_until` deadman either holds (loxone resumes) or drives all zones off.

mod config;
mod translate;

use std::net::UdpSocket;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use controller_protocol::{
    actions_changed, topics, ControlCommand, ControllerStatus, Mode, Payload, PlannedAction,
    ZoneHeat, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};

use crate::config::HeatingConfig;
use crate::translate::{translate, TranslateCfg};

/// The exact env token required (alongside `armed: true`) before any datagram is sent.
const ARM_TOKEN: &str = "i-understand-this-actuates";

fn resolve_armed(cfg: &HeatingConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// A UDP datagram sink (real socket in armed mode; the runtime only sends when armed).
trait UdpSender: Send {
    fn send(&self, datagram: &str) -> Result<()>;
}

struct RealUdp {
    socket: UdpSocket,
    target: String,
}

impl RealUdp {
    fn new(target: String) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        Ok(Self { socket, target })
    }
}

impl UdpSender for RealUdp {
    fn send(&self, datagram: &str) -> Result<()> {
        self.socket.send_to(datagram.as_bytes(), &self.target)?;
        Ok(())
    }
}

struct State {
    cfg: HeatingConfig,
    tcfg: TranslateCfg,
    target: String,
    client: AsyncClient,
    armed: bool,
    sender: Option<RealUdp>,
    last_seq: Option<u64>,
    last_actions: Vec<PlannedAction>,
    last_zones: Vec<ZoneHeat>,
    last_command_at: Option<DateTime<Utc>>,
    reverted: bool,
    valid_until: Option<DateTime<Utc>>,
}

impl State {
    async fn on_command(&mut self, bytes: &[u8]) {
        let cmd: ControlCommand = match serde_json::from_slice(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[heating] ignoring malformed command JSON: {e}");
                return;
            }
        };
        if let Err(why) = cmd.accept(&self.cfg.controller_id, self.last_seq, Utc::now()) {
            println!("[heating] ignoring command: {why}");
            return;
        }
        let Payload::Heating { zones } = &cmd.payload else {
            println!("[heating] ignoring non-heating payload");
            return;
        };

        self.last_seq = Some(cmd.command_seq);
        self.last_command_at = Some(Utc::now());
        self.valid_until = Some(cmd.valid_until);
        self.reverted = false;
        self.last_zones = zones.clone();

        let actions: Vec<PlannedAction> = translate(zones, &self.tcfg, &self.target)
            .into_iter()
            .collect();
        if !actions_changed(&self.last_actions, &actions) {
            println!(
                "[heating] command seq {} unchanged — skipping re-send",
                cmd.command_seq
            );
            return;
        }
        let ctx = format!("command seq {}", cmd.command_seq);
        self.apply(actions, &ctx).await;
    }

    async fn apply(&mut self, mut actions: Vec<PlannedAction>, ctx: &str) {
        println!(
            "[heating] {ctx} — {} datagram(s) [{}]:",
            actions.len(),
            if self.armed { "ARMED" } else { "dry-run" }
        );
        for act in actions.iter_mut() {
            if self.armed {
                if let Some(sender) = &self.sender {
                    match sender.send(&act.message) {
                        Ok(()) => act.published = true,
                        Err(e) => eprintln!("[heating] UDP send to {} failed: {e}", self.target),
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
        let Some(vu) = self.valid_until else { return };
        if Utc::now() < vu {
            return;
        }
        self.reverted = true;
        println!(
            "[heating] DEADMAN expired (valid_until {vu}) → failsafe '{}'",
            self.cfg.failsafe
        );
        if self.cfg.failsafe == "all_off" {
            // Drive every last-known zone off; loxone's own logic takes over from there.
            let off: Vec<ZoneHeat> = self
                .last_zones
                .iter()
                .map(|z| ZoneHeat {
                    zone: z.zone.clone(),
                    power_kw: 0.0,
                    on: false,
                })
                .collect();
            let actions: Vec<PlannedAction> = translate(&off, &self.tcfg, &self.target)
                .into_iter()
                .collect();
            self.apply(actions, "failsafe all_off").await;
        }
        // "hold" → send nothing; loxone's native heating control resumes.
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
            telemetry: serde_json::Value::Null, // loxone relay state isn't read back on this path
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
        .unwrap_or_else(|| "heating.json5".to_string());
    let cfg = HeatingConfig::load(&path)?;
    let armed = resolve_armed(&cfg);
    let target = cfg.loxone_target();
    if armed {
        println!("*** mpc-controller-heating ARMED — WILL SEND UDP to {target} ***");
    } else if cfg.armed {
        println!("--- mpc-controller-heating: config armed but MPC_CONTROLLER_ARM token absent → DRY-RUN ---");
    } else {
        println!("--- mpc-controller-heating DRY-RUN — logging only, loxone is untouched ---");
    }

    let sender = if armed {
        Some(RealUdp::new(target.clone())?)
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
    println!("[heating] listening on {control_topic} → UDP {target}");
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
        last_zones: Vec::new(),
        last_command_at: None,
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
                    let _ = state
                        .client
                        .publish(topics::health(&id), QoS::AtLeastOnce, true, "online")
                        .await;
                    println!("[heating] (re)connected, subscribed to {control_topic}");
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic == control_topic {
                        state.on_command(&p.payload).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[heating] mqtt connection: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
        }
    }
}
