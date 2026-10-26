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
| `boundary_types` Layered `solar_absorptance` | ratio | **dimensionless** (0–1, default 1.0) |
| Simple boundary `u` | heat transfer | **W/(m²·K)** |
| Simple boundary `g` | ratio | **dimensionless** (0–1) — effective solar transmittance of the FULL aperture (glass g × glazed-area fraction) |
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
  concrete:         { thermal_conductivity: 1.5,   specific_heat_capacity: 1020, density: 2250 },
  brick_440:        { thermal_conductivity: 0.059, specific_heat_capacity: 1000, density: 660 },   // Heluz Family 2v1 44
  floor_insulation: { thermal_conductivity: 0.035, specific_heat_capacity: 1270, density: 30 },
  spray_foam:       { thermal_conductivity: 0.030, specific_heat_capacity: 1400, density: 35 },    // closed-cell PUR under the rafters
  vencovka:         { thermal_conductivity: 0.090, specific_heat_capacity: 1000, density: 400 },   // Heluz věncovka ring-beam shell
}
```

`air` is **auto-supplied** (0.026 W/(m·K), 1012 J/(kg·K), 1.199 kg/m³); define it yourself only to
override those defaults. The real `model.json5` defines ~14 materials — the Heluz brick range
(`brick_440`/`300`/`175`/`175_accoustic`), `ext_plaster`/`int_plaster`, `floor_insulation`,
`anhydrite`, `concrete`, `drywall`, `rock_wool`, `spray_foam`, and `vencovka` — each from a
manufacturer datasheet; copy the pattern with your own values.

### `zones`

Each zone is a name → its air `volume` (m³). The zone name is the key.

```json5
zones: {
  livingroom: { volume: 62.5, ach: 0.25 },
  bedroom:    { volume: 48.0, ach: 0.25 },
}
```

- **`ach`** (optional, default 0): infiltration/ventilation air changes per hour — one extra
  conductance edge zone-air ↔ outside (`ρ·c_p·V·ach/3600`). Even a tight house leaks 0.2–0.5 ACH
  (a per-room W/K comparable to a whole insulated wall); leave it 0 and that loss gets laundered
  into the calibrated gains instead. Use ~0.25 for living space, more for leaky/ventilated spaces
  (garage, roof void); calibrate against the passive backtest.

- **`outside` and `ground` are reserved** — they are auto-injected as boundary zones with *infinite*
  heat capacity (their temperature is an input, not a state). **Defining either is a hard error.**
- A boundary may **not** connect a zone to itself.

### `boundary_types`

A reusable template for a wall / floor / roof / window. Two shapes (an untagged enum — chosen by which
keys are present):

**Layered** — a stack of material layers with mass. Layers are listed as the **physical stack from
`zones[0]`'s face to `zones[1]`'s face**: the first layer touches the *first* zone named in each
boundary, the last layer the *second* (`rc_network.rs` attaches the first layer to `zones[0]`). **Get
this direction right** — reversing it puts the mass and insulation on the wrong sides. The model expands
the stack into a chain of capacitive nodes.

```json5
// Ground-floor slab, listed room-side (zones[0]) → ground-side (zones[1]):
ground_level_floor: {
  layers: [
    { material: "anhydrite",        thickness: 0.05 },  // walking surface (room side)
    { marker: "heating" },                              // underfloor-heating actuator node, just under the screed
    { material: "floor_insulation", thickness: 0.14 },  // blocks downward loss to the ground
    { material: "concrete",         thickness: 0.20 },  // structural slab (ground side)
  ],
},
```

- `{ marker: "name" }` places a **named node between layers** — the actuator/measurement point. The
  underfloor heating injects its power at the `"heating"` marker (see [`docs`](#hvac--air-side-heating-and-cooling)).
- Rules: **at least one non-marker layer**, and **no two consecutive markers**.
- **`solar_absorptance`** (optional, default `1.0`): the fraction of incident solar the outer surface
  absorbs (0–1). A Layered boundary that touches `outside` and carries `azimuth`/`angle` becomes a
  **solar surface**, and its absorbed flux is `irradiance × area × solar_absorptance`. Lower it for a
  reflective or **ventilated** assembly — e.g. the `roof` type uses `0.2`, because the air gap under
  the black tiles carries most of the absorbed heat away. Omit it (⇒ `1.0`) for a plain opaque face.
- Beyond walls, the real `model.json5` instantiates this same Layered shape as:
  - **`first_level_floor`** — the inter-floor slab (UFH screed + marker + insulation + structural slab),
    listed upper-room (`zones[0]`) → lower-room (`zones[1]`);
  - **`first_level_ceiling`** — the light upper-room ceiling to the attic (drywall + rock wool);
  - **`ground_level_ceiling`** — a **bare concrete slab** where a ground-floor ceiling meets the attic
    *directly* (no heated room above it — the eave dead-space behind a knee wall, or behind the bathroom);
  - **`venec`** — the věncovka + EPS + reinforced-concrete ring-beam band below each ceiling, listed
    outside (`zones[0]`) → in (so `outside`/`garrage` is `zones[0]`);
  - **`roof`** — the insulated roof, carrying `solar_absorptance`;
  - **`plaster_partition`** — a single drywall layer.

**Simple** — a massless element given by its U-value (windows, doors). An **oriented exterior**
Simple boundary (it inherits its parent wall's `azimuth`/`angle`) with `g > 0` is a **transparent
aperture**: transmitted solar `g × area × irradiance` enters the interior zone, split ~30 % to the
room air and ~70 % into the floor-slab mass. Because the model applies `g` to the FULL aperture
area (frame included), author it as *glass g-value × glazed-area fraction*:

```json5
window:        { u: 0.74, g: 0.35 },  // 0.5 glass g × ~0.7 glazed fraction (frames ~30 % of small windows)
hs_portal:     { u: 0.96, g: 0.44 },  // 0.52 × ~0.85 (big lift-slide portals, slim frames)
entrance_door: { u: 0.83, g: 0.13 },  // mostly-opaque door with a glazed panel
interior_door: { u: 2.8,  g: 0.0 },   // interior: no solar regardless
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
  (touching `outside`) need them. Sub-boundaries **inherit** the parent's orientation. Use your
  house's true bearings — the example house is rotated ~50° off cardinal, so its faces are
  SE = 140°, SW = 230°, NW = 320°, NE = 50°.
- Zero-area boundaries are skipped.
- **Unheated buffer zones** (e.g. `attic`, `garrage`) are bounded like any other zone: their exterior
  envelope uses `exterior_wall` / `venec` / `roof` to `outside`, and the heated rooms around them connect
  via `first_level_ceiling` / `ground_level_ceiling` / interior walls. The attic, for instance, is closed
  by its two roof slopes **plus** a NW gable and NE/SE eave walls (`exterior_wall` + `venec`) to `outside`
  — give each sun-exposed face its `azimuth`/`angle` so it still collects solar onto the buffer.

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
  timezone: "Europe/Prague",   // IANA zone — offsets derive per timestamp, so DST needs no edits
  utc_offset_hours: 2,         // FALLBACK only when `timezone` is unset (goes stale at every DST changeover)
  ground_temperature_c: 16.0,  // optional (default 16) — the `ground` boundary temperature under the slab
  public_holidays: ["01-01", "05-01", "05-08", "07-05", "07-06", "09-28", "10-28",
                     "11-17", "12-24", "12-25", "12-26"],  // optional — "MM-DD", default: the Czech set above
  easter_holidays: true,       // optional (default true) — also treat Good Friday + Easter Monday as Sundays
}
```

Set `timezone` (validated at load). With it, the VT/NT tariff hours classify **per block**, the
consumption bins / PV-curve keys / backtest keys derive **per sample**, so a horizon or training
window crossing a DST changeover stays correct. Without it, `utc_offset_hours` applies year-round
and must be hand-edited twice a year.

`public_holidays` (fixed-date, `"MM-DD"`) and `easter_holidays` (Good Friday + Easter Monday, from
the Gregorian Easter computus — `optimize::price_forecast::easter_sunday`) feed the DAY-TYPE price
estimator below: a holiday is treated as a SUNDAY (the low-industrial-demand OTE spot shape), not
whatever weekday it happens to fall on. Both default to the Czech public-holiday set shown above —
a house on a different market/calendar should override `public_holidays` with its own dates (an
empty list `[]` disables fixed-date holidays entirely; `easter_holidays: false` disables the
computed Easter dates). Malformed entries (not a valid `"MM-DD"`) are rejected at config load.

*The day-type median price estimator.* Both the post-horizon outlook price (the terminal slab-heat
credit's displaced price, above) and the live plan's own unpublished-tail blocks (tomorrow before
the ~14:00 OTE auction) are estimated the same way (backtested against 4+ years of OTE
history: cheapest-4-hour regret 5.6 vs 6.7 EUR/MWh over the last 12 months, 6.6 vs 10.6 over the
whole backtest — plain repeat-yesterday persistence is measurably worse): `optimize::
price_forecast::day_type_median_price` takes the MEDIAN price at the SAME local 15-minute clock
slot over the most recent 4 days of the SAME day type (`Work` = Monday-Friday non-holiday, `Sat`,
`Sun` = Sunday or a public holiday) from a cached, bounded (≤ 30 day: 28 back + 2 forward, so
today's remainder and tomorrow's already-published curve are seen too) `ote_prices` history,
refreshed at most hourly (`app::PlanCache::price_history_eur_kwh` / `PRICE_HISTORY_TTL`, read with
its own short — ≤5 s — timeout so a struggling InfluxDB can't stall a cache refresh, and a FAILED
attempt is stamped too so it backs off to the hourly TTL rather than retrying every cache cycle) —
never per tick, never an unbounded query. Fewer than 2 matching days (thin history, a day type
barely seen yet) falls back to plain repeat-yesterday persistence. The history is cached as SPOT
EUR/kWh (the field name says so — `price_history_eur_kwh` — never raw EUR/MWh); the outlook usage
tariffs the median (`import = spot + distribution`, the SAME per-local-hour formula
`app::tariff_prices` uses) before comparing it against the persistence fallback, which is already
tariffed, so one outlook array is never a mix of spot and import-price scales. An estimated block
is still flagged `price_is_placeholder: true` — battery arbitrage never commits against an
estimate, exactly like the persistence/placeholder chain it augments; `/api/plan`'s placeholder
text names the method(s) actually used — a single source keeps the plain word (`"day-ahead prices
(N/144 blocks unpublished; day-type median)"`, or `"persistence"` / `"placeholder"`), and a run
that mixes fallbacks reports the split, e.g. `"day-ahead prices (94/144 unpublished: 60 day-type
median, 30 persistence, 4 fixed curve)"`.

### `grid` (connection limits)

```json5
grid: {
  max_import_kw: 17.0,   // optional — cap on grid→(load+battery+EV) per block (3×25 A ≈ 17 kW)
  max_export_kw: 17.0,   // optional — cap on (solar+battery)→grid per block
}
```

Both optional (absent = unconstrained). Without `max_import_kw` the optimizer can stack an 11 kW EV
charge + battery grid-charge + the house load into one cheap block — past what the main breaker can
physically deliver. Set it to the real service rating, slightly below for headroom.

### `horizon` (the multi-rate planning grid)

```json5
horizon: {
  hours: 36,          // optional (default 36) — total planning horizon
  fine_hours: 6,      // optional (default 6) — how much of it stays at 15-minute resolution
  outlook_hours: 36,  // optional (default 36) — post-horizon weather lookahead for the terminal
                       // slab-heat credit (see "The terminal slab-heat credit's displaced price"
                       // below); 0 disables it, up to 336 (14 days)
}
```

The plan covers `hours` total, but only the first `fine_hours` run at the full 15-minute (OTE
price grid) resolution — the rest coarsens to 1-hour blocks. This keeps the LP much smaller (54
blocks by default instead of a uniform 144 — a block count that scales with `fine_hours`, and an
LP whose build/solve cost scales roughly with the block count squared) so HiGHS can solve it
within the live one-minute tick even on a winter catch-up (see
`memory/mpchc-36h-lp-unsolvable-in-winter.md`); only the near-term
decisions the loop actually actuates need quarter-hour precision, since every re-plan re-optimizes
the far blocks anyway. Hourly blocks are **hour-aligned** (VT/NT, hourly prices and weather are
calendar-hour keyed): the fine section is rounded up to the next calendar-hour boundary if the
plan starts mid-hour, and a trailing partial hour is dropped — so the *effective* horizon can be
up to ~1 h short of `hours` (35–36 h for the default). `fine_hours >= hours` degenerates to a
uniform 15-minute grid over the whole horizon (what every test and `what_if` use; not the live
configuration). See `src/optimize/grid.rs` (`BlockGrid`) for the construction.

`hours` may not exceed the compile-time feed horizon (`HORIZON_HOURS` in `app/mod.rs`, 36) — the
weather/PV/price fine-lattice assembly is only built that far ahead. A larger value is rejected at
**config load** with a clear error rather than silently accepted and only discovered as every
single plan failing at runtime.

`outlook_hours` is a SEPARATE, later read (`app::current_plan`'s outlook fetch, starting exactly
where the horizon grid ends) — it never feeds the LP and so isn't bounded by `HORIZON_HOURS`, only
validated `<= 336` at config load. Raising it is a no-op until the weather scraper actually stores
that much forecast: **the open-meteo scraper's own `horizon_hours` (`scrapers/openmeteo/
scraper.json5`) must be at least `horizon.hours + horizon.outlook_hours`** to cover both the plan
horizon and the outlook past it — open-meteo's API serves up to 16 days (384 h), well past the 336 h
ceiling here. The live plan additionally TRUNCATES the outlook to however many hours the stored
forecast actually covers (`WeatherForecast::covered_hours`) — a scraper window shorter than
`outlook_hours` shrinks the outlook accordingly rather than forward-filling a flat guess over the
uncovered days.

Comfort is enforced at each block's **END**, not continuously through it — a block's soft-comfort
row checks the affine-predicted temperature at its own end only, so a fine (15-minute) block is
effectively checked every 15 minutes near-term, but an hourly block only constrains the top of the
hour: a mid-hour dip is not penalized. `fine_hours` is therefore also the span over which comfort
gets 15-minute resolution; beyond it, only the hourly checkpoints bind.

Timeline blocks report their own duration (`dt_minutes`): the publisher derives `valid_until` from
it and the dashboard plots hourly blocks four times as wide as fine ones.

### `comfort` (what temperature each room should be)

One top-level block says what every room should be: a **heat target** and band, an optional **cool
target** and band, a **priority**, and optional daily **windows** that shift the targets. The
equipment (`heating.zones`, `hvac.units`) says only what can be *done* about it.

```json5
comfort: {
  penalty: 50.0,            // price-units per K per step outside the hard-ish band edge (optional, default 50)
  overheat_penalty: 0.2,    // price-units per K per step above the heat ceiling of a room with NO AC (optional, default 0.2)
  overheat_max_c: 1.5,      // width (K) of that mild tier; beyond it the full `penalty` applies (optional, default 1.5)
  pull_eur_per_kh: 0.04,    // EUR per K·h a room sits BELOW its heat target, × the room's priority (optional, default 0 = no pull)
  mode_switch_hours: 24,    // hours of opposite thermal tendency before a room switches heating <-> cooling mode (optional, default 24)
  rooms: {
    livingroom: {
      heat: { target: 22.0, below: 1.0, above: 2.0 },   // floor 21, ceiling 24
      cool: { target: 25.0, delta: 1.0 },               // pre-cool floor 24, hold ceiling 26 — inert until an AC unit serves it
      priority: 1.0,                                    // optional (default 1); 0 = band only, no pull
      windows: [ { start: "22:00", end: "06:00", heat_target: 20.0 } ],  // optional — night setback: target 20, floor 19, ceiling stays 24
    },
    storage: { heat: { target: 18.0, delta: 3.0 }, priority: 0.0 },      // symmetric: 15 .. 21
  },
}
```

**Editing targets.** A room's `heat.target` / `cool.target` can be changed live through the brain's API
(`POST /api/comfort/<zone>`, see `docs/api.md`), which rewrites just that number in `config.json5` and
applies it on the next tick. The file remains the record: pull the live file into the repo before
deploying (the deploy guard refuses a differing `comfort:` block). The brain edits a room only if it
sits on ONE line of the form `<zone>: { heat: { target: <number>, ...`, as in the repo file; keep that
shape when editing by hand.

| Field | Unit | Notes |
|---|---|---|
| `penalty` | price-units/(K·step) | optional (default 50). Weight of the **hard-ish** band edge: below the heat floor, and above the cool ceiling of an AC room. Finite and `> 0` (zero is rejected at load — the edge is enforced only through this soft-slack weight, so `0` would make "never heat" optimal) |
| `overheat_penalty` | price-units/(K·step) | optional (default 0.2). Mild weight above the **heat ceiling** of a room with **no AC**. Finite, `> 0` and `< penalty`. Applies only to the first `overheat_max_c` K above the ceiling |
| `overheat_max_c` | K | optional (default 1.5). Width of the mild tier above the heat ceiling: ceiling → ceiling + `overheat_max_c` costs `overheat_penalty`, anything beyond costs the full `penalty`. Finite and `≥ 0` (`0` = no mild tier, a hard-ish ceiling). Without the cap a sustained negative price (or the terminal slab credit) could push a room arbitrarily far past its ceiling |
| `mode_switch_hours` | h | optional (default 24). How long the opposite thermal tendency must be seen **continuously** before a room switches between heating and cooling mode (see "Heating and cooling are exclusive per room" below). Finite and `≥ 0` (`0` = switch on the first opposite reading) |
| `pull_eur_per_kh` | EUR/(K·h) | optional (default 0). What one K·h below the heat target costs, × the room `priority`. Finite and `≥ 0`; `pull_eur_per_kh × (largest priority) < penalty` (a pull at or above the edge penalty would make the LP prefer floor slack over genuine warmth) |
| `rooms.<zone>.heat.target` | °C | where the room should sit |
| `rooms.<zone>.heat.delta` | K | symmetric half-width (below and above) |
| `rooms.<zone>.heat.below` / `above` | K | per-side override; a side without one falls back to `delta`. `delta` together with **both** overrides is rejected (`delta` would be dead). Each side must resolve (a `delta`, or both overrides). All `≥ 0` and finite |
| `rooms.<zone>.cool` | — | optional; same `{ target, delta \| below, above }` shape. **Required for a zone an `hvac.units` entry serves** (load error otherwise) |
| `rooms.<zone>.priority` | — | optional (default 1), finite and `≥ 0`; scales `pull_eur_per_kh` for this room |
| `rooms.<zone>.windows` | — | optional daily target schedule, below |

Every `heating.zones` entry must have a `comfort.rooms.<zone>` (its `heat` target) — a heated room with
no setpoint is a load error. A `comfort.rooms` entry with no equipment is inert (and every name is
checked against `model.json5`).

**The cost regions.** Per room and block, as temperature falls or rises through the bands (all linear,
the LP stays pure; `T` = the room's predicted air temperature):

| Temperature region | Cost |
|---|---|
| `T <` heat `target − below` | `penalty` per K per step |
| heat `[target − below, target)` | `pull_eur_per_kh × priority` per K·h — the price-driven flex: the room sits at target and dips toward the floor only when power is dear |
| heat `[target, target + above]` | energy only — pre-heat / banking headroom |
| heat `(target + above, target + above + overheat_max_c]`, **no AC** | `overheat_penalty` per K per step — mild, so passive sun warmth of up to `overheat_max_c` K (or a terminal slab credit) cannot distort the plan |
| `T >` heat `target + above + overheat_max_c`, **no AC** | `penalty` per K per step — the cap: free or negative-price energy cannot cook the room |
| AC room, cool `[target − below, target]` | energy only; cooling may pre-cool **no lower than** `target − below` (the pre-cool floor) |
| AC room, cool `(target, target + above]` | `pull_eur_per_kh × priority` per K·h (mirror of heating) |
| AC room, `T >` cool `target + above` | `penalty` per K per step |

The table lists both sides, but a room works on one at a time (the **room mode**, below): in `heating`
mode only the heat rows exist and cooling is forced to 0; in `cooling` mode only the cool rows exist and
floor/air heating is forced to 0. In heating mode the heat-ceiling rows (mild tier + cap) hold the whole
air prediction (slab and air heating), so an AC room's air heating cannot run past the ceiling. A **priority-0** room (or
`pull_eur_per_kh: 0`) has no pull: it keeps today's floor exactly (`target − below`; put `below: 0` and
`target` = the old `t_min`), but its ceiling is the mild tier (`overheat_penalty`) capped at
`overheat_max_c` above `target + above` — not the old hard `t_max` edge.

**Picking targets and priorities.** The floor (`target − below`) and ceiling (`target + above`) are what
the old `t_min`/`t_max` were; the new thing is the **target**. Without a pull, a cost-minimising plan
parks every room at its cheap edge (the floor), so the only reason a room ever sits warmer is the pull.
`priority` is the ordering knob (the house uses 1.0 for the family rooms, 0.5 for bathrooms and toilet,
0 for the rest). Give a room with no actuator that can reach its ceiling a **wide** `above` (storage:
`above: 7` ⇒ 25 °C): an unreachable tight ceiling distorts the whole plan, which is why the no-AC
ceiling is the mild tier rather than a hard cap.

**Reading `pull_eur_per_kh` as a price — the break-even.** The LP buys heat for room `z` beyond what its
floor needs whenever the import price is below `z`'s break-even (reported live per room in `/api/plan`'s
`pull_break_even_eur_per_kwh`; `heating.cop` is inside the formula, so compare it with the import price
directly). Read it as an **approximate indicator, not an exact indifference price**: the real trade-off
depends on the kernel's lag shape, the neighbours' bands, how much floor heat the room needs anyway, and
the horizon edge, so the LP can be indifferent noticeably above or below it. What holds: well below it
the pull buys at least as much heat as no pull, and well above it (a few ×) it buys no extra heat:

```
break_even[z] = heating.cop × Σ_z' (pull_eur_per_kh × priority_z') × G_{z',z}
```

where `G_{z',z}` is room `z`'s heater's horizon-summed Kelvin·hour effect on room `z'` (the sum of
`optimize::thermal::ThermalContext::kernels[(z', z)]` over the horizon's fine lags). **The sum runs over
every pulled room `z'`, not just `z`**: heating a room warms its coupled neighbours, and a pulled
neighbour's weight adds to what the LP is willing to pay (livingroom and kitchen are one thermally open
pair; the hall couples into both). Typical self-kernels are 2.3 (livingroom) to 19 (first-floor
bathroom) K·h per kWh, so a given pull gives a much higher break-even in a small room. The house starts
at `pull_eur_per_kh: 0.04` (**provisional**); the Jan-2026 winter replay
(`cargo run -- backtest-comfort --pull 0,0.02,0.04,0.08 ...`) reports cost (EUR/week) and the per-room
K·h below target and mean/min air temperature at each weight, and the owner picks the value from it.
Break-even is **not** a hard cut-off in winter: when a room needs floor heat anyway, heat bought earlier
both earns the pull and replaces floor heat bought later, so the LP shifts heat into cheap blocks well
above the break-even. Kernel caveat: the underfloor kernel over-responds roughly 1.5× in cold weather, so
the owner pays up to ~1.5× the nominal price per real K·h; re-read the break-even once that gain is
corrected.

**`windows` — a daily target schedule.** `[{ start, end, heat_target?, cool_target? }]`, local civil
time (`HH:MM`, DST-safe via `site.timezone`); `end < start` wraps past midnight (`22:00 → 06:00`);
`end == start` is rejected (an empty window — set the room's `heat_target` for an all-day target).
Inside a window a `heat_target` moves the heat **target and floor** (`target − below`) only; the heat
**ceiling stays the base ceiling** (base `target + above`), so a setback never turns banked or fireplace
heat into full-penalty overshoot. In the livingroom example the 22:00–06:00 setback gives target 20 and
floor 19, while the ceiling stays 24 (not 22). A window `cool_target` likewise moves the cool target and
the pre-cool floor, never the cool ceiling, and needs the room to have a `cool`. A window target must
not exceed its base ceiling (`heat_target ≤ heat.target + above`, `cool_target ≤ cool.target + above`),
or the load fails. Later windows win per field on overlap; a field a window omits keeps the base. The
optimizer prices the pre-heat before a morning target step into the cheap night hours automatically.
The band is evaluated at the instant a temperature *refers to* (the end of its block).

**Cooling is prepared but inert.** Every room may carry a `cool` target today although the house has no
AC: it is parsed, validated, exposed in the API (`cool.active: false`) and shown on the dashboard, but
adds **no LP term** until an `hvac.units` entry serves the room — then it acts with no other change.
The heat ceiling and the cool band **may overlap** (e.g. heat 21.5 with a ceiling of 24 and a cool floor
of 22): a room only ever works on one side at a time, see the next section. The pre-cool floor must stay
**0.5 K above** every heat target: the lowest `cool_target − below` (base or any window) must be ≥ the
highest heat `target` (base or any window) + 0.5, or the load fails — pre-cooling to or near the heat
target would trip the safety rule below (estimate error and cooling from a neighbour's unit are not
seen by the pre-cool cap) and switch the room to heating for `mode_switch_hours`.

**Heating and cooling are exclusive per room (the room mode).** Before every plan each controlled room
is given one mode, `heating` or `cooling`, from the house's own thermal tendency; the LP then keeps only
that side:

- *Tendency* = the **free drift**: the room's predicted air temperature with no heating and no cooling
  (weather, sun, internal gains, scheduled loads and stored slab heat — the same free response the LP
  uses) at +24 h (or the horizon end if shorter) minus its temperature now, in K/day. Losing heat (≤ 0)
  → heating; gaining (> 0) → cooling.
- *Safety rule*: a room whose estimated air temperature is **below its heat target in force** is always
  in `heating` mode, whatever the tendency (a sunny October day cannot switch off a cold room's floor).
- *Hysteresis*: otherwise a room changes mode only after the opposite tendency has been observed
  continuously for `comfort.mode_switch_hours` (24 h). With no history a room starts in heating (the
  conservative side) and a gaining room earns cooling through the same 24 h hysteresis. The state is persisted to `MPC_ROOM_MODE_STORE` (default `room_modes.json`), so a
  restart does not flip modes; bind-mount it like the other stores.
- *Heating mode*: the heat band is exactly as described above; cooling for the room is forced to 0 (no
  AC whatever the temperature) and the cool band adds no terms. In January a room at 26 °C with the
  fireplace lit stays in heating mode and is never cooled.
- *Cooling mode*: the cool band (AC rooms: pull above the cool target, pre-cool floor, penalty above the
  ceiling) is active; floor heating and HVAC air-heating are forced to 0 and the heat band adds no
  terms. A floor-heated room **without** AC in cooling mode floats (no floor/pull terms) until the
  safety rule returns it to heating mode, but keeps its heat ceiling on the slab heat it receives, so a
  heating neighbour cannot overheat it for free — while its own passive overheat earns an AC neighbour
  nothing for cooling.
- A unit shared across rooms still may not heat one of its rooms while cooling another in the same block.

`/api/zones` and `/api/rooms` report each room's `mode`, `mode_since` and `drift_k_per_day`.

**Removed fields.** The pre-`comfort` fields are rejected at load with an error naming the replacement:
`heating.zones.<z>.{t_min, t_max}` → `comfort.rooms.<z>.heat`; `overheat_c` → `heat.above` (the mild tier
past it is `comfort.overheat_penalty`); `warmth_value_eur_per_kh` → `comfort.pull_eur_per_kh ×
priority`; `heating.zones.<z>.windows` → `comfort.rooms.<z>.windows`; `heating.comfort_penalty`,
`hvac.comfort_penalty` → `comfort.penalty`; `heating.overheat_penalty` → `comfort.overheat_penalty`;
`hvac.comfort`, `hvac.default_comfort` → `comfort.rooms.<z>.cool`. There are no aliases. A `comfort` key
inside `heating` or `hvac` (`heating.comfort`, `heating.zones.<z>.comfort`, `hvac.units.<u>.comfort`,
including inside arrays) would be silently ignored, so it is rejected too, pointing at the top-level block. Listing a room twice in `comfort.rooms`
is rejected (it would otherwise keep only the last entry).

### `heating` (underfloor equipment)

```json5
heating: {
  cop: 1.0,                 // heat delivered per kWh electricity. 1.0 = resistive; >1 = a heat pump
  coupling_min_k: 0.05,     // optional (default 0.05) — drop a physically-negligible cross-zone coupling, see below
  relay_switch_cost_eur: 0, // optional (default 0 = off) — EUR per relay on/off change, prefers longer heating blocks, see below
  zones: {                  // a zone absent here is NOT heated; each needs a comfort.rooms.<zone> target
    livingroom: { max_heat_kw: 3.0, internal_gain_w: 351 },
    bedroom:    { max_heat_kw: 1.2 },
  },
  gain_groups: [ ["kitchen", "livingroom"] ],  // optional — see below
}
```

The temperatures a zone should hold are in [`comfort`](#comfort-what-temperature-each-room-should-be);
`heating` is equipment only.

| Field | Unit | Notes |
|---|---|---|
| `cop` | — | heat / electricity |
| `coupling_min_k` | K | optional (default 0.05); drops a negligible cross-zone slab coupling from the LP (and the reported temperature), see below — must be finite and `≥ 0`; `0` keeps every pair |
| `relay_switch_cost_eur` | EUR per switch | optional (default `0` = off, plan unchanged); price of ONE on↔off change of one zone's underfloor relay, see below — must be finite and `≥ 0` |
| `zones.*.max_heat_kw` | kW | the zone's underfloor circuit power (the relay rating); caps the optimizer's per-step heat for the zone |
| `zones.*.internal_gain_w` | W | optional (default 0); occupants/appliances/fireplace — the live fit refines it into a night/day/evening profile |
| `gain_groups` | — | optional list of zone-name lists; see below |
| `extra_gain_zones` | — | optional `[{ zone, max_w }]` for zones outside `zones` that may still be fitted a (capped) gain — a garage with a car; see below |

*The terminal slab-heat credit's displaced price.* `terminal_heat_value` values heat banked in the horizon's last ~6 h by what it saves the house from
paying LATER. A single flat number per zone — `terminal_value / heating.cop *
TERMINAL_HEAT_RETENTION` (`TERMINAL_HEAT_RETENTION = 0.8`), where `terminal_value` is the battery's
own median-import-based terminal value, the SAME for every zone and every day — would ignore WHEN the
banked heat is actually needed and what heating will cost then: before a cold snap landing in
an expensive stretch it under-values banking; when the post-horizon heating can happen in a cheap
window anyway it over-values it. The planner therefore estimates, per heated zone, the DISPLACED price —
the price a future plan would actually pay for that zone's post-horizon heat — from the post-horizon
weather outlook (`ForecastContext::outlook`, `horizon.outlook_hours` past the horizon — see the
`horizon` section above — never fed into the LP itself):
`optimize::coordinator::estimate_outlook_prices` persists the horizon's own import price forward
onto the outlook by LOCAL CLOCK time (preferring a real, non-placeholder horizon block over a
placeholder one at the same clock slot), and `displaced_price_by_zone` then takes, for each zone
with a positive outlook deficit (`outlook_deficit_kwh`), the energy-weighted mean price of the
CHEAPEST outlook blocks in a SEARCH WINDOW, enough to cover the deficit at the zone's `max_heat_kw`
(at least one block). The window is `[0, max(first dip, one day's worth of blocks))`, capped at the
outlook length — NOT simply "before the first dip": the outlook free response continues from the
horizon's own end-state with every actuator off, so on a heating tick nearly every zone is ALREADY
below its floor at outlook block 0, and pricing "before the dip" alone would then price almost every
zone at its own current (possibly peak) block — exactly the flat-scalar problem this credit exists
to fix (it shows up as a uniform credit across zones despite real 0.08 EUR/kWh night blocks
existing in the same outlook). A day (24 h) is one full price cycle, so a block-0 dip still
prices at the day's cheapest hours, not the current slot; a dip further out searches every block
before it. `terminal_heat_value[zone] = displaced_price / heating.cop *
TERMINAL_HEAT_RETENTION` — same formula, per-zone displaced price in place of the flat
`terminal_value`. A zone with no outlook coverage, no deficit, or a non-finite estimate falls back to
the flat `terminal_heat_value`, bit-for-bit — including whenever no outlook was supplied at all.
`terminal_heat_budget_kwh` (the per-zone banking cap) is unchanged by this. See `/api/plan`'s
`terminal_heat_credit_eur_per_kwh` (`docs/api.md`) for the value actually applied per zone on the
live plan.

The zone name must exist in `model.json5` and have a `"heating"` marker for the heat to land.

**`relay_switch_cost_eur`** — relay wear, not comfort. The slab responds over hours, so scattered
15-minute heating pulses buy almost no comfort yet switch a (mechanical) relay carrying kilowatts.
With a positive value the LP charges this many EUR for every on↔off change of a zone's relay: per
zone and block it adds `s_t ≥ |u_t − u_{t−1}|` (`u = heat_kw / max_heat_kw`, two linear rows, so the
LP stays pure) and `relay_switch_cost_eur × s_t` to the objective, which merges pulses into fewer,
longer runs while the comfort floors still hold. `0` (default) adds nothing: the plan is
bit-for-bit what it was before the knob. `u_{-1}` (the relay state before block 0) comes from the
loop's latch when the previous block's relays are still held on the first plan of a new block;
otherwise block 0's own switch is not counted (block 0 is pinned by the latch anyway). Switching
*inside* a multi-rate hourly tail block is not counted, and the far-horizon tail (blocks beyond the
near-term relay window) is a fractional duty, so its cost acts on duty changes. Tuning: the cost is
in the same EUR as the plan, so compare it with what one extra pulse saves — a 1–2 kW zone saves
about 0.01–0.05 EUR per cheap-quarter-hour pulse; start around `0.01` and raise until the pulses
merge as far as you want (`backtest-comfort --switch-cost 0,0.005,0.01,0.02` replays arms side by
side and reports executed switches per zone). Reported per plan as `relay_switches` /
`relay_switches_by_zone` on `/api/plan`. HVAC/AC units, the EV and the battery are not affected.

**`coupling_min_k`** — a speed knob, not a comfort one. Every heated zone's underfloor slab has an
impulse-response kernel onto every OTHER heated zone (heat flowing through the shared wall/floor);
with N heated zones that is N² kernel pairs, most of them a fraction of a Kelvin over the whole
horizon and negligible next to the ~17 self pairs (a zone heating itself) that dominate the actual
comfort decision. This is the LP's largest nonzero family (`O(zones × sources × blocks²)`), so
dropping the weak pairs entirely — rather than keeping every term — measurably shrinks solve time.
A pair is dropped when `Σ|kernel[j]| × that source's max_heat_kw` (the K a pulse held at full power
for the WHOLE horizon would cause in the target — an upper bound, not what any real plan does) falls
below `coupling_min_k`; a zone's own self pair is never dropped, however small. The reported/timeline
temperature is computed from the SAME pruned kernels the LP used, so the two never disagree about
which couplings exist. Measured on the real house (17 heated zones, 289 pairs): 17 self pairs over
7 K, 18 cross pairs over 1 K, ~128 pairs between 0.1–1 K, ~126 pairs under 0.1 K — the shipped
default, **0.05 K**, drops deep into that last bucket while leaving every pair that could plausibly
matter to comfort untouched. `0` disables the prune (keep every pair, today's pre-item-F behaviour);
raise it only if a live backtest shows it is still too conservative, and re-check
`/api/thermal/backtest` afterward — a pair dropped too aggressively shows up as the SAME kind of
persistent per-zone bias `gain_groups` (below) fixes for a different reason.

**How the relay is read.** `validate::read_heating_kw` turns the on-change heating relay log into a
power draw for the estimator, the live-input reads and the active backtest gain fit by reconstructing
the true time-weighted duty from the raw events (an hourly mean of the events would read a long ON
streak with no mid-streak event as "off" and a 10-second blip as a full hour). It needs the relay
state at the window start: one bounded `last()` read over the 7 days before it; a zone with no event
in those 7 days is assumed OFF until its first in-window event (logged only when that first event is
OFF, the one case where the assumption misreads earlier hours). Known data defect: a change-only log
that shows ON twice in a row has lost an OFF, and the read then holds ON until the next event (one
such 15 h case in three winter weeks of fixtures). There is no setting: an old config that still
carries `relay_duty: "events"` loads unchanged, while any other `relay_duty` value (e.g. `"legacy"`)
is a load error.

**`gain_groups`** — for an open-plan cluster (e.g. an open kitchen/livingroom), the live internal-gain
fit can fail to adapt *at all*: probing one zone alone barely moves *that zone's own* temperature (the
heat disperses into the group before it registers), so the fit's identifiability guard discards it and
the zone stays pinned to its static `internal_gain_w` forever — wrong the moment true occupancy differs
(e.g. the house sitting empty for a week). Listing those zones together in one `gain_groups` entry fits
ONE shared gain from the group's much larger combined response instead, split evenly back across the
members. Each zone belongs to at most one group; most houses need nothing here. Symptom this fixes:
`/api/thermal/backtest?mode=passive` shows a persistent multi-degree bias in exactly the zones that
never appear in `/api/calibration/gains`'s `live.gains_w` (only the config baseline).

**Which zones can be fitted a gain.** Only zones listed under `heating.zones` (the heated rooms) plus any named in **`extra_gain_zones`** ever receive an internal-gain
candidate. `extra_gain_zones` is for an unoccupied zone with a *real* source the fit should learn —
the house lists `garrage`, where a daily-driven car dumps engine heat every evening. Each entry is
`{ zone, max_w }`: `max_w` (optional) is the **physical ceiling** of that source, a flat W or a `{ night, day, evening }` profile (a car: `{ night: 0, day: 0, evening: 700 }` — a flat cap only let the fit move the same daily energy into the night) —
house knowledge, not a tuning knob. A least-squares fit otherwise sizes the source to whatever the
imperfect envelope needs (a 1.4 kW "car" once held a garage the model could not, and that heat
conducted +0.2…+0.6 K into every neighbouring room); bounded at what an engine can actually bring
home (~3–4 kWh ⇒ ~700 W over the evening daypart), the garage keeps a visible residual — it is
unheated, nothing plans on it — while the occupied rooms stay right. Every measured zone still
*constrains* the fit (the attic, garage and roof-void temperatures are all scored), but an unoccupied
zone has no occupants or appliances for a residual to represent: letting the solver place heat there
only papers over an envelope error with a phantom source that is real in the model and conducts into
the rooms next door (a fitted 676 W "night gain" in the attic once warmed the bedrooms below). Left
as a visible residual, that error points at the physics to fix — which is what
`/api/thermal/backtest?detail=1` is for. A zone whose bias the fit can *never* explain (it runs warm
with zero gains, so the fit reports it N/A) is the same signal from the other side: the model loses
too little heat there, and an envelope term (`ach`, U, absorptance) needs correcting, not a gain.

### `hvac` (air-side heating and cooling)

Optional and **inert until a unit is added** (the house has none today). Reversible heat pumps that act
on a room's **air** (not the slab). Equipment is **unit-based** — a unit serving one zone is a room
split; a unit serving several is a central/ducted system sharing one compressor. This block is
**equipment only**: the temperatures a served room holds are its [`comfort.rooms.<zone>`](#comfort-what-temperature-each-room-should-be)
`heat` and `cool` targets (a served room **must** have a `cool`; air-heating holds the heat floor,
cooling holds the cool ceiling, pre-cooling stops at `cool.target − below`).

```json5
hvac: {
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
    },
    guestroom_ac: { zones: ["guestroom"], max_cool_kw: 2.5, max_heat_kw: 0.0, cooling_cop: 3.2, heating_cop: 1.0 },
  },
}
```

| Field | Unit | Notes |
|---|---|---|
| `units.<u>.zones` | — | zones the unit serves (≥1); each needs `comfort.rooms.<zone>.cool` |
| `units.<u>.max_cool_kw` / `max_heat_kw` | kW | total capacity, **shared** across the served zones |
| `units.<u>.per_zone_max_kw` | kW | optional per-room delivery (damper) cap; default = unit total |
| `units.<u>.cooling_cop` / `heating_cop` | — | a **number** (constant) **or** a **`[{ t, cop }]` curve** |

**COP curves** (`CopSpec`): a constant `3.0`, or breakpoints `[{ t: <°C>, cop: <COP> }]` in
**strictly increasing** `t` with positive `cop`. Evaluated by clamped linear interpolation (flat beyond the
ends). The optimizer reads the COP at each block's outdoor temperature; because the forecast is a known
input the dispatch stays a linear program. A zone may belong to only one unit.

**The pre-cool floor (`cool.target − below`).** Without a floor, the only thing stopping the optimizer
from pre-cooling a room far below any sane target — e.g. to bank cheap/free electricity against an
expensive afternoon — is the far-away heat floor (the same slab-storage arbitrage that legitimately
pre-*heats* a room in the cheap window, mirrored for cooling). The cool band's `below` caps that downside
with a **hard, linear, one-sided cap directly on the cooling decision itself — no slack, no penalty**: for
each block `k` whose free response is above the floor, the CONDENSED cooling effect at that block is
bounded by `Σ_j K_cool(k−j)·cool[j] ≤ free_response[k] − (cool.target − below)`, and in a block whose
free response is at or below the floor that block's own cooling is 0, where `free_response[k]` is the zone's **UNACTUATED**
predicted temperature (drift only) and `K_cool(k−j)` is the same decaying self-kernel coefficient the
temperature-prediction row uses for block `j`'s cooling contribution to block `k` — exactly the cooling
contribution to the block-`k` prediction, recomputed fresh at every block (not an accumulated running
total). Consequences:
- **Conservative: heating's contribution is ignored.** The cap is judged against the *unactuated*
  response, not the actuated one — a room the optimizer is simultaneously heating does **not** get extra
  cooling headroom for that heat.
- **Summer: AC can hold the cool ceiling all day.** Because the coefficient decays with lag, an earlier
  block's contribution to a much-later block's cap genuinely fades. Cooling in a block whose free
  response is above the cool target served that target, so it is left out of the sum in every block
  whose free response is below the target: an evening drifting down to the floor does not forbid the
  afternoon's cooling, and that cooling's residue may leave the room a little under the floor then.
- **Winter/shoulder: no cooling at all** whenever the free response never rises above the floor — a
  room already at or below where it would drift to unaided cannot be cooled.
- `below: 0` means cooling may not pre-cool below the target at all; the widest `below` allowed puts
  the pre-cool floor 0.5 K above the room's highest heat target.
- A dual-served room (underfloor + AC) keeps its heat floor at the same time; its ceiling is the cool
  side (`cool.target + above`), with a `pull_eur_per_kh × priority` cost per K·h above `cool.target`.

**Today's live config has no `hvac.units`** — every cool target in `comfort` is dormant until a unit is
added (a future controllers deploy); adding one is a config release like any other (see the deploy
section), not a code change.

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
battery: {
  capacity_kwh: 10.0, min_soc_pct: 20.0, charge_kw: 5.3, discharge_kw: 5.3,
  round_trip_efficiency: 0.85,
  min_dispatch_kw: 2.45,        // optional (default 0, no floor): the ACTUATOR's dispatch floor —
                                 // the Growatt controller floors any nonzero charge/discharge
                                 // powerrate at 25% of its max scale (~9.8 kW here, so ~2.45 kW).
                                 // The LP keeps battery<->grid export and grid-charge each either
                                 // 0 or >= this value (fix-and-round pins the bound; a sub-floor
                                 // value would be actuated at the floor, up to ~8x the planned
                                 // energy). Plausible range 0..charge_kw (or discharge_kw, if
                                 // larger) — validated against both. A tiny routing tie-break in
                                 // the LP's objective makes battery energy beyond the house's real
                                 // deficit/surplus show up as the grid legs (not laundered through
                                 // battery<->load/solar<->battery), and the pinned re-solve caps
                                 // those two legs at that same real deficit/surplus so a pinned-off
                                 // grid leg can't resurface there instead.
  p10_precharge_guard: false,   // optional: when even the p10 (conservatively low) Solcast forecast
                                // fills the battery from tomorrow's surplus, halve this plan's
                                // terminal SoC value (less overnight pre-charge before a day that
                                // will fill the battery anyway). Inert until the forecast writer
                                // stores the p10 curve (hourly_json_p10).
                                // (Always on, not configurable: the Growatt refuses battery-sourced
                                // grid export while its PV input reads ~0 W, so the LP binds planned
                                // battery→grid to 0 in every block whose forecast pv_kw is at/below
                                // the PV-present threshold, 0.05 kW. Solar→grid, load-serving
                                // discharge, battery→EV and grid charging are unaffected.)
},
pv: {
  system_efficiency: 0.85,        // optional (default 0.85)
  arrays: [                        // the clear-sky fallback (Solcast is preferred when available)
    { name: "terasa", kwp: 7.0, tilt: 35.0, azimuth: 226.0 },   // tilt & azimuth in degrees
    { name: "ulice",  kwp: 6.5, tilt: 35.0, azimuth: 136.0 },
  ],
  nowcast: {                       // optional; all fields optional (shown at their defaults)
    enabled: true,                 // ship `false` here if the replay proof doesn't clear the bar
    window_minutes: 60,            // trailing measured-PV window read at plan time (15-180)
    min_forecast_kw: 0.5,          // below this forecast MEAN over the window, skip (dawn/dusk/
                                    // night noise dominates the ratio); must be > 0 — a 0 lets a
                                    // zero forecast sum through as a NaN ratio; 0.05 (the daylight
                                    // floor `DAYLIGHT_KW`) is the sensible lower end of the range,
                                    // ~4% of a 13.5 kWp array is the default
    efold_hours: 1.5,              // blend weight exp(-tau/efold_hours); clear-sky-index
                                    // autocorrelation decays over a few hours (>0)
    max_hours: 3.0,                // blocks at/beyond this lead are untouched; w(3h) ~= 0.14;
                                    // range (0, 12] — to switch the nowcast off use enabled: false
    clamp: [0.3, 1.5],             // [lo, hi] on the measured/forecast ratio: overcast-vs-sunny
                                    // forecast error is ~0.2-0.3 on the low side, upside bounded
                                    // by clear sky (0 < lo <= 1 <= hi)
  },
}
```

*PV intraday nowcast.* `pv.nowcast` blends the last `window_minutes` of measured PV against the
SAME calibrated Solcast curve the forward plan blocks use, as `r = measured / forecast` (clamped),
and multiplies it into the coming `max_hours` of the plan's PV curve with an exponential-decay
weight (`efold_hours`) — a cheap, zero-latency correction for today's actual weather on top of the
band calibration's mean-shape fit. Skipped (no placeholder unless the read itself failed/timed out
or was stale) at night/low sun, on partial sample coverage, right after a forecast refresh (the
snapshot the forward curve reads hasn't got the just-finished hour's key yet), or over a curtailed
window. `/api/plan`'s `pv_nowcast` object (`docs/api.md`) reports the outcome every cycle.
`cargo run --release -- backtest-pv-nowcast <days> [--efold ...] [--max-hours ...] [--clamp-hi ...]
[--min-forecast ...]` replays the nowcast against 14 days of stored snapshots (the accuracy proof; `days` ≤ 21).

*Terminal SoC value.* `app::terminal::terminal_soc_value_outlook` is now the PRIMARY valuation of the energy
left in the battery at the horizon end, not `app::terminal_soc_value`'s in-horizon median/break-even
cap: it prices leftover SoC by what it earns at its best **post-horizon** use (a 24 h day-type-median
import curve from the TRUE grid end, the same estimator the heat credit's outlook uses), capped by
the cheapest re-acquisition price at or before that use — a night refill before a morning peak caps
the peak's value at the refill price, not the peak itself. Used only when the day-type median covers
EVERY post-horizon block; a thin/cold-start price history falls back to the in-horizon-median
value, flagged in `/api/plan`'s `placeholder_inputs` ("terminal SoC value (outlook
uncovered …)"). `/api/plan`'s `terminal_soc_value_eur_per_kwh` + `terminal_soc_value_source`
(`docs/api.md`) report the value actually applied, after the `p10_precharge_guard` halving above.
The terminal slab-heat credit's flat fallback (`terminal_heat_value`, see "The terminal slab-heat
credit's displaced price" below) stays keyed to the in-horizon-median value
(`ForecastContext::terminal_heat_basis`), kept deliberately separate from the battery's own
`terminal_value` so the post-horizon valuation never inflates the heat credit too.

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
  fit `run_hours` simply runs as much as it can rather than making the plan infeasible. Also a HARD
  upper cap per window occurrence, `Σ on·dt ≤ run_hours + one fine (15-minute) block` — enough
  headroom that a target is always exactly reachable in whole blocks, but no more: without it the
  LP would happily run the load extra hours in a free-surplus/negative-price block once the target
  is already met.
- **Heat-when-on** — its `kind × power_w × power_factor` air-node heat couples into the thermal
  prediction **only in the blocks it runs** (a resistive boiler with `power_factor ≈ 1` dumps all of
  it into the room; a tank that carries the heat away uses a small factor). So scheduling it warms (or,
  for a `sink`, cools) the room exactly when it runs, and the comfort band sees it.

The reported schedule is in the plan's `controllable_load_kw` (per load, per block; `on · power_w`),
surfaced in `first_step` and the timeline, and republished to the dry-run boiler controller (see
[controllers.md](controllers.md)). **Ships dormant:** `controllable` defaults to `false`, so an
existing scheduled load is unchanged — the plan is byte-identical until you opt a load in.

### `estimator` (thermal state estimator)

```json5
estimator: {
  mode: "kalman",           // "anchor" (classic) | "kalman" (this house)
  // Noise priors (all optional):
  sigma_meas_k: 0.1,        // zone-sensor noise std (K)
  sigma_air_k: 0.3,         // per-hour process noise std on zone-air states
  sigma_mass_k: 0.05,       // per-hour process noise std on wall/slab states
  disturbance: false,       // constant-flux observer per measured zone (offset-free); FEEDS THE PLAN
  sigma_disturbance_w: 30.0,
  max_disturbance_w: 500.0, // hard clamp on |disturbance| (W)
}
```

The retired `solar_scale*` keys (a per-zone solar-gain scale, removed) are accepted and ignored, so an
older config keeps loading.

`anchor` (default) reproduces the classic estimator: open-loop drive over history + re-anchor of
measured air states. `kalman` makes a steady-state Kalman filter's measurement-corrected state the
plan's `x0` — proven to roughly halve the held-out prediction error vs the seed. The filter is
built once at startup in a background thread (a Riccati solve, seconds native / ~75 s static-musl);
until it lands, or if the build fails, the estimate falls back to `anchor`. The calibration fit is
untouched (it keeps the pure open-loop drive). Compare with `/api/thermal/backtest?x0=kalman`
(measurement updates only during the
warm-up; the scored window is a pure open-loop prediction from the filtered state).

`disturbance: true` (requires `mode: "kalman"`) augments the filter's state with one constant flux
per measured zone (W, random-walk std `sigma_disturbance_w`, hard-clamped to `max_disturbance_w`) —
the classic offset-free-MPC trick for a zone with a steady unmodelled gain or loss (a draughty
window, an unlisted appliance, a garage the model under- or over-sizes). Past the estimate itself,
this recovered flux is now **carried forward into the plan**: `current_plan` adds it to that zone's
`internal_gain_w` for the WHOLE horizon, on top of (added after, so both apply) the live internal-gain
re-fit — re-clamped to `max_disturbance_w` at that point too. Without this the forecast reverted to
the model's own bias one step past "now" even though the observer had already measured the offset;
with it, the forward prediction keeps tracking the measured loss/gain instead of drifting back toward
the un-corrected model (error stops growing with lead — see `kalman::tests::
disturbance_correction_keeps_the_24h_forecast_on_the_true_trajectory`). Surfaced per plan in
`disturbance_w` (`/api/plan`, `/api/plan/latest`) and, independently, the live current estimate in
`/api/state`'s own `disturbance_w`.

### Loop knobs (all optional, with defaults)

| Key | Default | Meaning |
|---|---|---|
| `consumption_history_days` | 30 | trailing window to train the consumption model |
| `mpc_tick_minutes` | 60 | how often the MPC loop re-plans (also the `/readyz` staleness threshold) |
| `internal_gain_window_days` | 7 | window for the live internal-gain re-fit |
| `internal_gain_recalibrate_hours` | 24 | re-fit cadence (0 disables) |
| `forecast_snapshot_minutes` | 60 | forward-prediction snapshot cadence, minutes (0 disables; otherwise >= 15 — the scoring is hourly) |

The config file's own path and the brain's JSON stores are environment variables, not
`config.json5` keys (they're deployment paths, not house physics/economics):

| Env var | Default | Holds |
|---|---|---|
| `MPC_CONFIG_PATH` | `config.json5` | the config file every loader reads and the comfort-target editor rewrites; in the container `/app/conf/config.json5` (a writable mounted directory — a rename over a single-file bind mount fails) |
| `MPC_FORECAST_STORE` | `forecast_snapshots.json` | the forward-prediction snapshots `/api/forecast/validation` scores |
| `MPC_LEDGER_STORE` | `decision_ledger.json` | the decision ledger (`/api/ledger`) — planned vs measured per block, retained 30 days (also the endpoint's `?days=` clamp) |
| `MPC_ROOM_MODE_STORE` | `room_modes.json` | per-room heating / cooling mode state (mode, since, hysteresis clock) so a restart does not flip modes; a few hundred bytes, rewritten only when a mode or its clock changes |
| `MPC_ACCURACY_HISTORY_STORE` | `accuracy_history.json` | the daily forecast-accuracy history — the long-lived record of the thermal model's accuracy across releases: per UTC anchor day (the day the prediction was made), zone and lead bin, the plan's own forward prediction scored as `[n, rmse_k, bias_k]`; retained 400 days, appended once a day by the `features` collector task, backfilled from the snapshot store on a cold start |
| `MPC_FEATURE_SAMPLES_STORE` | `feature_samples.json` | the sampled A/B records behind `/api/features` — the hourly comfort-targets (pull on vs off) counterfactual plans; retained 60 days, at most one sample per hour |

Bind-mount every store so its history survives a container recreation (see `deploy/`). A store file
that exists but cannot be parsed is never overwritten: the brain starts with an empty history in
memory and the next write first moves the broken file to `<path>.corrupt`. The accuracy-history and
feature-samples stores are written only by the `features` collector task (every 10 minutes, whenever
something changed), never on the planning path; the page the samples feed is documented under
`GET /api/features` in `docs/api.md`.

**Tick phase** (on-the-mark switching: actuate exactly on the quarter-hour marks): at `mpc_tick_minutes: 1` (the
live default) the loop re-anchors its ticks, once, to wall-clock second `:20` of each minute instead
of whatever second the process happened to start in (otherwise uniformly random over the 60 s
period). That puts the LAST tick before every quarter-hour mark at `mark − 40s`, so its plan — the
pre-boundary observation the rollover latch (`mpc_loop::PendingNext`) adopts — is normally ready
10–20 s before the mark rather than sometimes only a few seconds before it, without touching the
1-minute cadence itself: the very first tick after startup/a supervisor respawn still fires
immediately (no added latency to the first published plan), and every tick after that lands on
`:20`. Any other `mpc_tick_minutes` is left unaligned — no equivalent lead-time target is defined for
a longer cadence.

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

The dashboard's floor plan (`GET /api/rooms`) also shows each room's latest **humidity** and
**presence** (motion). Both are optional entries next to `temperature` under the same zone, with the
same `{bucket, measurement, tags, field}` shape — and **need no config for the stock loxone layout**:
when absent they are derived from the zone's `temperature` entry, using its bucket and its `room` tag:

| Entry | Default measurement | Default field | Value |
|---|---|---|---|
| `humidity` | `humidity` | `humidity_<room>` | relative humidity, % |
| `presence` | `presence` | `motion_<room>` | motion, `1` / `0` |

with `tags: { room: <room> }` and the temperature entry's bucket. So `livingroom` (room `obyvak`) reads
`humidity_obyvak` / `motion_obyvak` automatically. A room with no such series simply reports `null`
(the loxone house logs humidity for most rooms but motion only in a few). Override per zone only where
the names differ (a zone with no `room` tag derives nothing and needs explicit entries):

```json5
livingroom: {
  temperature: { /* … */ },
  humidity: { bucket: "loxone", measurement: "humidity", tags: { room: "obyvak" }, field: "humidity_obyvak" },
  presence: { bucket: "loxone", measurement: "presence", tags: { room: "obyvak" }, field: "motion_obyvak" },
},
```

Existing configs parse unchanged (the entries are just more named signals of the zone). These overrides never change the estimator's measured set: `mapped_zones()` (the Kalman filter's measured zones) only counts zones that have a `temperature` entry, and a zone with only `humidity`/`presence` is ignored by the estimator and by the heating-relay `room` lookup. Reads are
bounded: one `last()` query per distinct bucket over the last 24 h, cached 30 s.

---

## Tips & recipes

- **Names must match** across the three places: a zone in `config.json5` `heating.zones` /
  `comfort.rooms` / `hvac.units` / `zone_mappings` must be a real zone in `model.json5`.
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
   - always: add `comfort.rooms.office = { heat: { target, delta | below, above }, cool?, priority? }`.
   - heated? add `heating.zones.office = { max_heat_kw, internal_gain_w? }` (equipment only).
   - has AC/HVAC? list `office` in a unit's `zones` (its `comfort.rooms.office` then needs a `cool`).
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
| `heating.zones[z] has no comfort.rooms[z].heat target` | a heated zone has no setpoint in the `comfort` block |
| `hvac unit … serves zone … but comfort.rooms[z].cool is missing` | a unit serves a room you gave no `cool` target |
| `config uses removed comfort fields … X was removed — use Y` | the pre-`comfort` fields (`t_min`, `t_max`, `overheat_c`, `warmth_value_eur_per_kh`, zone `windows`, `hvac.comfort`, `default_comfort`, the two `comfort_penalty`s, `heating.overheat_penalty`) were removed; the message names the replacement |
| `… is not a config field — room comfort is the TOP-LEVEL comfort block` / `heating.comfort was removed` | a `comfort` key nested under `heating`, a zone, `hvac`, … — move it to the top-level `comfort` block |
| `comfort.rooms lists "x" twice` | the same room appears twice in `comfort.rooms` — merge the entries |
| `window … has start == end` | a `windows` entry with equal start and end (empty window) |
| `comfort.overheat_max_c must be finite and ≥ 0` | a negative or non-finite mild-tier width |
| `comfort.mode_switch_hours must be finite and ≥ 0` | a negative or non-finite mode hysteresis |
| `comfort.pull_eur_per_kh … must be < comfort.penalty` | the pull is not strictly below the band-edge penalty |
| COP curve must be strictly increasing / COP must be positive | a `cooling_cop`/`heating_cop` curve is out of order or non-positive |
| `…`delta` is ignored when both `below` and `above` are given` | a band set `delta` together with both per-side overrides |
