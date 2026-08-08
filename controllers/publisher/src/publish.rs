//! The MQTT side: a `Publisher` seam so the loop is testable, with a dry-run logger and the real
//! `rumqttc` client.

use std::time::Duration;

use anyhow::Result;
use rumqttc::{Client, LastWill, MqttOptions, QoS};

use crate::config::PublisherConfig;

/// Where a `ControlCommand` is sent. The dry-run impl logs; the MQTT impl publishes.
pub trait Publisher {
    fn publish(&mut self, topic: &str, payload: &str, retain: bool) -> Result<()>;
}

/// Dry-run (the default): log the would-publish message, send nothing.
pub struct LoggingPublisher;

impl Publisher for LoggingPublisher {
    fn publish(&mut self, topic: &str, payload: &str, _retain: bool) -> Result<()> {
        println!("[publisher dry-run] WOULD PUBLISH {topic} {payload}");
        Ok(())
    }
}

/// Armed: publish retained commands to the broker, with a Last-Will on the publisher's health topic.
pub struct MqttPublisher {
    client: Client,
}

impl MqttPublisher {
    pub fn connect(cfg: &PublisherConfig) -> Result<Self> {
        let mut opts = MqttOptions::new(&cfg.mqtt.client_id, &cfg.mqtt.host, cfg.mqtt.port);
        opts.set_keep_alive(Duration::from_secs(30));
        let health = controller_protocol::topics::health("publisher");
        opts.set_last_will(LastWill::new(
            health.clone(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        let (client, mut connection) = Client::new(opts, 32);
        // Drive the event loop in a background thread (acks, reconnects).
        std::thread::spawn(move || {
            for notification in connection.iter() {
                if let Err(e) = notification {
                    eprintln!("[publisher] mqtt event-loop error: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        });
        client.publish(health, QoS::AtLeastOnce, true, "online")?;
        Ok(Self { client })
    }
}

impl Publisher for MqttPublisher {
    fn publish(&mut self, topic: &str, payload: &str, retain: bool) -> Result<()> {
        // `try_publish`, never the blocking `publish`. The blocking form is an untimed send on the
        // bounded request channel, and rumqttc only drains that channel while a connection has
        // existed: once the broker is unreachable the event loop stays in its connect branch and
        // never calls `clean()`, so after ~32 queued messages the send blocks FOREVER. That is the
        // publisher's single poll thread, so the whole loop stops — no more plan polls, no more
        // commands, and no log line saying why. Dropping instead is correct here: commands are
        // retained and re-published on the next poll, and a command that genuinely never reaches
        // the broker must end in a controller deadman revert, which is exactly what happens.
        match self
            .client
            .try_publish(topic, QoS::AtLeastOnce, retain, payload.as_bytes())
        {
            Ok(()) => {
                println!("[publisher] PUBLISHED {topic} ({} bytes)", payload.len());
                Ok(())
            }
            Err(e) => {
                eprintln!(
                    "[publisher] DROPPED {topic}: {e} (broker unreachable or queue full; \
                     controllers will deadman-revert if this persists)"
                );
                Ok(())
            }
        }
    }
}
