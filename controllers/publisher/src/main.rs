//! `mpc-plan-publisher` — the north bridge.
//!
//! Polls the MPC's **read-only** `/api/plan/latest`, maps the coming-block plan into per-controller
//! [`controller_protocol::ControlCommand`]s, and republishes them to the inert `mpc/control/...` MQTT
//! namespace. This keeps the MPC binary itself free of any MQTT dependency — its read-only guarantee
//! stays structural. It publishes to the inert `mpc/control/...` namespace, gated
//! by its own `armed` flag — the per-domain controllers downstream apply the two-key hardware gate.
//! With `armed: false` it is dry-run: logs the would-publish JSON, touches nothing.

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
        "[publisher] polling {} every {}s (deadman {}s, max plan age {}s, seq base {})",
        cfg.mpc_url,
        cfg.poll_seconds,
        cfg.deadman_seconds,
        cfg.max_plan_age_seconds,
        Utc::now().timestamp_millis()
    );
    // A bounded read timeout so a stalled MPC response can never wedge the poll loop (ureq has no
    // default overall timeout — only a connect timeout).
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    loop {
        match poll(&agent, &cfg.mpc_url) {
            Ok(api) if api.age_seconds > cfg.max_plan_age_seconds => {
                // The MPC loop has stopped producing fresh plans (wedged loop, dead inputs) even
                // though the web layer still serves the last one. Publishing would re-stamp a fresh
                // deadman onto a stale decision — instead publish NOTHING so the retained commands
                // expire and every controller falls back to its failsafe / native logic.
                eprintln!(
                    "[publisher] plan is stale ({}s old > max {}s) — skipping ALL commands; \
                     controllers will deadman-revert",
                    api.age_seconds, cfg.max_plan_age_seconds
                );
            }
            Ok(api) if api.data.relaxed => {
                // Transient by design (a strict solve normally lands next tick): fractional
                // relays/EV on-offs must not be rounded onto the hardware; retained commands keep
                // their previous deadman, so a one-tick relaxation costs nothing.
                eprintln!(
                    "[publisher] plan is RELAXED (solver timeout/busy) — skipping commands this poll"
                );
            }
            Ok(api) if api.data.degraded => {
                // The brain flagged a safety-critical input fallback (fictional thermal seed / no
                // outside temperature). The plan is served for inspection only — actuating heating
                // decisions computed from a made-up house state is worse than letting the
                // controllers deadman-revert to their failsafe.
                eprintln!(
                    "[publisher] plan is DEGRADED (safety-critical input fallback) — skipping ALL \
                     commands; controllers will deadman-revert"
                );
            }
            Ok(api) => {
                // The command seq must survive publisher restarts (controllers reject seq <= their
                // in-memory high-water). Wall-clock millis are strictly increasing across restarts
                // at any realistic poll rate; a backward NTP step is tolerated (commands are
                // rejected only until the clock catches back up).
                let seq = Utc::now().timestamp_millis() as u64;
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

/// One read-only GET of the MPC plan API (bounded by the agent's timeout).
fn poll(agent: &ureq::Agent, url: &str) -> anyhow::Result<LatestResponse> {
    let body = agent.get(url).call()?.into_string()?;
    Ok(serde_json::from_str(&body)?)
}
