//! `mpc-controller-growatt` — the battery/inverter controller
//! (loxone_smart_home's own Growatt control is OFF — never two controllers on one inverter).
//!
//! Subscribes the north command topic, translates the battery intent into the Growatt MQTT command
//! vocabulary ([`translate`]), and — only when *both* the config `armed` flag and the
//! `MPC_CONTROLLER_ARM` env token are set — publishes it. Otherwise it logs the would-send messages.
//! A `valid_until` deadman reverts to `regular` (handing control back) if commands go silent.
//!
//! **Command acknowledgement (armed only).** Growatt drops/NAKs commands sent faster than ~1 Hz, so
//! a fire-and-forget publish can be silently lost. Each armed publish is therefore confirmed against
//! the bridge's `energy/solar/result` reply (`{command, success}`, matched by the command sub-path,
//! the same correlation loxone uses) and retried with backoff on failure/timeout. Because the event
//! loop must keep polling to *receive* those replies while a publish awaits its ack, the connection
//! is driven on its own task that forwards commands/telemetry/results to the worker via shared state.

mod config;
mod translate;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use controller_protocol::{
    actions_changed, topics, BatteryPayload, BatterySlot, ControlCommand, ControllerStatus, Mode,
    Payload, PlannedAction, SCHEMA_VERSION,
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::config::GrowattConfig;
use crate::translate::{translate, SlotWindow, TranslateCfg};

/// The exact env token required (alongside `armed: true`) before anything is sent to hardware.
const ARM_TOKEN: &str = "i-understand-this-actuates";
/// How long to wait for a command's `energy/solar/result` ack before retrying.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times to (re)send a single command before giving up (and logging the loss).
const MAX_ACK_ATTEMPTS: u32 = 4;

/// Hardware actuation needs BOTH the config flag and the env token — neither alone is enough.
fn resolve_armed(cfg: &GrowattConfig) -> bool {
    cfg.armed && std::env::var("MPC_CONTROLLER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// The block's 15-minute window as the inverter's local `HH:MM` (Growatt expects local time).
fn slot_window(block_start: DateTime<Utc>, offset_hours: i32) -> SlotWindow {
    let local = block_start + ChronoDuration::hours(offset_hours as i64);
    let stop = local + ChronoDuration::minutes(15);
    SlotWindow {
        start: local.format("%H:%M").to_string(),
        stop: stop.format("%H:%M").to_string(),
    }
}

/// Exponential-ish backoff between command resends (base 1 s, ×2, capped) — also honours Growatt's
/// ~1 Hz floor so a first retry never lands faster than the inverter accepts.
fn ack_backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(4)).clamp(1, 15))
}

/// Pending armed commands awaiting their `energy/solar/result` ack, keyed by command sub-path
/// (e.g. `batteryfirst/set/stopsoc`); the connection task fulfils the oneshot with the `success` flag.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

/// Live state shared with the connection-driver task: the freshest telemetry SoC (percent).
type SharedSoc = Arc<Mutex<Option<f64>>>;

struct State {
    cfg: GrowattConfig,
    tcfg: TranslateCfg,
    client: AsyncClient,
    armed: bool,
    last_seq: Option<u64>,
    last_actions: Vec<PlannedAction>,
    last_command_at: Option<DateTime<Utc>>,
    soc: SharedSoc,
    pending: Pending,
    reverted: bool,
    valid_until: Option<DateTime<Utc>>,
}

impl State {
    async fn soc_pct(&self) -> Option<f64> {
        *self.soc.lock().await
    }

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
        let actions = translate(battery, &self.tcfg, &window, self.soc_pct().await);

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
                act.published = self.publish_with_ack(&act.target, &act.message).await;
            }
            println!(
                "    {} {} {}  ({})",
                match (self.armed, act.published) {
                    (false, _) => "would-send",
                    (true, true) => "ACKED",
                    (true, false) => "UNACKED!",
                },
                act.target,
                act.message,
                act.reason
            );
        }
        self.last_actions = actions.clone();
        self.publish_status(actions).await;
    }

    /// Publish one armed command and confirm it against `energy/solar/result`, retrying with backoff.
    /// Returns `true` only on a positive ack; `false` if every attempt failed/timed out (logged, never
    /// silently dropped). The correlation key is the command sub-path after `command_base/`.
    async fn publish_with_ack(&self, target: &str, message: &str) -> bool {
        let sub = target
            .strip_prefix(&format!("{}/", self.cfg.command_base))
            .unwrap_or(target)
            .to_string();
        for attempt in 0..MAX_ACK_ATTEMPTS {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(sub.clone(), tx);
            match self
                .client
                .publish(target, QoS::AtLeastOnce, false, message.as_bytes().to_vec())
                .await
            {
                // `on_result` removed the pending entry when it delivered the ack, so the success
                // arm has nothing to clean up. Every other arm falls through to the unified cleanup
                // below before retrying.
                Ok(()) => match tokio::time::timeout(ACK_TIMEOUT, rx).await {
                    Ok(Ok(true)) => return true,
                    Ok(Ok(false)) => eprintln!("[growatt] {sub} NAKed (attempt {})", attempt + 1),
                    Ok(Err(_)) => {
                        eprintln!("[growatt] {sub} ack dropped (attempt {})", attempt + 1)
                    }
                    Err(_) => eprintln!("[growatt] {sub} ack timeout (attempt {})", attempt + 1),
                },
                Err(e) => eprintln!("[growatt] publish {target} failed: {e}"),
            }
            // Drop this attempt's pending entry before the next insert, so a late/duplicate ack can't
            // be misrouted to the next attempt's sender and the map can't accrue stale entries.
            self.pending.lock().await.remove(&sub);
            if attempt + 1 < MAX_ACK_ATTEMPTS {
                tokio::time::sleep(ack_backoff(attempt)).await;
            }
        }
        eprintln!("[growatt] GAVE UP on {sub} after {MAX_ACK_ATTEMPTS} attempts");
        false
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
            let actions = translate(&regular, &self.tcfg, &window, self.soc_pct().await);
            self.apply(actions, "failsafe revert_to_regular").await;
        }
        // "hold" → issue nothing; the inverter keeps its last mode / loxone resumes.
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
            telemetry: json!({ "soc_pct": self.soc_pct().await }),
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

/// Parse an `energy/solar/result` reply and fulfil the matching pending command's ack.
async fn on_result(bytes: &[u8], pending: &Pending) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    let Some(command) = v.get("command").and_then(|c| c.as_str()) else {
        return;
    };
    // `success` may be a bool or 1/0; absent ⇒ treat as success (the bridge echoed the command).
    let success = match v.get("success") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(1.0) != 0.0,
        _ => true,
    };
    if let Some(tx) = pending.lock().await.remove(command) {
        let _ = tx.send(success);
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

    let control_topic = cfg.control_topic.clone();
    let telemetry_topic = cfg.telemetry_topic.clone();
    // The bridge replies to commands on `<telemetry>/result` (e.g. `energy/solar/result`).
    let result_topic = format!("{telemetry_topic}/result");
    let controller_id = cfg.controller_id.clone();

    client.subscribe(&control_topic, QoS::AtLeastOnce).await?;
    client.subscribe(&telemetry_topic, QoS::AtMostOnce).await?;
    client.subscribe(&result_topic, QoS::AtLeastOnce).await?;
    client
        .publish(health, QoS::AtLeastOnce, true, "online")
        .await?;
    println!(
        "[growatt] listening on {control_topic} (telemetry {telemetry_topic}, acks {result_topic})"
    );

    let soc: SharedSoc = Arc::new(Mutex::new(None));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Vec<u8>>(16);

    // Connection-driver task: keep polling so command acks/telemetry are received while the worker
    // (below) awaits a publish ack. It forwards commands to the worker and fulfils pending acks.
    let driver_task = {
        let driver = client.clone();
        let soc = Arc::clone(&soc);
        let pending = Arc::clone(&pending);
        let (ct, tt, rt, cid) = (
            control_topic.clone(),
            telemetry_topic.clone(),
            result_topic.clone(),
            controller_id.clone(),
        );
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    // rumqttc doesn't replay subscriptions after a reconnect — re-subscribe on ConnAck.
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        let _ = driver.subscribe(&ct, QoS::AtLeastOnce).await;
                        let _ = driver.subscribe(&tt, QoS::AtMostOnce).await;
                        let _ = driver.subscribe(&rt, QoS::AtLeastOnce).await;
                        let _ = driver
                            .publish(topics::health(&cid), QoS::AtLeastOnce, true, "online")
                            .await;
                        println!("[growatt] (re)connected, subscribed to {ct}");
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        if p.topic == ct {
                            let _ = cmd_tx.send(p.payload.to_vec()).await;
                        } else if p.topic == tt {
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&p.payload) {
                                if let Some(s) = v.get("SOC").and_then(|x| x.as_f64()) {
                                    *soc.lock().await = Some(s);
                                }
                            }
                        } else if p.topic == rt {
                            on_result(&p.payload, &pending).await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[growatt] mqtt connection: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        })
    };

    let tcfg = cfg.translate_cfg();
    let mut state = State {
        cfg,
        tcfg,
        client,
        armed,
        last_seq: None,
        last_actions: Vec::new(),
        last_command_at: None,
        soc,
        pending,
        reverted: false,
        valid_until: None,
    };

    let mut deadman = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(bytes) => state.on_command(&bytes).await,
                None => {
                    // The driver task owns the only sender, so `None` means it ended (panic/abort) —
                    // log it so the controller stopping isn't a silent exit.
                    eprintln!("[growatt] connection task ended — shutting down");
                    break;
                }
            },
            _ = deadman.tick() => state.check_deadman().await,
        }
    }
    // The driver owns the eventloop, so the loop above exits only once it's already gone (cmd_tx
    // dropped) — this abort is then a no-op, but it ties the task's lifetime to main rather than
    // leaking the handle to runtime-drop. The broker publishes our `offline` last-will on the dropped
    // connection regardless.
    driver_task.abort();
    Ok(())
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

    #[test]
    fn ack_backoff_grows_and_respects_the_1hz_floor() {
        assert_eq!(ack_backoff(0), Duration::from_secs(1));
        assert_eq!(ack_backoff(1), Duration::from_secs(2));
        assert_eq!(ack_backoff(2), Duration::from_secs(4));
        assert!(ack_backoff(10) <= Duration::from_secs(15)); // capped
    }

    #[tokio::test]
    async fn on_result_fulfils_pending_by_command_subpath() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert("batteryfirst/set/stopsoc".to_string(), tx);
        on_result(
            br#"{"command":"batteryfirst/set/stopsoc","success":true}"#,
            &pending,
        )
        .await;
        assert!(rx.await.unwrap());
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn on_result_decodes_numeric_failure() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("modbus/set".to_string(), tx);
        on_result(br#"{"command":"modbus/set","success":0}"#, &pending).await;
        assert!(!rx.await.unwrap());
    }
}
