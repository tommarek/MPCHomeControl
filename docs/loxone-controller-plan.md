# Unified Loxone controller — plan

> **Actuation is two-key** (`armed: true` + the `MPC_CONTROLLER_ARM` env token; the gate).
> A single `controllers/loxone` crate that owns the UDP edge to the
> Loxone Miniserver — **all** Loxone-bound actuation (heating, EV power, and whatever comes later) in
> one place, exactly mirroring how `controllers/growatt` owns the inverter. It supersedes the separate
> `controllers/heating` + `controllers/ev` controllers and is the concrete realization of the
> **udp-out** half of the MQTT-migration design (`docs/mqtt-architecture.md`).

---

## 1. Goal

`controllers/growatt` is the model: one MQTT topic in (`mpc/control/growatt`), one device out (the
inverter), with the safety envelope (schema/seq/deadman gate, two-key arm, dry-run default, failsafe).
Do the same for Loxone: **one controller, one topic (`mpc/control/loxone`), one device (the Miniserver
over UDP)**, fronting every Loxone-driven output. Adding a future actuation (HVAC, a Loxone boiler
socket, blinds) must be a *config edit*, never a new controller.

Two virtual inputs are already wired on the Loxone side: `MPCHeatChodbaDole` (ground-hall heating) and
`EvChargePower` (EV power). The plan adopts that naming and fills in the rest (§7).

---

## 2. Architecture decision (the crux)

The Loxone Miniserver is a **generic virtual-input sink** — it just wants `key=value` pairs. So the
cleanest, most extensible split is:

- **The controller is a thin, safety-critical, *generic* UDP writer.** It receives a flat list of
  `(key, value)` writes and emits one `key=value;…` datagram. It knows nothing about heating vs EV —
  so it **never needs changing** when a new domain is added.
- **The publisher owns the semantic → VI-key mapping** (which plan field becomes which Loxone VI),
  driven by config. The publisher already reads the plan (`first_step.heat_kw`, `ev`, …) and already
  does the on/off threshold logic — so this is its natural job. A future domain = a few mapping lines
  in the publisher + the plan field it reads (the controller is untouched).

This mirrors `growatt` (the device-specific translation lives controller-side) **with one inversion
that's correct for Loxone**: because Loxone is a generic sink, the "translation" is just naming, and
naming belongs in config, not in compiled code. It's also the MQTT-migration design (`docs/mqtt-architecture.md`)
§6.2 mapping-table idea, implemented as publisher-config + a dumb writer.

> **Alternative considered:** a semantic composite payload (`Payload::Loxone { heating, ev, hvac }`)
> with the controller owning per-domain key maps (like `heating`/`ev` do today). Rejected for the main
> goal — every new domain would then need a controller change. Kept as a fallback in §10.

**Topic & device:** one `mpc/control/loxone` topic, `controller_id = "loxone"`, one `UdpClient` to
`192.168.0.200:4000`. One command → one datagram. One seq, one deadman.

---

## 3. Protocol addition (`controllers/protocol`)

One new payload variant, fully generic:

```rust
// controllers/protocol/src/lib.rs — Payload enum
Loxone { writes: Vec<LoxoneWrite> },

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LoxoneWrite {
    /// Exact Loxone virtual-input key, e.g. "MPCHeatChodbaDole", "EvChargePower".
    pub key: String,
    /// The value to write. f64 covers on/off (1/0), kW, and enum codes (e.g. HVAC mode 0/1/2).
    pub value: f64,
}
```

Everything else reuses the existing envelope: `ControlCommand { schema_version, controller_id,
issued_at, block_start, valid_until, plan_id, command_seq, payload }`, `ControlCommand::accept()`
(version → id → seq → deadman), `topics::{command,status,describe,health}`, `actions_changed`,
`PlannedAction`, `ControllerStatus`, `Mode`. `SCHEMA_VERSION` stays `1.x` (additive variant — old
controllers reject an unknown `kind`, which is the safe default).

---

## 4. The controller crate — `controllers/loxone`

A near-clone of `controllers/heating` (`main.rs` + `translate.rs` + `config.rs`), generalized from
"zones" to arbitrary key/value writes, plus a **heartbeat**.

### 4.1 `translate.rs` (pure, unit-tested)

Mirror `heating/translate.rs`: build one sorted `key=value;…` datagram, dropping any key containing
`;`/`=`/newline (the same corruption guard). The only new bit is **value formatting** — clean
integers, no trailing `.0`:

```rust
fn fmt_value(v: f64) -> String {
    if !v.is_finite() { return "0".into(); }              // never emit NaN/Inf
    if v.fract() == 0.0 { format!("{}", v as i64) }        // 1.0 → "1", on/off & enums stay crisp
    else { let s = format!("{v:.3}"); s.trim_end_matches('0').trim_end_matches('.').to_string() } // 3.60 → "3.6"
}
```

`translate(writes, target) -> Option<PlannedAction>` → `udp://<target>` + `"MPCHeatChodbaDole=1;…"`,
`reason` = `"N write(s)"`. `None` when there are no valid writes (mirrors heating's empty/all-dropped
handling).

### 4.2 `main.rs` (the runtime — clone heating's loop)

Reuse heating's `State` machine verbatim in shape:
- `on_command`: parse JSON → `ControlCommand::accept(&"loxone", last_seq, now)` → require
  `Payload::Loxone { writes }` → record seq/`valid_until`/`monotonic_deadline` → `translate` →
  `actions_changed` change-only skip → `apply`.
- `apply`: if armed, `UdpClient::send`; else log the would-send datagram; publish `ControllerStatus`.
- **Heartbeat:** every armed datagram **prepends `<heartbeat_key>=1`** (default `MPCActive=1`), and a
  **periodic timer re-sends the live datagram** (every ~10 s, the `HEARTBEAT_REFRESH` constant) so
  Loxone can treat a stale `MPCActive` as "brain gone" independently of the deadman. The refresh runs
  unconditionally **while armed and the gate hasn't been released** (it's not gated on the payload
  changing).
- `check_deadman`: on `valid_until` expiry → **send `<heartbeat_key>=0`** (release: Loxone reverts to
  native control across the board) and/or the configured `failsafe` (`hold` = also stop sending).
- Two-key arm (`armed` config + `MPC_CONTROLLER_ARM=i-understand-this-actuates`), MQTT LWT on
  `mpc/health/loxone`, re-subscribe on ConnAck — all identical to heating.

### 4.3 `config.rs`

```json5
// loxone.json5
{
  armed: false,                       // + MPC_CONTROLLER_ARM env → actually send
  mqtt:  { host: "127.0.0.1", port: 1883, client_id: "mpc-controller-loxone" },
  controller_id: "loxone",
  control_topic: "mpc/control/loxone",
  loxone: { host: "192.168.0.200", port: 4000 },
  heartbeat_key: "MPCActive",         // the global gate VI; "" disables it
  failsafe: "hold",                   // hold (go quiet — for the digital-input/Off-Delay wiring) | release (MPCActive=0, analog-value gate)
}
```

No `zone_map`/`key_prefix` here — the keys arrive fully-formed from the publisher (that's the point).

---

## 5. Publisher addition (`controllers/publisher`)

A new optional `loxone` config block + a builder in `build.rs::commands()` that maps `first_step` →
`Payload::Loxone { writes }`. It **supersedes** the `heating` + `ev` blocks (configure `loxone` *or*
those, never both — else double-actuation). `growatt` (the inverter) is unaffected.

```json5
// publisher.json5 — the new block
loxone: {
  controller_id: "loxone",
  heating: {
    on_threshold_kw: 0.05,            // kW → 1/0
    zone_keys: {                      // MPC zone (English) → Loxone VI (the translation point)
      ground_hall: "MPCHeatChodbaDole",
      livingroom:  "MPCHeatObyvak",
      bedroom:     "MPCHeatLoznice",
      // … one per heated zone (§7)
    },
  },
  ev: { power_key: "EvChargePower" }, // first-block charge_kw of the controllable charger → kW
  // future: hvac: {...}, boiler: {...}, shading: {...}
}
```

Builder sketch (mirrors the existing `heating`/`ev` arms in `build.rs`):

```rust
if let Some(lx) = &cfg.loxone {
    let mut writes = Vec::new();
    // heating: one write per zone that has a configured VI key
    for (zone, &kw) in &fs.heat_kw {
        if let Some(key) = lx.heating.zone_keys.get(zone) {
            writes.push(LoxoneWrite { key: key.clone(),
                value: f64::from(kw > lx.heating.on_threshold_kw) }); // 1/0
        }
    }
    // ev: the controllable charger's first-block power
    if let Some(key) = &lx.ev.as_ref().map(|e| &e.power_key) {
        if let Some(c) = api.data.ev.iter().find(|c| c.controllable_now && !c.charge_kw.is_empty()) {
            writes.push(LoxoneWrite { key: key.clone(), value: c.charge_kw[0] });
        }
    }
    writes.sort_by(|a, b| a.key.cmp(&b.key));   // deterministic
    out.push((lx.controller_id.clone(), envelope(&lx.controller_id, Payload::Loxone { writes })));
}
```

Adding a future domain = read its `first_step` field + push more `LoxoneWrite`s (and a config
section). The controller never changes.

---

## 6. The `MPCActive` safety gate (the unified failsafe)

A single heartbeat VI is the whole-house safety interlock, simpler and stronger than per-domain off
values:

- The controller sends `MPCActive=1` with every armed datagram and refreshes it on a timer.
- On deadman / disarm / process death (MQTT LWT), `MPCActive` goes `0` (driven, *and* Loxone can also
  age it out on its own staleness timer).
- **On the Loxone side, every MPC-driven output is `AND`-ed with `MPCActive`:** `if MPCActive then use
  the MPC value else native control`. So a dead, disarmed, or stale brain → the house falls back to
  Loxone's own logic everywhere at once.

This is the failsafe equivalent of growatt's "revert to regular," generalized across all domains.

---

## 7. Virtual-input naming scheme (the full set)

Convention: **`MPC<Domain><Detail>`**, CamelCase, Czech room names (matching your `MPCHeatChodbaDole`).
The publisher's `zone_keys` map is the single translation point (MPC English zone → Loxone Czech VI),
mirroring how `zone_mappings` already translates for sensors.

**Gate (1):**
| VI | Value | Meaning |
|---|---|---|
| `MPCActive` | 1/0 | brain alive **and** armed — gate everything below on it |

**Heating — one per heated zone (`config.heating.zones`), value 1/0** (all 17 relay-heated rooms):

| MPC zone | Loxone VI | | MPC zone | Loxone VI |
|---|---|---|---|---|
| ground_hall ✓ | `MPCHeatChodbaDole` | | office | `MPCHeatPracovna` |
| entrance | `MPCHeatZadveri` | | first_floor_hall | `MPCHeatChodbaNahore` |
| ground_closet | `MPCHeatSatnaDole` | | first_floor_bathroom | `MPCHeatKoupelnaNahore` |
| technical_room | `MPCHeatTechnickaMistnost` | | first_floor_closet | `MPCHeatSatnaNahore` |
| toilet | `MPCHeatZachod` | | bedroom | `MPCHeatLoznice` |
| ground_bathroom | `MPCHeatKoupelnaDole` | | room_1 | `MPCHeatPokoj1` |
| livingroom | `MPCHeatObyvak` | | room_2 | `MPCHeatPokoj2` |
| kitchen | `MPCHeatKuchyne` | | guestroom | `MPCHeatHosti` |
| storage | `MPCHeatSpajz` | | | |

`attic`, `garrage`, `outside` are **not heated** — no VI. **Dormancy:** a room registered in
`config.heating.zones` + `zone_keys` still produces **no datagram** until its `model.json5` floor
boundary carries a `"heating"` marker (the heated set is the intersection of marker ∩ config ∩ state).
The 7 first-floor rooms are now **active** — their `model.json5` floor boundaries (with the `"heating"` marker) have landed.

**EV (kW):** `EvChargePower` ✓ *(suggest `MPCEvPower` for prefix consistency — your call; it's one
config row either way)*.

**Future (config rows when they arrive — controller untouched):**
| Domain | VI(s) | Value |
|---|---|---|
| Reversible HVAC/AC | `MPCHvac<Room>Mode` | 0 off / 1 cool / 2 heat |
| Hot-water boiler (Loxone smart socket) | `MPCBoilerOn` | 1/0 (the controllable-load decision, PR #27) |
| Shading (overheating / passive-solar) | `MPCShade<Room>` | 0..1 blind position |

---

## 8. Reuse map (don't reinvent — cite)

- `controller_common::{UdpClient, monotonic_deadline, default_mqtt_host/port, default_loxone_host/port}`
  (`controllers/common/src/lib.rs`).
- `controller_protocol::{ControlCommand, accept, topics, actions_changed, PlannedAction,
  ControllerStatus, Mode, SCHEMA_VERSION}` + the new `Payload::Loxone` / `LoxoneWrite`
  (`controllers/protocol/src/lib.rs`).
- `controllers/heating/src/{main.rs, translate.rs, config.rs}` — the template; the new crate is heating
  generalized to arbitrary writes + the heartbeat.
- `controllers/publisher/src/{build.rs::commands, config.rs}` — add the `loxone` block + builder arm
  (the `heating`/`ev` arms are the pattern).

## 9. Critical files

- `controllers/protocol/src/lib.rs` — `Payload::Loxone`, `LoxoneWrite` (+ tests).
- `controllers/loxone/` (new) — `Cargo.toml`, `src/{config.rs, translate.rs, main.rs}`, `loxone.json5`.
- `controllers/publisher/src/{config.rs, build.rs}` — `LoxonePub` block + builder; deprecate `heating`/`ev`.
- root `Cargo.toml` — add `controllers/loxone` to the workspace.
- `docs/controllers.md` — document the unified controller, the `MPCActive` gate, the VI scheme.
- (later) delete `controllers/heating`, `controllers/ev`.

---

## 10. Migration & rollout (shadow-first, non-disruptive)

> **Status: complete.** The phases below are the (now-finished) shadow-first
> rollout sequence; the controller actuates behind the two-key gate + the Loxone-side `MPCActive` watchdog.

1. **Build** the protocol variant + `controllers/loxone` + the publisher `loxone` block. Dry-run by
   default; nothing actuates. CI stays green (the core MPC stays MQTT-free — controllers own MQTT).
2. **Dry-run validation:** run `mpc-controller-loxone` against the live `mpc/control/loxone`; confirm
   it logs exactly `MPCActive=1;EvChargePower=…;MPCHeatChodbaDole=…;…` — i.e. it emits the strings your
   two wired VIs expect, plus the gate.
3. **Raw-UDP test** (already covered) proves the Loxone side receives those keys.
4. **Wire the Loxone side:** `MPCActive` gates every MPC output; each `MPCHeat<Room>` is `AND`-ed with
   its interlocks. Add VIs zone-by-zone.
5. **Arm** (`armed:true` + `MPC_CONTROLLER_ARM`) → it sends for real; watch the VIs update, `MPCActive`
   holding the gate. Deadman/disarm drops `MPCActive`.
6. **Retire** `controllers/heating` + `controllers/ev` once `loxone` is proven (and remove their
   publisher blocks). `growatt` stays; a future Loxone-socket boiler folds into `loxone`.

---

## 11. Decisions to confirm (recommendation in **bold**)

1. **Payload shape:** **generic VI-write bag** (controller never changes per domain) vs. semantic
   composite (controller owns maps). *I recommend the bag (§2).*
2. **Heating value:** **on/off `1/0`** (underfloor is a relay) vs. modulating kW. *Bag supports both;
   start 1/0.*
3. **EV VI name:** keep **`EvChargePower`** vs. rename `MPCEvPower` for prefix consistency. *Cosmetic —
   one config row.*
4. **Failsafe:** **`hold` (go quiet → the Loxone Off-Delay watchdog times the gate out)** — the right
   fit for the recommended digital-input/pulse wiring. `release` (`MPCActive=0`) is only for an
   analog-*value* gate; in pulse mode it's a counter-productive extra pulse. *Decided: `hold`.*
5. **Retire heating/ev now** vs. keep during migration. *Recommend keep until `loxone` is armed-proven,
   then delete.*

## 12. Verification / keystone tests

- **translate:** writes → sorted `key=value;…`, clean value formatting (`1` not `1.0`, `3.6` not
  `3.600`), malformed keys dropped, empty → `None`.
- **publisher:** a sample `/api/plan/latest` → one `mpc/control/loxone` command whose writes include
  the heartbeat-free payload (`MPCHeatChodbaDole=1` when `heat_kw[ground_hall] > threshold`,
  `EvChargePower=<first-block kW>`), deterministically ordered.
- **deadman:** after `valid_until`, the controller emits `MPCActive=0` (release) / stops (hold).
- **arm gate:** dry-run logs but never sends; armed requires both keys.
- **no-MQTT gate:** `cargo tree -p mpc_home_control` stays MQTT-free (the controller is a separate
  crate).
- Full workspace `cargo test` / `clippy --all-targets -D warnings` / `fmt`; the repo's iterative
  multi-agent review after the build; commit clean (no AI attribution).

## 13. Out of scope (this controller)

Loxone→MQTT **ingress** (sensors) — that's the gateway's `udp-in`, a separate piece (see the
MQTT-migration design, `docs/mqtt-architecture.md`). The real HVAC/boiler/shading domains (their plan
fields + config rows) land when that hardware does. Arming is a deliberate, separate step.
