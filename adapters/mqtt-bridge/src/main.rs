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

/// Map one delivered message onto every matching signal and write (or log) the resulting points.
async fn on_message(
    topic: &str,
    payload: &[u8],
    signals: &[SignalMap],
    writer: &Arc<InfluxWriter>,
) {
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
        let bucket = sig.bucket.clone();
        let w = Arc::clone(writer);
        // The write is blocking (ureq); run it off the mqtt event loop so a slow/unreachable influx
        // never stalls message delivery.
        let line_for_log = line.clone();
        let dest = bucket
            .clone()
            .unwrap_or_else(|| w.default_bucket().to_string());
        let result = tokio::task::spawn_blocking(move || w.write(bucket.as_deref(), &line)).await;
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

    let mut opts = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    let signals = cfg.signals.clone();
    let topics: Vec<&str> = signals.iter().map(|s| s.topic.as_str()).collect();
    let (client, mut eventloop) = AsyncClient::new(opts, 256);
    subscribe_all(&client, &topics, "bridge").await;

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
                let ok = subscribe_all(&client, &topics, "bridge").await;
                println!(
                    "[bridge] (re)connected, {ok}/{} topic(s) subscribed",
                    signals.len()
                );
            }
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                on_message(&p.topic, &p.payload, &signals, &writer).await;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[bridge] mqtt connection: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
