//! `mpc-adapter-mqtt-source` — brings live MQTT values into the read-only MPC *without* MQTT linking
//! into the MPC, and without a write-to-Influx hop.
//!
//! It subscribes the configured MQTT topics, keeps the latest value of each in memory, and serves it
//! over a tiny HTTP endpoint (`GET /v1/value/<name>`) the MPC pulls via its `http` `SourceLocator`.
//! This is the "middle ground": the MPC stays a pure HTTP/Influx/Postgres puller (the structural
//! no-MQTT guarantee holds — this is a separate binary), but a genuinely MQTT-only signal (e.g. the
//! TeslaMate `charge_limit_soc` target, which TeslaMate never persists to Postgres) reaches it live.
//!
//! **Read-only by construction:** it only *subscribes* MQTT and serves `GET`s — it never publishes and
//! never writes any store, so there is nothing to arm.

mod config;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions};
use tokio::sync::RwLock;

use crate::config::{SourceConfig, TopicMap};
use mqtt_common::{parse_and_scale, subscribe_all, topic_matches};

/// The latest cached value of one named signal.
struct Entry {
    value: f64,
    /// When *this adapter* received the message (monotonic). Freshness (`max_age_seconds`) is measured
    /// from here, not from any timestamp embedded in the payload — correct for a live push feed, but it
    /// means a replayed retained message reads as fresh on arrival. Keep `max_age_seconds` modest.
    received: Instant,
    max_age_seconds: Option<u64>,
}

type Store = Arc<RwLock<HashMap<String, Entry>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "mqtt-source.json5".to_string());
    let cfg = SourceConfig::load(&path).with_context(|| format!("loading {path}"))?;
    if cfg.topics.is_empty() {
        eprintln!("[mqtt-source] WARNING: no topics configured — nothing to expose");
    }

    let store: Store = Arc::new(RwLock::new(HashMap::new()));

    // The MQTT subscriber runs in the background, updating the shared store.
    let mqtt_store = Arc::clone(&store);
    let mqtt_cfg = cfg.mqtt.clone();
    let topics = cfg.topics.clone();
    let total_topics = cfg.topics.len();
    // `/healthz` must catch both ways data can silently stop: a panicked loop (the join handle is
    // finished) and a loop that's alive but subscribed to nothing after a reconnect (`live_subs == 0`).
    let live_subs = Arc::new(AtomicUsize::new(0));
    let loop_subs = Arc::clone(&live_subs);
    let mqtt_task = Arc::new(tokio::spawn(async move {
        mqtt_loop(mqtt_cfg, topics, mqtt_store, loop_subs).await
    }));
    // The `/healthz` handler needs its own clone; `main` keeps `mqtt_task` so it can abort the loop
    // when the HTTP server stops (below) rather than detaching the handle to runtime-drop.
    let health_task = Arc::clone(&mqtt_task);

    let app = Router::new()
        .route("/v1/value/:name", get(get_value))
        .route(
            "/healthz",
            get(move || {
                let t = Arc::clone(&health_task);
                let subs = Arc::clone(&live_subs);
                async move {
                    if t.is_finished() {
                        (StatusCode::SERVICE_UNAVAILABLE, "mqtt subscriber stopped")
                    } else if total_topics > 0 && subs.load(Ordering::Relaxed) == 0 {
                        (StatusCode::SERVICE_UNAVAILABLE, "no topics subscribed")
                    } else {
                        (StatusCode::OK, "ok")
                    }
                }
            }),
        )
        .route("/", get(index))
        .with_state(store);

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    println!(
        "[mqtt-source] serving {} topic(s) on http://{}/v1/value/<name> (read-only)",
        cfg.topics.len(),
        cfg.bind
    );
    let serve_result = axum::serve(listener, app).await;
    // If the HTTP server stops (shutdown or bind/accept error), abort the subscriber loop explicitly
    // so the task's lifetime is tied to main rather than left to runtime-drop.
    mqtt_task.abort();
    serve_result?;
    Ok(())
}

/// `GET /v1/value/<name>` → `{ name, value, age_seconds }`, or `404` if unknown / stale (so the MPC's
/// best-effort read degrades to `None`).
async fn get_value(Path(name): Path<String>, State(store): State<Store>) -> impl IntoResponse {
    let g = store.read().await;
    match g.get(&name) {
        Some(e) => {
            let elapsed = e.received.elapsed();
            // Compare the full Duration (not truncated seconds) so a value 1 s over the bound isn't
            // served as fresh.
            if e.max_age_seconds
                .is_some_and(|m| elapsed > Duration::from_secs(m))
            {
                return (StatusCode::NOT_FOUND, "stale").into_response();
            }
            Json(serde_json::json!({ "name": name, "value": e.value, "age_seconds": elapsed.as_secs() }))
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "unknown").into_response(),
    }
}

/// `GET /` → the names currently cached (a human index).
async fn index(State(store): State<Store>) -> impl IntoResponse {
    let g = store.read().await;
    let names: Vec<&String> = g.keys().collect();
    Json(serde_json::json!({ "names": names }))
}

async fn mqtt_loop(
    cfg: crate::config::MqttConfig,
    topics: Vec<TopicMap>,
    store: Store,
    live_subs: Arc<AtomicUsize>,
) {
    let mut opts = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
    opts.set_keep_alive(Duration::from_secs(30));
    let (client, mut eventloop) = AsyncClient::new(opts, 256);
    let topic_strs: Vec<&str> = topics.iter().map(|t| t.topic.as_str()).collect();
    live_subs.store(
        subscribe_all(&client, &topic_strs, "mqtt-source").await,
        Ordering::Relaxed,
    );

    loop {
        match eventloop.poll().await {
            // rumqttc does not replay subscriptions after a reconnect — re-subscribe on every ConnAck.
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                let ok = subscribe_all(&client, &topic_strs, "mqtt-source").await;
                live_subs.store(ok, Ordering::Relaxed);
                println!(
                    "[mqtt-source] (re)connected, {ok}/{} topic(s) subscribed",
                    topics.len()
                );
            }
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                for tm in topics
                    .iter()
                    .filter(|tm| topic_matches(&tm.topic, &p.topic))
                {
                    let value = match parse_and_scale(&p.payload, tm.pointer.as_deref(), tm.scale) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("[mqtt-source] {}: {e} — skipped", tm.name);
                            continue;
                        }
                    };
                    store.write().await.insert(
                        tm.name.clone(),
                        Entry {
                            value,
                            received: Instant::now(),
                            max_age_seconds: tm.max_age_seconds,
                        },
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[mqtt-source] mqtt connection: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
