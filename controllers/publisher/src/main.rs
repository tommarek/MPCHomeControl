//! `mpc-plan-publisher` — the north bridge.
//!
//! Polls the MPC's **read-only** `/api/plan/latest`, maps the coming-block plan into per-controller
//! [`controller_protocol::ControlCommand`]s, and republishes them to the inert `mpc/control/...` MQTT
//! namespace. This keeps the MPC binary itself free of any MQTT dependency — its read-only guarantee
//! stays structural. **Dry-run by default** (logs the would-publish JSON, touches nothing).

mod build;
mod config;
mod plan;
mod publish;

use std::time::Duration;

use chrono::Utc;
use controller_protocol::topics;

use crate::config::PublisherConfig;
use crate::plan::LatestResponse;
use crate::publish::{LoggingPublisher, MqttPublisher, Publisher};

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "publisher.json5".to_string());
    let cfg = PublisherConfig::load(&path)?;

    let mut publisher: Box<dyn Publisher> = if cfg.armed {
        println!(
            "*** mpc-plan-publisher ARMED — WILL PUBLISH to mqtt://{}:{} (inert mpc/control namespace) ***",
            cfg.mqtt.host, cfg.mqtt.port
        );
        Box::new(MqttPublisher::connect(&cfg)?)
    } else {
        println!("--- mpc-plan-publisher DRY-RUN — logging only, publishing nothing ---");
        Box::new(LoggingPublisher)
    };

    println!(
        "[publisher] polling {} every {}s (deadman {}s)",
        cfg.mpc_url, cfg.poll_seconds, cfg.deadman_seconds
    );
    let mut seq: u64 = 0;
    loop {
        match poll(&cfg.mpc_url) {
            Ok(api) => {
                seq += 1;
                for (id, cmd) in build::commands(&api, &cfg, seq, Utc::now()) {
                    let topic = topics::command(&id);
                    match serde_json::to_string(&cmd) {
                        Ok(json) => {
                            if let Err(e) = publisher.publish(&topic, &json, true) {
                                eprintln!("[publisher] publish to {topic} failed: {e}");
                            }
                        }
                        Err(e) => eprintln!("[publisher] serialize {id} command failed: {e}"),
                    }
                }
            }
            // A poll failure (e.g. 503 while the loop warms up, or the MPC down) is logged and
            // retried — it never crashes the publisher.
            Err(e) => eprintln!("[publisher] poll {} failed: {e}", cfg.mpc_url),
        }
        std::thread::sleep(Duration::from_secs(cfg.poll_seconds.max(1)));
    }
}

/// One read-only GET of the MPC plan API.
fn poll(url: &str) -> anyhow::Result<LatestResponse> {
    let body = ureq::get(url).call()?.into_string()?;
    Ok(serde_json::from_str(&body)?)
}
