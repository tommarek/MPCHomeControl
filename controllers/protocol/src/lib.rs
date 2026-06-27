//! The universal, language-agnostic **controller protocol** — the contract between the home-energy
//! MPC (`mpc_home_control`) and any hardware controller that drives a subsystem (battery/inverter,
//! heating, HVAC, …).
//!
//! The MPC's plan-publisher emits a [`ControlCommand`]; a controller — written in Rust, Python, or
//! anything — translates it into a real device protocol (Growatt MQTT, a Loxone UDP virtual input,
//! …) and reports a [`ControllerStatus`]. The messages are plain JSON and **transport-agnostic**; the
//! reference transport is MQTT (topic helpers in [`topics`]), but the types don't depend on it.
//!
//! Two invariants make this safe to run against a live house:
//! - every command carries a **`valid_until` deadman** — a controller that stops hearing fresh
//!   commands reverts to its failsafe (handing control back to the existing system);
//! - controllers are **dry-run by default** ([`Mode::DryRun`]): they compute the device messages they
//!   *would* send ([`PlannedAction`] with `published: false`) and log them, touching no hardware.
//!
//! See `docs/controllers.md` for the full spec and a worked controller in Python.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The protocol version this crate implements (semver `major.minor`). A consumer must refuse a
/// command whose **major** differs (see [`ControlCommand::version_compatible`]).
pub const SCHEMA_VERSION: &str = "1.0";

/// The major component of a `major.minor[.patch]` version string.
fn major_of(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

/// Whether two protocol versions are compatible (same major).
pub fn versions_compatible(a: &str, b: &str) -> bool {
    major_of(a) == major_of(b)
}

/// The mode a controller runs in (echoed in its status). Dry-run is the default everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Compute and log the device messages, but send nothing. The default.
    DryRun,
    /// Actually publish/send to hardware. Reached only behind an explicit two-key arm.
    Armed,
}

/// Battery/inverter operating mode — the loxone Growatt vocabulary (mirrors `app::classify_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatterySlot {
    Regular,
    ChargeFromGrid,
    DischargeToGrid,
    SellProduction,
    BatteryHold,
    InverterOff,
}

/// The mode of an HVAC zone for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HvacMode {
    Off,
    Cool,
    Heat,
}

/// The per-subsystem command payload, tagged on `kind`. A new subsystem is a new variant; the generic
/// [`Payload::Load`] covers EV chargers / water heaters / future flexible loads without a protocol
/// change — so the contract already spans "all possible sections".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// Battery/inverter setpoint for the block.
    Battery(BatteryPayload),
    /// Per-zone underfloor heating.
    Heating { zones: Vec<ZoneHeat> },
    /// Per-zone reversible HVAC (cooling and/or air-heating).
    Hvac { zones: Vec<ZoneHvac> },
    /// Generic named flexible loads (EV charger, water heater, …).
    Load { channels: Vec<LoadChannel> },
    /// A flat set of Loxone virtual-input writes — the unified Loxone controller writes them all in
    /// one UDP datagram. Generic key→value, so a new Loxone-driven actuation is a publisher config
    /// row, not a protocol change. See `docs/loxone-controller-plan.md`.
    Loxone { writes: Vec<LoxoneWrite> },
}

/// Battery/inverter command — mirrors `app::ModeStep` plus the SoC band a controller needs to
/// translate the `slot` into hardware stop-SoC targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryPayload {
    pub slot: BatterySlot,
    pub export_enabled: bool,
    pub inverter_on: bool,
    pub charge_kw: f64,
    pub discharge_kw: f64,
    pub min_soc_kwh: f64,
    pub max_soc_kwh: f64,
    /// Live SoC at command time (kWh), if known — needed to translate `battery_hold` into a stop-SoC
    /// that pins the battery (neither charge nor drain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soc_kwh: Option<f64>,
}

/// One zone's underfloor-heating decision. `power_kw` is the planned power; `on` is the binary relay
/// decision (`power_kw` over a threshold) — a relay-driven controller uses `on`, a modulating one may
/// use `power_kw`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneHeat {
    pub zone: String,
    pub power_kw: f64,
    pub on: bool,
}

/// One zone's HVAC decision (cooling and air-heating are mutually exclusive in a block; `mode`
/// disambiguates and `cool_kw`/`heat_kw` carry the magnitudes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneHvac {
    pub zone: String,
    pub cool_kw: f64,
    pub heat_kw: f64,
    pub mode: HvacMode,
}

/// One generic flexible-load channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadChannel {
    pub channel: String,
    pub power_kw: f64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_soc: Option<f64>,
}

/// One Loxone virtual-input write: the exact VI key and the value to set (`1`/`0` for a relay, kW for
/// a setpoint, or an enum code). The keys come from the publisher's mapping, so the controller that
/// sends them stays fully generic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoxoneWrite {
    pub key: String,
    pub value: f64,
}

/// A command from the MPC to one controller: the universal envelope + a typed [`Payload`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlCommand {
    /// The protocol version the producer speaks (semver `major.minor`).
    pub schema_version: String,
    /// The controller this is addressed to (e.g. `"growatt"`, `"heating"`).
    pub controller_id: String,
    /// When the MPC computed the underlying plan (the plan's `computed_at`).
    pub issued_at: DateTime<Utc>,
    /// The 15-minute block this command applies from (UTC).
    pub block_start: DateTime<Utc>,
    /// **Deadman**: after this instant the controller must revert to its failsafe. Keyed on the
    /// timestamp, **not** "a message arrived", so a repeated *stale* command still expires.
    pub valid_until: DateTime<Utc>,
    /// Identifies the plan that produced this command.
    pub plan_id: String,
    /// A monotonic counter from the producer; with `plan_id` it gives idempotency/ordering over an
    /// at-least-once transport (a controller ignores a command whose `command_seq` it already applied).
    pub command_seq: u64,
    pub payload: Payload,
}

impl ControlCommand {
    /// Whether this command is still within its deadman window at `now`.
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        now < self.valid_until
    }

    /// Whether this command supersedes `prev` (a strictly newer producer sequence number).
    pub fn supersedes(&self, prev: &ControlCommand) -> bool {
        self.command_seq > prev.command_seq
    }

    /// Whether this command's protocol major version matches ours (refuse otherwise).
    pub fn version_compatible(&self) -> bool {
        versions_compatible(&self.schema_version, SCHEMA_VERSION)
    }

    /// Whether to apply this command (`Ok`) or why to skip it (`Err`) — the version, addressee,
    /// ordering, and deadman checks in one place, so every controller gates identically. `last_seq`
    /// is the last `command_seq` this controller applied (or `None` if none yet).
    pub fn accept(
        &self,
        expected_id: &str,
        last_seq: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if !self.version_compatible() {
            return Err(format!(
                "incompatible schema_version {}",
                self.schema_version
            ));
        }
        if self.controller_id != expected_id {
            return Err(format!("addressed to {:?}", self.controller_id));
        }
        if let Some(s) = last_seq {
            if self.command_seq <= s {
                return Err(format!("stale seq {} <= {}", self.command_seq, s));
            }
        }
        if !self.is_fresh(now) {
            return Err("expired (past valid_until)".to_string());
        }
        Ok(())
    }
}

/// Whether two action sets differ in their `target`/`message` — the basis for change-only sending
/// (skip re-issuing an identical device command).
pub fn actions_changed(prev: &[PlannedAction], next: &[PlannedAction]) -> bool {
    prev.len() != next.len()
        || prev
            .iter()
            .zip(next)
            .any(|(a, b)| a.target != b.target || a.message != b.message)
}

/// What a controller can do — declared so the producer can clamp commands to real bounds and flag
/// config/hardware mismatches instead of silently overriding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub schema_version: String,
    pub controller_id: String,
    pub spec: CapabilitySpec,
}

/// The per-subsystem capability bounds (tagged on `kind`, mirroring [`Payload`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilitySpec {
    Battery {
        max_charge_kw: f64,
        max_discharge_kw: f64,
        min_soc_kwh: f64,
        max_soc_kwh: f64,
        supported_slots: Vec<BatterySlot>,
    },
    Heating {
        zones: Vec<ZoneCapability>,
    },
    Hvac {
        zones: Vec<ZoneCapability>,
    },
    Load {
        channels: Vec<String>,
    },
}

/// Per-zone actuator bounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneCapability {
    pub zone: String,
    pub max_kw: f64,
    /// Whether the actuator modulates (true) or is on/off only (false — a relay).
    pub supports_modulation: bool,
}

/// One translated device message — the audit record. Computed (and logged) in both modes; only
/// actually transmitted when [`Mode::Armed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedAction {
    /// MQTT topic, or a UDP `host:port` target.
    pub target: String,
    /// JSON payload, or the UDP datagram text.
    pub message: String,
    /// `false` in dry-run (computed but not sent); `true` when actually published/sent.
    pub published: bool,
    /// Human-readable rationale, e.g. `"slot=charge_from_grid → acchargeenabled=1"`.
    pub reason: String,
}

/// A controller's report: its mode, deadman state, the device telemetry it observed, and the actions
/// from its last command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub schema_version: String,
    pub controller_id: String,
    pub at: DateTime<Utc>,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_at: Option<DateTime<Utc>>,
    pub deadman_expired: bool,
    /// Device telemetry the controller observed (free-form per subsystem; `null` when none).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub telemetry: serde_json::Value,
    pub actions: Vec<PlannedAction>,
}

/// The reference **MQTT transport** topic conventions. The protocol types above are
/// transport-agnostic; these helpers are the topic names the MQTT publisher and controllers agree on.
pub mod topics {
    /// Command topic the producer publishes to (retained): `mpc/control/<id>`.
    pub fn command(controller_id: &str) -> String {
        format!("mpc/control/{controller_id}")
    }
    /// Status topic a controller publishes to: `mpc/status/<id>`.
    pub fn status(controller_id: &str) -> String {
        format!("mpc/status/{controller_id}")
    }
    /// Capability topic a controller publishes to (retained): `mpc/describe/<id>`.
    pub fn describe(controller_id: &str) -> String {
        format!("mpc/describe/{controller_id}")
    }
    /// Health topic (the controller's MQTT Last-Will): `mpc/health/<id>`.
    pub fn health(controller_id: &str) -> String {
        format!("mpc/health/{controller_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn battery_command() -> ControlCommand {
        ControlCommand {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: "growatt".to_string(),
            issued_at: utc("2026-06-23T12:00:00Z"),
            block_start: utc("2026-06-23T12:00:00Z"),
            valid_until: utc("2026-06-23T12:16:30Z"),
            plan_id: "plan-1".to_string(),
            command_seq: 7,
            payload: Payload::Battery(BatteryPayload {
                slot: BatterySlot::ChargeFromGrid,
                export_enabled: false,
                inverter_on: true,
                charge_kw: 3.0,
                discharge_kw: 0.0,
                min_soc_kwh: 2.0,
                max_soc_kwh: 10.0,
                soc_kwh: Some(6.1),
            }),
        }
    }

    #[test]
    fn control_command_round_trips() {
        let cmd = battery_command();
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ControlCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn payload_kinds_are_tagged() {
        let heating = Payload::Heating {
            zones: vec![ZoneHeat {
                zone: "livingroom".into(),
                power_kw: 2.4,
                on: true,
            }],
        };
        let json = serde_json::to_string(&heating).unwrap();
        assert!(
            json.contains(r#""kind":"heating""#),
            "tagged on kind: {json}"
        );
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), heating);

        let load = Payload::Load {
            channels: vec![LoadChannel {
                channel: "ev".into(),
                power_kw: 7.0,
                enabled: true,
                target_c: None,
                target_soc: Some(80.0),
            }],
        };
        let json = serde_json::to_string(&load).unwrap();
        assert!(json.contains(r#""kind":"load""#));
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), load);

        let loxone = Payload::Loxone {
            writes: vec![LoxoneWrite {
                key: "MPCHeatChodbaDole".into(),
                value: 1.0,
            }],
        };
        let json = serde_json::to_string(&loxone).unwrap();
        assert!(
            json.contains(r#""kind":"loxone""#),
            "tagged on kind: {json}"
        );
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), loxone);
    }

    #[test]
    fn battery_slot_uses_loxone_vocabulary() {
        assert_eq!(
            serde_json::to_string(&BatterySlot::ChargeFromGrid).unwrap(),
            r#""charge_from_grid""#
        );
        assert_eq!(
            serde_json::to_string(&BatterySlot::SellProduction).unwrap(),
            r#""sell_production""#
        );
    }

    #[test]
    fn deadman_freshness() {
        let cmd = battery_command(); // valid_until 12:16:30
        assert!(cmd.is_fresh(utc("2026-06-23T12:10:00Z")));
        assert!(
            !cmd.is_fresh(utc("2026-06-23T12:16:30Z")),
            "expiry is exclusive"
        );
        assert!(!cmd.is_fresh(utc("2026-06-23T12:20:00Z")));
    }

    #[test]
    fn version_compatibility_is_by_major() {
        assert!(versions_compatible("1.0", "1.3"));
        assert!(versions_compatible("1.9", SCHEMA_VERSION));
        assert!(!versions_compatible("2.0", "1.0"));
        let mut cmd = battery_command();
        assert!(cmd.version_compatible());
        cmd.schema_version = "2.0".into();
        assert!(!cmd.version_compatible());
    }

    #[test]
    fn supersedes_by_seq() {
        let a = battery_command(); // seq 7
        let mut b = battery_command();
        b.command_seq = 8;
        assert!(b.supersedes(&a));
        assert!(!a.supersedes(&b));
        assert!(!a.supersedes(&a)); // equal seq does not supersede
    }

    #[test]
    fn status_omits_null_telemetry() {
        let status = ControllerStatus {
            schema_version: SCHEMA_VERSION.to_string(),
            controller_id: "heating".to_string(),
            at: utc("2026-06-23T12:00:00Z"),
            mode: Mode::DryRun,
            last_command_at: None,
            deadman_expired: false,
            telemetry: serde_json::Value::Null,
            actions: vec![],
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("telemetry"),
            "null telemetry skipped: {json}"
        );
        assert!(!json.contains("last_command_at"));
        assert!(json.contains(r#""mode":"dry_run""#));
    }

    #[test]
    fn topic_helpers() {
        assert_eq!(topics::command("growatt"), "mpc/control/growatt");
        assert_eq!(topics::status("heating"), "mpc/status/heating");
        assert_eq!(topics::health("growatt"), "mpc/health/growatt");
    }

    #[test]
    fn accept_gates_version_addressee_ordering_and_deadman() {
        let now = utc("2026-06-23T12:10:00Z"); // before valid_until 12:16:30
        let cmd = battery_command(); // id "growatt", seq 7
        assert!(cmd.accept("growatt", Some(6), now).is_ok());
        assert!(cmd.accept("growatt", None, now).is_ok());

        assert!(
            cmd.accept("heating", Some(6), now).is_err(),
            "wrong addressee"
        );
        assert!(
            cmd.accept("growatt", Some(7), now).is_err(),
            "seq not newer"
        );
        assert!(
            cmd.accept("growatt", Some(8), now).is_err(),
            "older than last applied"
        );
        assert!(
            cmd.accept("growatt", Some(6), utc("2026-06-23T12:20:00Z"))
                .is_err(),
            "past the deadman"
        );

        let mut wrong_ver = battery_command();
        wrong_ver.schema_version = "2.0".into();
        assert!(
            wrong_ver.accept("growatt", Some(6), now).is_err(),
            "major mismatch"
        );
    }

    #[test]
    fn actions_changed_detects_target_and_message_differences() {
        let a = vec![PlannedAction {
            target: "t".into(),
            message: "1".into(),
            published: false,
            reason: "r".into(),
        }];
        assert!(!actions_changed(&a, &a.clone()));
        let mut b = a.clone();
        b[0].message = "2".into();
        assert!(actions_changed(&a, &b));
        assert!(actions_changed(&a, &[]));
    }
}
