# MQTT architecture & the Loxone UDP→MQTT migration (design)

> **Status: partially implemented.** The `controllers/` actuation side (publisher → MQTT → the
> growatt/loxone controllers) is built; the broader Loxone UDP⇄MQTT gateway
> migration below is still planned. This document defines (1) the target MQTT topic
> structure for the whole house — Loxone read/write *and* MPC-brain read/write — and (2) the plan for
> a self-hosted gateway repo that converts Loxone UDP ⇄ MQTT and persists everything to InfluxDB.
> It is grounded in what the system actually reads and writes today (`loxone_smart_home` Python,
> `mpc_home_control` Rust, the `controllers/` + `adapters/` workspace). Decisions still open for the
> user are collected in **§9**.

---

## 1. Goal & non-negotiables

**Goal.** Make **MQTT the single message bus** for the house. Today Loxone speaks **UDP** in both
directions (Loxone → UDP listener → InfluxDB; and MQTT-bridge → UDP → Loxone virtual inputs). We want
Loxone's data to land on a clean, well-structured `loxone/…` topic tree, the MPC brain to read those
topics and publish its control intent, and a thin edge gateway to do the UDP↔MQTT translation Loxone
still requires.

**Non-negotiables (carried from the existing design):**

1. **Production stays untouched during the migration.** The live Python `loxone_smart_home` operates
   the house *right now*. Everything new runs **in parallel / shadow** until explicitly cut over
   (see `memory/mpchc-no-disrupt-production.md`). The migration is phased so the house never loses
   its data path.
2. **The MPC core stays MQTT-free.** `mpc_home_control` must not link an MQTT crate — it's
   CI-enforced (`cargo tree -p mpc_home_control | grep rumqttc` must be empty,
   `.github/workflows/ci.yml`). The brain participates in MQTT **only through sidecars**: it *reads*
   via the bridge→InfluxDB (or `mqtt-source`→HTTP) and *writes* via the `publisher` controller. This
   structural read-only property is a feature, not an accident — keep it.
3. **The InfluxDB schema is preserved verbatim.** The MPC's Flux queries (`influxdb.rs`,
   `live_inputs.rs`, the `zone_mappings`) key on exact `bucket / measurement / field / tag` names.
   The historian writes the **same** points, so the MPC's reads don't change at all. The bus swap is
   invisible to the optimizer.
4. **Every actuation keeps its safety envelope.** The `ControlCommand` deadman (`valid_until`),
   monotonic `command_seq`, two-key arm (`armed` config + `MPC_CONTROLLER_ARM` env), dry-run default,
   and per-controller failsafe all carry over unchanged.

---

## 2. Where we are today (the starting point)

```
            ┌─────────────────────── Loxone Miniserver (192.168.0.200) ──────────────────────┐
   sensors  │  UDP virtual outputs  ── :2000 ─▶                          ◀─ :4000  virtual    │
            └──────────────────────────────────│──────────────────────────────────│──────────┘
                                                ▼                                  ▲
                                   ┌── udp_listener (Py) ──┐          ┌── mqtt_bridge (Py) ──┐
                                   │ parse "ts;name;val;   │          │ JSON → key=val; →    │
                                   │ room;type;t1;t2"      │          │ UDP to :4000         │
                                   └──────────│────────────┘          └──────────▲───────────┘
                          write loxone bucket │  republish loxone/status         │ subscribe
                                              ▼  (JSON, every 30 s)              │ energy/solar,
                                          InfluxDB ◀──────────────── MQTT broker (mosquitto) ── teplomer/*
                                              ▲                         ▲   ▲
            energy/solar (Growatt) ───────────┘   weather, ote ─────────┘   └── teslamate/cars/+
```

- **Ingress:** Loxone UDP packet = `timestamp;measurement_name;value;room;measurement_type;tag1;tag2`
  (semicolon-delimited, Prague-time stamp). Written to bucket **`loxone`**, `measurement =
  measurement_type` (`temperature`/`humidity`/`relay`/`ev`/`presence`/`brightness`/…), `field =
  measurement_name`, tags `room`/`tag1`/`tag2`. Room names are **Czech** (`obyvak`, `loznice`, …).
- **Republish:** `udp_listener` already mirrors its whole cache to **`loxone/status`** (one JSON blob,
  every 30 s). ← *this is the hinge that makes a non-disruptive migration possible (see §8).*
- **Egress:** `mqtt_bridge` subscribes a configured topic list (`energy/solar`, `teplomer/TC`, …),
  flattens JSON to `key=value;…`, and sends it as UDP to Loxone `:4000`.
- **Other producers already on MQTT:** `energy/solar` (Growatt telemetry JSON, ~5 s), `energy/solar/
  command/*` (Growatt control), `weather` (forecast JSON), `teplomer/TC`+`teplomer/RH` (plain floats),
  `teslamate/cars/1/+` (EV).
- **Broker:** mosquitto on the loxone `caddy_net` docker network, host `mqtt`, port `1883`, no auth
  (internal network).
- **The MPC control plane already exists** (`controllers/protocol`): `mpc/control/<id>`,
  `mpc/status/<id>`, `mpc/describe/<id>`, `mpc/health/<id>` (LWT), JSON `ControlCommand` envelope.

So we already have a partial MQTT bus and a proven JSON control protocol. The migration is mostly
about (a) moving Loxone **ingress** onto a structured `loxone/…` tree, (b) moving Loxone **egress**
from UDP-from-Python to MQTT-from-controllers, and (c) consolidating the UDP↔MQTT edge into one
self-hosted gateway.

---

## 3. Target architecture

```
        ┌──────────────────────── Loxone Miniserver (192.168.0.200) ────────────────────────┐
        │   UDP virtual outputs ── :2000 ─▶                        ◀─ :4000  virtual inputs   │
        └──────────────────────────────│──────────────────────────────────────│──────────────┘
                                        ▼                                      ▲
                          ╔═════════════╪══════════════════════════════════════╪═════════════╗
                          ║   loxone-mqtt-gateway  (NEW self-hosted repo)       │             ║
                          ║   ┌─ udp-in ──────────┐            ┌─ udp-out ──────┴──────────┐  ║
                          ║   │ parse UDP → publish│            │ subscribe loxone/cmd/# →  │  ║
                          ║   │ loxone/<…> (JSON)  │            │ key=val; → UDP :4000      │  ║
                          ║   └─────────│──────────┘            └────────────▲──────────────┘  ║
                          ╚═════════════╪═════════════════════════════════════╪════════════════╝
                                        ▼  publish                            │ publish
   ┌────────────────────────────────  MQTT broker (mosquitto, caddy_net)  ────┴───────────────────┐
   │  TELEMETRY            loxone/climate/<room>/temperature, …  energy/solar  weather/forecast    │
   │  COMMANDS (→Loxone)   loxone/cmd/heating/<zone>, loxone/cmd/ev/<charger>/…                     │
   │  CONTROL (brain↔ctl)  mpc/control/<id>   mpc/status/<id>   mpc/health/<id>   mpc/describe/<id> │
   │  BRAIN STATE          mpc/plan/latest   mpc/plan/timeline                                      │
   └───────▲────────────────────────▲───────────────────────────────▲───────────────▲─────────────┘
           │ subscribe loxone/#,…    │ subscribe mpc/control/<id>     │ poll          │ subscribe
   ┌───────┴─────────┐     ┌─────────┴──────────┐         ┌──────────┴────┐   ┌───────┴──────────┐
   │ historian       │     │ controllers/*      │         │ publisher     │   │ mqtt-source      │
   │ (MQTT→InfluxDB) │     │ heating/ev/growatt │         │ /api/plan →   │   │ (MQTT→HTTP for   │
   │ schema-preserving│    │ /boiler → loxone/cmd│        │ mpc/control/* │   │  live-only vals) │
   └───────│─────────┘     └────────────────────┘         └───────────────┘   └────────│─────────┘
           ▼                                                                            ▼
       InfluxDB ◀──────────────── MPC brain reads (influxdb.rs, Flux) ────────────── HTTP GET
                                  *** mpc_home_control stays MQTT-free ***
```

**Key moves vs. today:**

- `udp_listener` (Py) ⟶ **gateway `udp-in`** that publishes a *structured* `loxone/…` tree (not one
  `loxone/status` blob).
- `mqtt_bridge` (Py, MQTT→UDP) ⟶ **gateway `udp-out`** that subscribes `loxone/cmd/#` and drives the
  Loxone virtual inputs. The *controllers* publish `loxone/cmd/…` instead of sending UDP themselves.
- A **historian** subscribes the telemetry plane and writes InfluxDB with the **current schema**, so
  the MPC's reads are unchanged. (This can be the existing `adapters/mqtt-bridge` generalized, or a
  component of the gateway — see §9.)
- The brain's control plane (`mpc/control/*`) and the `publisher`/controllers are **already built** —
  they slot straight in. The only controller change is heating/EV emitting `loxone/cmd/…` (MQTT)
  rather than raw UDP.

---

## 4. Design principles for the topic tree

1. **Identity in the topic, value in the payload.** One signal = one topic. Hierarchy encodes *what
   and where*; the payload carries *the value + metadata*. (Not one fat JSON per room — that defeats
   selective subscription and retained-state semantics.)
2. **Producer-rooted namespaces.** `loxone/…` = anything Loxone emits; `energy/…` = the inverter;
   `weather/…`, `prices/…`, `teslamate/…` keep their roots. Don't relabel a producer's data into a
   foreign root — it makes the bridge a 1:1 transport and keeps ownership obvious.
3. **Loxone-native vocabulary on `loxone/…`.** Use the **Czech room names** Loxone already emits
   (`obyvak`, `loznice`, `technicka_mistnost`). The MPC's `zone_mappings` already translate those to
   English zone keys — so preserving them is what makes the historian a drop-in (§5.5). The
   translation lives in exactly one place (config), as it does today.
4. **Retained = current state; non-retained = events.** Sensor/telemetry topics are **retained**
   (a new subscriber, e.g. a restarted brain sidecar, gets the last value immediately). Commands are
   **retained** too (a reconnecting controller catches the latest), but bounded by the `valid_until`
   deadman so a stale retained command can never actuate. One-shot events (button presses) are
   non-retained.
5. **A self-describing JSON envelope** for every value (units, timestamp, source) so the bus is
   debuggable and the historian needs no out-of-band schema. The existing `mqtt-common` parser
   already supports JSON+pointer extraction, so this is compatible with the current bridge.
6. **QoS 1 everywhere that matters.** Telemetry QoS 1 retained; commands/health QoS 1; high-rate
   inverter telemetry may stay QoS 0 (matches today). Health uses MQTT **Last-Will** (`offline`).
7. **Versioned, stable, lowercase, `/`-delimited, `snake_case` levels.** No spaces, no PII, no
   wildcards in published topics.

---

## 5. The MQTT topic tree (the concrete structure)

Four planes: **telemetry** (house → bus), **loxone-commands** (brain → Loxone), **control** (brain ↔
controllers, already exists), and **brain-state** (brain → observers).

### 5.1 Payload envelope

Every telemetry/command value is a small JSON object (retained):

```json
{ "v": 22.5, "ts": "2026-06-26T12:00:00Z", "u": "degC", "src": "loxone" }
```

| field | meaning | required |
|---|---|---|
| `v`  | the value (number, or bool→`true/false`, or string for enums) | yes |
| `ts` | ISO-8601 UTC timestamp of the reading | yes |
| `u`  | unit (`degC`, `%`, `kW`, `W`, `lux`, `bool`, …) | optional |
| `src`| origin (`loxone`, `growatt`, `mpc`, …) for provenance | optional |

> *Bare-scalar fallback:* Loxone's own MQTT publisher (Gen2) can only emit a bare value per virtual
> output. Where a topic is published directly by Loxone (not via the gateway), a **bare scalar**
> (`22.5`) is accepted — `mqtt-common` already parses bare scalars. The gateway wraps bare Loxone
> values into the envelope on the way through. Recommendation in §9.

### 5.2 Telemetry plane — `loxone/…` (and sibling producer roots)

`loxone/<domain>/<room>/<metric>` — room is the **Loxone-native** identifier.

| Topic | Example `v` | Persisted to InfluxDB (bucket / measurement / tags / field) |
|---|---|---|
| `loxone/climate/<room>/temperature` | `22.5` | `loxone` / `temperature` / `{room}` / `temperature` |
| `loxone/climate/<room>/humidity` | `45` | `loxone` / `humidity` / `{room}` / `humidity` |
| `loxone/climate/venek/temperature` | `18.3` | `loxone` / `temperature` / `{room:venek}` / `temperature_outside` |
| `loxone/heating/<room>/relay` | `1` | `loxone` / `relay` / `{room, tag1:heating}` / `relay` |
| `loxone/heating/<room>/flow_temp` | `34.0` | `loxone` / `temperature` / `{room, tag1:heating}` / `flow_temp` |
| `loxone/presence/<room>/occupied` | `1` | `loxone` / `presence` / `{room}` / `presence` |
| `loxone/light/<room>/brightness` | `1200` | `loxone` / `brightness` / `{room}` / `brightness` |
| `loxone/shading/<room>/position` | `0.8` | `loxone` / `shading` / `{room}` / `position` |
| `loxone/ev/<charger>/connected` | `1` | `loxone` / `ev` / `{charger}` / `ev_connected` |
| `loxone/ev/<charger>/power` | `3.6` | `loxone` / `ev` / `{charger}` / `ev_charging_power` |
| `loxone/ev/<charger>/session_kwh` | `8.2` | `loxone` / `ev` / `{charger}` / `ev_session_energy` |
| `loxone/water/<device>/temperature` | `61.0` | `loxone` / `temperature` / `{room, tag1:water}` / `water_temp` |
| `loxone/water/<device>/power` | `1.6` | `loxone` / `power` / `{device}` / `water_power` *(boiler smart-socket draw)* |
| `loxone/weather/rain` | `0` | `loxone` / `rain` / `{}` / `rain` |
| `loxone/weather/wind_speed` | `4.2` | `loxone` / `wind_speed` / `{}` / `wind_speed` |
| `loxone/weather/storm_warning` | `0` | `loxone` / `storm_warning` / `{}` / `storm_warning` |

Sibling producer roots (kept as-is; the historian persists them; the gateway never relabels them):

| Topic | Producer | Persisted to |
|---|---|---|
| `energy/solar` | Growatt inverter (JSON telemetry) | `solar` / `solar` / … (`SOC`, `InputPower`, `GridPower`, …) |
| `energy/solar/command/#` | Growatt control (south of the growatt controller) | — (control) |
| `weather/forecast` | open-meteo scraper (JSON, hourly) | `weather_forecast` / `weather_forecast` / `{room:outside,type:hour}` / `temperature_2m`, `cloudcover`, … |
| `prices/ote/spot` | OTE collector | `ote_prices` / `electricity_prices` / `{}` / `price` |
| `teslamate/cars/<id>/#` | TeslaMate | `ev` / `teslamate` / `{car}` / `battery_level`, `charge_limit_soc`, … |

> The `<room>` set is fixed and canonical — the Czech room tags already in the `loxone` bucket:
> `zadveri, satna_dole, technicka_mistnost, chodba_dole, zachod, koupelna_dole, obyvak, kuchyne,
> spajz, pracovna, satna_nahore, koupelna_nahore, loznice, pokoj_1, pokoj_2, chodba_nahore, hosti,
> garaz, puda, venek`. The MPC's `zone_mappings` map these to its English zone keys.

### 5.3 Loxone-command plane — `loxone/cmd/…` (brain → Loxone)

What the brain wants Loxone to actuate. **Simple** topics (one value each) so Loxone can subscribe
natively *or* the gateway's `udp-out` can convert them to virtual inputs. These replace the UDP
virtual-input keys (`mpc_heat_<zone>`, `mpc_ev_<ch>_kw`) the heating/EV controllers send today.

| Topic | Example `v` | Replaces UDP virtual input | Consumed by |
|---|---|---|---|
| `loxone/cmd/heating/<room>/on` | `1` | `mpc_heat_<room>` | Loxone relay logic |
| `loxone/cmd/heating/<room>/power_kw` | `2.4` | (modulating) | Loxone |
| `loxone/cmd/ev/<charger>/power_kw` | `3.6` | `mpc_ev_<ch>_kw` | Loxone wallbox — **`0` = stop** (the only EV command this house wires) |
| `loxone/cmd/ev/<charger>/enable` | `1` | `mpc_ev_<ch>_on` | Loxone wallbox — *optional; unused here* |
| `loxone/cmd/ev/<charger>/target_soc` | `80` | `mpc_ev_<ch>_target` | Loxone wallbox — *optional; needs car SoC, unavailable here* |
| `loxone/cmd/water/<device>/enable` | `1` | — (new: boiler smart socket) | Loxone / smart socket |
| `loxone/cmd/hvac/<room>/mode` | `"cool"` | — (future) | Loxone / AC |

**This house — EV is power-only.** The wallbox is `control: modulating`; feeding `power_kw = 0` to
its Loxone limit input (`Lm`) stops charging cleanly (tested), so the **power command alone is the
on/off** — no separate `enable` is wired. `target_soc` is also unused (no car SoC is available). The
MPC controller still emits `mpc_ev_<ch>_on` / `_target` regardless; they're simply left unconnected on
the Loxone side (a harmless, ignored virtual input). The brain reads `ev_connected` +
`ev_charging_power` back from InfluxDB (UDP telemetry) as feedback — those are *inputs*, not virtual
inputs to create. *(If a future wallbox can't stop at 0, the explicit `enable` maps to the Wallbox
block's `Ls` load-shedding input, inverted.)*

**Authority & safety on the Loxone side (unchanged principle):** every `loxone/cmd/…` value is
*advisory* — Loxone AND-s it with its own interlocks (temperature limits, schedules, manual
override). The deadman lives upstream (the controller stops publishing → the gateway stops driving
the VI → Loxone falls back to native control). A `loxone/cmd/…` topic carries a companion
`valid_until` either in the payload envelope or enforced by the publishing controller's cadence.

### 5.4 Control plane — `mpc/…` (brain ↔ controllers) — *already built, unchanged*

| Topic | Payload | QoS / retain | Direction |
|---|---|---|---|
| `mpc/control/<id>` | `ControlCommand` JSON (envelope + `payload`) | 1 / retain | publisher → controller |
| `mpc/status/<id>` | `ControllerStatus` JSON | 1 / no | controller → bus |
| `mpc/describe/<id>` | `Capability` JSON | 1 / retain | controller → bus |
| `mpc/health/<id>` | `online` / `offline` | 1 / retain (LWT) | controller ↔ broker |

`<id> ∈ { growatt, heating, ev, boiler, hvac }`. The `ControlCommand` envelope (`schema_version`,
`controller_id`, `issued_at`, `block_start`, `valid_until`, `plan_id`, `command_seq`, `payload`) and
its `accept()` gate (version → id → seq → deadman) are exactly as in `controllers/protocol`. This is
the **rich** control plane; the controllers translate it into the **simple** `loxone/cmd/…` or
`energy/solar/command/…` south-side topics.

### 5.5 Brain-state plane — `mpc/plan/…` (brain → observers) — *optional, new*

For dashboards/Grafana/Loxone-status displays that want the plan without polling HTTP:

| Topic | Payload | retain |
|---|---|---|
| `mpc/plan/latest` | the full plan JSON (same shape as `/api/plan/latest`) | retain |
| `mpc/plan/timeline` | the per-block rows (same as `/api/plan/timeline`) | retain |

Published by the `publisher` sidecar (it already polls `/api/plan/latest`). The MPC core still doesn't
touch MQTT — the sidecar does.

### 5.6 The two-hop command flow (worked example: heating)

```
brain plan ──HTTP──▶ publisher ──▶ mpc/control/heating         (rich ControlCommand, deadman+seq+arm)
                                       │  heating controller subscribes, validates, translates
                                       ▼
                                  loxone/cmd/heating/obyvak/on = 1   (simple, retained)
                                       │  gateway udp-out subscribes
                                       ▼
                                  UDP "mpc_heat_obyvak=1" → Loxone :4000  (until Loxone speaks MQTT)
```

The rich plane keeps the safety envelope and translation in the controller; the simple plane is what
Loxone (or the gateway) consumes. When Loxone Gen2 MQTT is enabled, Loxone subscribes `loxone/cmd/#`
directly and the `udp-out` hop is retired — no other change.

### 5.7 Currently-unused sensors → planned brain uses

Two Loxone sensor families already flow to InfluxDB and ride the telemetry plane (§5.2) but the
optimizer doesn't read them yet (it reads only `temperature` today). Two **low-effort, high-value**
uses are in scope for the brain — and they cost *no* transport work, since the data is already on the
bus. A richer occupancy model is deferred to a separate issue.

- **Presence / motion → a whole-house "away" flag.** The motion sensors cover the high-traffic areas
  (the `obyvak` sensor sits *between the kitchen and the living room* — i.e. it watches the main open
  living space). All motion quiet for *N* hours ⇒ **away**. Two uses: (a) **gate the calibration** —
  drop away periods from the trailing-window internal-gain / consumption fit so empty-house weeks
  (the "away last week, hot water ≈ 0" case) don't poison it; (b) **comfort setback** — deep-setback
  every zone while away and pre-heat at the cheapest hours before return. Sparse, high-traffic
  placement is exactly right for away-detection: anyone home trips a sensor.
- **Humidity → a condensation / mold guardrail.** A per-zone soft **dew-point** constraint bounds how
  hard the optimizer may set a zone back — keep the surface above the dew point implied by the room's
  RH — so aggressive cost-driven setback can't drive the bathrooms/bedrooms into mold territory. This
  is a safety rail on the optimizer's own aggression, not a comfort tweak.

> **Deferred (tracked as a GitHub issue):** learning **occupancy patterns** from motion *and* humidity
> history to *forecast* per-room internal gains and non-HVAC consumption. Motion covers the
> living/circulation areas; humidity *spikes* cover the wet/activity rooms that have no motion sensor
> (a shower spikes bathroom RH, cooking spikes the kitchen) — so the two are complementary. This needs
> a forecast model (the MPC is forward-looking), not a live read, so it's a separate build. Note the
> underfloor thermal mass means occupancy only pays off *predictively* (pre-heat ahead of expected
> occupancy), never reactively.

---

## 6. The self-hosted gateway repo

**Name (proposed):** `loxone-mqtt-gateway` (a.k.a. *loxbridge*). A small, self-hosted service that
owns the Loxone **edge**: UDP ⇄ MQTT, plus (optionally) the InfluxDB historian.

### 6.1 Components (roles)

| Component | Role | Replaces |
|---|---|---|
| **`udp-in`** | Listen on `:2000`; parse Loxone UDP packets; publish the structured `loxone/…` tree (retained, enveloped). | Python `udp_listener` ingestion |
| **`udp-out`** | Subscribe `loxone/cmd/#`; debounce/coalesce; send `key=value;…` UDP to Loxone `:4000`. Deadman-aware (drop expired). | Python `mqtt_bridge` |
| **`historian`** | Subscribe the telemetry plane (`loxone/#`, `energy/#`, `weather/#`, `prices/#`, `teslamate/#`); write InfluxDB **preserving the current schema**. | Python `udp_listener` Influx writes |
| **`mapping` (config)** | One declarative table: Loxone UDP field ⇄ MQTT topic ⇄ Influx point. Single source of truth for all three components. | scattered Python settings |

> `historian` overlaps with the existing `adapters/mqtt-bridge` (MQTT→Influx, with protected-bucket
> guards and the two-key arm). **Recommendation:** make the gateway repo own `udp-in`/`udp-out`/
> `mapping`, and **reuse `adapters/mqtt-bridge`** (generalized) as the historian rather than rewriting
> it — see §9.

### 6.2 The mapping table (the heart of the gateway)

A declarative JSON5 list — each row ties the three representations together so `udp-in`, `udp-out`,
and `historian` stay consistent:

```json5
// loxone-mqtt-gateway/mapping.json5
signals: [
  // Loxone sends UDP "...;temperature;22.5;obyvak;temperature;..." ; we publish + persist it.
  { udp: { measurement: "temperature", room: "obyvak" },
    topic: "loxone/climate/obyvak/temperature",
    influx: { bucket: "loxone", measurement: "temperature", field: "temperature", tags: { room: "obyvak" } },
    unit: "degC" },

  { udp: { measurement: "relay", room: "obyvak", tag1: "heating" },
    topic: "loxone/heating/obyvak/relay",
    influx: { bucket: "loxone", measurement: "relay", field: "relay", tags: { room: "obyvak", tag1: "heating" } } },

  // EV: a Loxone smart-socket / wallbox power reading needed for MPC EV control.
  { udp: { measurement: "ev", room: "garaz" },
    topic: "loxone/ev/garage/power",
    influx: { bucket: "loxone", measurement: "ev", field: "ev_charging_power", tags: { charger: "garage" } },
    unit: "kW" },
],

// Brain → Loxone: simple command topics → UDP virtual inputs.
commands: [
  { topic: "loxone/cmd/heating/+/on",        vi_key: "mpc_heat_{1}" },        // {1} = the wildcard room
  { topic: "loxone/cmd/ev/+/power_kw",       vi_key: "mpc_ev_{1}_kw" },       // this house: the only EV command (0 = stop)
  // optional, unused in this house (left unconnected on the Loxone side):
  // { topic: "loxone/cmd/ev/+/enable",      vi_key: "mpc_ev_{1}_on" },       // power_kw=0 already stops; no separate enable
  // { topic: "loxone/cmd/ev/+/target_soc",  vi_key: "mpc_ev_{1}_target" },   // needs car SoC, unavailable here
],
```

`udp-in` reads `signals[].udp → topic (+influx)`; `historian` reads `signals[].topic → influx`;
`udp-out` reads `commands[].topic → vi_key`. New sensor? One row, all three components pick it up.

### 6.3 Tech choice

**Recommendation: Rust**, consistent with `mpc_home_control` and the `controllers/`+`adapters/`
workspace (CLAUDE.md's stated direction is *"consolidating, in Rust over time, the logic of the Python
loxone_smart_home"*). Concretely:

- Reuse `adapters/mqtt-common` (topic matching, QoS, the bare/JSON payload parser) and the
  `controller_protocol` envelope conventions.
- Reuse `adapters/mqtt-bridge`'s InfluxDB line-protocol writer + protected-bucket guard for the
  historian.
- Same **deployment story** as the MPC brain: static musl `cargo zigbuild`, a tiny alpine Docker
  image, `--restart unless-stopped` on `caddy_net` (see `memory/mpchc-shadow-deployment.md`).

*Alternative:* evolve the existing Python `loxone_smart_home` modules in place (fastest, but keeps the
edge in Python and splits the stack). Decision in §9.

### 6.4 Repo layout (proposed)

```
loxone-mqtt-gateway/
├── Cargo.toml                 # workspace: udp-in, udp-out, historian, common
├── crates/
│   ├── common/                # mapping.json5 loader, payload envelope, shared types
│   ├── udp-in/                # Loxone UDP listener → MQTT publisher
│   ├── udp-out/               # MQTT subscriber → Loxone UDP virtual inputs
│   └── historian/             # MQTT → InfluxDB (or: depend on mpc's adapters/mqtt-bridge)
├── mapping.json5              # the single source of truth (§6.2)
├── deploy/
│   ├── Dockerfile             # static musl binary in alpine
│   ├── docker-compose.yml     # on caddy_net, restart unless-stopped
│   └── run-container.sh
└── README.md
```

### 6.5 Operational concerns (designed in, not bolted on)

- **Time:** Loxone stamps Prague-local; normalize to UTC at `udp-in` (as the Python listener does).
- **Retained-state hygiene:** on a sensor going permanently away, publish an empty retained payload to
  clear it. A `valid_until`/`max_age` in the envelope lets the brain treat a stale retained value as
  missing (the MPC already has recency guards).
- **Birth/Will:** each gateway component sets an MQTT LWT on `loxone/gw/<component>/health`
  (`online`/`offline`), mirroring the controllers.
- **Back-pressure / dedup:** `udp-out` coalesces — only send a virtual input when the value changed
  (the heating controller already does change-only republishing).
- **Idempotent historian:** writes are last-value-wins per timestamp (matches today); non-finite
  values dropped at the boundary (matches `mqtt-common`).
- **Security:** broker stays on the internal `caddy_net`; if exposed, add MQTT auth (username/password
  via env, never in config) + per-client ACLs (`loxone/#` publish only by the gateway; `mpc/control/#`
  publish only by the publisher). Tokens/passwords are **env-sourced**, never committed.

---

## 7. How the MPC brain fits (and stays MQTT-free)

- **Reads:** the **historian** writes the same InfluxDB points the MPC already queries → the MPC's
  `influxdb.rs`/`live_inputs.rs`/`zone_mappings` are **unchanged**. For any value that's MQTT-only and
  history-less (e.g. a live EV target), `adapters/mqtt-source` exposes it over HTTP and the MPC reads
  it via an `http` `SourceLocator` — the pattern already exists.
- **Writes:** the `publisher` sidecar polls `/api/plan/latest` and emits `mpc/control/*`; the
  controllers translate to `loxone/cmd/*` / `energy/solar/command/*`. The MPC binary itself never
  links MQTT — the `cargo tree` CI gate still passes.

Net: the bus migration touches the **edge** (gateway) and the **controllers' south side** (UDP →
`loxone/cmd`), not the brain.

---

## 8. Migration plan (phased, shadow-first, non-disruptive)

The Python `udp_listener` already republishes everything to `loxone/status` — we exploit that to run
the new tree in parallel before touching the live UDP path.

**Phase 0 — Stand up the broker-side scaffolding (zero risk).**
Define `mapping.json5`. Build the gateway's `historian` and point it at a **scratch bucket**
(`loxone_shadow`) so nothing collides with production. Verify points match the real schema.

**Phase 1 — Shadow ingress via `loxone/status` (zero Loxone changes).**
Gateway `udp-in` (in "republish mode") **subscribes the existing `loxone/status` JSON** and fans it
out into the structured `loxone/…` tree. No UDP contention, no Loxone reconfiguration. Now both the
old path and the new tree carry the same data. Diff the shadow historian against the live `loxone`
bucket until they agree.

**Phase 2 — Take over raw UDP ingress.**
Point Loxone's UDP virtual output at the gateway's `:2000` (or run the gateway's `udp-in` on the
listener port and have it **also forward** the raw datagram to the Python listener for a while, so the
old path keeps working). Gateway now parses Loxone UDP directly and publishes `loxone/…`. The Python
`udp_listener` can be retired once the gateway's historian is authoritative for the `loxone` bucket.

**Phase 3 — Command egress over MQTT.**
Switch the heating/EV controllers' south side from "send UDP" to "publish `loxone/cmd/…`". Gateway
`udp-out` subscribes `loxone/cmd/#` and drives the Loxone virtual inputs — i.e. the same UDP packets
Loxone already understands, now sourced from the controllers via MQTT. The Python `mqtt_bridge` is
retired. The controllers already **actuate** (two-key gate); this phase reroutes that live actuation
through the gateway rather than direct UDP — still a separate, deliberate step gated by production safety.

**Phase 4 — (optional) Loxone-native MQTT.**
If/when Loxone Gen2 MQTT is enabled, Loxone publishes `loxone/…` and subscribes `loxone/cmd/…`
directly; the gateway's `udp-in`/`udp-out` shrink to only the signals Loxone can't do natively, then
potentially retire. The topic tree doesn't change — only who fills it.

At every phase the live house keeps its current data + control path until the new one is proven and
explicitly cut over.

---

## 9. Decisions to confirm (my recommendation in **bold**)

These are the genuine forks. I've picked a default for each so we can proceed without a quiz —
flag any you'd steer differently.

1. **Payload format on `loxone/…`:** **JSON envelope `{v,ts,u}`, retained** (debuggable, unit-safe,
   already parseable by `mqtt-common`), with bare-scalar accepted where Loxone publishes natively.
   *Alt:* bare scalars everywhere (simplest, but loses units/timestamps/provenance).
2. **Gateway language:** **Rust**, reusing `mqtt-common` + the bridge writer + the musl/Docker deploy
   story (matches the Rust-consolidation direction). *Alt:* extend the Python `loxone_smart_home`.
3. **Historian:** **reuse/generalize `adapters/mqtt-bridge`** rather than write a new one (it already
   has the schema-preserving writer, protected-bucket guard, and two-key arm). *Alt:* a fresh
   historian crate inside the gateway repo.
4. **Repo boundary:** **a new dedicated `loxone-mqtt-gateway` repo** for the Loxone edge (as you
   asked), depending on the published `mqtt-common`/bridge crates. *Alt:* fold it into the MPC repo's
   `adapters/`.
5. **Room vocabulary on the bus:** **keep Loxone-native Czech room names** on `loxone/…` (zero MPC
   change; translation stays in `zone_mappings`). *Alt:* rename to the MPC's English zone keys at the
   gateway (cleaner topics, but moves the translation and risks drift).
6. **Command granularity:** **per-metric simple topics** (`loxone/cmd/ev/garage/power_kw`) so Loxone
   VIs map 1:1. *Alt:* one JSON command per subsystem (richer, but Loxone can't unpack JSON from a VI).

---

## 10. Open questions for the user

- Does the **Loxone Miniserver** here support native MQTT (Gen2 + MQTT extension), or must everything
  stay UDP-virtual-I/O at the Loxone edge? (Sets whether Phase 4 is reachable.)
- Which **future sensors** for EV-charging / heating control should be first-class in `mapping.json5`
  now (e.g. per-phase wallbox current, hot-water tank temperature, the boiler smart-socket power,
  per-room setpoints)? List them and I'll add rows.
- Should `prices/ote` and `weather/forecast` **also** move onto MQTT (today they're written to Influx
  by Python collectors), or stay Influx-only? (Affects whether the historian is the sole writer.)
```
