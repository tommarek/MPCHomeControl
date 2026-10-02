# Monitoring & reporting API

The MPC brain (`cargo run -- serve`) exposes a **read-only** JSON API on `:3000`
(`MPC_BIND=0.0.0.0` to expose from a container). It never writes InfluxDB (only its own
forecast-snapshot file) and never actuates (the controllers actuate separately).

`GET /` serves the **dashboard** — a self-contained multi-screen web app (Home + Energy, Heating,
House, Model, System), embedded in the binary (ECharts vendored, works offline), driven entirely
by the endpoints below. `GET /api` returns a machine-readable index of every endpoint.

## Response envelope

Every **data** endpoint wraps its payload so a dashboard can show freshness:

```json
{ "computed_at": "2026-06-23T11:30:00+00:00", "age_seconds": 12, "data": { … } }
```

- `computed_at` — when the payload was computed (cached results report the original time).
- `age_seconds` — how long ago that was (0 for a fresh computation).
- `data` — the payload documented below.

Heavier endpoints (DB + estimator/optimizer) are cached for 60 s and bounded by a 45 s timeout
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
| `GET /api/version` | `{git_sha, built_at, config_fingerprint, model_fingerprint, estimator}` — what's deployed. `estimator: {configured, active, build_failed, building}` says which state estimator is *actually* running: a background Kalman build that fails leaves the brain on the anchor estimator indefinitely, and this is the only place a monitor can see that. |

### Model & envelope

- **`GET /api/model/topology`** — the building's **thermal envelope**, static (built from the model at startup, served with no DB): `{ zones: [{ name, volume_m3 (null for the outside/ground reservoirs), role: interior|outside|ground }], boundaries: [{ id, zone_a, zone_b, area_m2, azimuth_deg, tilt_deg, kind: interior|exterior|roof|ground, type_name, u_value (W/m²K), r_value (m²K/W), ua (W/K), solar_absorptance, layers: [{ material, thickness_mm, conductivity (W/mK), marker }] | null, initial_marker }], ground_temperature_c }`. Drives the **House** screen. `u_value` is the conventional ISO 6946 value (interior/exterior surface films included); `layers` are in the model's `zones[0]`→`zones[1]` order (exterior-first for walls, room-first for floors/roofs — the dashboard orients them for display).
- **`GET /api/model/solar?sky=clear|now`** — live per-surface solar gain at request time: `{ sky, sun: { azimuth_deg, elevation_deg, up }, boundaries: [{ id, irradiance_wm2, solar_w, beam_w, diffuse_w, total_w, cos_incidence, mode }] }`. Surfaces facing `outside` are included with `mode ∈ absorbed | transmitted`: opaque `Layered` surfaces ABSORB (`solar_w = irradiance × absorptance × area`), `Simple` panes with `g > 0` TRANSMIT (`solar_w = irradiance × g × area` — the RC network's window path, typically the dominant gain). `irradiance_wm2`/`solar_w` are the beam + diffuse total (ground-reflected is always 0 today); `beam_w`/`diffuse_w` split that same total so a client can tell direct sun from diffuse sky light (e.g. a north-facing wall at mid-morning is diffuse-only: real physics, but "☀ on surfaces" read as direct sun before this split existed); `total_w` repeats `solar_w` under an explicit name. `cos_incidence` is the raw geometric cosine of the sun's incidence angle on the surface (negative when the sun is behind it, before the beam term's own clamp to 0). `sky` echoes which mode actually served the response: default/`clear` applies no cloud (reads the pure orientation effect — which faces are catching sun); `sky=now` scales the model by the current cloud fraction from the live weather forecast, falling back to `clear` (reported as such in `sky`) if that feed is unavailable.

### Live & state

- **`GET /api/live`** — measured **current** telemetry for the energy-flow view (cached 5 s, single-flight shared across pollers; best-effort per field, `null` if a feed is stale — Growatt >10 min, outside temperature >30 min): `{ at, solar_kw, grid_kw (+=import), house_kw, battery_kw (+=charge), soc_pct, soc_kwh, outside_temp_c }`.
- **`GET /api/history?hours=N`** — measured PV power and battery SoC over the recent part of the day, for the dashboard's history-vs-forecast overlay. 15-minute means of the live Growatt telemetry (`solar` bucket): `InputPower` → **kW**, `INVPowerToLocalLoad` → **kW** (measured house consumption), `SOC` → **kWh** (via the configured battery capacity). `hours` defaults to "since ~local midnight" (clamped 1–48); empty arrays when a series has no data. `{ pv_kw: [[iso, kW], …], house_kw: [[iso, kW], …], soc_kwh: [[iso, kWh], …] }`.
- **`GET /api/zones`** — per-zone comfort band + heater limit + internal gain, over the heated ∪
  HVAC-served zones: `[{ zone, t_min, t_max, t_min_now, t_max_now, overheat_c, t_max_boost_now,
  heated, hvac, windows, max_heat_kw, internal_gain_w }]`. `t_min_now`/`t_max_now` are the band **in
  force at `computed_at`** — the daily override windows (night setback etc.) resolved in site-local
  time with the optimizer's own rule — and are what a client should shade and judge comfort against;
  `t_min`/`t_max` are the static fallback. `overheat_c` is the configured overheat allowance
  (`heating.zones[z].overheat_c`, K — 0 when unset or on a non-underfloor-heated/HVAC-served zone)
  and `t_max_boost_now = t_max_now + overheat_c` the derived banking ceiling, so a client never has
  to re-implement schedule/overheat resolution itself. `max_heat_kw`/`internal_gain_w`/`windows` are
  null / empty for an HVAC-only zone (it has no heating config).
- **`GET /api/state`** — current per-zone air temperature: `{ zones: [{zone, temp_c}], disturbance_w?, solar_scale? }`. The model estimate — the Kalman-filtered state (`estimator.mode: kalman`) or the classic drive **re-anchored to each zone's latest measured reading** (`anchor`) — so it reflects disturbances the model can't see (e.g. windows left open overnight) rather than the free-running prediction. `disturbance_w` is the observer's per-zone constant flux (W), present only when `estimator.disturbance` is on. `solar_scale` is the observer's per-zone solar-gain scale (`s_z = 1 + δ_z`), present only when `estimator.solar_scale` is on and at least one zone has both a sensor and a solar path.
- **`GET /api/zones/series?hours=N`** — recent **measured** per-zone air-temperature series for the comfort-grid sparklines (default 24 h, clamped 1–48), 30-minute means: `[{ zone, series: [[iso, °C], …] }]`. Zones with no data are omitted.
- **`GET /api/plan`** — on-demand whole-house plan (recomputes). Aggregates (cost EUR/CZK, grid/heating/cooling/HVAC-heating/battery kWh, PV curtailed, calibration scale, `pv_nowcast`, `placeholder_inputs`), the immediate `first_step`, `next_step` (item G: block 1 of `timeline` with its start instant `t` — the same shape as one `timeline` row, `null` if the plan has fewer than 2 blocks — so a client can show "next block: …" without indexing `timeline` itself; item 3, rework cycle 2/3: from `t − 120s` onward `next_step` is the ENTIRE block — heat/cool/hvac
relays, battery `charge_kw`/`discharge_kw`/`slot`/`export_enabled`/`inverter_on`, controllable-load
relays, and (rework cycle 4, item 4) the per-charger EV `charge_kw` — FROZEN verbatim to whatever the
loop committed at the start of that window, and `frozen` is `true`; before `t − 120s`, `next_step` is
the tick's own fresh (unfrozen) block 1. The publisher applies exactly this snapshot as its next
command at `apply_at = t` ONLY while `frozen` is `true`, and, once promoted, that snapshot stays
authoritative for the CURRENT command too until the block ends (rework cycle 3, rule 3), regardless
of what a later tick's plan says — so what actually REACHES THE HARDWARE for a promoted block is
pinned exactly once. **This is narrower than "the loop's own internal LP re-solve always agrees with
what it promoted."** Of the fields above, only the underfloor-heating relay binaries are hard-pinned
INSIDE the LP itself (`committed_heat`/`heat_relay`, block 0 — see `unified.rs`); battery, EV, and
controllable-load decisions are not LP-level equality constraints, so a later tick's own fresh solve
for the already-promoted block may re-optimize them differently INTERNALLY (its own forecast/
reporting for that block, and the ordinary `timeline` row for it — never the frozen `next_step` /
what a controller actually applies, and never re-sent: items 2/3's same-block guard rejects a
diverged re-actuation outright). The gap is therefore display-only — the dashboard, decision log, or
`/api/plan/timeline`'s block 0 can disagree with what was truly actuated by up to one block's worth of
battery/EV/load numbers, self-correcting at the next tick's fresh measurement — never an
actuation-safety gap), and the per-block `timeline` (below). HVAC fields (`cooling_kwh`, `hvac_heating_kwh`, and the per-block `cool_kw`/`hvac_heat_kw` maps) are `0`/empty unless an `hvac` block is configured. Three honesty flags: `degraded` (safety-critical input fell back — the publisher refuses to actuate), `relaxed` (the strict fix-and-round pipeline itself failed or timed out and only the plain relaxed LP answered; possibly fractional relays — not actuated, not latched), and `rounded` (the NORMAL result of the strict fix-and-round pipeline — relaxed LP → deterministic rounding → fully-pinned re-solve; integral and actuated). Since item F removed branch-and-bound entirely, `rounded` is the ordinary case on every healthy tick, not a fallback signal. Curtailment-risk fields `p10_surplus_kwh` / `curtailment_risk_kwh` (kWh, from the Solcast p10 percentile) are `null` until the forecast writer stores the p10 curve. `disturbance_w` (empty unless `estimator.disturbance` is on) is the Kalman observer's per-zone constant flux (W, + heats) as folded into THIS plan's `internal_gain_w` for the whole horizon — the offset-free correction, distinct from `/api/state`'s independently-read current value (the two agree when both ran off the same tick, but are computed separately). `solar_scale` (empty unless `estimator.solar_scale` is on) is the Kalman observer's per-zone solar-gain scale (`s_z = 1 + δ_z`) as folded into THIS plan's window/opaque-surface solar gain over the whole horizon and outlook (`optimize::coordinator::ForecastContext.solar_scale`); the constant-flux `disturbance_w` fold above is unaffected. `terminal_heat_credit_eur_per_kwh` (EUR per kWh thermal, empty when no zone got a positive credit) is the terminal slab-heat credit ACTUALLY applied per zone this solve — the price of the future heating each zone's banked heat is estimated to displace, taken from the post-horizon weather outlook (persisted prices; the cheapest blocks within the first 24 h of the outlook, or up to the zone's first dip below its floor when that comes later) when one covered that zone, else the flat median-import-based value every zone used to share. See `docs/configuration.md`'s "terminal slab-heat credit" paragraph for the mechanism. `export_pv_gated_blocks` (count) is how many blocks over the horizon are PV-dark (forecast `pv_kw` at/below the PV-present threshold) AND not already bound to 0 by the export-off or placeholder-price gate — i.e. where `battery.export_needs_pv`'s gate is the sole reason `batt_to_grid` is forced to 0. This is NOT a count of blocks where an ungated plan would actually have chosen to export there (it may find export unprofitable for other reasons); `0` when the flag is off or every dark block was already gated some other way. `terminal_soc_value_eur_per_kwh` (EUR/kWh) is the value of the energy left in the battery at the horizon end AS ACTUALLY PASSED to the LP (after the `p10_precharge_guard` halving, if applied); `terminal_soc_value_source` is `"outlook"` (the post-horizon day-type-median curve, `app::terminal_soc_value_outlook` — the normal case) or `"horizon_median"` (the OLD in-horizon-median value, `app::terminal_soc_value` — a thin/cold-start price history that doesn't cover every post-horizon block, flagged in `placeholder_inputs` as "terminal SoC value (outlook uncovered …)"). The on-demand `GET /api/plan` path runs with no cache (no `price_history` — the same on-demand limitation the heat-credit outlook has), so it always reports `horizon_median` plus the placeholder note; `GET /api/plan/latest` (the MPC loop's actuated plan, which does carry the cache) is where `outlook` normally appears; `backtest-terminal 1 --live` builds a shared cache so its OLD/NEW comparison can show both. See `docs/configuration.md`'s "Terminal SoC value" paragraph for the mechanism. `warmth_reward_eur` (EUR, `0` when no zone has a configured `warmth_value_eur_per_kh`) is the total within-horizon priority-zone warmth reward this plan earned (Σ w·K·h above the floor, INCLUDING warmth the zone gets for free from sun or neighbours — so it is not the value of the heat bought for it) — NOT part of `total_cost_eur`/`total_cost_czk` (grid cash alone); `warmth_kh` (empty under the same condition) is the Kelvin·hours each priority zone’s air sat above its effective floor this horizon, capped at the ceiling; `warmth_break_even_eur_per_kwh` (empty under the same condition) is, per priority zone, the import price (EUR/kWh electricity) at which the LP is indifferent between spending one more kWh heating that zone for the reward and not — it SUMS every priority zone’s own `warmth_value_eur_per_kh` weighted by that zone’s thermally-coupled effect on the target (cross-zone gains, not just the target’s own self-kernel), so a tightly-coupled cluster’s break-even runs above any one zone’s own target price — see `docs/configuration.md`'s `warmth_value_eur_per_kh` section for the formula and the kernel-gain caveat. `pv_nowcast` (always present) is the intraday nowcast's outcome this cycle: `{applied, reason, ratio, window_minutes, applied_hours, blocks, source_age_s, measured_kwh, forecast_kwh, delta_kwh}` — `applied: false` with a `reason` (e.g. `"disabled"`, `"stale (age N s)"`, `"low forecast (... kW < ... kW)"`, `"curtailed window"`, `"forecast refreshed"`, `"partial coverage (n/m samples)"`) when it was skipped that cycle; see `docs/configuration.md`'s "PV intraday nowcast" paragraph.
- **`GET /api/plan/latest`** — the latest plan published by the MPC loop (no recompute; `503` while warming up). `data` is the same plan shape as `/api/plan` (the envelope's `computed_at` is when it was published).
- **`GET /api/plan/timeline`** — just the latest plan's per-block rows (the chart-ready shape). Each block carries `dt_minutes` (item F's multi-rate grid: 15 for a near-term fine block, 60 for an hourly one further out — see `docs/configuration.md`'s `horizon` section); `t` is the block's START instant, so a block's coverage is `[t, t + dt_minutes)`:

```json
[ { "t": "2026-06-23T11:30:00+00:00", "dt_minutes": 15, "import_price": 0.12, "export_price": 0.05,
    "pv_kw": 4.1, "soc_kwh": 6.2, "charge_kw": 0.0, "discharge_kw": 1.3,
    "grid_import_kw": 0.0, "grid_export_kw": 0.0, "curtail_kw": 0.0,
    "heat_kw": {"livingroom": 0.0}, "cool_kw": {}, "hvac_heat_kw": {}, "ev_charge_kw": {},
    "temp_c": {"livingroom": 21.4},
    "slot": "regular", "export_enabled": true, "inverter_on": true,
    "price_is_placeholder": false, "frozen": false } ]
```

`frozen` (rework cycle 2 item 3, always present) is `true` only on [`PlanReport::next_step`] once
the loop's pre-mark freeze window has pinned it (see below) — every ordinary `timeline` row, block 0
included, always reports `false`: it's the tick's own fresh LP output, never the frozen snapshot.

**`slot` is the battery action in `loxone_smart_home`'s own vocabulary** (`app::classify_mode`):
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

**`heat_kw` is not always a literal setpoint (item H).** Underfloor heating is a mechanical relay:
one on/off decision per whole block, never sub-block modulation. The solver only pins an actual
integral relay decision for the first **two** blocks (30 minutes — `HEAT_COOL_PIN_BLOCKS` in
`unified.rs`; the earlier "roughly the first 2 hours" wording was wrong and is withdrawn — see
`docs/configuration.md`'s note on why it's exactly blocks 0 and 1, the two blocks item G ever
actuates); every `heat_kw` entry beyond that is the relaxed LP's **average power over the block** (a
real quantity — it's what the terminal-value/cost accounting uses — but not a value any relay can
hold continuously).
It becomes real whole-block switching once the per-minute re-plan's own fix-and-round window reaches
that block, typically producing a different mix of on/off sub-blocks that average to roughly the same
energy, not a constant partial-power run. A client rendering the timeline should treat a relay zone's
(one present in some block's `heat_kw`) near-term entries as on/off and its far-horizon entries as an
*expected* duty — e.g. `heat_kw / max_heat_kw` (from `/api/zones`) as a fraction, or that fraction × 4
as "on-blocks per hour" — never as a literal kW draw; the dashboard's Heating screen does this (see
`src/dashboard/app.js`'s `relayDuty`/`isNearTermBlock`). `hvac_heat_kw`/`cool_kw` are a genuinely
continuous, reversible AC setpoint (no relay involved) and carry no such caveat.

### Capabilities & EV

- **`GET /api/capabilities`** — what this house has, for conditional UI: `{ has_hvac, has_ev, chargers: [name…] }`.
- **`GET /api/ev`** — per-charger live state + planned charge schedule (present only with EV configured): `[{ name, status, on_our_charger, controllable_now, charging_elsewhere, soc_pct, target_pct, target_capped, capacity_kwh, active_car, strategy, charger_power_kw, charged_kwh, deadline_source, deadline_hm, deadline_at, charge_kw:[…], solar_kw:[…], grid_kw:[…], batt_kw:[…] }]`. `status` ∈ `charging | connected | charging_away | away`; `deadline_source` ∈ `pref | learned | config` says which deadline won (`deadline_hm` is the resolved **site-local** time — see [ev.md](ev.md)); `target_capped` means the stored preference exceeded the car's own charge limit and was capped to it. `deadline_at` is that same deadline as an absolute RFC3339 instant (the next occurrence of `deadline_hm` in site-local time, omitted when there is no deadline): use it rather than re-resolving `deadline_hm`, which would land on the wrong moment for a client in another timezone.
- **`GET /api/ev/<name>/preference`** / **`POST /api/ev/<name>/preference`** / **`DELETE /api/ev/<name>/preference`** — read / merge / clear the live override (`strategy`, `max_rate_kw`, `target_pct`, `deadline`). The POST **merges per field** (any subset; omitted fields keep their stored values); DELETE reverts everything to config / the car. The **only** MPC write — to its own `MPC_EV_PREF_STORE` file, never InfluxDB/MQTT. `404` for an unknown charger; `400` for an invalid body (`target_pct` outside 0..100, non-finite `max_rate_kw`, unparseable `deadline`). With the `MPC_API_TOKEN` env var set on the server, both mutating verbs require a matching `X-MPC-Token` header (`401` otherwise; the dashboard prompts once and remembers it) — set it if untrusted devices share the LAN. In the Docker deploy, put it in `<deploy dir>/api.env` as a single `export MPC_API_TOKEN="..."` line, `chmod 600` (the container script refuses to source a group/world-readable one, and forwards the variable by NAME so the value never reaches the `docker run` command line). Without the file the routes stay **open to the LAN** and the script says so on startup. See [ev.md](ev.md).

### Accuracy & calibration

- **`GET /api/pv/backtest?days=N`** — PV forecast vs actual Growatt generation (default 7, 1–60), excluding curtailed hours, with each day's forecast source. Also `leads: [{lead_from_h, lead_to_h, all, solcast, other}]` — accuracy per lead-time bucket ([0,1),[1,2),[2,3),[3,6),[6,12),[12,24),[24,48) h) over every stored snapshot of the last 14 days, split by source class (each score `{n, rmse_kw, bias_kw, forecast_kwh, actual_kwh}`). Every bucket is **already daylight-only**: both this binning and the whole-day score skip hours where forecast AND measured are both below `DAYLIGHT_KW` (0.05 kW) — the near-term buckets were split finer (0–1/1–2/2–3/3–6 h, replacing the old single 0–6 h bucket) so they're directly comparable to the 6–12/12–24/24–48 h buckets despite differing hour-of-day composition (the broad buckets' early leads skew toward peak-sun hours). A snapshot's lead is computed to the SECOND (not truncated to whole minutes), and a negative lead — a remnant snapshot recorded a few seconds after the hour it's scoring just ended, before that hour's key even entered its curve — is excluded from every bucket rather than rounding into `[0,1)` against a spurious zero forecast. Also `nowcast` — the nowcast replay (the pv-nowcast accuracy proof; not a like-for-like of live — each reference hour is evaluated once, so the refresh hours never get a nowcast here (conservative), while live resumes ~30 min after a refresh on a shorter, noisier window the hourly replay does not exercise), using the config's `pv.nowcast` params: `{params: {efold_hours, max_hours, clamp, min_forecast_kw, window_minutes}, all: [{lead_from_h, lead_to_h, n, plain: {rmse_kw, bias_kw}, nowcast: {rmse_kw, bias_kw}}] (3 bins: [0,1),[1,2),[2,3) h), applied: [same shape, only reference hours that produced a ratio], n_ref, n_ref_applied, n_ref_gated, n_ref_curtailed, n_ref_refreshed, n_ref_missing (n_ref == the sum of those five), n_clamp_lo, n_clamp_hi, n_neutral_calibration_days, mean_scale}`. The reference-hour snapshot is picked at `hour_end(h) + 120s` (`SNAPSHOT_LANDING_GRACE_S`, parity with live: real snapshots land a few seconds into the next hour) with NO fallback to an older snapshot — a snapshot that exists but doesn't yet have hour `h`'s key counts as `n_ref_refreshed` (live's own "forecast refreshed" skip), distinct from `n_ref_missing` (no snapshot recorded at all by then). Both arms (`plain`, `nowcast`) are scored on the identical sample set against a per-date trailing BAND calibration — the same shape `app::build_cache` fits for the live plan (a per-local-hour-band ratio, not a bare scalar), refit each date from the preceding ≤7 scored days and gated on the same `CALIBRATION_MIN_SCORED_HOURS` (`docs/configuration.md`'s "PV intraday nowcast" paragraph; `n_neutral_calibration_days` counts the dates that gate forced to neutral). A reference hour that's gated/curtailed/refreshed/missing still contributes its targets to `all` with `nowcast == plain`. `cargo run --release -- backtest-pv-nowcast <days> [--efold ...] [--max-hours ...] [--clamp-hi ...] [--min-forecast ...]` replays the same math as a parameter sweep over one InfluxDB read (`days` capped at 21), next to `backtest-terminal`. `cargo run --release -- backtest-kalman-solar <days> [--from <file>] [--dump <file>] [--sigma-dist <W>] [--json <out>]` is the real-data replay proof for the Kalman per-zone solar-gain scale (old/flag-off vs new/flag-on, scored per zone x lead bin plus the night-bias-after-a-sunny-afternoon metric) — see `src/solar_scale_backtest.rs`'s module doc and `docs/configuration.md`'s `estimator` section. `cargo run --release -- backtest-heating --start <rfc3339, hour-aligned> --days <1..=7> [--warmup-h 48] [--model <path>] [--config <path>] [--out <json>] [--dump <fixture>] [--from <fixture>] [--legacy-duty] [--on-duty 0.7] [--min-on-h 2] [--off-h 4] [--off-duty 0.1]` validates the condensed heating kernels (`optimize::thermal::build_kernels`) against measured winter data — see `src/heating_backtest.rs`'s module doc and the "Winter heating backtest" paragraph below. The live read (`validate::read_heating_kw`) is now event-based by default too (`heating.relay_duty: "events"`, see `docs/configuration.md`); `--legacy-duty` here replays the OLD zero-fill semantics on the same raw events, and `heating.relay_duty: "legacy"` is the equivalent live config revert. `cargo run --release -- audit-relay-duty [--days N<=7] [--json <out>]` runs the real `read_heating_kw` + live gain re-fit path twice on the same window (legacy vs events) and prints the kWh/gains/RMSE delta per zone — see `src/relay_duty.rs`'s module doc.

**Winter heating backtest.** Run this once a winter, before the heating season and again mid-season, on 2–3 bounded (≤7-day) windows chosen by eye from the daily-mean outside temperature (a cold spell, a mild week, and one in between) — through the SSH tunnel locally, e.g. `cargo run --release -- backtest-heating --start 2026-01-05T00:00:00Z --days 7 --out jan-cold.json`. Each run reads ONE bounded window (plus a 48 h warm-up by default) and is fully offline-replayable: pass `--dump jan-cold-raw.json` on the live run to save the raw series, then `--from jan-cold-raw.json` to re-score it later (e.g. with a candidate `--model`) with no InfluxDB token. Scheduled loads carry no measured `sensor_power_w` in this historical drive (only their configured/fitted magnitude) — a `sensor`-driven load's flux comes from the live draw only in the real MPC loop, not here. The report has four parts:
  - **Active backtest** — RMSE/bias/max-error per zone, split into all / heating-on / after-pulse (≤6 h after the last on-hour) / other hours, pre- (static config gains) and post- (per-window NNLS fit, which keeps the live loop's 3600 s end-of-hour drive — only the scoring and the kernel check use hour means) internal-gain calibration — plus each zone's true vs **legacy-duty** kWh delivered (the OLD `aggregateWindow(mean, createEmpty:false)` zero-fills hours between on-change relay events; this tool reconstructs the true time-weighted duty from the raw events instead, the same computation the live `read_heating_kw` now uses by default — `--legacy-duty` replays the old zero-fill semantics on the same events, driving the model AND the kernel/episode checks with it consistently, so the undercount is visible directly; both totals are always reported even without the flag).
  - **Kernel check** — `k_hat`/`s_hat` per heated zone from a whole-window least-squares fit of the measured-minus-modelled residual against the zone's own impulse-response (kernel, built to ≥ 2× the read window — see `kernel horizon` in the report header) convolution of its recorded heating, profiling response speed (`s_hat`) alongside strength (`k_hat`), both sides compared as HOURLY MEANS (matching how the measured series is actually read, not an end-of-hour point): `k_hat` near 1 means the model's K-per-kWh gain is right; `s_hat` near 1 means its speed is right (`s_hat > 1` = the real response is SLOWER than modelled). Also `k_hat_diurnal_ctrl` — an INDEPENDENT re-fit (re-profiling `s_hat` from scratch, not reusing the plain fit's) with one extra global sin/cos (local hour-of-day) nuisance term — and `diurnal_sensitive` (the two disagree by > 15 %): a model error shaped like the daily solar/occupancy cycle can otherwise masquerade as a confident but wrong gain/speed, so a proposal needs both to agree. Read the 95 % leave-one-day-out jackknife CI on `k_hat` before trusting it — `not identifiable` (`k_hat` absent) means too little heating in the window (< 5 kWh) or too collinear with the per-day trend terms (VIF > 10) to say anything; **the proposal bar additionally requires `identified: true`** — `k_hat`/`s_hat` are still reported even when `false` (nothing is hidden) but are flagged untrustworthy when `k_hat ≤ 0` (physically impossible) or `s_hat` lands on a grid edge (0.5 or 2.0 — the true speed may lie outside the searched range).
  - **Episode table + lag** — for ON streaks (`--on-duty`/`--min-on-h`/`--off-h`/`--off-duty` tune the detector; detected only in the scored window, never the warm-up) the measured response (raw, model-drift-corrected, and pre-trend-corrected — the pre-trend slope is fitted on the measured temperature itself) vs the modelled one at 1/3/6/12 h leads, and the event-averaged response curve's time-to-peak / time-to-63% (censored, not reported, if the curve is still rising at the lag horizon).
  - **Catch-up** — minutes from floor−1 K to floor at `max_heat_kw`, linearly interpolated between fine steps (no 15-min ceiling): modelled (the kernel's own step response, searched over its full built horizon — see the report's `kernel horizon` line), measured-implied (`k_hat`/`s_hat`-rescaled), and direct (the median across qualifying episodes whose first 3 ON hours ran at duty ≥ 0.9, i.e. close enough to a real step; `None` unless at least 2/3 of those reached +1 K — a median over "reached" alone is a lower bound). A value that never reaches +1 K within the kernel's horizon prints as `> <kernel horizon> h`, not a bare "None".

  The relay logs **on change only** (a handful of points/room/day) — a window with sparse logging near its edges can leave a zone's `relay_state_before` unknown (flagged in the report; treated as OFF) or starve the episode detector of qualifying streaks; a short/mild window may report `not identifiable` for low-heating zones. `--model`/`--config` point at a candidate pair (never the live `model.json5`/`config.json5`) to score a proposed change on the same window before/after.

**Warmth backtest.** `cargo run --release -- backtest-warmth --start <rfc3339, hour-aligned> --days <1..=7> [--step-hours 1] [--plant-gain 1.0] [--config <path>] [--dump <fixture>] [--from <fixture>] [--out <json>]` is the priority-zones (`heating.zones.*.warmth_value_eur_per_kh`, see `docs/configuration.md`) real-data proof: a rolling-horizon replay of the PRODUCTION planning pipeline (the SAME `SolveJob` + `app::fix_and_round` pipeline a live tick runs, with the thermal model as the plant) run TWICE from the same seed — OLD (every zone's `warmth_value_eur_per_kh` zeroed) vs NEW (the config as written) — so any cost/temperature/energy delta is attributable to the reward alone. Each tick re-plans both arms from measured PV/load/weather (perfect foresight — the item under test is the reward, not the forecast) and steps each arm's OWN plant state forward with the discretized model under that arm's own plan, injecting heat at each zone's `"heating"` marker exactly as the kernel build does; `--plant-gain 0.67` derates the executed heat pulse to approximate the ~1.5× cold-weather kernel over-response (`docs/configuration.md`'s kernel-gain caveat) without waiting for that fit to be corrected. **Window rule**: OTE publishes day D's prices on D-1 by ~14:00 local, so the window's END must be at least 37 h before `now()` or the core window couldn't be executed at real (not estimated) prices — `run()` enforces this and prints the bound it checked, same rule as `backtest-terminal`. Reads are bounded (≤7-day chunks, one series at a time, paused between): OTE prices (28 d of history for the day-type-median estimator), Growatt PV/load/SoC, and `backtest-heating`'s own outside/weather/zone/relay window (48 h warm-up by default) — `--dump`/`--from` round-trip everything through one `warmth-backtest-v1` fixture, same pattern as `backtest-heating`. The printed table (and `--out` JSON) report, per arm: realized cost (EUR), end SoC, mean/time-limited/failed solve counts; per zone: heating kWh split by price band (`pv_surplus` = zero/dust grid import that block, `nt`/`vt` by the tariff's local-hour mask), Kelvin·hours above floor (capped at the ceiling) / below floor / above ceiling, and the end-of-window temperature, plus a per-tick `ledger` row `{t, arm, heat_kw, import_kwh, export_kwh, cost_eur, plant_temp_c}` (the plant's own per-zone air temperature at the end of that tick's executed step) — plus `delta_cost_eur` (NEW − OLD), `warmth_reward_eur` (NEW's total within-horizon-equivalent Σ w·K·h, informational), and the spec's own acceptance-bar term `sum_w_delta_kh_eur` = Σ w·(K·h above floor NEW − OLD), `w` from the NEW config: `acceptance_bar_met` is `delta_cost_eur <= sum_w_delta_kh_eur` (the INCREMENTAL reward the warmth values actually bought, not the OLD arm's own unrelated K·h). `backtest-warmth --live [--config <path>]` instead compares OLD vs NEW on the CURRENT on-demand plan (`app::current_plan`), mirroring `backtest-terminal --live`, printing each priority zone's `warmth_kh` and `warmth_break_even_eur_per_kwh` (`/api/plan`'s own fields), heating kWh split by price band, and the min/max temperature reached over the horizon, alongside the cost delta — the spec's proof (a). Known simplifications (printed in the header, never silent): internal gains come from the CONFIG baseline only (no live Kalman gain/disturbance/solar-scale fit); no EV scheduling.
- **`GET /api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=`** — thermal model accuracy per zone (RMSE / bias / max error). The scored range is always `-(warmup+window)h .. now()`; `window_hours` (1–720) and `warmup_hours` (0–720, sum capped at 720) are the only range knobs — explicit `start`/`stop` are rejected. `detail=1` (passive only) returns the hourly diagnostic instead of the bare score list: `{ scores, hours:[iso…], outside_c:[…], ghi_wm2:[…], cloud:[…], zones:[{ zone, predicted_c:[…], measured_c:[…|null] }] }`, all parallel to `hours` — for checking whether a zone's bias is solar-shaped, diurnal or flat.
  - `passive` (default): free-response drift (summer). `window_hours` default 24, `warmup_hours` default 48.
  - `active`: driven by recorded heating relays; **fits** internal gains and returns `{before, after, gains_w}` (before/after = per-zone scores without/with the fitted gains).
  - `x0=kalman` (**passive only**): measurement updates run only during the warm-up, then the window is scored as a pure open-loop prediction from the Kalman-filtered state — the held-out estimator comparison (with `estimator.solar_scale` on, that open-loop roll applies the per-zone solar scale learned during the warm-up, exactly as the plan's forecast does). `400` with `mode=active` (that fit seeds its own state, so `x0` has no seam to apply to and would be silently ignored), and `400` when no filter is available — distinguishing `estimator.mode: anchor` from "still building at startup" (the Riccati solve takes seconds, tens under static-musl), so a retry is worthwhile only in the latter case.
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
    `ledger.rs::relay_duty`) — never zero-filled. Conversely, `planned.heat_kw`/`ev_charge_kw`/
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

- **`GET /api/forecast/validation`** — "predict now, score later". The loop snapshots its forward temperature prediction periodically (`forecast_snapshot_minutes`); this scores the most recent snapshot with ≥3 h elapsed against the measured hourly temperatures: `{anchored_at, scored_until, zones: [{zone, n, rmse_k, mean_bias_k, points:[{t, predicted_c, measured_c}]}], mean_rmse_k, leads, snapshots_scored, zones_unavailable}`. `zones_unavailable` lists zones whose measurement READ failed (excluded from `zones`/`mean_rmse_k`) — distinct from a zone with no history yet. `leads` resolves accuracy by how far ahead the prediction was made — bins [0,3),(3,6),(6,12),(12,24),(24,36) h over ALL stored snapshots, each `{lead_from_h, lead_to_h, n, rmse_k, mean_bias_k, zones:[…]}` (bins with `n: 0` had no scoreable points; the store holds ~4 days).

## Configuration

`config.json5` knobs that affect the API:
- `mpc_tick_minutes` — how often the loop re-plans (also sets the `/readyz` staleness threshold).
- `internal_gain_recalibrate_hours` / `internal_gain_window_days` — the live gain re-fit cadence/window.
- `forecast_snapshot_minutes` — how often the forward prediction is snapshotted (0 disables).

Environment:
- `MPC_BIND` — bind host (`0.0.0.0` in a container).
- `MPC_FORECAST_STORE` — path to the forecast-snapshot JSON file (default `forecast_snapshots.json` in the working directory). **Bind-mount this** to persist forward-validation history across container recreation.
- `MPC_LEDGER_STORE` — path to the decision-ledger JSON file (default `decision_ledger.json`). **Bind-mount this** too — it's the `/api/ledger` history, retained 30 days.

## Grafana

The server already runs Grafana (`loxone-db-grafana`). Because the brain is read-only it can't write
InfluxDB, so drive Grafana from these endpoints with the **Infinity** datasource
(`yesoreyeram-infinity-datasource`): type `JSON`, source `URL`, and a root selector of `data` (or
`data.timeline`) to step into the envelope. A starter dashboard is in
[`deploy/grafana/mpc-brain-dashboard.json`](../deploy/grafana/mpc-brain-dashboard.json) — import it
and point the Infinity datasource at `http://mpc-brain:3000` (on the `caddy_net` network) or the
published `127.0.0.1:3000`.
