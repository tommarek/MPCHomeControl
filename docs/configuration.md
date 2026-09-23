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
}
```

Set `timezone` (validated at load). With it, the VT/NT tariff hours classify **per block**, the
consumption bins / PV-curve keys / backtest keys derive **per sample**, so a horizon or training
window crossing a DST changeover stays correct. Without it, `utc_offset_hours` applies year-round
and must be hand-edited twice a year.

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
  hours: 36,      // optional (default 36) — total planning horizon
  fine_hours: 6,  // optional (default 6) — how much of it stays at 15-minute resolution
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

`hours` may not exceed the compile-time feed horizon (`HORIZON_HOURS` in `app.rs`, 36) — the
weather/PV/price fine-lattice assembly is only built that far ahead. A larger value is rejected at
**config load** with a clear error (rework cycle 1, finding 8); previously it was silently accepted
and only discovered as every single plan failing at runtime.

Comfort is enforced at each block's **END**, not continuously through it — a block's soft-comfort
row checks the affine-predicted temperature at its own end only, so a fine (15-minute) block is
effectively checked every 15 minutes near-term, but an hourly block only constrains the top of the
hour: a mid-hour dip is not penalized. `fine_hours` is therefore also the span over which comfort
gets 15-minute resolution; beyond it, only the hourly checkpoints bind (rework cycle 1, finding 10).

Timeline blocks report their own duration (`dt_minutes`): the publisher derives `valid_until` from
it and the dashboard plots hourly blocks four times as wide as fine ones.

### `heating` (underfloor)

```json5
heating: {
  cop: 1.0,                 // heat delivered per kWh electricity. 1.0 = resistive; >1 = a heat pump
  comfort_penalty: 50.0,    // price-units per K per step a zone is outside its band
  overheat_penalty: 0.2,    // optional (default 0.2) — mild penalty for the optional overheat tier, see below
  coupling_min_k: 0.05,     // optional (default 0.05) — drop a physically-negligible cross-zone coupling, see below
  zones: {                  // a zone absent here is NOT heated
    livingroom: { max_heat_kw: 3.0, t_min: 21.0, t_max: 24.0, internal_gain_w: 351 },
    bedroom:    { max_heat_kw: 1.2, t_min: 20.0, t_max: 21.0 },
    // office:  { max_heat_kw: 1.0, t_min: 19.0, t_max: 22.0, overheat_c: 2.0 },  // example (dark; opt-in per zone)
  },
  gain_groups: [ ["kitchen", "livingroom"] ],  // optional — see below
}
```

| Field | Unit | Notes |
|---|---|---|
| `cop` | — | heat / electricity |
| `comfort_penalty` | price-units/(K·step) | soft-comfort weight; must be > 0 when any `heating.zones` entry is configured (zero is rejected at load — comfort is enforced only through this soft-slack weight) |
| `overheat_penalty` | price-units/(K·step) | optional (default 0.2); mild weight for the overheat tier — must be finite and `> 0` (zero is rejected at load: comfort ceilings are enforced only through soft-slack weights), and `< comfort_penalty` whenever any zone sets `overheat_c` |
| `coupling_min_k` | K | optional (default 0.05); drops a negligible cross-zone slab coupling from the LP (and the reported temperature), see below — must be finite and `≥ 0`; `0` keeps every pair |
| `zones.*.max_heat_kw` | kW | the zone's underfloor circuit power (the relay rating); caps the optimizer's per-step heat for the zone |
| `zones.*.t_min` / `t_max` | °C | comfort band edges |
| `zones.*.overheat_c` | K | optional (default 0 = off); extra headroom above `t_max` this zone may bank into, see below |
| `zones.*.internal_gain_w` | W | optional (default 0); occupants/appliances/fireplace — the live fit refines it into a night/day/evening profile |
| `zones.*.windows` | — | optional daily band schedule: `[{ start: "22:00", end: "06:00", t_min: 18.0 }]` overrides the band inside the window (night setback); absent fields keep the base; end ≤ start wraps midnight |
| `gain_groups` | — | optional list of zone-name lists; see below |
| `extra_gain_zones` | — | optional `[{ zone, max_w }]` for zones outside `zones` that may still be fitted a (capped) gain — a garage with a car; see below |

**`overheat_c` / `overheat_penalty`** — a second, softer comfort tier for banking near-free surplus
energy into the slab instead of wasting it (curtailment-bound PV with export disabled, deeply
negative spot prices). A zone with `overheat_c: 2.0` may drift up to 2 K above `t_max`, penalized at
the mild `overheat_penalty` instead of the full `comfort_penalty`. The `overheat_c` K of headroom
itself is a hard, structural cap (the `slack_over` LP variable is bounded `[0, overheat_c]`) — but
past `t_max + overheat_c` the temperature is **not** separately capped: the ordinary `comfort_penalty`
tier simply applies again, exactly as soft as it is above today's plain `t_max` (the LP just finds it
uneconomical to pay that penalty in practice). Absent or `0` on a zone ⇒ exactly today's single-tier
band. The committed `config.json5` enables `overheat_c: 1.0` on six zones (kitchen, livingroom,
ground_hall, both bathrooms, toilet); every other zone leaves it unset.
Underfloor zones only — a zone that is *also* HVAC-served is rejected at config load if it sets
`overheat_c > 0` (its effective ceiling is `hvac.comfort[z].t_cool`, not the underfloor `t_max`; see
`ControlConfig::load`'s cross-check in `config.rs`). The night-setback schedule still drives the
*base* `t_max` each block; `overheat_c` rides on top of whatever that block's effective ceiling is.

**Known gap (historical, fixed in rework cycle 1):** with a NARROW comfort band relative to a
relay's per-pulse temperature impulse (e.g. ~1 K bands with a strong relay), relay-binary
quantization could park a whole heating pulse's overshoot in the mild `overheat_penalty` tier
instead of the heavy `comfort_penalty` one, at ordinary grid prices with no PV or free energy
involved — measured up to 1.33 K over `t_max` and a ~50% increase in grid cash on an 8 kW relay /
1 K band scenario (not reproducible with a realistic ≥3 K band). That mechanism needed a TRUE
branch-and-bound relay (forced to literally 0 or full power against the whole objective); item F
(2026-09, HiGHS interior-point + fix-and-round, no branch-and-bound at all) removed it — a bare
relaxed solve has no reason to overshoot — but item F's own fix-and-round PINNED re-solve then
reintroduced a WORSE version of the same shape: pinning the whole near-term `BINARY_HEAT_BLOCKS`
window (8 blocks / 2 h) forced every one of them to full power or off, even where the relaxed LP
only wanted some of them partially heated. Measured on `overheat_activates_at_default_with_future_
demand`'s scenario (16 blocks, 1 K band, a curtailment-bound PV spike with real future demand to
displace): relaxed peak 21.890 °C (inside band) → **pinned peak 25.018 °C — +3.02 K over `t_max`
(22.0), +1.02 K past the `t_max + overheat_c` ceiling (24.0)**. Rework cycle 1, finding 3 fixed
this at the source: `round_binaries` pinned only block 0, leaving blocks `1..BINARY_HEAT_BLOCKS` a
free `[0, 1]` relay/mode interval in the pinned re-solve too. Rework cycle 2, finding 2 widened the
pin to blocks 0 AND 1 — both blocks item G's publisher ever actuates (the covering-block current
command and the frozen-gated next command; see `HEAT_COOL_PIN_BLOCKS`'s doc in `unified.rs`) — since
pinning only block 0 left a fractional block 1 invisible to the integrality check, and the publisher
would turn a partial-power AVERAGE into a full-power relay block once it became the actuated NEXT
command. Blocks `2..BINARY_HEAT_BLOCKS` still keep a free `[0, 1]` interval. Re-measured on the SAME
scenario, same test, with the wider (blocks 0+1) pin: pinned peak is still **21.890 °C — identical to
the relaxed peak, 0 K overshoot**, comfortably inside the 24.0 °C ceiling. Practical guidance
unchanged: don't configure `overheat_c` on a zone with a comfort band narrower than a few K relative
to its relay's pulse size; watch `/api/plan/timeline` after enabling it for overshoot with no
PV/free-energy in play. (The terminal SLAB-heat credit, `terminal_heat_value`, was separately checked
and does **not** drive this — probed up to `terminal_value: 5.0` with no measurable effect on when
the tier engages.)

One side effect of the block-0-only pin: the ORIGINAL activation mechanism this same scenario used
to demonstrate (a near-term relay forced to a quantized full-power pulse by branch-and-bound) no
longer applies to EITHER the relaxed OR the now-correctly-pinned solve — baseline and with-tier
peaks come out identical (21.890 °C both) here. The default `overheat_penalty` is still exercised
by `overheat_banks_free_surplus_and_curtails_less` (the terminal-credit displacement path, in the
calibration table below); this scenario now only proves the CEILING, not activation.

*Tuning `overheat_penalty`.* A plain "avoid curtailment" benefit is tiny by itself — the LP's own
curtailment penalty is a token 0.0004 price-units/kWh, so simply not wasting surplus PV is nowhere
near enough to justify banking heat above `t_max`. What actually makes the overheat tier pay for
itself is **displacement**: heat banked now, while marginal energy is free, reduces the paid heating
the zone would otherwise need later in the *same* horizon to hold its band. That only has value when
the horizon actually contains that future demand (a colder stretch, a tight band) — a free-surplus
block with nothing to displace affords the tier essentially no economic value, and the mild penalty
alone won't move it.

The default is empirically calibrated against two scenarios run at the shipped default
(`optimize::unified::tests`, `overheat_activates_at_default_with_future_demand` /
`overheat_not_used_without_free_energy`), on the same synthetic test-house zone
(`thermal_for`'s 16 m² slab / 40 m³ room, ~0.3 K/kWh self-kernel for a one-block pulse):

- **Free-surplus + in-horizon future demand** — a curtailment-bound PV block (export disabled) with a
  subsequent cold stretch the zone must pay to reheat from: banking heat now displaces real future
  paid heating, a genuine (non-token) saving, in principle. **The bisected activation threshold
  previously reported here (≈11.8 price-units/(K·step)) is withdrawn as stale**: it was measured
  against the pre-finding-3 quantized relay-pulse mechanism (see the "Known gap" note above), which
  rework cycle 1 removed by pinning only block 0. Re-measured on `overheat_activates_at_default_with_
  future_demand`'s exact scenario after that fix (this session): the fix-and-round peak now matches
  the baseline exactly — **21.890 °C both**, at the shipped default `overheat_penalty: 0.2` — no
  activation is observable in this scenario any more, at any path. No replacement number is given
  here; bisect against your own house/scenario if you need one.
- **Grid-only at a normal NT effective price (~0.10 EUR/kWh)** — no PV, no free or negative-priced
  energy: the tier does not activate at any positive `overheat_penalty` in this scenario, because
  there is no marginal saving to bank against. (This is about the tier's *economic* activation on a
  realistic band, not an absolute guarantee for every band — see the relay-quantization known gap
  above, which reproduces overshoot at ordinary prices only on a much narrower band than this
  scenario uses.)

The shipped default, **`overheat_penalty: 0.2`**, is inert whenever there's nothing to bank against
(the second scenario) and, post finding-3, shows no activation on the first scenario either (see
above) — the default is not currently validated against a live displacement threshold, only against
the ceiling (no overshoot) and the curtailment-avoidance threshold below. It's also below the
threshold (~0.245, measured separately) a pure curtailment-avoidance benefit needs at a deeply-negative
spot price with no future demand at all — so a strongly negative price block engages the tier even
without a subsequent cold stretch in the horizon.

Because both benchmark scenarios are on the same synthetic test-house zone, the default is only a
starting point for a real house: a smaller/less-insulated slab has a *larger* self-kernel (more K per
kWh), which raises the activation threshold and makes the same `overheat_penalty` engage more readily
(and vice versa for a heavier/better-insulated slab). After enabling the tier on a real zone, check
`/api/plan/timeline` on a curtailment day: if it never engages even with a clear in-horizon cold
stretch after the surplus, lower `overheat_penalty`. If it engages on ordinary NT nights with no free
energy in play, first check the comfort band width against the relay's pulse size — the known
relay-quantization gap above is the expected cause on a narrow band, not a bug; widen the band (or
disable `overheat_c` on that zone) rather than raising `overheat_penalty` as a workaround. If the band
is already realistic (≥3 K) and it still engages with no free energy in play, that is unexpected —
raise it as a stopgap but investigate.

The zone name must exist in `model.json5` and have a `"heating"` marker for the heat to land.

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

**`gain_groups`** — for an open-plan cluster (e.g. an open kitchen/livingroom), the live internal-gain
fit can fail to adapt *at all*: probing one zone alone barely moves *that zone's own* temperature (the
heat disperses into the group before it registers), so the fit's identifiability guard discards it and
the zone stays pinned to its static `internal_gain_w` forever — wrong the moment true occupancy differs
(e.g. the house sitting empty for a week). Listing those zones together in one `gain_groups` entry fits
ONE shared gain from the group's much larger combined response instead, split evenly back across the
members. Each zone belongs to at most one group; most houses need nothing here. Symptom this fixes:
`/api/thermal/backtest?mode=passive` shows a persistent multi-degree bias in exactly the zones that
never appear in `/api/calibration/gains`'s `live.gains_w` (only the config baseline).

**Which zones can be fitted a gain.** Only zones listed under `heating.zones` (the occupied rooms —
those with a comfort spec) plus any named in **`extra_gain_zones`** ever receive an internal-gain
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
on a room's **air** (not the slab): cooling above `t_cool`, air-heating below `t_heat`. Equipment is
**unit-based** — a unit serving one zone is a room split; a unit serving several is a central/ducted
system sharing one compressor.

```json5
hvac: {
  comfort_penalty: 50.0,        // optional (default 50)
  comfort: {                    // per-room deadband [t_heat, t_cool] (°C); free-float between
    bedroom:    { t_heat: 20.0, t_cool: 26.0 }, // full override; inherits t_cool_min (23) below
    room_1:     { t_heat: 20.0, t_cool: 26.0 },
    livingroom: { t_heat: 20.0, t_cool: 26.0 },
    // guestroom has NO entry here — default_comfort below supplies its whole band, with t_heat
    // falling back to its own underfloor heating.zones.guestroom.t_min (20.5).
  },
  default_comfort: {             // fallback for a served zone with no entry above (§ below)
    t_cool_min: 23.0,
    t_cool: 25.0,
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
    },
    guestroom_ac: { zones: ["guestroom"], max_cool_kw: 2.5, max_heat_kw: 0.0, cooling_cop: 3.2, heating_cop: 1.0 },
  },
}
```

| Field | Unit | Notes |
|---|---|---|
| `comfort_penalty` | price-units/(K·step) | optional (default 50); must be > 0 when any `hvac.comfort` zone is configured (zero is rejected at load — HVAC comfort is enforced only through this soft-slack weight) |
| `comfort.<zone>.t_heat` / `t_cool` | °C | the room's deadband; `t_cool ≥ t_heat` |
| `comfort.<zone>.t_cool_min` | °C | optional pre-cool floor, `t_heat ≤ t_cool_min ≤ t_cool`; see below. Default (absent) = `t_heat` — no separate guard |
| `default_comfort.t_heat` / `t_cool_min` / `t_cool` | °C | house-wide fallback comfort; see below |
| `units.<u>.zones` | — | zones the unit serves (≥1) |
| `units.<u>.max_cool_kw` / `max_heat_kw` | kW | total capacity, **shared** across the served zones |
| `units.<u>.per_zone_max_kw` | kW | optional per-room delivery (damper) cap; default = unit total |
| `units.<u>.cooling_cop` / `heating_cop` | — | a **number** (constant) **or** a **`[{ t, cop }]` curve** |

**COP curves** (`CopSpec`): a constant `3.0`, or breakpoints `[{ t: <°C>, cop: <COP> }]` in
**strictly increasing** `t` with positive `cop`. Evaluated by clamped linear interpolation (flat beyond the
ends). The optimizer reads the COP at each block's outdoor temperature; because the forecast is a known
input the dispatch stays a linear program. Every zone named in a unit (or `per_zone_max_kw`) must have a
`comfort` entry **or** be covered by `default_comfort` (next).

**`t_cool_min` — the pre-cool floor.** Between `t_cool_min` and `t_cool` the room free-floats; cooling
only engages once the temperature would otherwise exceed `t_cool`. Without it, the only thing stopping
the optimizer from pre-cooling a room far below any sane target — e.g. to bank cheap/free electricity
against an expensive afternoon — is the far-away `t_heat` edge (the same slab-storage arbitrage that
legitimately pre-*heats* a room in the cheap window, mirrored for cooling). `t_cool_min` caps that
downside: the LP adds a soft floor `T_zone[block] ≥ t_cool_min` (penalized like any other comfort
violation) **only** in blocks where the zone's *unactuated* (free-response) temperature is already
above `t_cool_min` — i.e. only where a dip below it could only have come from cooling. A block that is
naturally at or below `t_cool_min` (winter, or a room that's cool anyway) gets no such row, so this
never fights the heating floor; a dual-served room (underfloor + HVAC) keeps its own `t_min` floor at
the same time, since the two are gated independently.

**`default_comfort` — a house-wide fallback.** Rather than repeat `{ t_cool_min: 23.0, t_cool: 25.0 }`
in every room's `comfort` entry, set it once in `default_comfort` and it applies to every HVAC-served
zone that has **no entry of its own** in `comfort`. A zone that DOES have its own entry keeps its own
`t_heat`/`t_cool` outright and only inherits `default_comfort.t_cool_min` when its own entry leaves
`t_cool_min` unset (field-by-field override, not all-or-nothing). `default_comfort.t_heat` is itself
optional: a dual-served zone (also underfloor-heated) falls back to its own underfloor `t_min`; an
HVAC-only zone has no such fallback, so config load fails loudly if nothing supplies a `t_heat` for it
(no `default_comfort.t_heat`, no per-zone override, no underfloor floor). The documented default shown
above — `{ t_cool_min: 23.0, t_cool: 25.0 }`, `t_heat` omitted — is exactly the "23–25 °C everywhere"
policy: every dual-served room free-floats down to its own heating floor and up to 25, with cooling
guarded at 23.

**Today's live config has no `hvac` block at all** — every knob on this page, `t_cool_min` and
`default_comfort` included, is dormant until one is added (a future controllers deploy); adding it is a
model/config release like any other (see the deploy section), not a code change.

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
  p10_precharge_guard: false,   // optional: when even the p10 (conservatively low) Solcast forecast
                                // fills the battery from tomorrow's surplus, halve this plan's
                                // terminal SoC value (less overnight pre-charge before a day that
                                // will fill the battery anyway). Inert until the forecast writer
                                // stores the p10 curve (hourly_json_p10).
},
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
| `forecast_snapshot_minutes` | 60 | forward-prediction snapshot cadence (0 disables) |

**Tick phase** (item G, "switch exactly on the quarter-hour marks"): at `mpc_tick_minutes: 1` (the
live default) the loop re-anchors its ticks, once, to wall-clock second `:20` of each minute instead
of whatever second the process happened to start in (otherwise uniformly random over the 60 s
period). That puts the LAST tick before every quarter-hour mark at `mark − 40s`, so its plan — the
pre-boundary observation the rollover latch (`mpc_loop::PendingNext`) adopts — is normally ready
10–20 s before the mark rather than sometimes only a few seconds before it, without touching the
1-minute cadence itself: the very first tick after startup/a supervisor respawn still fires
immediately (unchanged latency to the first published plan), and every tick after that lands on
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
