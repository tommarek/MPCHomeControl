//! Shared helpers for the hardware-controller crates.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Convert a wall-clock `valid_until` into a monotonic deadline for the deadman, immune to later
/// clock steps. Read both clocks adjacently and anchor to the monotonic one: computing `remaining`
/// from one `Utc::now()` and then adding it to a *later* `Instant::now()` would push the deadline out
/// by the gap between the two reads. (An already-past `valid_until` yields `ZERO` → fire on the next
/// check, the intended fail-safe; the next command re-arms it.)
pub fn monotonic_deadline(valid_until: DateTime<Utc>) -> Instant {
    let now_utc = Utc::now();
    let now_mono = Instant::now();
    let remaining = (valid_until - now_utc).to_std().unwrap_or(Duration::ZERO);
    now_mono + remaining
}

/// A UDP datagram sink to a `host:port` target (the Loxone Miniserver virtual inputs). Binds an
/// ephemeral local port; the controller runtime only constructs one when armed.
pub struct UdpClient {
    socket: UdpSocket,
    target: SocketAddr,
}

impl UdpClient {
    /// Resolve the target ONCE, here. `send_to(&String)` re-ran `ToSocketAddrs` on every datagram —
    /// a cheap parse for the default IP literal, but `loxone.host` is a free-form config string, so
    /// pointing it at a hostname turned every send into a blocking `getaddrinfo`. `send` is called
    /// from `apply` and `heartbeat_refresh`, both driven straight off the `tokio::select!` loop, so
    /// a slow or unreachable resolver would stall the MQTT event loop, the command handler and the
    /// deadman tick — on the controller that gates the whole house. An unresolvable target is now a
    /// startup error instead of a per-datagram hazard.
    pub fn bind(target: String) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        let target = target
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("loxone target {target:?} resolved to no address"))?;
        Ok(Self { socket, target })
    }

    pub fn send(&self, datagram: &str) -> Result<()> {
        self.socket.send_to(datagram.as_bytes(), self.target)?;
        Ok(())
    }

    /// The resolved target, for logging.
    pub fn target(&self) -> SocketAddr {
        self.target
    }
}

// Shared `#[serde(default)]` infrastructure constants, single-sourced here so every controller's
// config agrees (each controller still defines its own `default_client_id`, which must be distinct).

/// Default MQTT broker host (the loxone broker, reached on localhost).
pub fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}

/// Default MQTT broker port.
pub fn default_mqtt_port() -> u16 {
    1883
}

/// Default Loxone Miniserver host for the UDP virtual-input controllers.
pub fn default_loxone_host() -> String {
    "192.168.0.200".to_string()
}

/// Default Loxone Miniserver UDP port.
pub fn default_loxone_port() -> u16 {
    4000
}
