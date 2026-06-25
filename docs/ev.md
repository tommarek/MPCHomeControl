# EV charging

An EV charger is modelled as a **controllable electrical flexible load** with a battery, a target SoC,
and a deadline. The optimizer schedules the charge toward the target at lowest cost — co-optimised
with the home battery, solar, heating, and HVAC in the one LP — but **only while the car is
controllable on our own wallbox**. Everything is **shadow / read-only**: the plan is a recommendation;
nothing is actuated until a controller is explicitly armed.

EV support activates only when at least one charger is configured — the dashboard's EV screen and the
`/api/ev*` endpoints appear conditionally (via `/api/capabilities`).

## Three tiers of charger

A charger declares how controllable it is (`control`):

| `control` | The MPC can… | In the LP | Hardware |
|---|---|---|---|
| **`modulating`** (default) | set the charge power *and* on/off | a continuous load `[0 … max_kw]` per block | a modulating wallbox (e.g. Loxone) |
| **`on_off`** | switch on/off at a fixed rate | a load that is `0` or `max_kw` | a relay-switched charger |
| **`monitored`** | observe only | folded into the house load as an expected draw | any charger we can read but not command |

A `monitored` charger never produces a command; its expected draw just makes the plan (and the
battery) react around it.

## Four strategies

The strategy (a config default, **overridable live** from the dashboard) sets how the optimizer trades
cost, solar, and the deadline:

| `strategy` | Behaviour |
|---|---|
| **`cost_optimized`** (default) | meet the target by the deadline at the lowest cost (charges the cheapest / solar blocks) |
| **`solar_only`** | charge only from surplus PV — never grid or battery; may miss the target |
| **`solar_preferred`** | solar first, top up from cheap grid to meet the deadline |
| **`charge_now`** | charge at full rate immediately until the target, price-blind |

The target is a **soft** constraint: energy still missing at the deadline is penalised heavily (so the
plan meets it whenever feasible) but never makes the LP infeasible. The home battery → car path is
**off by default** (double-conversion-lossy); enable it per charger with `allow_battery_to_ev`, and
even then it's gated by the battery-wear term.

## Configuring a charger

In `config.json5` under the top-level `chargers` list (see [configuration.md](configuration.md) for
the full field list):

```json5
chargers: [
  {
    name: "garage",            // dashboard key + the controller's MQTT channel
    control: "modulating",
    max_kw: 11.0,
    min_kw: 1.4,               // most chargers can't modulate below ~6 A
    efficiency: 0.9,           // AC→DC: energy reaching the car per kWh drawn
    battery_kwh: 75.0,         // usable car-battery capacity (for %↔kWh)
    allow_battery_to_ev: false,
    strategy: "cost_optimized",
    target_pct: 80,
    deadline: "07:00",         // local "charged-by" time
    // Each role is a SourceLocator (see docs/data-sources.md) — any backend. These are this
    // house's real wallbox signals: the loxone `ev` measurement (the same data
    // loxone_smart_home's battery-protect logic reads). `ev_connected` is the authoritative
    // on-our-wallbox flag (loxone-side); `ev_charging_power` is the live power in kW. SoC isn't
    // in InfluxDB, so it comes from TeslaMate's Postgres — add it to unlock %-target scheduling.
    sources: {
      on_charger: { type: "influx", bucket: "loxone", measurement: "ev", field: "ev_connected" },     // 1/0
      power:      { type: "influx", bucket: "loxone", measurement: "ev", field: "ev_charging_power" }, // kW
      // SoC from TeslaMate's Postgres. With more than one car, filter by car_id (here car 2 = "Tess"),
      // and union `charges` (joined via charging_processes) so a charge session's seconds-fresh value wins:
      soc: { type: "postgres", connection: "teslamate",
             query: "select battery_level::float8 from (select date, battery_level from positions where car_id=2 union all select c.date, c.battery_level from charges c join charging_processes cp on c.charging_process_id=cp.id where cp.car_id=2) t where date > now() - interval '30 days' order by date desc limit 1" },
      // The car's charge limit is MQTT-only in TeslaMate — pull it live via the mqtt-source sidecar
      // (subscribes teslamate/cars/2/charge_limit_soc). See docs/data-sources.md.
      target: { type: "http", url: "http://mpc-mqtt-source:8088/v1/value/ev_target", pointer: "/value" },
    },
  },
],
```

> **TeslaMate freshness:** TeslaMate only polls the car while it's online, so a long-parked car's SoC
> /target are its last-known values (and were stale for *days* when TeslaMate's Tesla-API auth had
> lapsed). The values go live the moment the car wakes — and when it plugs into our wallbox, the
> loxone `ev_connected`/`ev_charging_power` are live immediately (loxone-side) and charging wakes
> TeslaMate, so the scheduling case is always fresh.

### The fusion rule: "in TeslaMate ≠ on our charger"

The car's data (SoC, target, whether it's charging) can come from the car's own telemetry (TeslaMate),
but **the car may be charging somewhere else entirely**. So the live state (`src/ev/state.rs`) fuses
two distinct questions from different sources:

- **"Is the car on *our* wallbox right now?"** → the **wallbox** signal (`on_charger` / `power`) is
  authoritative. Only then is the charger `controllable_now` and schedulable.
- **"What's the SoC / target / is it charging away?"** → the car telemetry (`soc` / `target` /
  `tesla_power`).

This yields a status of `charging` / `connected` / `charging_away` / `away`. The optimizer only ever
schedules a charger that is controllable on our wallbox; a car charging away is observed, not planned.

Roles (all optional, each a `SourceLocator`): `on_charger`, `power`, `soc`, `target`, `capacity`,
`tesla_power`. A controllable charger needs at least an `on_charger` or `power` source (the wallbox).

### What the optimizer needs — capacity, SoC, and unknown cars

Scheduling charges toward **energy-to-target = (target − SoC) / 100 × capacity_kwh**, so it needs both
the **SoC** and the **battery capacity**:

- **Capacity** always resolves: a per-car `capacity_kwh` → a `capacity` source → the charger's
  `battery_kwh` config (the guaranteed fallback). Set `battery_kwh` to the real pack size so the
  kWh estimate is right (≈ 75 for a Model Y/3 LR).
- **No SoC ⇒ no scheduled charge.** Without SoC the energy-to-target is unknown, so the optimizer
  won't plan a target charge. But if the wallbox shows the car **connected and drawing**, the MPC
  folds the **measured** power into the house load (~1 h nowcast) so it still accounts for it — the
  home battery is never planned to discharge into a charge it can't see. This covers an **untracked
  car** (not in TeslaMate — a guest car, a non-Tesla) or a stale SoC feed: it charges normally under
  loxone's own control; the MPC just observes its load instead of optimizing it.

### More than one car on a wallbox

Add a `cars` list to the charger — each entry a `{ name, present, soc, target?, capacity_kwh? }`. The
wallbox roles (`on_charger`/`power`) stay car-agnostic; the SoC/target come from whichever car's
`present` signal (1/0) is set, so a two-car house never plans against the wrong car's SoC. Because the
wallbox can't identify the car, **you derive `present` per house** — e.g. a TeslaMate query for "this
car is plugged in *and* at home". `/api/ev` reports the chosen `active_car`. No `present` car ⇒ SoC
unknown ⇒ the unschedulable-but-accounted path above.

## Setting strategy & rate live — the preference API

The dashboard can override the config per charger; the preference is the **only thing the MPC writes**,
to its own JSON file (`MPC_EV_PREF_STORE`, default `ev_prefs.json`) — never to InfluxDB / MQTT / loxone.

| Endpoint | Purpose |
|---|---|
| `GET /api/capabilities` | `{ has_hvac, has_ev, chargers }` — drives conditional UI |
| `GET /api/ev` | per-charger live state + the planned charge schedule (by source) |
| `GET /api/ev/<name>/preference` | the stored preference for one charger |
| `POST /api/ev/<name>/preference` | set `strategy` / `max_rate_kw` / `target_pct` / `deadline` (any subset) |

Precedence for every control is **live preference > the car's own limit > config default** (e.g. the
effective target is capped by the car's `charge_limit_soc`). A `404` is returned for an unknown
charger; the body is validated before it is persisted.

## The dashboard EV screen

Conditional on `has_ev`. Per charger it shows the status badge (on our wallbox / charging away / idle
/ driving), the car SoC → target, the first-block charge power, the planned session energy, and a
**source-stacked** charge-schedule chart (solar / grid / battery → car per 15-min block). The
**strategy / target / deadline** controls `POST` to the preference endpoint, and the next plan reflects
them. Shadow only — nothing is actuated.

## Actuation (the EV controller — dry-run draft)

The path to hardware mirrors the other [controllers](controllers.md) and ships **dry-run**:

```
plan /api/plan/latest ─▶ mpc-plan-publisher ─(MQTT mpc/control/ev)─▶ mpc-controller-ev ─(UDP)─▶ Loxone wallbox
```

- The **publisher** emits a `Payload::Load` with one channel per charger **controllable on our wallbox
  right now** (monitored / away cars carry no command): the first block's planned power as the
  setpoint, the effective target SoC riding along.
- **`controllers/ev`** translates each channel into Loxone UDP virtual inputs — `<stem>_kw` (modulating
  setpoint), `<stem>_on` (enable), `<stem>_target` (SoC %), where `<stem>` is `mpc_ev_<channel>` unless
  overridden. A modulating wallbox uses `_kw`; an on/off one uses `_on`.

Like every controller it is **dry-run by default** behind two gates — config `armed: true` **and**
`MPC_CONTROLLER_ARM=i-understand-this-actuates` — and carries a `valid_until` deadman (`hold` →
loxone resumes, or `all_off`). Nothing is armed; the live house is driven by `loxone_smart_home`.
