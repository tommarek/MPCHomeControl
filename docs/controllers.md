# Controllers — the universal hardware-control protocol

The MPC (`mpc_home_control`) plans the whole house but is **strictly read-only**: it never touches
hardware. **Controllers** are the actuation layer — one per subsystem (battery/inverter, heating,
HVAC, …) and hardware (Growatt, Loxone, future HCSC). Each speaks its **own** device protocol but a
**single, universal, language-agnostic protocol** to the MPC, so a controller can be written in Rust,
Python, or anything.

> **Two-key actuation.** The `growatt` (battery/inverter), `loxone` (heating + EV UDP edge),
> and `publisher` (north bridge) controllers actuate the house; `heating`/`ev` (superseded
> by the unified `loxone`) and `boiler` (stub) stay dry-run. Each **hardware** controller (`growatt`,
> `loxone`) needs BOTH keys before it touches a device — the config `armed: true` flag **and** the
> `MPC_CONTROLLER_ARM=i-understand-this-actuates` env token. The `publisher` only bridges plans onto the
> inert `mpc/control/...` MQTT namespace, so it gates on its `armed` flag alone; nothing reaches hardware
> without a downstream controller's two keys. Before arming the `loxone` controller the Loxone-side wiring
> (the `MPCActive` watchdog + `failsafe: hold`, see below) must be in place and the Miniserver's own
> heating control turned OFF, so the MPC is the single master.

## Architecture

```
mpc_home_control (read-only, UNCHANGED) ── GET /api/plan/latest ──▶  (HTTP JSON)
        │  (read-only HTTP)
        ▼
mpc-plan-publisher  (north bridge)
   poll the plan → ControlCommand (with a TTL) → publish MQTT  mpc/control/<ctrl>  (retained + LWT)
        │  (MQTT — the mpc/control namespace the hardware controllers consume)
        ├──▶ mpc-controller-growatt  ─ translate ▶  energy/solar/command/...   (Growatt MQTT; loxone's own Growatt control off)
        ├──▶ mpc-controller-heating  ─ translate ▶  UDP key=value ▶ Loxone Miniserver:4000  (NEW virtual inputs)
        ├──▶ mpc-controller-ev       ─ translate ▶  UDP key=value ▶ Loxone Miniserver:4000  (wallbox virtual inputs)
        └──▶ mpc-controller-loxone   ─ translate ▶  UDP key=value ▶ Loxone Miniserver:4000  (UNIFIED heating+EV+future; supersedes the two above)
```

The MPC binary has **no MQTT dependency** — it only serves its existing read-only API. The publisher
is the one component that puts commands on MQTT, into the `mpc/control/...` namespace that the
hardware controllers consume. They translate that into the real device protocols, each gated by its
own two-key arm.
This keeps the MPC's read-only guarantee **structural** (`cargo tree -p mpc_home_control` contains no
`rumqttc`).

## The protocol (`controllers/protocol`)

Plain JSON, transport-agnostic; MQTT is the reference transport. Every message shares a common
envelope; the command payload is a **tagged union on `kind`** so a new subsystem is a new variant.

### Operations (MQTT topics)

| Operation | Direction | Topic | Payload |
|---|---|---|---|
| **Command** | publisher → controller | `mpc/control/<id>` (retained) | `ControlCommand` |
| **Describe** | controller → MPC | `mpc/describe/<id>` (retained) | `Capability` |
| **Status** | controller → MPC | `mpc/status/<id>` | `ControllerStatus` |
| **Health** | controller → broker | `mpc/health/<id>` (MQTT Last-Will) | `online` / `offline` |
| **Failsafe** | implicit | — | on `valid_until` expiry / LWT, revert to safe |

### `ControlCommand`

```json
{
  "schema_version": "1.0",
  "controller_id": "growatt",
  "issued_at": "2026-06-23T12:00:00+00:00",
  "block_start": "2026-06-23T12:00:00+00:00",
  "valid_until": "2026-06-23T12:02:00+00:00",
  "plan_id": "2026-06-23T12:00:00+00:00",
  "command_seq": 42,
  "payload": { "kind": "battery", "slot": "charge_from_grid", "export_enabled": false,
               "inverter_on": true, "charge_kw": 3.0, "discharge_kw": 0.0,
               "min_soc_kwh": 2.0, "max_soc_kwh": 10.0, "soc_kwh": 6.1 }
}
```

- **`valid_until` is the deadman.** A controller applies a command only while `now < valid_until`, and
  reverts to its failsafe once it expires. It keys on the *timestamp*, not "a message arrived", so a
  repeated *stale* command still expires.
- **`command_seq`** is a monotonic counter from the publisher; a controller ignores a command whose
  seq it already applied (idempotency/ordering over at-least-once MQTT).
- **`schema_version`** — a controller refuses a command whose **major** differs.

### Payload catalogue (covers all sections)

```json
{ "kind": "battery", "slot": "...", "export_enabled": true, "inverter_on": true,
  "charge_kw": 0.0, "discharge_kw": 0.0, "min_soc_kwh": 2.0, "max_soc_kwh": 10.0, "soc_kwh": 6.1 }

{ "kind": "heating", "zones": [ { "zone": "livingroom", "power_kw": 2.4, "on": true } ] }

{ "kind": "hvac", "zones": [ { "zone": "bedroom", "cool_kw": 1.5, "heat_kw": 0.0, "mode": "cool" } ] }

{ "kind": "load", "channels": [ { "channel": "ev", "power_kw": 7.0, "enabled": true,
                                  "target_soc": 80.0 } ] }

{ "kind": "loxone", "writes": [ { "key": "MPCActive", "value": 1 },
                                { "key": "MPCHeatChodbaDole", "value": 1 },
                                { "key": "EvChargePower", "value": 3.6 } ] }
```

`battery.slot` is the loxone vocabulary: `regular | charge_from_grid | discharge_to_grid |
sell_production | battery_hold | inverter_off`. The generic `load` kind covers EV chargers, water
heaters, and any future flexible load without a protocol change. The `loxone` kind is a flat set of
virtual-input writes for the unified Loxone controller (below).

### `Capability` and `ControllerStatus`

A controller publishes a `Capability` (its actuators + bounds + supported modes) so the publisher can
clamp commands; and a `ControllerStatus` after each command — its `mode` (`dry_run`/`armed`), deadman
state, device telemetry, and the `actions` it took:

```json
{ "schema_version": "1.0", "controller_id": "growatt", "at": "…", "mode": "dry_run",
  "deadman_expired": false, "telemetry": { "soc_pct": 61.0 },
  "actions": [ { "target": "energy/solar/command/batteryfirst/set/powerrate",
                 "message": "{\"value\":57}", "published": false,
                 "reason": "charge 3.00kW/5.30kW = 57%" } ] }
```

Each `PlannedAction` is the audit record — computed and logged in both modes, with `published: false`
in dry-run and `true` after an armed send.

## The controllers

### Growatt (`mpc-controller-growatt`)

Translates a `battery` command into the real Growatt MQTT vocabulary (`energy/solar/command/...`).
loxone_smart_home's own Growatt control is OFF, so the MPC controller now owns the inverter — a
mutually-exclusive cut-over (never two controllers on one inverter). The translation:

| `slot` | Growatt MQTT |
|---|---|
| `regular` | `loadfirst/set/stopsoc {min%}` |
| `charge_from_grid` | `batteryfirst/set/{timeslot, stopsoc=max%, powerrate=pct(charge_kw), acchargeenabled=1}` |
| `discharge_to_grid` | `gridfirst/set/{timeslot, stopsoc=min%, powerrate=pct(discharge_kw)}` |
| `sell_production` | `gridfirst/set/{timeslot, stopsoc=100%, powerrate=pct(discharge_kw)}` (export PV, keep battery) |
| `battery_hold` | `batteryfirst/set/{timeslot, stopsoc=live-SoC%, acchargeenabled=0}` |
| `inverter_off` | `modbus/set {id:0, type:"16b", registerType:"H", value:0}` (short-circuit) |

**Mode exclusivity:** the non-selected battery-first/grid-first slot is explicitly disabled
(`…/set/timeslot {enabled:false}`) on every command, so the inverter is never left with two slots
enabled (mirrors loxone's `ensure_exclusive`). Plus the orthogonal `export/enable|disable
{value:true}` and the inverter on/off (`modbus` holding reg0). `pct(kw)` =
`round(kw / battery_power_max_kw × 100)` (battery power at 100%, ~9.8 kW), quantized to the integer
`powerrate` and floored at 1% for a nonzero setpoint. Live SoC comes from the controller's own
`energy/solar` subscription (fresher than the command's `soc_kwh`). On deadman expiry it reverts to
`regular` (or `hold`).

> **Implemented** (see issue #23): the command-ack/retry loop on `energy/solar/result`, the reserve-SoC floor,
> and the payload/exclusivity/powerRate fixes are landed. A dedicated broker-down actuation gate is still
> tracked as a refinement — today the `valid_until` deadman (revert to `regular` on command silence) is
> the broker-down backstop.

### Heating (`mpc-controller-heating`) — legacy single-zone path (superseded)

> **Superseded by `mpc-controller-loxone`** (the unified Loxone controller below). For new setups,
> configure the publisher's `loxone` block — not `heating`/`ev`. Kept for reference during migration.

This controller sends per-zone state as a single UDP **virtual-input** datagram to the Miniserver, in
the `key=value;…` format loxone already ingests for sensors:

```
mpc_heat_kitchen=0;mpc_heat_livingroom=1
```

The key for a zone is `mpc_heat_<zone>` (or a `zone_map` override). On deadman expiry it `hold`s
(stops sending — loxone's own logic resumes) or drives `all_off`.

#### Loxone-side wiring (you add this)

Because this path is new, add the receiving side in **Loxone Config**:

1. **A UDP input.** Under the Miniserver's network inputs, add a **Virtual UDP Input** listening on the
   port the controller targets (default `4000`). loxone already parses `key=value;key=value`.
2. **One Virtual Input Command per zone.** For each heated zone, add a *Virtual Input Command* that
   parses its key, e.g. recognises `mpc_heat_livingroom=\v` → a digital input that is `1` when on.
   Use the exact key the controller sends (`mpc_heat_<zone>`, or your `zone_map` value).
3. **Drive the relay.** Wire that virtual input into the zone's heating-relay logic — typically
   AND-ed with your existing thermostat/safety limits (a max-temperature cutout, a schedule guard)
   so the MPC requests heat but loxone keeps the safety interlocks.
4. **Failsafe.** Because the controller's deadman defaults to `hold` (it just stops sending), leave the
   zone's native loxone logic able to take over when the virtual input goes stale — e.g. fall back to a
   local thermostat after N minutes without an MPC update.

### EV (`mpc-controller-ev`) — the Loxone wallbox path

The publisher emits a `load` payload with one channel per charger **controllable on our wallbox right
now** (monitored / away cars carry none). `mpc-controller-ev` translates each channel into Loxone UDP
virtual inputs — `<stem>_kw` (modulating power setpoint), `<stem>_on` (enable), `<stem>_target`
(SoC %), where `<stem>` is `mpc_ev_<channel>` unless overridden in `channel_map`. A modulating wallbox
reads `_kw`; an on/off one reads `_on`. Wire those into the wallbox logic exactly as for heating
(AND-ed with your safety interlocks); the deadman defaults to `hold`. Full feature docs: [ev.md](ev.md).

### Boiler (`mpc-controller-boiler`) — controllable-load path (stub)

When a scheduled load is marked **`controllable`** (the boiler / hot-water scenario — see
[configuration.md](configuration.md#controllable-loads-load-shifting)), the MPC load-shifts it and
reports the per-block on/off schedule in `first_step.controllable_load_kw`. With a `boiler` block in
the publisher config, the publisher emits a generic **`load`** payload — one `LoadChannel` per
controllable load (`channel` = the load's label/zone, `power_kw` = the coming block's planned draw,
`enabled` = the MPC's on/off decision for that block — the publisher sets it from its
`on_threshold_kw`) — reusing the same `Payload::Load` envelope as the EV path.

`mpc-controller-boiler` subscribes `mpc/control/boiler`, gates the command exactly like every other
controller (version / addressee / `command_seq` / deadman), and translates the channels into a
device command. **It is a stub:** the real boiler hardware (a Modbus relay / smart socket) isn't wired
yet, so `translate` produces only a logged **would-send** record (`stub://<target>  <channel>_kw=…;
<channel>_on=…`) — it never drives a device, even with `armed: true`. The scaffold around it
(subscribe, two-key arm, deadman with `hold` / `all_off` failsafe, dry-run default, status/health) is
complete; when the Modbus boiler arrives, only `translate.rs` grows the real datagram. Config:
[`controllers/boiler/boiler.example.json5`](../controllers/boiler/boiler.example.json5).

### Loxone (`mpc-controller-loxone`) — the unified Loxone UDP path

One controller that owns the UDP edge to the Miniserver: **all** Loxone-bound actuation (heating
relays, EV power, future HVAC/boiler/shading) in one `key=value;…` datagram, exactly as
`mpc-controller-growatt` owns the inverter. It **supersedes** the separate `heating` + `ev`
controllers — configure the publisher's `loxone` block *or* `heating`/`ev`, not both.

The split is deliberate: the controller is a **generic writer** (it sends whatever `{key, value}`
writes it's handed via `Payload::Loxone`), and the publisher's `loxone` block owns the
plan-field → virtual-input-key mapping. So a new Loxone-driven actuation is a publisher config row,
never a controller change.

```json5
// publisher config — supersedes the heating/ev blocks
loxone: {
  controller_id: "loxone",
  heating: { on_threshold_kw: 0.05,
             zone_keys: { ground_hall: "MPCHeatChodbaDole", livingroom: "MPCHeatObyvak" /* … one per heated zone */ } },
  ev: { power_key: "EvChargePower" },
}
```

**The `MPCActive` gate (failsafe).** The controller prepends `MPCActive=1` to every datagram and
re-sends the live datagram on a **10 s heartbeat**, so a dead/disarmed brain stops the pulse train.
`AND` every MPC-driven output on the Loxone side with an "alive" signal derived from `MPCActive`, so a
silent brain reverts the whole house to native control. Two-key arm, deadman,
status/health — all as every other controller.

#### Loxone-side wiring (digital-input pulse → Off-Delay watchdog)

A Loxone virtual input *latches* an analog value — so a constant `MPCActive=1` would stick at `1`
forever and never detect silence. The robust pattern is a **pulse watchdog**:

```
MPCActive  (UDP virtual input, "Use as digital input" ✓)  →  Off Delay (30 s)  →  ALIVE
```

- **`MPCActive`** — wire it as a **digital input** (no `\v`): each received packet fires a brief pulse
  *regardless of value*, so the 10 s heartbeat is a clean pulse train. → an **Off Delay (~30 s)**
  retriggered by each pulse → `ALIVE`. A 10 s beat under a 30 s window tolerates **two** lost UDP
  packets before a false alarm.
- **value VIs** (`MPCHeatChodbaDole`, `EvChargePower`, …) — wire as **analog** (`…=\v`, you need the
  value), then gate each: **`value AND ALIVE`** → the relay / wallbox. When `ALIVE` drops, the `AND`
  forces native control regardless of the latched value.
- **`failsafe: "hold"`** (the default) suits this wiring: on the deadman the controller simply goes
  quiet, the Off Delay times out, and `ALIVE` drops. *Don't* use `release` here — its `MPCActive=0` is
  just one more pulse that retriggers the Off Delay and *delays* the fallback; `release` is only for an
  analog-*value* gate.
- **Failsafe timing:** controller/link death → native in ~Off-Delay (30 s). A publisher/brain crash is
  caught by the controller's own deadman first (`deadman_seconds`, publisher default 120 s) and *then*
  the Off Delay — worst case ≈ `deadman_seconds` + 30 s. Lower `deadman_seconds` (keep it ≥ ~3× the
  poll) for a faster brain-death fallback.

Config: [`controllers/loxone/loxone.json5`](../controllers/loxone/loxone.json5); full
design + the virtual-input naming scheme in [loxone-controller-plan.md](loxone-controller-plan.md).

## Dry-run, arming, and the deadman (safety)

- **Dry-run is the default everywhere.** A controller computes and logs the device messages it *would*
  send (`would-send …`) and touches nothing.
- **Arming is two-key** on the hardware controllers: the config `armed: true` **and** the environment
  variable `MPC_CONTROLLER_ARM=i-understand-this-actuates`. Neither alone actuates; a loud banner
  states the resolved mode at startup.
- **The deadman** (`valid_until`) means a stalled publisher/MPC causes controllers to revert and hand
  control back. MQTT Last-Will additionally signals a crashed component.

## Write a controller in any language

Any program that can speak MQTT + JSON can be a controller. It must:

1. Subscribe to `mpc/control/<your-id>` and parse the `ControlCommand` JSON.
2. **Gate** each command exactly as `ControlCommand::accept` does: refuse a different `schema_version`
   major; ignore one not addressed to you; ignore a `command_seq` ≤ the last you applied; ignore one
   past `valid_until`.
3. Translate the payload to your hardware — but only actually send when *you* are armed.
4. Revert to a safe state when the current command's `valid_until` passes.
5. (Optional) publish `mpc/status/<id>`, `mpc/describe/<id>`, and an `mpc/health/<id>` Last-Will.

A complete ~40-line example is in
[`controllers/examples/python_controller_stub.py`](../controllers/examples/python_controller_stub.py).

## Running the pipeline locally (dry-run)

The committed configs are **armed** (`armed: true`), but the hardware controllers also need the
`MPC_CONTROLLER_ARM` env token — so **with that token unset they stay dry-run**, even at `armed: true`.
For a safe local run, point at a **local** broker (never the house broker or loxone) and **do not**
export `MPC_CONTROLLER_ARM`:

```bash
# 1) a local broker (e.g. `mosquitto`), and the MPC serving its read-only API on :3000
cargo run -- serve

# 2) the publisher (armed → publishes to the LOCAL broker's mpc/control namespace)
cargo run -p mpc-plan-publisher -- controllers/publisher/publisher.json5

# 3) the controllers — dry-run here because MPC_CONTROLLER_ARM is unset (log the would-send messages)
cargo run -p mpc-controller-growatt -- controllers/growatt/growatt.json5
cargo run -p mpc-controller-heating -- controllers/heating/heating.json5
cargo run -p mpc-controller-ev      -- controllers/ev/ev.example.json5
cargo run -p mpc-controller-boiler  -- controllers/boiler/boiler.example.json5
cargo run -p mpc-controller-loxone  -- controllers/loxone/loxone.json5
```

With `MPC_CONTROLLER_ARM` unset (and a local broker), you can watch the whole pipeline — the publisher
posting `mpc/control/...`, each hardware controller logging the exact device messages it *would* send —
without anything reaching real hardware.
