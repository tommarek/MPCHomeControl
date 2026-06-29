# Configuration & model authoring guide

`mpc_home_control` describes your house with **two JSON5 files** in the working directory:

| File | Holds | Read by |
|---|---|---|
| **`model.json5`** | the *physical building* — zones, materials, the walls/floors/roofs between them | `model.rs` → `rc_network.rs` → `state_space.rs` |
| **`config.json5`** | *operation + economics* — site, heating, HVAC, tariff, battery, PV, the InfluxDB mappings | `optimize/config.rs` and `influxdb.rs` |

Together they are meant to be the **complete** definition of the house — there are no house-specific
constants hidden in the Rust. JSON5 means you get `// comments`, `trailing commas,` and unquoted keys.

## The golden rule of units

This is the one thing to get right.

- **`model.json5` quantities are read by [`uom`](https://docs.rs/uom) in SI _base_ units.** You write
  a bare number; it is interpreted in the base unit of its dimension. No unit suffixes.
- **`config.json5` values are plain `f64` in human units** (kW, °C, CZK, %, degrees, EUR/MWh).

| `model.json5` field | Dimension | Unit you write |
|---|---|---|
| `materials.*.thermal_conductivity` | thermal conductivity | **W/(m·K)** |
| `materials.*.specific_heat_capacity` | specific heat | **J/(kg·K)** |
| `materials.*.density` | density | **kg/m³** |
| `zones.*.volume` | volume | **m³** |
| `boundaries.*.area`, `sub_boundaries.*.area` | area | **m²** |
| `boundary_types` layer `thickness` | length | **m** |
| Simple boundary `u` | heat transfer | **W/(m²·K)** |
| Simple boundary `g` | ratio | **dimensionless** (0–1) |
| `boundaries.*.azimuth`, `boundaries.*.angle` | angle | ⚠️ **degrees** (raw `f64`) |

> ⚠️ **The one trap:** `azimuth` and `angle` are kept as raw `f64` **degrees**, *not* `uom` angles —
> because `uom`'s serde reads a bare angle as **radians**. Everywhere else in `model.json5`, write SI
> base units; for these two, write degrees.

---

## `model.json5` — the physical building

Four top-level keys: `materials`, `zones`, `boundary_types`, `boundaries`.

```json5
{
  materials: { /* name → physical properties */ },
  zones: { /* name → volume */ },
  boundary_types: { /* name → a Layered or Simple template */ },
  boundaries: [ /* instantiate a boundary_type between two zones */ ],
}
```

### `materials`

A library of physical properties, referenced by name from boundary layers.

```json5
materials: {
  concrete:         { thermal_conductivity: 1.5,   specific_heat_capacity: 1000, density: 2000 },
  brick_440:        { thermal_conductivity: 0.059, specific_heat_capacity: 1000, density: 850 },
  floor_insulation: { thermal_conductivity: 0.035, specific_heat_capacity: 1450, density: 30 },
}
```

`air` is **auto-supplied** (0.026 W/(m·K), 1012 J/(kg·K), 1.199 kg/m³); define it yourself only to
override those defaults.

### `zones`

Each zone is a name → its air `volume` (m³). The zone name is the key.

```json5
zones: {
  livingroom: { volume: 62.5 },
  bedroom:    { volume: 48.0 },
}
```

- **`outside` and `ground` are reserved** — they are auto-injected as boundary zones with *infinite*
  heat capacity (their temperature is an input, not a state). **Defining either is a hard error.**
- A boundary may **not** connect a zone to itself.

### `boundary_types`

A reusable template for a wall / floor / roof / window. Two shapes (an untagged enum — chosen by which
keys are present):

**Layered** — a stack of material layers with mass. Layers are listed as the **physical stack** from
one zone's face to the other; the model expands them into a chain of capacitive nodes.

```json5
ground_level_floor: {
  layers: [
    { material: "concrete",         thickness: 0.20 },
    { material: "floor_insulation", thickness: 0.14 },
    { marker: "heating" },                              // underfloor-heating actuator node
    { material: "anhydrite",        thickness: 0.05 },
  ],
},
```

- `{ marker: "name" }` places a **named node between layers** — the actuator/measurement point. The
  underfloor heating injects its power at the `"heating"` marker (see [`docs`](#hvac--air-side-heating-and-cooling)).
- Rules: **at least one non-marker layer**, and **no two consecutive markers**.

**Simple** — a massless element given by its U-value and solar gain (windows, doors):

```json5
window:        { u: 0.74, g: 0.5 },   // U-value W/(m²·K); g = solar heat-gain coefficient (0–1)
entrance_door: { u: 1.2,  g: 0.0 },
```

### `boundaries`

Instantiates a `boundary_type` between a pair of zones, with an area and (for exterior faces)
orientation.

```json5
boundaries: [
  { boundary_type: "ground_level_floor", zones: ["livingroom", "ground"], area: 25.0 },
  {
    boundary_type: "exterior_wall",
    zones: ["outside", "livingroom"],
    area: 18.4,
    azimuth: 226,   // compass bearing, degrees (0 = N, 90 = E, 180 = S, 270 = W)
    angle: 90,      // tilt from horizontal, degrees (90 = vertical wall, 0 = flat roof)
    sub_boundaries: [                                    // carve windows/doors out of the wall
      { boundary_type: "window", area: 3.2 },
    ],
  },
]
```

- **`area`** in m². **`sub_boundaries`** carve smaller elements (windows, doors) out of a parent; each
  sub-area must be **≤ the parent area**, and the leftover area auto-fills with the parent type.
- **`azimuth` / `angle`** (degrees) orient a face for **solar gain**; only exterior boundaries
  (touching `outside`) need them. Sub-boundaries **inherit** the parent's orientation.
- Zero-area boundaries are skipped.

---

## `config.json5` — operation & economics

`config.json5` is parsed by **two independent deserializers** that each ignore the other's keys
(neither sets `deny_unknown_fields`): `influxdb.rs` reads `db` + `zone_mappings`;
`optimize/config.rs` reads `site`, `heating`, `hvac`, `tariff`, `battery`, `pv`, `chargers` (EV), and
the loop knobs. So all the blocks below coexist in one file.

> **EV chargers** live in the top-level `chargers` list — each a controllable/monitored flexible load whose
> SoC/wallbox signals are addressed by a [`SourceLocator`](data-sources.md) (any backend). The full
> field list, fusion rules, strategies, and the live preference API are in **[ev.md](ev.md)**; the
> pluggable data-source layer those `sources` use is in **[data-sources.md](data-sources.md)**.

### `site`

```json5
site: {
  latitude: 49.494934,
  longitude: 17.390341,
  utc_offset_hours: 2,         // fixed offset to local civil time (+1 winter / +2 summer; no DST handling)
  ground_temperature_c: 16.0,  // optional (default 16) — the `ground` boundary temperature under the slab
}
```

### `heating` (underfloor)

```json5
heating: {
  cop: 1.0,                 // heat delivered per kWh electricity. 1.0 = resistive; >1 = a heat pump
  comfort_penalty: 50.0,    // price-units per K per step a zone is outside its band
  zones: {                  // a zone absent here is NOT heated
    livingroom: { max_heat_kw: 4.0, t_min: 20.0, t_max: 23.0, internal_gain_w: 351 },
    bedroom:    { max_heat_kw: 2.0, t_min: 19.0, t_max: 23.0 },
  },
}
```

| Field | Unit | Notes |
|---|---|---|
| `cop` | — | heat / electricity |
| `comfort_penalty` | price-units/(K·step) | soft-comfort weight |
| `zones.*.max_heat_kw` | kW | the circuit's electric power |
| `zones.*.t_min` / `t_max` | °C | comfort band edges |
| `zones.*.internal_gain_w` | W | optional (default 0); occupants/appliances/fireplace |

The zone name must exist in `model.json5` and have a `"heating"` marker for the heat to land.

### `hvac` (air-side heating and cooling)

Optional and **inert until a unit is added** (the house has none today). Reversible heat pumps that act
on a room's **air** (not the slab): cooling above `t_cool`, air-heating below `t_heat`. Equipment is
**unit-based** — a unit serving one zone is a room split; a unit serving several is a central/ducted
system sharing one compressor.

```json5
hvac: {
  comfort_penalty: 50.0,        // optional (default 50)
  comfort: {                    // per-room deadband [t_heat, t_cool] (°C); free-float between
    bedroom:    { t_heat: 20.0, t_cool: 26.0 },
    room_1:     { t_heat: 20.0, t_cool: 26.0 },
    livingroom: { t_heat: 20.0, t_cool: 26.0 },
  },
  units: {
    bedroom_ac: {                            // a reversible split unit in one room
      zones: ["bedroom"],
      max_cool_kw: 3.5, max_heat_kw: 3.5,
      cooling_cop: 3.0, heating_cop: 3.5,    // constant COPs
    },
    upstairs_ducted: {                       // central unit: several rooms, one shared compressor
      zones: ["room_1", "livingroom"],
      max_cool_kw: 8.0, max_heat_kw: 9.0,                       // capacity SHARED across the rooms
      per_zone_max_kw: { room_1: 4.0, livingroom: 5.0 },        // optional per-room damper caps
      cooling_cop: [ { t: 25, cop: 3.6 }, { t: 35, cop: 2.3 } ], // COP curve vs outdoor °C
      heating_cop: [ { t: -10, cop: 2.0 }, { t: 7, cop: 3.5 }, { t: 15, cop: 4.6 } ],
      single_mode: true,        // ducted single-compressor: heat OR cool the group per block, not both
    },
  },
}
```

| Field | Unit | Notes |
|---|---|---|
| `comfort_penalty` | price-units/(K·step) | optional (default 50) |
| `comfort.<zone>.t_heat` / `t_cool` | °C | the room's deadband; `t_cool ≥ t_heat` |
| `units.<u>.zones` | — | zones the unit serves (≥1) |
| `units.<u>.max_cool_kw` / `max_heat_kw` | kW | total capacity, **shared** across the served zones |
| `units.<u>.per_zone_max_kw` | kW | optional per-room delivery (damper) cap; default = unit total |
| `units.<u>.cooling_cop` / `heating_cop` | — | a **number** (constant) **or** a **`[{ t, cop }]` curve** |
| `units.<u>.single_mode` | — | optional (default `false`); `true` forbids simultaneous heat+cool |

**COP curves** (`CopSpec`): a constant `3.0`, or breakpoints `[{ t: <°C>, cop: <COP> }]` in
**strictly increasing** `t` with positive `cop`. Evaluated by clamped linear interpolation (flat beyond the
ends). The optimizer reads the COP at each block's outdoor temperature; because the forecast is a known
input the dispatch stays a linear program. Every zone named in a unit (or `per_zone_max_kw`) must have a
`comfort` entry.

### `tariff` (Czech D57d defaults)

Optional — the real values are the defaults, applied if the block is absent. OTE spot is EUR/MWh; the
fees are **CZK/kWh**, converted with `eur_czk_rate`.

```json5
tariff: {
  eur_czk_rate: 25.0,
  distribution_high_czk: 0.919,   // VT (high-tariff) distribution + system services
  distribution_low_czk: 0.281,    // NT (low-tariff)
  low_tariff_hours: "0-10,11-12,13-14,15-17,18-24",  // NT local-hour ranges (end exclusive)
  sell_fee_czk: 0.5,              // export = spot − this
  export_price_min_czk: 0.5,      // never export below this spot
  battery_amortisation_czk: 1.0,  // battery wear per kWh discharged
  inverter_off_price_czk: -2.0,   // inverter off below this spot (deeply negative)
}
```

### `battery` and `pv`

Both optional with the real hardware as defaults.

```json5
battery: { capacity_kwh: 10.0, min_soc_pct: 20.0, charge_kw: 5.3, discharge_kw: 5.3, round_trip_efficiency: 0.85 },
pv: {
  system_efficiency: 0.85,        // optional (default 0.85)
  arrays: [                        // the clear-sky fallback (Solcast is preferred when available)
    { name: "terasa", kwp: 7.0, tilt: 35.0, azimuth: 226.0 },   // tilt & azimuth in degrees
    { name: "ulice",  kwp: 6.5, tilt: 35.0, azimuth: 136.0 },
  ],
}
```

### `scheduled_loads` (auto-fitted appliances)

Optional. A **scheduled load** is a known appliance that injects or removes heat at a room's **air
node** on a daily/seasonal schedule the physics model has no source for — e.g. a domestic-hot-water
heat pump that draws heat *out* of its room while it runs, or a wood stove lit on a routine. You
declare the **direction** and the **schedule**; the magnitude (W) is either **set** (`power_w`, when
you know the draw), **learnt from measured data** (omit `power_w` — the same trajectory fit that
learns the per-zone internal gains), or **monitored from a live signal** (`sensor` — the
calibration/backtest derives the flux from the appliance's real electrical draw). The model applies
`magnitude × profile` as a flux in both the optimizer prediction and the backtest/fit drive.

A scheduled load can additionally be marked **`controllable`** — then it isn't a fixed-schedule flux
but a **deferrable electrical load the optimizer switches** (load-shifting): see
[Controllable loads](#controllable-loads-load-shifting) below.

```json5
scheduled_loads: [
  {
    zone: "technical_room",     // must be a zone in model.json5
    label: "water heat-pump",   // optional; for logs/reports
    kind: "sink",               // "sink" removes heat (cools the room) | "source" adds heat
    power_w: 800,               // optional: set to fix the draw (W); omit to auto-fit from data
    // optional: monitor the real draw — derive the historical flux from the measured power (W). The
    // schedule + power_w stay the forecast; only the calibration/backtest read this signal.
    sensor: { type: "influx", bucket: "loxone", measurement: "power", field: "hp_power_w" },
    power_factor: 2.0,          // multiple of P_elec that becomes ZONE heat (≈ COP−1 for a heat-pump sink)
    windows: [                  // local civil-time windows the load is active
      { months: [5, 6, 7, 8, 9],          start: "10:00", end: "20:00" }, // summer: daytime
      { months: [10, 11, 12, 1, 2, 3, 4], start: "01:00", end: "05:00" }, // winter: overnight
    ],
  },
]
```

| Field | Unit | Notes |
|---|---|---|
| `zone` | — | zone whose **air node** the flux lands at; must exist in `model.json5` |
| `label` | — | optional display name (logs, the active-backtest report) |
| `kind` | — | `"sink"` (−, cools) or `"source"` (+, heats) — fixes the sign of the magnitude |
| `power_w` | W | optional; **set** (> 0) to fix the draw, the model uses it as-is and the calibration won't touch it; **omit** to auto-fit. With a `sensor` this stays the **forecast** magnitude |
| `sensor` | — | optional [data source](data-sources.md) reading the appliance's **electrical power** (W). When set, the calibration/backtest derives the flux from this *measured* draw; never a fit candidate |
| `power_factor` | — | optional (default `1.0`); the fraction of electrical power that becomes **zone heat**: `1.0` for a resistive source, `≈ COP − 1` for a heat-pump sink. Heat flux = `P × power_factor`, sign from `kind`. Used with a `sensor` (scales the measured draw) and for a `controllable` load (scales its rated `power_w`) |
| `controllable` | — | optional (default `false`). `true` ⇒ the optimizer **switches** this load on/off within its windows to run for `run_hours` at the cheapest blocks (load-shifting). Requires `power_w` and `run_hours`. See [below](#controllable-loads-load-shifting) |
| `run_hours` | h | required when `controllable` (> 0): the run-time the optimizer must schedule within the windows |
| `windows[].months` | 1–12 | optional; empty ⇒ every month |
| `windows[].start` / `end` | `"HH:MM"` | local civil time; `start` inclusive, `end` exclusive; `end ≤ start` wraps past midnight |

**Set `power_w`** when you know the appliance's draw (e.g. a nameplate-rated heat pump): the model
applies it directly and the live re-fit leaves it alone. **Omit `power_w`** to have the calibration
fit it (W, ≥ 0), so the example heat pump can need only its schedule and `kind`. The fit attributes
the windowed effect to the load rather than smearing it into the always-on internal gain (a flat gain
and a time-localized load are collinear against a single mean, but separate against the per-hour
trajectory). A fitted load whose window doesn't overlap the fit window, or that barely moves any zone,
is dropped (fitted to 0) and logged; a fixed load always applies. Each load's magnitude in use (and
whether it's `configured`, `fitted`, or `measured`) is surfaced under `/api/calibration/gains` →
`live.scheduled`. The schedule is **local** time — set `site.utc_offset_hours` correctly.

**Add a `sensor`** to monitor the appliance's *real* electrical power and **derive** the zone heat
flux from the measured draw — the most robust option for an appliance that runs irregularly (an
away week, a variable run length): the calibration/backtest is grounded in the actual run rather than
an assumed schedule magnitude. The schedule (`windows`/`months`) still gates *when* the flux applies
(the seasonal duct stays authoritative), and `power_w` stays the **forecast** magnitude (the future
draw isn't knowable, so the live plan can't read a sensor). Per step the historical drive applies
`sign × P_elec × power_factor`: set `power_factor ≈ 1.0` for a resistive heater (all the electricity
becomes room heat) or `≈ COP − 1` for a heat-pump **sink** (it removes `P·(COP−1)` from the room while
moving the rest into its tank). A sensor-driven load is a **known** input, never fitted (its measured
flux is already in the calibration baseline). The `sensor` is a [data source](data-sources.md) like
the zone temperatures — for the house's water heat-pump that means a **Loxone Smart Socket → Loxone
Miniserver → InfluxDB** path (a smart socket reports its power, the Miniserver writes it to Influx),
addressed by an `{ type: "influx", … }` locator. The feature **ships dormant**: with no `sensor`
configured (or its signal not yet wired into InfluxDB) the load behaves exactly as before
(`power_w`/fitted), so it can be turned on per appliance once the signal is flowing.

#### Controllable loads (load-shifting)

Set **`controllable: true`** to turn a scheduled load from a *passive* flux into a **deferrable
electrical load the optimizer switches** — the boiler / domestic-hot-water scenario. Instead of
running on a fixed schedule, the optimizer chooses *when* to run it **within its `windows`** so that it
accumulates `run_hours` of run-time at the **cheapest blocks** (responding to the spot price exactly
like the underfloor heating, but as a simple relay).

```json5
scheduled_loads: [
  {
    zone: "technical_room",     // where its waste heat lands (the air node)
    label: "boiler",            // the schedule / controller channel key
    kind: "source",             // "source" warms the room while it runs; "sink" cools it
    controllable: true,         // the optimizer switches it (default false = passive flux)
    power_w: 2000,              // REQUIRED: the rated electrical draw (W) priced when on
    run_hours: 3,               // REQUIRED: run-time to schedule within the windows (h)
    power_factor: 0.1,          // fraction of the draw that heats the ROOM (rest goes into the tank)
    windows: [                  // the load-shift may run only inside these local-time windows
      { start: "00:00", end: "06:00" },  // e.g. the cheap overnight window
    ],
  },
]
```

What the optimizer does with it, end to end:

- **Decision** — a per-block on/off relay, forced off outside the `windows`. Its rated `power_w` is
  added to the house electrical load (met from solar / battery / grid) and **priced at the import
  tariff**, so running it is a real cost the optimizer shifts to cheap blocks — the load-shift.
- **Run-hours** — a *soft* target: `Σ on·dt ≥ run_hours`, slack-penalized, so a window too short to
  fit `run_hours` simply runs as much as it can rather than making the plan infeasible.
- **Heat-when-on** — its `kind × power_w × power_factor` air-node heat couples into the thermal
  prediction **only in the blocks it runs** (a resistive boiler with `power_factor ≈ 1` dumps all of
  it into the room; a tank that carries the heat away uses a small factor). So scheduling it warms (or,
  for a `sink`, cools) the room exactly when it runs, and the comfort band sees it.

The reported schedule is in the plan's `controllable_load_kw` (per load, per block; `on · power_w`),
surfaced in `first_step` and the timeline, and republished to the dry-run boiler controller (see
[controllers.md](controllers.md)). **Ships dormant:** `controllable` defaults to `false`, so an
existing scheduled load is unchanged — the plan is byte-identical until you opt a load in.

### Loop knobs (all optional, with defaults)

| Key | Default | Meaning |
|---|---|---|
| `consumption_history_days` | 30 | trailing window to train the consumption model |
| `mpc_tick_minutes` | 60 | how often the MPC loop re-plans (also the `/readyz` staleness threshold) |
| `internal_gain_window_days` | 7 | window for the live internal-gain re-fit |
| `internal_gain_recalibrate_hours` | 24 | re-fit cadence (0 disables) |
| `forecast_snapshot_minutes` | 60 | forward-prediction snapshot cadence (0 disables) |

### `db` and `zone_mappings`

`db` and `zone_mappings` are read by `influxdb.rs`. Each zone maps to the InfluxDB series holding its
measured temperature.

```json5
db: { host: "http://localhost:8086", org: "loxone" },
zone_mappings: {
  livingroom: {
    temperature: {
      bucket: "loxone", measurement: "temperature",
      tags: { room: "obyvak" }, field: "temperature_obyvak",
    },
  },
}
```

---

## Tips & recipes

- **Names must match** across the three places: a zone in `config.json5` `heating.zones` /
  `hvac.comfort` / `zone_mappings` must be a real zone in `model.json5`.
- **Where does a new value go?** Physical fact about the building → `model.json5`. Operational or
  economic knob → `config.json5`.
- **Optional blocks** (`hvac`, `tariff`, `battery`, `pv`, the loop knobs) fall back to sensible
  defaults when absent; `site` and `heating` are required.

### Recipe: add a new room end-to-end

1. **`model.json5`** — add the zone and its boundaries:
   - `zones: { office: { volume: 38.0 } }`
   - one boundary per wall/floor/ceiling, with `area` and (for exterior walls) `azimuth`/`angle`; add a
     `{ marker: "heating" }` layer to the floor type if it is underfloor-heated.
2. **`config.json5`** — make it controllable:
   - heated? add `heating.zones.office = { max_heat_kw, t_min, t_max, internal_gain_w? }`.
   - has AC/HVAC? add `hvac.comfort.office = { t_heat, t_cool }` and list `office` in a unit's `zones`.
   - add `zone_mappings.office` so the live temperature is read.
3. Run `cargo run` — the demo loads both files, builds the model, and runs the plan; a malformed file
   or a dangling reference fails fast with a message.

### Validation errors you may hit

| Message (paraphrased) | Cause |
|---|---|
| `'outside'/'ground' is a reserved zone name` | you defined a reserved boundary zone — remove it |
| sub-boundary area exceeds the parent | a `sub_boundaries` area is larger than its boundary's `area` |
| two consecutive markers / no non-marker layer | a `Layered` type's `layers` violate the marker rules |
| missing material / boundary_type / zone reference | a name doesn't resolve — check spelling across files |
| `hvac unit … references zone … with no hvac.comfort entry` | a unit serves a zone you didn't give a `comfort` deadband |
| COP curve must be strictly increasing / COP must be positive | a `cooling_cop`/`heating_cop` curve is out of order or non-positive |
| `t_cool must be ≥ t_heat` | an `hvac.comfort` deadband is inverted |
