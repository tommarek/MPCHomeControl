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
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload.as_bytes())?;
        println!("[publisher] PUBLISHED {topic} ({} bytes)", payload.len());
        Ok(())
    }
}
