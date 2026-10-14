# Monitoring & reporting API

The MPC brain (`cargo run -- serve`) exposes a **read-only** JSON API on `:3000` (the writes are the EV charging preference and the room comfort targets, below)
(`MPC_BIND=0.0.0.0` to expose from a container). It never writes InfluxDB (only its own
forecast-snapshot file) and never actuates (the controllers actuate separately).

> **API contract.** The Home Assistant dashboard in `~/git/selfhosted`
> (`homeassistant/config/mpc-dashboard/`, served by the `mpc_brain` integration) consumes this API.
> Any change to an endpoint's path, query, or JSON shape MUST be mirrored there in the same piece of
> work; bump `API_VERSION` (`src/web.rs`, reported as `api_version` by `/api/version`) on breaking
> changes. The dashboard checks `api_version` and shows a banner on a mismatch.

The brain serves **no dashboard**: the UI lives in Home Assistant (`/mpc`). `GET /` returns one line
of plain text saying so (`MPC brain: read-only API. The dashboard lives in Home Assistant (/mpc). API
index: /api`). `GET /api` returns a machine-readable index of every endpoint.

## Response envelope

Every **data** endpoint wraps its payload so a dashboard can show freshness:

```json
{ "computed_at": "2026-06-23T11:30:00+00:00", "age_seconds": 12, "data": { … } }
```

- `computed_at` — when the payload was computed (cached results report the original time).
- `age_seconds` — how long ago that was (0 for a fresh computation).
- `data` — the payload documented below.

Heavier endpoints (DB + estimator/optimizer) are cached for 60 s (`/api/features`: 5 min) and bounded by a 55 s timeout
(`504` on timeout, `500` on error). The health/probe endpoints (`/health`, `/livez`, `/readyz`,
`/api/version`, `/api`) return bare JSON without the envelope.

The JSON examples under **Endpoints** below show the `data` payload only — every data endpoint wraps
it in the envelope above.

## Endpoints

### Probes & identity

| Endpoint | Purpose |
|---|---|
| `GET /livez` | Liveness — always `200` (`{status, uptime_seconds}`). For restart decisions. |
| `GET /readyz` | Readiness — `200` iff the loop published a plan recently, else `503` (`{ready, plan_available, last_tick_age_seconds, max_tick_age_seconds}`). |
| `GET /health` | Topology + liveness (`git_sha`, `uptime_seconds`, `thermal_states`, `heated_zones`). |
| `GET /api/version` | `{api_version, git_sha, built_at, config_fingerprint, model_fingerprint, estimator}` — what's deployed. `api_version` (integer, currently `3`) is the API contract version: it goes up on every breaking change to an endpoint's path, query or JSON shape, so a client can tell it is talking to a brain it was not written for. `estimator: {configured, active, build_failed, building}` says which state estimator is *actually* running: a background Kalman build that fails leaves the brain on the anchor estimator indefinitely, and this is the only place a monitor can see that. |

### Model & envelope

- **`GET /api/model/topology`** — the building's **thermal envelope**, static (built from the model at startup, served with no DB): `{ zones: [{ name, volume_m3 (null for the outside/ground reservoirs), ach (air changes/h to outside, 0 = not modelled), role: interior|outside|ground }], boundaries: [{ id, zone_a, zone_b, area_m2, azimuth_deg, tilt_deg, kind: interior|exterior|roof|ground, type_name, u_value (W/m²K), r_value (m²K/W), ua (W/K), solar_absorptance, layers: [{ material, thickness_mm, conductivity (W/mK), marker }] | null, initial_marker }], ground_temperature_c }`. Drives the **House** screen. `u_value` is the conventional ISO 6946 value (interior/exterior surface films included); `layers` are in the model's `zones[0]`→`zones[1]` order (exterior-first for walls, room-first for floors/roofs — the dashboard orients them for display).
- **`GET /api/model/solar?sky=clear|now`** — live per-surface solar gain at request time: `{ sky, sun: { azimuth_deg, elevation_deg, up }, boundaries: [{ id, irradiance_wm2, solar_w, beam_w, diffuse_w, total_w, cos_incidence, mode }] }`. Surfaces facing `outside` are included with `mode ∈ absorbed | transmitted`: opaque `Layered` surfaces ABSORB (`solar_w = irradiance × absorptance × area`), `Simple` panes with `g > 0` TRANSMIT (`solar_w = irradiance × g × area` — the RC network's window path, typically the dominant gain). `irradiance_wm2`/`solar_w` are the beam + diffuse total (ground-reflected is always 0 today); `beam_w`/`diffuse_w` split that same total so a client can tell direct sun from diffuse sky light (e.g. a north-facing wall at mid-morning is diffuse-only: real physics, but "☀ on surfaces" read as direct sun before this split existed); `total_w` repeats `solar_w` under an explicit name. `cos_incidence` is the raw geometric cosine of the sun's incidence angle on the surface (negative when the sun is behind it, before the beam term's own clamp to 0). `sky` echoes which mode actually served the response: default/`clear` applies no cloud (reads the pure orientation effect — which faces are catching sun); `sky=now` scales the model by the current cloud fraction from the live weather forecast, falling back to `clear` (reported as such in `sky`) if that feed is unavailable.

### Live & state

- **`GET /api/live`** — measured **current** telemetry for the energy-flow view (cached 5 s, single-flight shared across pollers; best-effort per field, `null` if a feed is stale — Growatt >10 min, outside temperature >30 min): `{ at, solar_kw, grid_kw (+=import), house_kw, battery_kw (+=charge), soc_pct, soc_kwh, outside_temp_c }`.
- **`GET /api/history?hours=N`** — measured PV power and battery SoC over the recent part of the day, for the dashboard's history-vs-forecast overlay. 15-minute means of the live Growatt telemetry (`solar` bucket): `InputPower` → **kW**, `INVPowerToLocalLoad` → **kW** (measured house consumption), `SOC` → **kWh** (via the configured battery capacity). `hours` defaults to "since ~local midnight" (clamped 1–48); empty arrays when a series has no data. `{ pv_kw: [[iso, kW], …], house_kw: [[iso, kW], …], soc_kwh: [[iso, kWh], …] }`.
- **`GET /api/zones`** — per-zone comfort targets + heater limit + internal gain, over the heated ∪
  HVAC-served zones: `[{ zone, heated, hvac, heat: {target, low, high}, cool: {target, low, high,
  active} | null, priority, mode, mode_since, drift_k_per_day, windows, max_heat_kw, internal_gain_w }]`. `heat` and `cool` are the bands
  **in force at `computed_at`** — the room's daily target windows (night setback etc.) resolved in
  site-local time with the optimizer's own rule — and are what a client should shade and judge comfort
  against: `target` is where the room should sit, `low` the hard-ish floor (`target − below`) and `high`
  the ceiling (`target + above`; a window moves `target`/`low` but never the ceiling, which stays the base `target + above`). `cool` is the prepared cooling band (`null` when the room has none);
  `cool.active` is `true` only when an `hvac.units` entry serves the room — otherwise the target is
  parsed and shown but adds nothing to the plan. `priority` is the room's pull weight (`0` = band only).
  `windows` lists the configured schedule `[{start, end, heat_target, cool_target}]` (null targets keep
  the base). `max_heat_kw`/`internal_gain_w` are null for an HVAC-only zone (it has no heating config).
  `mode` is `"heating"` or `"cooling"`: the one side of its band the room works on (the other side's
  actuators are forced off), decided from the room's free drift — its predicted temperature with no
  heating and no cooling over the next 24 h — with a 24 h hysteresis and a safety rule that keeps a room
  below its heat target in `heating`. `mode_since` is when it entered that mode (UTC) and
  `drift_k_per_day` the free-drift slope (K/day, **+ = the room gains heat**). All three are `null`
  until the loop has decided a mode (additive; no `api_version` bump).
  *(`api_version` 3 replaced `t_min`, `t_max`, `t_min_now`, `t_max_now`, `overheat_c`,
  `t_max_boost_now` with these fields.)*
- **`GET /api/heating/day?date=YYYY-MM-DD`** — one **site-local calendar day** for the dashboard's heating chart (everything except the relay history, which Home Assistant owns). `date` is the site-local day (`site.timezone`, DST-correct; default today); allowed `today−13 … today+1`, otherwise (or malformed) `400`. Additive, no `api_version` change. Cached 60 s for today/tomorrow, 10 min for a day that has ended; every Influx read is best-effort (a failed read gives `null` values / empty series, never a 500). Envelope `data`:
  `{ date, start, end, now, eur_czk_rate, slots: […], zones: […] }` — `start`/`end` are local midnight and the next local midnight as UTC (23 h / 25 h on DST days), `eur_czk_rate` the rate of the plan (today/tomorrow) or of that day's ledger rows (a past day).
  **`slots`**: one per 15 min from `start` to `end` (96; 92 / 100 on the DST days), stamped at the slot START: `{ t, source: "measured"|"plan"|"none", import_price (EUR/kWh | null), price_is_placeholder, outside_c, pv_kw, house_kw }`. A slot is `measured` when it ends at or before `now`, or contains `now` and has measured data; otherwise `plan` while the latest plan covers it (an hourly plan block fills its four slots), else `none` (all values `null`). Measured: `import_price` from the ledger row covering the slot (the plan's for the block in progress, which the ledger records only when it ends), `outside_c` the 15-min mean of the `outside` zone sensor, `pv_kw`/`house_kw` the 15-min means `/api/history` serves (`InputPower`, `INVPowerToLocalLoad`); plan: the latest plan block's `import_price`, forecast `outside_c`, `pv_kw` and base `load_kw` (as `house_kw`, without heating/EV).
  **`zones`**: every controlled zone as `/api/zones` lists them: `{ zone, heated, max_heat_kw | null, heat: {low, high, target} (the band in force now, as /api/zones), measured: [[t_end, °C], …], predicted: [[t_end, °C], …], plan_heat: [[t, dt_minutes, kW, "plan"|"ledger"], …] }`. `measured` are 15-min means within `[start, min(end, now)]`, stamped at the window END (like `/api/history` and `/api/zones/series`); `predicted` is the latest plan's `temp_c` for blocks ending after `now` inside the day, stamped at the block END; `plan_heat` is the planned heat per block inside the day, zero entries omitted: past blocks from the ledger (`planned.heat_kw`, i.e. what the plan of that time decided, `"ledger"`), the block in progress and future blocks from the latest plan (`"plan"`, hourly blocks keep `dt_minutes` 60).
  Example slot: `{"t":"2026-10-13T10:15:00Z","source":"plan","import_price":0.22,"price_is_placeholder":false,"outside_c":7.0,"pv_kw":1.5,"house_kw":0.4}`; example zone: `{"zone":"kitchen","heated":true,"max_heat_kw":1.2,"heat":{"low":19.5,"high":21.0,"target":20.0},"measured":[["2026-10-13T10:00:00Z",21.73]],"predicted":[["2026-10-13T10:30:00Z",21.9]],"plan_heat":[["2026-10-13T10:15:00Z",15,1.2,"plan"]]}`.
  **`TimelineBlock.outside_c`** (also in `/api/plan/latest` and `/api/plan/timeline`): the outside temperature forecast (°C) the planner used for the block (block mean of the optimizer context's `temperature_c`); omitted for plans saved before it existed.
- **`GET /api/energy/day?date=YYYY-MM-DD`** — one **site-local calendar day** for the dashboard's energy chart (battery mode, price, PV, house load, battery, grid, cost; plan vs reality). Same `date` semantics, range (`today−13 … today+1`, else or malformed `400`), window (23 h / 25 h on DST days), caching (60 s; 10 min for a day that has ended) and best-effort reads as `/api/heating/day`; one Influx read (the battery SoC). Additive, no `api_version` change. Envelope `data`:
  `{ date, start, end, now, eur_czk_rate, battery_capacity_kwh (config; null without a battery), heating_cop, slots: […], totals: {measured, plan} }`. `heat_kw` is heating power as the plan / ledger state it (thermal, summed over zones): electricity = `heat_kw / heating_cop`.
  **`slots`**: one per 15 min (96; 92 / 100 on DST days), stamped at the slot START: `{ t, source: "measured"|"plan"|"none", mode, export_enabled, inverter_on, import_price, export_price (EUR/kWh | null), price_is_placeholder, pv_kw, load_kw, heat_kw, ev_kw, charge_kw, discharge_kw, import_kw, export_kw, soc_kwh, cost_eur, planned, misses }`. Source rule as `/api/heating/day` (ended slots are measured; the slot containing `now` is measured once the ledger or the SoC read has data for it; otherwise `plan` while the latest plan covers it, else `none`: everything `null`, `misses: []`).
  *Measured slot*: `mode` / `export_enabled` / `inverter_on` / prices are the ledger row's (the plan block's for the slot in progress, which the ledger records only when it ends). The flows are the ledger's measured kWh over the row length (kW; `load_kw` the TOTAL house load, `heat_kw` / `ev_kw` the sum of the known zones / chargers, `null` when none is known), `cost_eur` the row's `realized_cost_eur`; **all `null` when the row is not scored (or there is none), never 0**. `soc_kwh` is the measured Growatt SOC (% × capacity) as `/api/history`, the 15-min mean ending at the slot END, `null` when the window has no sample. `planned` = what the plan of that time decided (the row's `planned`): `{ mode, pv_kw, load_kw (base load), heat_kw, ev_kw, charge_kw, discharge_kw, import_kw, export_kw, cost_eur }`, `null` on plan / none slots. `misses` is the ledger row's `misses` AS STORED: an array of free-text strings (the same `reason` strings `/api/ledger`'s `misses[]` carries), `[]` when none.
  *Plan slot*: the latest plan block's values in kW (hourly blocks split into four equal slots; `load_kw` the BASE load without heating / EV; `soc_kwh` the block's end-of-block SoC, equal across a split block), `cost_eur` the block's net cost for the slot's 15 min (`import·import_price − export·export_price + wear·discharge`, wear = `tariff.battery_amortisation_czk` in EUR), `planned: null`.
  **`totals`**: `{ measured: { pv_kwh, load_kwh, heat_kwh, ev_kwh, import_kwh, export_kwh, charge_kwh, discharge_kwh, cost_eur, planned_cost_eur }, plan: { same keys without planned_cost_eur } }` — sums over the slots of that source; `null` slot values add nothing.
- **`GET /api/state`** — current per-zone air temperature: `{ zones: [{zone, temp_c}], disturbance_w? }`. The model estimate — the Kalman-filtered state (`estimator.mode: kalman`) or the classic drive **re-anchored to each zone's latest measured reading** (`anchor`) — so it reflects disturbances the model can't see (e.g. windows left open overnight) rather than the free-running prediction. `disturbance_w` is the observer's per-zone constant flux (W), present only when `estimator.disturbance` is on. *(`solar_scale` was removed with the Kalman solar scale, without an `api_version` bump.)*
- **`GET /api/rooms?hours=N`** — the per-room card data behind the dashboard's **floor plan**, one merged
  entry per room (TTL 30 s per `hours`, timeout-bounded, in the usual envelope). `hours` (default 12,
  clamped 1–72) sets how far ahead each room's `plan` slice reaches; the dashboard's time bar asks for the
  whole plan (`hours=48`). `plan_heat_kwh_12h` always sums the next 12 h whatever `hours` is. **Off the planning path:** it only
  *reads* what the loop already published (`/api/plan/latest`'s plan), the ledger store, `/api/state`'s
  cache entry (never computed inline: a missing or older-than-60 s entry is refreshed by a detached
  background `/api/state` computation, and this call serves the stale entry, up to 5 min old, or
  `estimate_c: null`) and the `/api/zones` band function; it never
  takes the solver permit and never recomputes a plan. Plus ONE Flux `last()` read per sensor
  bucket, concurrent and capped at 10 s each (a single query on the stock `loxone` layout, `range(start: -24h)`) for the latest
  temperature / humidity / presence. Rooms = every interior model zone, alphabetically; the floor-plan
  facts (room name, floor, area, polygon, label) are NOT served here — the dashboard owns its geometry
  and merges them from its own `floorplan.json` by `zone` (removed in `api_version` 2). Every field is always present and unknown is `null` (a zone with no
  sensor has `measured: null`; a failed sensor read nulls those fields and sets `sensor_error` — never a
  500). The per-bucket reads fail independently, so `sensor_error` can come with readings from the
  other buckets; `sensor_failed` is `true` only on the rooms whose temperature signal maps to a bucket
  whose read failed or timed out (`false` for a room with no temperature signal, or whose bucket read
  fine but had nothing in the last 24 h). `sensors_at` is when a fully successful sensor read completed:
  it is `null` whenever `sensor_error` is set, even when other buckets' readings are present (each
  reading carries its own `at`). `plan_stale` is `true` once the latest plan is older than `/readyz`'s
  limit (`max_tick_age_seconds`: 5 ticks, at least 10 min), `false` when it is fresh, `null` with no
  plan. `{ plan_at, plan_stale, sensors_at, sensor_error, site: {lat, lon}, rooms: [{ zone, volume_m3, ach (model; null when 0 = not modelled), heated, max_heat_kw,
  internal_gain_w, measured: {temp_c, at, age_s}, sensor_failed, humidity: {pct, at, age_s}, presence: {motion, at,
  age_s}, estimate_c, disturbance_w (the state estimate's; the plan's own when `/api/state`
  is unavailable; the per-room `solar_scale` was removed without an `api_version` bump), heat: {target, low, high}, cool: {target, low, high, active}, priority (same
  semantics as `/api/zones`), mode, mode_since, drift_k_per_day (as `/api/zones`: `"heating"`/`"cooling"`, UTC, K/day), heating: {on, kw}, heating_today_kwh, plan: {t[], dt_minutes[], heat_kw[],
  temp_c[]}, plan_heat_kwh_12h, pull: {eur_per_kh, kh, break_even_eur_per_kwh},
  heat_credit_eur_per_kwh }] }`. `heating` is `null` for a zone without a heater, when the latest plan
  is stale (`plan_stale`) or has no block running now, or when that block does not plan the zone;
  otherwise `on`/`kw` are the **committed plan block's relay decision** (the block running now, whose
  heater relays the loop pinned and the controllers were told to apply; `on` once `kw` > 0.05, the
  dashboard's solver-dust cutoff) — NOT a measured relay read-back. A stale plan's `plan` slice and
  `plan_heat_kwh_12h` are still reported. `heating_today_kwh` (any
  zone with a heater, whatever the plan) is the ledger's *measured* `heat_kwh` summed over blocks
  starting on the current **site-local** date (each block is dated with the UTC offset at its own
  instant, so DST days are right; `null` when none has a measured value yet). `plan` holds the latest
  plan's blocks still running or starting within the next `hours` (12 by default; `null` for a zone the plan does not
  control); `plan_heat_kwh_12h = Σ heat_kw·dt` over the latest plan's blocks, each clipped to the next
  12 h (now → now + 12 h), whatever `hours` is. `pull` only exists for zones that carry a pull
  (`comfort.pull_eur_per_kh × priority > 0`): `eur_per_kh` is that product, `kh` the plan's K·h below
  the heat target within the band, `break_even_eur_per_kwh` the plan's `pull_break_even_eur_per_kwh` (an approximate indicator of the import price below which the pull buys extra heat, not an exact indifference price).
  `heat`/`cool`/`priority` are `null` for a zone with no comfort entry. Humidity/presence locators: `docs/configuration.md` (`zone_mappings`).
  `presence.motion` is the last logged motion value (loxone logs on change), so read it with its `age_s`.
  `site` is the house's location in decimal degrees (`config.json5` `site.latitude` / `longitude`),
  always present; the dashboard's 3D floor plan places its sun from it.
- **`GET /api/zones/series?hours=N`** — recent **measured** per-zone air-temperature series for the Home screen's indoor-comfort sparklines and the house timeline's measured past (default 24 h, clamped 1–48), 30-minute means: `[{ zone, series: [[iso, °C], …] }]`. Zones with no data are omitted.
- **`GET /api/plan`** — on-demand whole-house plan (recomputes). Aggregates (cost EUR/CZK, grid/heating/cooling/HVAC-heating/battery kWh, PV curtailed, calibration scale, `pv_nowcast`, `placeholder_inputs`), the immediate `first_step`, `next_step` (on-the-mark switching: block 1 of `timeline` with its start instant `t` — the same shape as one `timeline` row, `null` if the plan has fewer than 2 blocks — so a client can show "next block: …" without indexing `timeline` itself; the pre-mark freeze window: from `t − 120s` onward `next_step` is the ENTIRE block — heat/cool/hvac
relays, battery `charge_kw`/`discharge_kw`/`slot`/`export_enabled`/`inverter_on`, controllable-load
relays, and the per-charger EV `charge_kw` — FROZEN verbatim to whatever the
loop committed at the start of that window, and `frozen` is `true`; before `t − 120s`, `next_step` is
the tick's own fresh (unfrozen) block 1. The publisher applies exactly this snapshot as its next
command at `apply_at = t` ONLY while `frozen` is `true`, and, once promoted, that snapshot stays
authoritative for the CURRENT command too until the block ends, regardless
of what a later tick's plan says — so what actually REACHES THE HARDWARE for a promoted block is
pinned exactly once. **This is narrower than "the loop's own internal LP re-solve always agrees with
what it promoted."** Of the fields above, only the underfloor-heating relay binaries are hard-pinned
INSIDE the LP itself (`committed_heat`/`heat_relay`, block 0 — see `unified.rs`); battery, EV, and
controllable-load decisions are not LP-level equality constraints, so a later tick's own fresh solve
for the already-promoted block may re-optimize them differently INTERNALLY (its own forecast/
reporting for that block, and the ordinary `timeline` row for it — never the frozen `next_step` /
what a controller actually applies, and never re-sent: the controllers' same-block guard rejects a
diverged re-actuation outright). The gap is therefore display-only — the dashboard, decision log, or
`/api/plan/timeline`'s block 0 can disagree with what was truly actuated by up to one block's worth of
battery/EV/load numbers, self-correcting at the next tick's fresh measurement — never an
actuation-safety gap), and the per-block `timeline` (below). HVAC fields (`cooling_kwh`, `hvac_heating_kwh`, and the per-block `cool_kw`/`hvac_heat_kw` maps) are `0`/empty unless an `hvac` block is configured. Three honesty flags: `degraded` (safety-critical input fell back — the publisher refuses to actuate), `relaxed` (the strict fix-and-round pipeline itself failed or timed out and only the plain relaxed LP answered; possibly fractional relays — not actuated, not latched), and `rounded` (the NORMAL result of the strict fix-and-round pipeline — relaxed LP → deterministic rounding → fully-pinned re-solve; integral and actuated). The planner runs fix-and-round with no branch-and-bound at all, so `rounded` is the ordinary case on every healthy tick, not a fallback signal. Curtailment-risk fields `p10_surplus_kwh` / `curtailment_risk_kwh` (kWh, from the Solcast p10 percentile) are `null` until the forecast writer stores the p10 curve. `disturbance_w` (empty unless `estimator.disturbance` is on) is the Kalman observer's per-zone constant flux (W, + heats) as folded into THIS plan's `internal_gain_w` for the whole horizon — the offset-free correction, distinct from `/api/state`'s independently-read current value (the two agree when both ran off the same tick, but are computed separately). `terminal_heat_credit_eur_per_kwh` (EUR per kWh thermal, empty when no zone got a positive credit) is the terminal slab-heat credit ACTUALLY applied per zone this solve — the price of the future heating each zone's banked heat is estimated to displace, taken from the post-horizon weather outlook (persisted prices; the cheapest blocks within the first 24 h of the outlook, or up to the zone's first dip below its floor when that comes later) when one covered that zone, else the flat median-import-based value shared by every zone. See `docs/configuration.md`'s "terminal slab-heat credit" paragraph for the mechanism. `export_pv_gated_blocks` (count) is how many blocks over the horizon are PV-dark (forecast `pv_kw` at/below the PV-present threshold) AND not already bound to 0 by the export-off or placeholder-price gate — i.e. where the always-on PV-dark export gate is the sole reason `batt_to_grid` is forced to 0. This is NOT a count of blocks where an ungated plan would actually have chosen to export there (it may find export unprofitable for other reasons); `0` when every dark block was already gated some other way. `terminal_soc_value_eur_per_kwh` (EUR/kWh) is the value of the energy left in the battery at the horizon end AS ACTUALLY PASSED to the LP (after the `p10_precharge_guard` halving, if applied); `terminal_soc_value_source` is `"outlook"` (the post-horizon day-type-median curve, `app::terminal::terminal_soc_value_outlook` — the normal case) or `"horizon_median"` (the in-horizon-median value, `app::terminal_soc_value` — a thin/cold-start price history that doesn't cover every post-horizon block, flagged in `placeholder_inputs` as "terminal SoC value (outlook uncovered …)"). The on-demand `GET /api/plan` path runs with no cache (no `price_history` — the same on-demand limitation the heat-credit outlook has), so it always reports `horizon_median` plus the placeholder note; `GET /api/plan/latest` (the MPC loop's actuated plan, which does carry the cache) is where `outlook` normally appears. *(Removed without an `api_version` bump: `solar_scale` and `terminal_soc_value_legacy_eur_per_kwh`.)* See `docs/configuration.md`'s "Terminal SoC value" paragraph for the mechanism. `pull_cost_eur` (EUR, `0` when no room carries a pull, i.e. `comfort.pull_eur_per_kh × priority` is `0` everywhere) is the total within-horizon cost this plan paid for sitting below the heat targets (Σ pull × priority × K·h below target) — NOT part of `total_cost_eur`/`total_cost_czk` (grid cash alone); `pull_kh` (empty under the same condition) is the Kelvin·hours each pulled room’s air sat below its heat target within the pull band this horizon (and above the cool target of an AC room); `pull_break_even_eur_per_kwh` (empty under the same condition) is, per pulled room, an APPROXIMATE indicator (not an exact indifference price) of the import price (EUR/kWh electricity) below which the LP spends extra kWh heating that room for the pull — well below it the pull buys at least as much heat as no pull, well above it (a few ×) none; it SUMS every pulled room’s own `pull_eur_per_kh × priority` weighted by that room’s thermally-coupled effect on the target (cross-zone gains, not just the target’s own self-kernel), so a tightly-coupled cluster’s break-even runs above any one room’s own target price — see `docs/configuration.md`'s `comfort` section for the formula and the kernel-gain caveat. *(`api_version` 3 renamed `warmth_reward_eur` → `pull_cost_eur`, `warmth_kh` → `pull_kh`, `warmth_break_even_eur_per_kwh` → `pull_break_even_eur_per_kwh`.)* `relay_switches` / `relay_switches_by_zone` (additive; count, per heated zone and summed) are the planned relay on↔off changes over the horizon — Σ|u_t − u_{t−1}| with `u = heat_kw / max_heat_kw`, integral over the pinned near-term blocks and fractional in the free tail; the entry change at block 0 is counted only when the previous block's relay state is known; switching inside a multi-rate hourly block is invisible. This is what `heating.relay_switch_cost_eur` prices (docs/configuration.md). `pv_nowcast` (always present) is the intraday nowcast's outcome this cycle: `{applied, reason, ratio, window_minutes, applied_hours, blocks, source_age_s, measured_kwh, forecast_kwh, delta_kwh}` — `applied: false` with a `reason` (e.g. `"disabled"`, `"stale (age N s)"`, `"low forecast (... kW < ... kW)"`, `"curtailed window"`, `"forecast refreshed"`, `"partial coverage (n/m samples)"`) when it was skipped that cycle; see `docs/configuration.md`'s "PV intraday nowcast" paragraph.
- **`GET /api/plan/latest`** — the latest plan published by the MPC loop (no recompute; `503` while warming up). `data` is the same plan shape as `/api/plan` (the envelope's `computed_at` is when it was published).
- **`GET /api/plan/timeline`** — just the latest plan's per-block rows (the chart-ready shape). Each block carries `dt_minutes` (the multi-rate grid: 15 for a near-term fine block, 60 for an hourly one further out — see `docs/configuration.md`'s `horizon` section); `t` is the block's START instant, so a block's coverage is `[t, t + dt_minutes)`:

```json
[ { "t": "2026-06-23T11:30:00+00:00", "dt_minutes": 15, "import_price": 0.12, "export_price": 0.05,
    "pv_kw": 4.1, "soc_kwh": 6.2, "charge_kw": 0.0, "discharge_kw": 1.3,
    "grid_import_kw": 0.0, "grid_export_kw": 0.0, "curtail_kw": 0.0,
    "heat_kw": {"livingroom": 0.0}, "cool_kw": {}, "hvac_heat_kw": {}, "ev_charge_kw": {},
    "temp_c": {"livingroom": 21.4}, "outside_c": 7.6,
    "slot": "regular", "export_enabled": true, "inverter_on": true,
    "price_is_placeholder": false, "frozen": false } ]
```

`frozen` (always present) is `true` only on [`PlanReport::next_step`] once
the loop's pre-mark freeze window has pinned it (see below) — every ordinary `timeline` row, block 0
included, always reports `false`: it's the tick's own fresh LP output, never the frozen snapshot.

**`slot` is the battery action in `loxone_smart_home`'s own vocabulary** (`app::mode::classify_mode`):
`regular` (self-consumption, incl. passive solar-charge/load-discharge), `charge_from_grid`,
`discharge_to_grid`, `sell_production` (exporting surplus solar with the battery passive),
`battery_hold` (importing while the battery is held for a pricier block), `inverter_off`. In a
`rounded` plan (see `PlanReport::rounded` below), a `regular` block NEVER carries battery→grid
export or grid→battery charge: the LP's fix-and-round pinning keeps each of those two legs either
`0` or `>= battery.min_dispatch_kw` (the Growatt powerrate floor — `docs/configuration.md`'s
`battery` section), so whenever either leg is nonzero the block is labelled `charge_from_grid`/
`discharge_to_grid`, never `regular`. **Nor does it carry battery discharge beyond the house's real
electrical deficit, or battery charge beyond its real solar surplus** — a load-first inverter
physically cannot route battery energy to the house load past what solar already covers (or draw
solar into the battery past what the load leaves spare), so a `regular` block's discharge/charge
is always bounded by that real deficit/surplus too (the routing-loophole caps on `batt_to_load`/
`solar_to_batt` — closes the gap where a pinned-off export would otherwise resurface as extra
battery-to-load draw while solar exports the same kWh in its place, same cost, same fiction under
the `regular` label). Two documented exceptions: a `relaxed` (advisory, not actuated) plan has no
such guarantee — the strict pipeline's pinned re-solve itself failed, so nothing enforced the
floor — and a `rounded` plan produced by the SoC-guard retry (logged as `[solve] dispatch floor:
un-guarded pin failed …`) may leave the guard-freed blocks' legs below the floor. In both cases a
sub-floor leg is labelled `regular` (the demotion guard: better to report "no dispatch" than a
value the real controller would round up to the floor, actuating up to ~8× the planned energy).

**`heat_kw` is not always a literal setpoint.** Underfloor heating is a mechanical relay:
one on/off decision per whole block, never sub-block modulation. The solver only pins an actual
integral relay decision for the first **two** blocks (30 minutes — `HEAT_COOL_PIN_BLOCKS` in
`unified.rs`; see `docs/configuration.md`'s note on why it's exactly blocks 0 and 1, the two blocks
on-the-mark switching ever actuates); every `heat_kw` entry beyond that is the relaxed LP's **average power over the block** (a
real quantity — it's what the terminal-value/cost accounting uses — but not a value any relay can
hold continuously).
It becomes real whole-block switching once the per-minute re-plan's own fix-and-round window reaches
that block, typically producing a different mix of on/off sub-blocks that average to roughly the same
energy, not a constant partial-power run. A client rendering the timeline should treat a relay zone's
(one present in some block's `heat_kw`) near-term entries as on/off and its far-horizon entries as an
*expected* duty — e.g. `heat_kw / max_heat_kw` (from `/api/zones`) as a fraction, or that fraction × 4
as "on-blocks per hour" — never as a literal kW draw; the dashboard's Heating screen does this (see
the Home Assistant dashboard's `app.js`, `relayDuty`/`isNearTermBlock`). `hvac_heat_kw`/`cool_kw` are a genuinely
continuous, reversible AC setpoint (no relay involved) and carry no such caveat.

### Capabilities & EV

- **`GET /api/capabilities`** — what this house has, for conditional UI: `{ has_hvac, has_ev, chargers: [name…] }`.
- **`GET /api/ev`** — per-charger live state + planned charge schedule (present only with EV configured): `[{ name, status, on_our_charger, controllable_now, charging_elsewhere, soc_pct, target_pct, target_capped, capacity_kwh, active_car, strategy, charger_power_kw, charged_kwh, deadline_source, deadline_hm, deadline_at, charge_kw:[…], solar_kw:[…], grid_kw:[…], batt_kw:[…] }]`. `status` ∈ `charging | connected | charging_away | away`; `deadline_source` ∈ `pref | learned | config` says which deadline won (`deadline_hm` is the resolved **site-local** time — see [ev.md](ev.md)); `target_capped` means the stored preference exceeded the car's own charge limit and was capped to it. `deadline_at` is that same deadline as an absolute RFC3339 instant (the next occurrence of `deadline_hm` in site-local time, omitted when there is no deadline): use it rather than re-resolving `deadline_hm`, which would land on the wrong moment for a client in another timezone.
- **`GET /api/ev/<name>/preference`** / **`POST /api/ev/<name>/preference`** / **`DELETE /api/ev/<name>/preference`** — read / merge / clear the live override (`strategy`, `max_rate_kw`, `target_pct`, `deadline`). The POST **merges per field** (any subset; omitted fields keep their stored values); DELETE reverts everything to config / the car. The **only** MPC write — to its own `MPC_EV_PREF_STORE` file, never InfluxDB/MQTT. `404` for an unknown charger; `400` for an invalid body (`target_pct` outside 0..100, non-finite `max_rate_kw`, unparseable `deadline`). With the `MPC_API_TOKEN` env var set on the server, both mutating verbs require a matching `X-MPC-Token` header (`401` otherwise; the dashboard prompts once and remembers it) — set it if untrusted devices share the LAN. In the Docker deploy, put it in `<deploy dir>/api.env` as a single `export MPC_API_TOKEN="..."` line, `chmod 600` (the container script refuses to source a group/world-readable one, and forwards the variable by NAME so the value never reaches the `docker run` command line). Without the file the routes stay **open to the LAN** and the script says so on startup. See [ev.md](ev.md).

### Room comfort targets

The one place the brain edits `config.json5`: a room's `heat` / `cool` **target** (nothing else), so a
target can be changed from the Home Assistant room page. The file stays the record: the brain
replaces only the number after `heat: { target:` / `cool: { target:` on that room's line (comments and
every other byte unchanged), validates the whole candidate file with the normal config loader, and only
then renames it over the original; any refusal or failure leaves the file untouched. The new targets
are used from the next MPC tick (the loop logs `[mpc] config reloaded (fingerprint …)`), and
`config_fingerprint` in `/api/version` changes. Needs one `comfort.rooms` entry per line (as in the
repo file); the live `config.json5` must be writable by the brain (see CLAUDE.md, Deployment).

- **`GET /api/comfort`** — envelope `data`: `{ config_fingerprint, rooms: [{ zone, heat: {target, low, high}, cool: {target, low, high} | null, priority, file_target_heat, file_target_cool | null }], projection, projection_status, projection_error }`. `low`/`high` are `target − below` / `target + above` as written in the file (windows not applied). `projection_status` is `computing` (first call or a stale cache: the brain reads a year of history in the background, a few minutes; `projection` is `null` meanwhile; poll again), `ready`, or `error` (`projection_error` says why; the year-long read is not retried for 60 min, to spare the database). A stale projection keeps being served while it refreshes. The GET never waits for Influx.
- **`projection`**: `{ from, to, coverage_days, partial, method: "heat_loss_model", note, eur_czk_rate, rooms: { <zone>: { zone, heat_cost_per_k_eur_per_year, heat_cost_per_k_czk_per_year, heating_hours, reason, g_room_w_per_k, g_house_w_per_k, cool_cost_per_k_eur_per_year, cool_cost_per_k_czk_per_year, cooling_hours } } }` (`rooms` is an object keyed by zone; `eur_czk_rate` is CZK per EUR). What **+1 K on the target** would have cost per year over the last 365 days (the UI multiplies by the chosen ΔT; it is linear): positive costs more, negative saves. The model: a steady-state solve of the RC network gives `g_house` (W per K of the room's air that leaves the HOUSE, to outside and ground; heat that only moves into neighbouring heated rooms is not a house cost; `g_room` is the room's own total and is not used for the cost). In every hour the room needs heat (`g_out·(target−T_out) + g_gnd·(target−T_ground) > internal_gain`), +1 K costs `g_house / heating.cop` watts, priced at that hour's real import price (OTE spot + distribution, VT/NT). Cooling (`cool_cost_*`) is `null` unless an `hvac` unit serves the room; raising the cool target 1 K saves, so it is negative. `heat_*` are `null` with a Czech `reason` (shown verbatim by the dashboard) for a room with no heating power ("místnost nemá topení (max_heat_kw 0)", e.g. `storage`), no heating equipment ("místnost nemá topení"), no underfloor marker in the model ("model nemá pro místnost podlahové topení") or a failed solve ("výpočet tepelné ztráty selhal: …"). The top-level `projection_error` stays English. Interior rooms legitimately come out near zero. `partial: true` when fewer than 300 days of matching temperature and price data were found (`coverage_days`); the sums are scaled to a full year. The numbers are for the target in the file when the projection was computed. `note` carries the caveat to show: *Model estimate from last year's weather and prices; the planner shifts heating to cheap hours, so the real cost is likely lower. ±20 %.* Cached `MPC_COMFORT_PROJECTION_TTL` seconds (default 86400).
- **`POST /api/comfort/<zone>`** — body `{ "heat_target": 22.5 }` and/or `{ "cool_target": 25.0 }` (unknown fields are rejected; at least one required; both are applied in one write). Steps of 0.5; heat 15.0–26.0 °C, cool 20.0–30.0 °C. Returns `{ room: <the room's entry as in GET>, config_fingerprint }`. `404` unknown room; `422` out of range / wrong step / `cool_target` on a room with no `cool` block / the result would put the heat target at or above the cool target ("heat target must be below the cool target (…)"; the file validator itself allows overlapping bands) / the line cannot be edited / the edited file fails config validation (the validator's message); `500` the file cannot be written (original intact). Writes are serialised. Honours `MPC_API_TOKEN` like the EV routes (`401`).

### Accuracy & calibration

- **`GET /api/pv/backtest?days=N`** — PV forecast vs actual Growatt generation (default 7, 1–60), excluding curtailed hours, with each day's forecast source. Also `leads: [{lead_from_h, lead_to_h, all, solcast, other}]` — accuracy per lead-time bucket ([0,1),[1,2),[2,3),[3,6),[6,12),[12,24),[24,48) h) over every stored snapshot of the last 14 days, split by source class (each score `{n, rmse_kw, bias_kw, forecast_kwh, actual_kwh}`). Every bucket is **already daylight-only**: both this binning and the whole-day score skip hours where forecast AND measured are both below `DAYLIGHT_KW` (0.05 kW) — the near-term buckets were split finer (0–1/1–2/2–3/3–6 h, replacing the old single 0–6 h bucket) so they're directly comparable to the 6–12/12–24/24–48 h buckets despite differing hour-of-day composition (the broad buckets' early leads skew toward peak-sun hours). A snapshot's lead is computed to the SECOND (not truncated to whole minutes), and a negative lead — a remnant snapshot recorded a few seconds after the hour it's scoring just ended, before that hour's key even entered its curve — is excluded from every bucket rather than rounding into `[0,1)` against a spurious zero forecast. Also `nowcast` — the nowcast replay (the pv-nowcast accuracy proof; not a like-for-like of live — each reference hour is evaluated once, so the refresh hours never get a nowcast here (conservative), while live resumes ~30 min after a refresh on a shorter, noisier window the hourly replay does not exercise), using the config's `pv.nowcast` params: `{params: {efold_hours, max_hours, clamp, min_forecast_kw, window_minutes}, all: [{lead_from_h, lead_to_h, n, plain: {rmse_kw, bias_kw}, nowcast: {rmse_kw, bias_kw}}] (3 bins: [0,1),[1,2),[2,3) h), applied: [same shape, only reference hours that produced a ratio], n_ref, n_ref_applied, n_ref_gated, n_ref_curtailed, n_ref_refreshed, n_ref_missing (n_ref == the sum of those five), n_clamp_lo, n_clamp_hi, n_neutral_calibration_days, mean_scale}`. The reference-hour snapshot is picked at `hour_end(h) + 120s` (`SNAPSHOT_LANDING_GRACE_S`, parity with live: real snapshots land a few seconds into the next hour) with NO fallback to an older snapshot — a snapshot that exists but doesn't yet have hour `h`'s key counts as `n_ref_refreshed` (live's own "forecast refreshed" skip), distinct from `n_ref_missing` (no snapshot recorded at all by then). Both arms (`plain`, `nowcast`) are scored on the identical sample set against a per-date trailing BAND calibration — the same shape `app::build_cache` fits for the live plan (a per-local-hour-band ratio, not a bare scalar), refit each date from the preceding ≤7 scored days and gated on the same `CALIBRATION_MIN_SCORED_HOURS` (`docs/configuration.md`'s "PV intraday nowcast" paragraph; `n_neutral_calibration_days` counts the dates that gate forced to neutral). A reference hour that's gated/curtailed/refreshed/missing still contributes its targets to `all` with `nowcast == plain`. `cargo run --release -- backtest-pv-nowcast <days> [--efold ...] [--max-hours ...] [--clamp-hi ...] [--min-forecast ...]` replays the same math as a parameter sweep over one InfluxDB read (`days` capped at 21). `cargo run --release -- backtest-heating --start <rfc3339, hour-aligned> --days <1..=7> [--warmup-h 48] [--model <path>] [--config <path>] [--out <json>] [--dump <fixture>] [--from <fixture>] [--on-duty 0.7] [--min-on-h 2] [--off-h 4] [--off-duty 0.1]` validates the condensed heating kernels (`optimize::thermal::build_kernels`) against measured winter data — see `src/heating_backtest.rs`'s module doc and the "Winter heating backtest" paragraph below; it reads the relay as the same event-based duty the live `validate::read_heating_kw` uses (see `docs/configuration.md`'s "How the relay is read").

**Winter heating backtest.** Run this once a winter, before the heating season and again mid-season, on 2–3 bounded (≤7-day) windows chosen by eye from the daily-mean outside temperature (a cold spell, a mild week, and one in between) — through the SSH tunnel locally, e.g. `cargo run --release -- backtest-heating --start 2026-01-05T00:00:00Z --days 7 --out jan-cold.json`. Each run reads ONE bounded window (plus a 48 h warm-up by default) and is fully offline-replayable: pass `--dump jan-cold-raw.json` on the live run to save the raw series, then `--from jan-cold-raw.json` to re-score it later (e.g. with a candidate `--model`) with no InfluxDB token. Scheduled loads carry no measured `sensor_power_w` in this historical drive (only their configured/fitted magnitude) — a `sensor`-driven load's flux comes from the live draw only in the real MPC loop, not here. The report has four parts:
  - **Active backtest** — RMSE/bias/max-error per zone, split into all / heating-on / after-pulse (≤6 h after the last on-hour) / other hours, pre- (static config gains) and post- (per-window NNLS fit, which keeps the live loop's 3600 s end-of-hour drive — only the scoring and the kernel check use hour means) internal-gain calibration — plus each zone's kWh delivered (the time-weighted duty reconstructed from the raw on-change relay events, the same computation the live `read_heating_kw` uses).
  - **Kernel check** — `k_hat`/`s_hat` per heated zone from a whole-window least-squares fit of the measured-minus-modelled residual against the zone's own impulse-response (kernel, built to ≥ 2× the read window — see `kernel horizon` in the report header) convolution of its recorded heating, profiling response speed (`s_hat`) alongside strength (`k_hat`), both sides compared as HOURLY MEANS (matching how the measured series is actually read, not an end-of-hour point): `k_hat` near 1 means the model's K-per-kWh gain is right; `s_hat` near 1 means its speed is right (`s_hat > 1` = the real response is SLOWER than modelled). Also `k_hat_diurnal_ctrl` — an INDEPENDENT re-fit (re-profiling `s_hat` from scratch, not reusing the plain fit's) with one extra global sin/cos (local hour-of-day) nuisance term — and `diurnal_sensitive` (the two disagree by > 15 %): a model error shaped like the daily solar/occupancy cycle can otherwise masquerade as a confident but wrong gain/speed, so a proposal needs both to agree. Read the 95 % leave-one-day-out jackknife CI on `k_hat` before trusting it — `not identifiable` (`k_hat` absent) means too little heating in the window (< 5 kWh) or too collinear with the per-day trend terms (VIF > 10) to say anything; **the proposal bar additionally requires `identified: true`** — `k_hat`/`s_hat` are still reported even when `false` (nothing is hidden) but are flagged untrustworthy when `k_hat ≤ 0` (physically impossible) or `s_hat` lands on a grid edge (0.5 or 2.0 — the true speed may lie outside the searched range).
  - **Episode table + lag** — for ON streaks (`--on-duty`/`--min-on-h`/`--off-h`/`--off-duty` tune the detector; detected only in the scored window, never the warm-up) the measured response (raw, model-drift-corrected, and pre-trend-corrected — the pre-trend slope is fitted on the measured temperature itself) vs the modelled one at 1/3/6/12 h leads, and the event-averaged response curve's time-to-peak / time-to-63% (censored, not reported, if the curve is still rising at the lag horizon).
  - **Catch-up** — minutes from floor−1 K to floor at `max_heat_kw`, linearly interpolated between fine steps (no 15-min ceiling): modelled (the kernel's own step response, searched over its full built horizon — see the report's `kernel horizon` line), measured-implied (`k_hat`/`s_hat`-rescaled), and direct (the median across qualifying episodes whose first 3 ON hours ran at duty ≥ 0.9, i.e. close enough to a real step; `None` unless at least 2/3 of those reached +1 K — a median over "reached" alone is a lower bound). A value that never reaches +1 K within the kernel's horizon prints as `> <kernel horizon> h`, not a bare "None".

  The relay logs **on change only** (a handful of points/room/day) — a window with sparse logging near its edges can leave a zone's `relay_state_before` unknown (flagged in the report; treated as OFF) or starve the episode detector of qualifying streaks; a short/mild window may report `not identifiable` for low-heating zones. `--model`/`--config` point at a candidate pair (never the live `model.json5`/`config.json5`) to score a proposed change on the same window before/after.

**Comfort backtest (comfort-pull arms).** `cargo run --release -- backtest-comfort --start <rfc3339, hour-aligned> --days <1..=7> [--step-hours 1] [--plant-gain 1.0] [--pull 0,0.02,0.04,0.08] [--switch-cost 0,0.01,0.02] [--config <path>] [--dump <fixture>] [--from <fixture>] [--out <json>]` is the real-data proof of the comfort pull (`comfort.pull_eur_per_kh`, see `docs/configuration.md`'s `comfort` section): a rolling-horizon replay of the PRODUCTION planning pipeline (the SAME `SolveJob` + `app::fix_and_round` pipeline a live tick runs, with the thermal model as the plant) run once per `--pull` value from the same seed — one arm each, default `0` and the configured pull — so any cost/temperature/energy delta is attributable to the pull alone. `--switch-cost` crosses every pull with each `heating.relay_switch_cost_eur` value (labels become `pull=P,switch=C`; with no `--pull` only the configured pull is used), and each zone then also reports `relay_switches` — executed on↔off changes at 15-minute resolution — with the raw 0/1 series in `arms[].exec_on`. Each tick re-plans every arm from measured PV/load/weather (perfect foresight — the item under test is the pull, not the forecast) and steps each arm's OWN plant state forward with the discretized model under that arm's own plan, injecting heat at each zone's `"heating"` marker exactly as the kernel build does; `--plant-gain 0.67` derates the executed heat pulse to approximate the ~1.5× cold-weather kernel over-response (`docs/configuration.md`'s kernel-gain caveat). **Window rule**: OTE publishes day D's prices on D-1 by ~14:00 local, so the window's END must be at least 37 h before `now()` or the core window couldn't be executed at real (not estimated) prices — `run()` enforces this and prints the bound it checked. Reads are bounded (≤7-day chunks, one series at a time, paused between): OTE prices (28 d of history for the day-type-median estimator), Growatt PV/load/SoC, and `backtest-heating`'s own outside/weather/zone/relay window (48 h warm-up by default) — `--dump`/`--from` round-trip everything through one `warmth-backtest-v1` fixture (the schema name predates the command rename and is unchanged, so dumps written by an older binary replay under this one — the way a pre-switch baseline and the new arms share inputs). The printed table (and `--out` JSON `arms[]`) report, per arm: realized cost (EUR over the window and `cost_eur_per_week`), end SoC, mean/time-limited/failed solve counts; per zone: heating kWh split by price band (`pv_surplus` = zero/dust grid import that block, `nt`/`vt` by the tariff's local-hour mask), Kelvin·hours below the heat target / below the floor (`target − below`) / above the ceiling, the plant's mean and minimum air temperature and its end-of-window temperature, plus a per-tick `ledger` row `{t, arm, heat_kw, import_kwh, export_kwh, cost_eur, plant_temp_c}`. `backtest-comfort --live [--config <path>]` instead compares pull 0 vs the configured pull on the CURRENT on-demand plan (`app::current_plan`), printing each pulled room's `pull_kh` and `pull_break_even_eur_per_kwh` (`/api/plan`'s own fields), heating kWh split by price band, and the min/max temperature reached over the horizon, alongside the cost delta. Known simplifications (printed in the header, never silent): internal gains come from the CONFIG baseline only (no live Kalman gain/disturbance fit); no EV scheduling.
- **`GET /api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=`** — thermal model accuracy per zone (RMSE / bias / max error). The scored range is always `-(warmup+window)h .. now()`; `window_hours` (1–720) and `warmup_hours` (0–720, sum capped at 720) are the only range knobs — explicit `start`/`stop` are rejected. `detail=1` (passive only) returns the hourly diagnostic instead of the bare score list: `{ scores, hours:[iso…], outside_c:[…], ghi_wm2:[…], cloud:[…], zones:[{ zone, predicted_c:[…], measured_c:[…|null] }] }`, all parallel to `hours` — for checking whether a zone's bias is solar-shaped, diurnal or flat.
  - `passive` (default): free-response drift (summer). `window_hours` default 24, `warmup_hours` default 48.
  - `active`: driven by recorded heating relays; **fits** internal gains and returns `{before, after, gains_w}` (before/after = per-zone scores without/with the fitted gains).
  - `x0=kalman` (**passive only**): measurement updates run only during the warm-up, then the window is scored as a pure open-loop prediction from the Kalman-filtered state — the held-out estimator comparison. `400` with `mode=active` (that fit seeds its own state, so `x0` has no seam to apply to and would be silently ignored), and `400` when no filter is available — distinguishing `estimator.mode: anchor` from "still building at startup" (the Riccati solve takes seconds, tens under static-musl), so a retry is worthwhile only in the latter case.
- **`GET /api/calibration/gains`** — the live internal-gain self-correction, plus each scheduled
  load's magnitude (`source` is `"measured"` when a `sensor` drives the flux from the real draw,
  `"configured"` when `power_w` is set, else `"fitted"`):

```json
{ "live": { "fitted_at": "…", "window_days": 7, "gains_w": { "livingroom": { "night": 40, "day": 60, "evening": 320 } },
            "scheduled": [{"label": "water heat-pump", "zone": "technical_room",
                           "magnitude_w": 1600, "source": "configured"}] },
  "config_baseline_w": { "livingroom": { "night": 351, "day": 351, "evening": 351 } },
  "recalibrate_hours": 24, "window_days": 7 }
```

### Decision ledger

- **`GET /api/ledger?days=N`** (default 7, clamp 1–30) — planned vs measured per block, and the
  realized vs planned cost it implies. Every other accuracy endpoint above scores a *forecast*; this
  scores a *decision*: the block `mpc_loop` actually committed to the controllers, joined against the
  measured Growatt/heating/EV telemetry once the block has ended and the data has had time to land.
  TTL-cached 60 s, served from the brain's own in-memory store (`MPC_LEDGER_STORE`) — no DB read on
  this endpoint.

```json
{ "days": 7, "unscored": 3,
  "rows": [{ "t": "…", "dt_minutes": 15, "recorded_at": "…", "source": "frozen",
             "slot": "discharge_to_grid", "export_enabled": true, "inverter_on": true,
             "degraded": false, "relaxed": false, "rounded": false, "drifted": false,
             "price_is_placeholder": false, "import_price": 0.18, "export_price": 0.05,
             "wear_eur_per_kwh": 0.04, "eur_czk_rate": 25.0,
             "planned": { "pv_kw": 0.0, "load_kw": 0.4, "charge_kw": 0.0, "discharge_kw": 4.47,
                          "grid_import_kw": 0.0, "grid_export_kw": 4.47, "heat_kw": {},
                          "ev_charge_kw": {}, "controllable_load_kw": {} },
             "measured": { "pv_kwh": 0.0, "load_kwh": 0.46, "charge_kwh": 0.0, "discharge_kwh": 0.13,
                           "import_kwh": 0.02, "export_kwh": 0.0, "heat_kwh": {}, "ev_kwh": {} },
             "scored": true, "score_note": null, "scored_at": "…",
             "planned_cost_eur": -0.20, "realized_cost_eur": 0.004,
             "misses": ["export not actuated: planned 4.47 kW, measured 0.00 kW (PV 0.00 kW)"] }],
  "by_mode": [{ "mode": "discharge_to_grid", "n": 12,
                "planned_charge_kwh": 0, "measured_charge_kwh": 0, "charge_efficacy": null,
                "planned_discharge_kwh": 4.5, "measured_discharge_kwh": 0.6, "discharge_efficacy": 0.13,
                "planned_export_kwh": 4.5, "measured_export_kwh": 0.0, "export_efficacy": 0.0,
                "planned_import_kwh": 0, "measured_import_kwh": 0, "import_efficacy": null }],
  "by_day": [{ "date": "2026-09-30", "n_scored": 90, "n_unscored": 6, "n_placeholder": 0,
               "planned_cost_eur": -1.2, "realized_cost_eur": 0.3,
               "planned_cost_czk": -30, "realized_cost_czk": 7.5,
               "planned_heating_kwh": 2.0, "measured_heating_kwh": 1.8,
               "planned_import_kwh": 1.0, "measured_import_kwh": 1.1,
               "planned_export_kwh": 4.5, "measured_export_kwh": 0.0 }],
  "misses": [{ "t": "…", "slot": "discharge_to_grid", "reason": "export not actuated: …" }] }
```

  Semantics:
  - `source` is which decision the row records: `"frozen"` (the pre-mark freeze-window commitment),
    `"block0"` (a fresh clean plan's block 0), `"degraded"` (no clean plan covered the block),
    `"late"` (a clean block 0 first tracked more than 3 min into the block — the loop just
    started/recovered mid-block, so the controllers may have been running something else for its
    first few minutes), or `"plan-snapshot"`/`"log"` (backfilled via `ledger import`). `drifted: true`
    means a LATER clean plan disagreed with the recorded decision mid-block — informational only; the
    controllers never see that drift (the publisher only ever promotes the first/frozen commitment).
  - `scored: false` means the measured Growatt telemetry wasn't complete (or had a non-finite reading)
    for every 15-min quarter of the block yet (`score_note` names the first missing/bad field) — it is
    retried for 48 h, then given up on (`score_note: "aged out"`). `measured.heat_kwh`/`ev_kwh` are
    **omitted per item** (the key is simply absent, never `null`) when that zone's/charger's
    measurement couldn't be reconstructed (relay duty needs a known prior state — see
    `relay_duty.rs::relay_duty`) — never zero-filled. Conversely, `planned.heat_kw`/`ev_charge_kw`/
    `controllable_load_kw` only serialize their NON-ZERO entries — there an absent key means 0 kW
    planned, the opposite meaning from the measured side's absence (both compactions exist purely to
    keep the 30-day store small; a client reading an older snapshot should treat a missing planned
    entry as 0 and a missing measured entry as unknown).
  - Costs: `planned_cost_eur`/`realized_cost_eur` are both `import_price·import_kWh −
    export_price·export_kWh + wear·discharge_kWh`, the SAME formula each side — `realized_cost_eur`
    is `null` until the block is scored and had known prices. CZK uses the row's own `eur_czk_rate`,
    snapshotted when the row was recorded (so an edited exchange rate never retroactively repriced
    history).
  - `by_mode`/`by_day` totals are built from **scored, non-degraded, non-relaxed** rows only, on both
    the planned and the measured side — mixing an unscored row's planned side into a total whose
    measured side only ever sees scored rows would bias every efficacy ratio and cost gap toward
    "the decision wasn't carried out" when the real reason is just "not scored yet" (the publisher
    never actuates a degraded or relaxed plan either, so those rows carry no real decision to score).
    `by_mode[].n` and `by_day[].n_scored` count the rows that fed the totals; `by_day[].n_unscored`
    counts the rest (unscored, degraded, or relaxed) for that day. The top-level `unscored` is
    unaffected — it counts every unscored row in the window regardless of mode/day/degraded/relaxed.
    Of `n_scored`, `by_day[].n_placeholder` further counts rows whose price was a placeholder
    (`price_is_placeholder: true`, e.g. a backfilled import with no real price at decision time) —
    those are EXCLUDED from `planned_cost_eur`/`realized_cost_eur` (both currencies, both sides) so a
    made-up price can never masquerade as real realized economics; their kWh contributions above are
    unaffected (price-independent).
  - `by_mode`'s `*_efficacy` is `measured/planned`, `null` when planned is below 0.01 kWh (a
    near-zero plan makes the ratio meaningless). `by_day`'s `date` is the **site-local** calendar
    date. `misses` lists newest first; a miss only ever fires on a scored, non-degraded, non-relaxed
    row (the publisher refuses to actuate either kind), and a `null` measurement never triggers one.
  - CLI (read-only towards InfluxDB; writes only `MPC_LEDGER_STORE`): `ledger import [--log <file|->]
    [--plan <file.json>]...` backfills rows from a saved decision log or `/api/plan/latest` snapshot;
    `ledger score` runs one scoring pass; `ledger show [--days N]` prints this same report as JSON.
    **The running server holds its store in memory**, loaded once at startup — run this CLI only
    against a store the server is NOT using (point `MPC_LEDGER_STORE` at your own file); a write here
    against the server's own file is silently lost at the server's next persist, which never re-reads
    the file from disk. The live server itself records an ended block into memory immediately but only
    writes the store to disk on the ledger scorer's 5-minute cadence (whenever something actually
    changed) — a crash loses at most that window's worth of ended blocks, never more.

### Forward validation

- **`GET /api/forecast/validation`** — "predict now, score later". The loop snapshots its forward temperature prediction periodically (`forecast_snapshot_minutes`); this scores the most recent snapshot with ≥3 h elapsed against the measured hourly temperatures: `{anchored_at, scored_until, zones: [{zone, n, rmse_k, mean_bias_k, points:[{t, predicted_c, measured_c}]}], mean_rmse_k, leads, snapshots_scored, zones_unavailable}`. `zones_unavailable` lists zones whose measurement READ failed (excluded from `zones`/`mean_rmse_k`) — distinct from a zone with no history yet. `leads` resolves accuracy by how far ahead the prediction was made — bins [0,3),(3,6),(6,12),(12,24),(24,36) h over ALL stored snapshots, each `{lead_from_h, lead_to_h, n, rmse_k, mean_bias_k, zones:[…]}` (bins with `n: 0` had no scoreable points; the store holds ~4 days). *(The optional per-bin `unscaled` score and the snapshots' `zones_unscaled` arm were removed with the Kalman solar scale, without an `api_version` bump; a stored snapshot that still carries `zones_unscaled` loads unchanged.)*

### New features

- **`GET /api/features`** — the "New features" dashboard screen: one entry per shipped feature still
  under evaluation (features that proved themselves are removed from the registry and listed in
  `graduated`, below; features that did not are removed from the code and the page), each with its
  release proof (from the commit message) and the strongest honest comparison the stored data
  supports — before/after a release, or both arms of an A/B computed on the same live data — with
  sample sizes and a verdict computed from the data. Envelope-wrapped, TTL-cached 5 min, bounded by
  the usual 55 s timeout. Everything is aggregated by pure functions in `src/features.rs` over the
  decision ledger rows and the feature samples (below). All days are **UTC** days (`YYYY-MM-DD`).

  `data`:

  ```json
  {
    "generated_at": "2026-10-13T07:15:56Z",
    "features": [
      {
        "id": "comfort_targets",
        "name": "Comfort targets",
        "shas": ["9a1431c"],
        "releases": [{ "sha": "9a1431c", "at": "2026-10-11T07:47:00Z" }],
        "live_since": "2026-10-11T07:47:00Z",
        "status": "live",
        "what": "one-line description",
        "kind": "ab_live",
        "kind_label": "A/B live",
        "n": 6,
        "verdict": { "state": "info", "text": "the pull plans +2.00 kWh more heat for +0.200 EUR planned cost per plan (6 solved A/B plans, …)" },
        "baseline": {
          "text": "the release proof, as in the commit message",
          "values": { "pull_eur_per_kh": 0.04, "replay_extra_eur_per_week": 1.6 }
        },
        "charts": [
          { "title": "…", "y_unit": "EUR", "x": ["2026-10-12"],
            "series": [{ "name": "mean extra cost", "values": [0.2] }] }
        ],
        "tables": [
          { "title": "…", "columns": ["date", "solved plans", "mean extra EUR"], "rows": [["2026-10-12", 6, 0.2]] }
        ],
        "notes": ["caveats the reader needs to interpret the comparison"]
      }
    ],
    "graduated": [
      { "name": "Dispatch floor", "sha": "330be56", "date": "2026-10-01",
        "result": "Unactuated regular-mode dispatch 5.0 -> 0.3 per 100 regular blocks (2/40 -> 1/384)." }
    ]
  }
  ```

  `graduated[]` is static text (no metrics): features that proved themselves and became the only
  behaviour — `name`, `sha` (one or several, comma-separated), `date` (`YYYY-MM-DD` it went live,
  `null` for an offline tool), one-line final `result`. The page shows it as a "Graduated" list at
  the bottom.

  Fields of one feature (all always present, in registry order — the order of the table below):
  - `id` (string, stable), `name`, `what` (one line), `shas` (all commits of the feature),
    `releases[{sha, at}]` (`at` RFC 3339 UTC), `live_since` (first
    release instant, or `null`).
  - `status`: `"live"`.
  - `kind` / `kind_label` — what sort of evidence the comparison is; the label is shown verbatim:
    `ab_live` "A/B live" (both arms computed on the same live data).
  - `n` — the sample count the verdict rests on, or `null` when there is none.
  - `verdict` — `{state, text}`, computed from the data. `state` is one of `info` (a number to read,
    no judgement), `insufficient` (text starts `not enough data (n=…)` — below the per-feature
    minimum sample size, constants `MIN_*` in `features.rs`), `off` (feature configured off).
  - `baseline` — `{text, values}`: the release proof (commit message) and its key numbers as a flat
    `name -> number` map.
  - `charts[]` — category-axis charts: `{title, y_unit, x: [label…], series: [{name, values:
    [number|null…]}]}`; every `values` has the length of `x`, `null` is a gap. May be empty.
  - `tables[]` — `{title, columns: [string…], rows: [[cell…]…]}`; a cell is a number, a string or
    `null`. May be empty.
  - `notes[]` — caveats (confounders, what an entry counts); show them.

  | `id` | status | kind | comparison (`charts` / `tables`) | `n` is |
  |---|---|---|---|---|
  | `comfort_targets` | live | `ab_live` | measured heat per pulled room per day since the release, split PV-covered / night tariff / day tariff (ledger); hourly pull-off counterfactual: mean extra planned cost and heat per plan, and the live plans' K·h below target. Verdict `off` when `comfort.pull_eur_per_kh` is 0 or every room has priority 0, "no heat demand yet" before heating starts. *(`api_version` 3: this replaces the `priority_zones` feature id; its stored samples start afresh.)* | solved A/B plans |

  *(Without an `api_version` bump — the dashboard tolerates their absence: the `terminal_soc`,
  `pv_nowcast`, `relay_duty` and `solar_scale` entries left `features[]` (the first three moved to
  `graduated[]`, the solar scale was removed), and `status: "staged"`, `kind: "ab_replay"` and the
  verdict states `helped` / `neutral` / `worse` no longer occur.)*

  The comparison stands on the **feature samples** (`MPC_FEATURE_SAMPLES_STORE`, 60 days, at most one
  row per hour), written only by the collector task, off the planning path: the hourly
  **comfort-targets counterfactual** ("pull off vs pull on") in `pull`. With a positive
  `comfort.pull_eur_per_kh × priority` anywhere, once an hour the collector re-solves the plan with
  `pull_eur_per_kh` 0 (ONE on-demand-path plan — never the loop's solver permit — over the loop's own
  slow-input cache and kernels) and records both arms' planned cost and per-zone planned heat. Both
  arms share the loop's slow-input cache, kernels, block-0 relay pin and controllable-load tally
  (published by the loop each tick); the counterfactual re-reads the thermal state, prices and
  weather a moment later. It records `skipped` instead of solving when there is no live plan / the
  live plan is degraded, no heat is planned (`identical`), or the on-demand solver is busy; a block
  rollover between the two arms discards the sample. The arms differ by the pull AND by the loop's
  block-0 relay pin and tick time — read the cost difference as indicative, the heat difference as
  the signal. A samples file written by an older release (with `terminal` / `nowcast` / `relay_duty`
  rows) still loads; those rows are dropped.

  The same collector task keeps the **accuracy history** (`MPC_ACCURACY_HISTORY_STORE`, 400 days), the
  long-lived record of the thermal model's forecast accuracy across releases (not shown on the page):
  keyed by the UTC day the snapshots were ANCHORED (the prediction was made), so a release splits it
  cleanly — every prediction of a day was made by one model. Per day, per zone, `scaled` = the plan's own
  prediction per lead bin 0-6 / 6-12 / 12-24 / 24-36 h: `[n, rmse_k, mean_bias_k]` (predicted −
  measured, hour-aligned points only). A day is appended once all its targets have elapsed and been
  measured (anchor day + 2 days + 13 h) AND its snapshots cover ≥ 20 distinct anchor hours — a
  partially covered day is skipped, never stored. One bounded 60 h measured read per day; on a cold
  start every qualifying day still in the snapshot store is backfilled, at most 2 per pass, and a
  recorded day is never rewritten. The cells are stored under the key `scaled` (unchanged, so an older
  binary still reads the file); the retired `unscaled` / `scaled_ab` / `night*` keys an older release
  wrote are dropped on the next write.

## Configuration

`config.json5` knobs that affect the API:
- `mpc_tick_minutes` — how often the loop re-plans (also sets the `/readyz` staleness threshold).
- `internal_gain_recalibrate_hours` / `internal_gain_window_days` — the live gain re-fit cadence/window.
- `forecast_snapshot_minutes` — how often the forward prediction is snapshotted (0 disables).

Environment:
- `MPC_BIND` — bind host (`0.0.0.0` in a container).
- `MPC_FORECAST_STORE` — path to the forecast-snapshot JSON file (default `forecast_snapshots.json` in the working directory). **Bind-mount this** to persist forward-validation history across container recreation.
- `MPC_LEDGER_STORE` — path to the decision-ledger JSON file (default `decision_ledger.json`). **Bind-mount this** too — it's the `/api/ledger` history, retained 30 days.
- `MPC_ACCURACY_HISTORY_STORE` — path to the daily forecast-accuracy history JSON (default `accuracy_history.json`, 400 days). **Bind-mount this** — it is the long-lived record of the model's accuracy across releases and cannot be rebuilt once the snapshot store has rolled over.
- `MPC_ROOM_MODE_STORE` — path to the per-room heating/cooling mode state (default `room_modes.json`). Bind-mount it so a restart does not flip modes.
- `MPC_FEATURE_SAMPLES_STORE` — path to the sampled A/B records JSON (default `feature_samples.json`, 60 days). Bind-mount it too (`/api/features`).

## Grafana

The server already runs Grafana (`loxone-db-grafana`). Because the brain is read-only it can't write
InfluxDB, so drive Grafana from these endpoints with the **Infinity** datasource
(`yesoreyeram-infinity-datasource`): type `JSON`, source `URL`, and a root selector of `data` (or
`data.timeline`) to step into the envelope. A starter dashboard is in
[`deploy/grafana/mpc-brain-dashboard.json`](../deploy/grafana/mpc-brain-dashboard.json) — import it
and point the Infinity datasource at `http://mpc-brain:3000` (on the `caddy_net` network) or the
published `127.0.0.1:3000`.
