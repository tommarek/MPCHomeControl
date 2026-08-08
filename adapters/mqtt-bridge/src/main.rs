//! `mpc-adapter-mqtt-bridge` — a data-source adapter that brings MQTT data into the read-only MPC
//! *without* MQTT ever linking into the MPC.
//!
//! It subscribes the configured MQTT topics, normalises each message into a numeric point, and writes
//! it to the InfluxDB **pull store** the MPC then reads via its `SourceLocator`s (e.g. TeslaMate's
//! `teslamate/cars/<id>/battery_level` → an `ev`-bucket measurement the EV `sources` map points at).
//! This is the structural reason the MPC stays MQTT-free: the bridge is a separate binary, and the
//! MPC only ever pulls over HTTP. It is **dry-run by default** — writing requires both the config
//! `armed` flag and the `MPC_ADAPTER_ARM` env token. Even armed, it writes only its *own* normalised
//! measurements; it never touches the live loxone/growatt data.

mod config;
mod influx;

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions};

use crate::config::{BridgeConfig, SignalMap};
use crate::influx::InfluxWriter;
use mqtt_common::{parse_and_scale, subscribe_all, topic_matches};

/// The exact env token required (alongside `armed: true`) before any point is written.
const ARM_TOKEN: &str = "i-understand-this-writes";

fn resolve_armed(cfg: &BridgeConfig) -> bool {
    cfg.armed && std::env::var("MPC_ADAPTER_ARM").as_deref() == Ok(ARM_TOKEN)
}

/// A parsed point queued for the writer task: destination bucket (None = default) + the line.
struct QueuedWrite {
    bucket: Option<String>,
    line: String,
}

/// Map one delivered message onto every matching signal and queue the resulting points for the
/// writer task. Queueing (never awaiting the write here) keeps the mqtt event loop polling during
/// an Influx outage — otherwise each 10 s write timeout suspends the loop past the 30 s
/// keep-alive, the broker drops the connection, and the redelivery repeats the stall.
fn on_message(
    topic: &str,
    payload: &[u8],
    retain: bool,
    signals: &[SignalMap],
    tx: &mpsc::Sender<QueuedWrite>,
) {
    // DROP retained deliveries. The bridge stamps every point `Utc::now()`, and a broker redelivers
    // retained messages on every SUBSCRIBE — which this bridge issues on every ConnAck. So a broker
    // restart or a network blip would rewrite an arbitrarily old retained value (TeslaMate publishes
    // `battery_level`, `charge_limit_soc` and `charger_power` retained) into InfluxDB stamped
    // "now". Every downstream freshness bound is a pure recency query (`range(start: -Nm) |> last()`),
    // so nothing can tell: a stale SoC that should have aged past CAR_FRESH_MIN is accepted as
    // current and the plan is built on it. A retained message says nothing about WHEN its value was
    // produced; live publishes arrive on their own.
    if retain {
        return;
    }
    for sig in signals.iter().filter(|s| topic_matches(&s.topic, topic)) {
        // Identify the signal (measurement.field), not just the topic: several signals can share one
        // topic with different pointers, so the topic alone can't say which one was dropped.
        let value = match parse_and_scale(payload, sig.pointer.as_deref(), sig.scale) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[bridge] {topic} → {}.{}: {e} — skipped",
                    sig.measurement, sig.field
                );
                continue;
            }
        };
        // `value` is finite, so this only fails if the format itself can't be built.
        let Some(line) = influx::line_protocol(&sig.measurement, &sig.tags, &sig.field, value)
        else {
            continue;
        };
        // Telemetry repeats on its own cadence, so dropping on a full queue (Influx down) loses
        // nothing durable — and never blocks the poll loop.
        if let Err(e) = tx.try_send(QueuedWrite {
            bucket: sig.bucket.clone(),
            line,
        }) {
            eprintln!("[bridge] write queue full — dropping a point ({e})");
        }
    }
}

/// The dedicated writer task: drains the queue, one blocking (ureq) write at a time.
async fn write_worker(mut rx: mpsc::Receiver<QueuedWrite>, writer: Arc<InfluxWriter>) {
    while let Some(q) = rx.recv().await {
        let w = Arc::clone(&writer);
        let dest = q
            .bucket
            .clone()
            .unwrap_or_else(|| w.default_bucket().to_string());
        let line_for_log = q.line.clone();
        let result =
            tokio::task::spawn_blocking(move || w.write(q.bucket.as_deref(), &q.line)).await;
        match result {
            Ok(Ok(true)) => println!("[bridge] wrote → {dest}: {line_for_log}"),
            Ok(Ok(false)) => println!("[bridge] would-write → {dest}: {line_for_log}"),
            Ok(Err(e)) => eprintln!("[bridge] write to {dest} failed: {e}"),
            Err(e) => eprintln!("[bridge] write task panicked: {e}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bridge.json5".to_string());
    let cfg = BridgeConfig::load(&path).with_context(|| format!("loading {path}"))?;
    let armed = resolve_armed(&cfg);

    if cfg.signals.is_empty() {
        eprintln!("[bridge] WARNING: no signals configured — nothing to bridge");
    }
    if armed {
        println!(
            "*** mpc-adapter-mqtt-bridge ARMED — WILL WRITE to {} (org {}) ***",
            cfg.influx.url, cfg.influx.org
        );
    } else if cfg.armed {
        println!("--- mpc-adapter-mqtt-bridge: config armed but MPC_ADAPTER_ARM token absent → DRY-RUN ---");
    } else {
        println!(
            "--- mpc-adapter-mqtt-bridge DRY-RUN — logging the line protocol it would write ---"
        );
    }

    let token = cfg.resolve_token();
    if armed && token.is_none() {
        anyhow::bail!(
            "armed but no write token in ${} / $INFLUX_TOKEN / $INFLUXDB_TOKEN",
            cfg.influx.token_env
        );
    }
    let writer = Arc::new(InfluxWriter::new(cfg.influx.clone(), token, armed));
    // Decouple writes from the poll loop (see on_message). 256 points ≈ minutes of telemetry.
    let (write_tx, write_rx) = mpsc::channel::<QueuedWrite>(256);
    tokio::spawn(write_worker(write_rx, Arc::clone(&writer)));

    let mut opts = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    let signals = cfg.signals.clone();
    let topics: Vec<&str> = signals.iter().map(|s| s.topic.as_str()).collect();
    let (client, mut eventloop) = AsyncClient::new(opts, 256);
    subscribe_all(&client, &topics, "bridge");

    println!(
        "[bridge] {} signal(s) → {} (bucket {})",
        signals.len(),
        cfg.influx.url,
        cfg.influx.bucket
    );

    loop {
        match eventloop.poll().await {
            // rumqttc does not replay subscriptions after a reconnect — re-subscribe on every ConnAck.
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                let ok = subscribe_all(&client, &topics, "bridge");
                println!(
                    "[bridge] (re)connected, {ok}/{} topic(s) subscribed",
                    signals.len()
                );
            }
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                on_message(&p.topic, &p.payload, p.retain, &signals, &write_tx);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[bridge] mqtt connection: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SignalMap;

    /// A retained delivery carries no information about WHEN its value was produced, and this bridge
    /// stamps every point `Utc::now()`. Since it re-subscribes on every ConnAck, a broker restart
    /// would otherwise launder an arbitrarily old value into InfluxDB as current — invisible to
    /// every downstream freshness bound, which are all pure recency queries.
    #[test]
    fn retained_deliveries_are_dropped() {
        let signals = vec![SignalMap {
            topic: "teslamate/cars/1/battery_level".to_string(),
            measurement: "tesla".to_string(),
            field: "battery_level".to_string(),
            tags: Default::default(),
            scale: 1.0,
            pointer: None,
            bucket: None,
        }];
        let (tx, mut rx) = mpsc::channel(4);
        on_message("teslamate/cars/1/battery_level", b"81", true, &signals, &tx);
        assert!(
            rx.try_recv().is_err(),
            "a retained delivery must not be written"
        );
        on_message(
            "teslamate/cars/1/battery_level",
            b"81",
            false,
            &signals,
            &tx,
        );
        assert!(rx.try_recv().is_ok(), "a live delivery must be written");
    }
}
