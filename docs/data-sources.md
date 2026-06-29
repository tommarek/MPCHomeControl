# Data sources — the pluggable, read-only input layer

The MPC core (optimizer, forecasts, thermal model, the rolling loop) consumes **typed inputs** and
never knows where they come from. The **data-source layer** is the seam that fetches those inputs, so
a house whose data lives somewhere other than the default InfluxDB can be reconfigured **without
touching the core** — symmetric to the [controllers](controllers.md) on the output side.

> **Read-only by construction.** Every source here is a *query* or a *GET*. A source client can read
> but never write or actuate. MQTT — the house's *actuation* transport — is deliberately **absent**
> from the MPC: it reaches the inputs only through a separate **bridge sidecar** that normalises it
> into a pull store. This is why `cargo tree -p mpc_home_control` links no `rumqttc`, and the
> guarantee is **structural**, not a convention (see the production-safety note in `CLAUDE.md`).

## The shape of a source

A signal is addressed by a **`SourceLocator`** (`src/source/mod.rs`) — a tagged JSON5 object that says
which backend holds it and where:

```json5
// InfluxDB — a field in a measurement (the default backend)
{ type: "influx", bucket: "ev", measurement: "teslamate", field: "battery_level",
  tags: { car: "1" }, scale: 1.0 }

// Read-only Postgres SELECT — the value is the last row's final column
{ type: "postgres", connection: "teslamate", scale: 1.0,
  query: "select date_trunc('minute', date), battery_level::float8 from charging_processes order by date desc limit 1" }

// Read-only HTTP GET of a JSON document — `pointer` is an RFC-6901 JSON pointer
{ type: "http", url: "http://tesla-bridge.local/vehicle", pointer: "/charge_state/battery_level", scale: 1.0 }
```

| Field | Influx | Postgres | Http |
|---|---|---|---|
| address | `bucket` / `measurement` / `field` (+ `tags`) | `query` (+ `connection`) | `url` (+ `pointer`) |
| `scale` | ✓ multiplier on the raw value (e.g. `0.001` for W→kW) | ✓ | ✓ |
| recency | bounded to the read's `max_age` window | the query's responsibility | the endpoint's responsibility |

`SourceClients` (the registry) dispatches a `read_locator(loc, max_age_min)` to the right backend and
returns a scaled `f64` (or `None` — every input is **best-effort**: a stale / missing / unreachable
source degrades to `None`, never a panic). It also delegates the Influx-native reads (zone series,
prices, raw rows) the rest of the readers already use.

### The four backends

| Backend | Crate / mechanism | Reaches the MPC by | Notes |
|---|---|---|---|
| **InfluxDB** | `influxrs` (Flux over HTTP) | direct pull (default) | the existing path; all zone/price/telemetry reads |
| **Postgres** | `tokio-postgres` (NoTls, pure-Rust) | direct pull | read-only `SELECT`; e.g. a TeslaMate database |
| **HTTP** | `ureq` (rustls, pure-Rust) | direct pull | read-only `GET` of a JSON API; e.g. a Tesla bridge |
| **MQTT** | `adapters/mqtt-bridge` (→ Influx) or `adapters/mqtt-source` (→ HTTP) | **sidecar → pull** | never links into the MPC |

Postgres and HTTP are pure-Rust and musl-friendly (no C / OpenSSL), so they cross-build for the
Synology MPC brain exactly like the rest of the binary.

### Secrets stay in the environment

- **Influx** — `INFLUX_TOKEN` / `INFLUXDB_TOKEN` (as today; never stored in config).
- **Postgres** — the DSN (with its password) is read from the environment, never the config. A
  locator's `connection: "teslamate"` resolves to `MPC_PG_TESLAMATE`; `connection` omitted resolves to
  `MPC_PG_DEFAULT`. Example: `export MPC_PG_TESLAMATE="host=teslamate-db user=teslamate password=… dbname=teslamate"`.
- **HTTP** — put any token directly in the `url` (or front the API with a local unauthenticated proxy).

A Postgres query must return its rows **newest-last** with the value in the **final column**, cast to
`double precision` (`::float8`) — booleans and integers are coerced too, but `numeric` must be cast.

### More than one instance of a backend

The layer scales to several DBs / brokers per house:

- **Multiple Postgres DBs** — already implicit: each `connection: "<name>"` resolves to `MPC_PG_<NAME>`.
- **Multiple MQTT brokers** — run one `mqtt-bridge` / `mqtt-source` **sidecar per broker**; the MPC
  only ever sees the resulting Influx/HTTP sources, so brokers fan out without touching it.
- **Multiple InfluxDBs** — declare extra instances in a `data_sources.influx` block and point a
  locator at one with `connection`:
  ```json5
  data_sources: {
    influx: { secondary: { host: "http://influx2:8086", org: "loxone", token_env: "INFLUX2_TOKEN" } },
  },
  // then: { type: "influx", connection: "secondary", bucket: "b", measurement: "m", field: "f" }
  ```
  Each instance's token comes from the environment (never the config). With an explicit `token_env`
  that var is used; **omit it** and the instance resolves `INFLUX_<NAME>_TOKEN` first, then the shared
  `INFLUX_TOKEN` — so several instances don't collide on one variable (the `MPC_PG_<NAME>` convention,
  applied to Influx). The host can likewise be overridden per instance with `INFLUX_<NAME>_HOST` (e.g.
  to repoint a secondary at a staging DB), mirroring the primary's `INFLUX_HOST`. A locator with no
  `connection` uses the default `db` instance; one naming an unconfigured instance reads `None` (never
  silently the wrong DB).

## The MQTT bridge sidecar — `adapters/mqtt-bridge`

MQTT is how the house is *actuated*, so it must never link into the read-only MPC. When a signal is
only available on MQTT (a TeslaMate car, an MQTT-only sensor), the **bridge** subscribes it and writes
the normalised value into the InfluxDB pull store the MPC then reads — so the data enters the system
while the MPC stays MQTT-free.

```
teslamate/cars/1/battery_level  ──(MQTT)──▶  mqtt-bridge  ──(Influx write)──▶  ev/teslamate/battery_level
                                                                                      │
                                                          MPC reads via a { type:"influx", … } locator
```

It is **dry-run by default** — it logs the line protocol it *would* write. Writing needs **both** the
config `armed: true` **and** `MPC_ADAPTER_ARM=i-understand-this-writes`. Even armed it writes only its
**own** normalised measurements (a dedicated `ev`/`teslamate` bucket); it never touches the live
loxone/growatt data. That last guarantee is **structural, not just documented**: config load rejects
any signal whose bucket is a live house bucket (`loxone`, `solar`, `weather_forecast`, `ote_prices`),
so a bucket typo can't corrupt real data even when armed. Its config maps each topic to a destination:

```json5
{
  armed: false,
  mqtt:   { host: "127.0.0.1", port: 1883, client_id: "mpc-adapter-mqtt-bridge" },
  influx: { url: "http://127.0.0.1:8086", org: "loxone", bucket: "ev", token_env: "INFLUXDB_TOKEN" },
  signals: [
    { topic: "teslamate/cars/1/battery_level",    measurement: "teslamate", field: "battery_level",    tags: { car: "1" } },
    { topic: "teslamate/cars/1/charge_limit_soc",  measurement: "teslamate", field: "charge_limit_soc", tags: { car: "1" } },
    { topic: "teslamate/cars/1/charger_power",     measurement: "teslamate", field: "charger_power",    tags: { car: "1" } },
    // A JSON payload: pull one field out with a pointer
    // { topic: "device/state", measurement: "device", field: "w", pointer: "/power/active", scale: 1.0 },
  ],
}
```

Topic filters support MQTT `+` / `#` wildcards; a bare-number, bare-bool, or JSON-pointer payload all
parse to a float, then `scale` applies. `telegraf`'s MQTT input remains a valid alternative — the
bridge ships this first-class so no extra moving part is required.

See `adapters/mqtt-bridge/bridge.example.json5` for the full annotated example.

## The mqtt-source sidecar — `adapters/mqtt-source`

The bridge *persists* MQTT into Influx (good when you want history / series). When you only need the
**latest live value** of an MQTT-only signal, the `mqtt-source` adapter is lighter: it subscribes the
topics, keeps the last value of each in memory, and serves them over a tiny HTTP endpoint the MPC
pulls with a normal `http` locator — **no write-to-Influx hop**.

```
teslamate/cars/2/charge_limit_soc ──(MQTT)──▶ mqtt-source ──(GET /v1/value/ev_target)──▶ MPC (http locator)
```

```json5
// mqtt-source config: topic → a stable URL `name` the MPC pulls
{
  mqtt: { host: "mosquitto", port: 1883 },
  bind: "0.0.0.0:8088",
  topics: [
    { name: "ev_target", topic: "teslamate/cars/2/charge_limit_soc" },        // retained → always served
    // { name: "ev_soc", topic: "teslamate/cars/2/battery_level", max_age_seconds: 1800 }, // stale ⇒ 404
  ],
}
```

The MPC then reads it like any HTTP source: `{ type: "http", url:
"http://mpc-mqtt-source:8088/v1/value/ev_target", pointer: "/value" }`. `max_age_seconds` makes a
silent feed read `404` → `None` at the MPC (omit it for retained / rarely-changing signals like a
charge limit). It is **read-only**: it only subscribes and serves `GET`s — nothing to arm, and MQTT
still never links into the MPC binary (separate crate).

**Bridge vs source:** use the **bridge** when you want the value in Influx for history / backtests;
use **mqtt-source** when you just want the live latest value and want to skip the Influx round-trip.
Both keep the MPC pull-only. See `adapters/mqtt-source/mqtt-source.example.json5`.

## EV signal routing

The **EV layer** is built on this seam: each charger's `sources` map (`docs/ev.md`) addresses
its SoC / target / wallbox-power signals by `SourceLocator`, so the same charger works whether the SoC
comes from Influx, a TeslaMate Postgres DB, a Tesla HTTP bridge, or an MQTT topic via the sidecar —
with no code change.

## Scheduled-load sensors

A **scheduled load** (`docs/configuration.md`) can also carry a `sensor` — a `SourceLocator` reading
the appliance's electrical power (e.g. a Loxone Smart Socket's power in InfluxDB). When set, the
calibration derives that load's room-heat flux from the *measured* draw (`P × power_factor`) instead
of a fitted/configured magnitude, while the schedule stays the forecast. Same plug-anywhere seam: the
power can come from any backend the bridge can feed.

## Overriding a core signal — the `data_sources` block

The same `SourceLocator` mechanism is wired into the core reads through an optional top-level
`data_sources` block. Each entry maps a signal to a locator; an **unmapped** signal falls back to its
built-in InfluxDB default, so the current house needs **no config change** (every default reproduces
today's read byte-for-byte — asserted by a test). Migrated groups so far:

| signal | default | shape |
|---|---|---|
| `growatt.<metric>` | `solar`/`solar`/`<metric>` (e.g. `InputPower`, `SOC`) | latest scalar (any backend) |
| `weather_temperature` | `weather_forecast`/`weather_forecast`/`temperature_2m` (`room=outside`,`type=hour`) | hourly series (Influx) |
| `weather_cloud` | …/`cloudcover` | hourly series (Influx) |
| `curtailment_export` | `solar`/`solar`/`export_enabled` | hourly series (Influx) |
| `curtailment_soc` | `solar`/`solar`/`battery_soc` | hourly series (Influx) |
| `prices` | `ote_prices`/`electricity_prices`/`price` (EUR/MWh; `scale` converts) | day-ahead series (Influx) |
| `pv_forecast` | `solar`/`solar_forecast_history`/`hourly_json` (**bucket** honored; measurement per-call) | JSON curve (Influx) |
| `heating_relay` | `loxone`/`relay`, `tag1=heating` (**field** is the per-zone room) | per-zone series (Influx) |

```json5
data_sources: {
  growatt: {
    // override where one metric is read; unlisted metrics keep the solar-bucket default
    SOC:        { type: "influx", bucket: "solar", measurement: "inverter", field: "soc_pct" },
    InputPower: { type: "http", url: "http://inverter.local/api", pointer: "/pv/power_w" },
  },
  // a house on a different weather feed remaps the forecast (Influx series only, for now)
  weather_temperature: { type: "influx", bucket: "weather2", measurement: "owm", field: "temp_c",
                         tags: { room: "outside" } },
}
```

All six original signal groups now resolve through `data_sources`. The Growatt group is
backend-agnostic (latest scalar → Influx / Postgres / HTTP); the rest are **Influx-only** for now,
since windowed-mean series / the price parse / the JSON curve over Postgres/HTTP aren't implemented.
Two groups honor only part of their locator (noted in the table): `pv_forecast` uses the **bucket**
(the measurement is chosen per call; the curve fields are structural), and `heating_relay` uses the
bucket/measurement/tags with the **field** set to each zone's room.

> **What's left.** Only the *backends* for the non-Growatt groups (a series/price/curve read over
> Postgres or HTTP) — the locations are all config-driven. The zone-temperature reads remain on the
> existing `zone_mappings` block (`influxdb.rs`), which is already per-house config.
