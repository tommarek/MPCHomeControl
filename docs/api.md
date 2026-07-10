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
| `GET /api/version` | `{git_sha, built_at, config_fingerprint, model_fingerprint}` — what's deployed. |

### Model & envelope

- **`GET /api/model/topology`** — the building's **thermal envelope**, static (built from the model at startup, served with no DB): `{ zones: [{ name, volume_m3 (null for the outside/ground reservoirs), role: interior|outside|ground }], boundaries: [{ id, zone_a, zone_b, area_m2, azimuth_deg, tilt_deg, kind: interior|exterior|roof|ground, type_name, u_value (W/m²K), r_value (m²K/W), ua (W/K), solar_absorptance, layers: [{ material, thickness_mm, conductivity (W/mK), marker }] | null, initial_marker }], ground_temperature_c }`. Drives the **House** screen. `u_value` is the conventional ISO 6946 value (interior/exterior surface films included); `layers` are in the model's `zones[0]`→`zones[1]` order (exterior-first for walls, room-first for floors/roofs — the dashboard orients them for display).
- **`GET /api/model/solar`** — live per-surface **clear-sky solar gain** at request time: `{ sun: { azimuth_deg, elevation_deg, up }, boundaries: [{ id, irradiance_wm2, solar_w }] }`. Only opaque (`Layered`) surfaces facing `outside` are included (matching the RC network's solar rule); `solar_w = irradiance × absorptance × area`. Cloud is **not** applied (clear-sky), so it reads the orientation effect — which faces are catching sun.

### Live & state

- **`GET /api/live`** — measured **current** telemetry for the energy-flow view (not cached; best-effort per field, `null` if a feed is stale): `{ at, solar_kw, grid_kw (+=import), house_kw, battery_kw (+=charge), soc_pct, soc_kwh, outside_temp_c }`.
- **`GET /api/history?hours=N`** — measured PV power and battery SoC over the recent part of the day, for the dashboard's history-vs-forecast overlay. 15-minute means of the live Growatt telemetry (`solar` bucket): `InputPower` → **kW**, `SOC` → **kWh** (via the configured battery capacity). `hours` defaults to "since ~local midnight" (clamped 1–48); empty arrays when a series has no data. `{ pv_kw: [[iso, kW], …], soc_kwh: [[iso, kWh], …] }`.
- **`GET /api/zones`** — per-zone comfort band + heater limit + internal gain (from `config.heating`): `[{ zone, t_min, t_max, max_heat_kw, internal_gain_w }]`.
- **`GET /api/state`** — current per-zone air temperature: `{ zones: [{zone, temp_c}], estimator_mode, kalman_diff_k?, disturbance_w? }`. The model estimate (a drive over recent history to recover the unobservable wall/slab masses) **re-anchored to each zone's latest measured reading**, so it reflects disturbances the model can't see (e.g. windows left open overnight) rather than the free-running prediction. With `estimator.mode: shadow|kalman`, `kalman_diff_k` is the per-zone Kalman-vs-anchor difference (K, the shadow-period validation signal) and `disturbance_w` the observer's per-zone constant flux (W) when `estimator.disturbance` is on.
- **`GET /api/zones/series?hours=N`** — recent **measured** per-zone air-temperature series for the comfort-grid sparklines (default 24 h, clamped 1–48), 30-minute means: `[{ zone, series: [[iso, °C], …] }]`. Zones with no data are omitted.
- **`GET /api/plan`** — on-demand whole-house plan (recomputes). Aggregates (cost EUR/CZK, grid/heating/cooling/HVAC-heating/battery kWh, PV curtailed, calibration scale, `placeholder_inputs`), the immediate `first_step`, and the per-block `timeline` (below). HVAC fields (`cooling_kwh`, `hvac_heating_kwh`, and the per-block `cool_kw`/`hvac_heat_kw` maps) are `0`/empty unless an `hvac` block is configured. Three honesty flags: `degraded` (safety-critical input fell back — the publisher refuses to actuate), `relaxed` (the fix-and-round fallback's re-solve failed; possibly fractional relays — not actuated, not latched), and `rounded` (the strict MILP stalled and this plan is the relaxed→round→re-solve result — integral and actuated normally; transparency only). Curtailment-risk fields `p10_surplus_kwh` / `curtailment_risk_kwh` (kWh, from the Solcast p10 percentile) are `null` until the forecast writer stores the p10 curve.
- **`GET /api/plan/latest`** — the latest plan published by the MPC loop (no recompute; `503` while warming up). `data` is the same plan shape as `/api/plan` (the envelope's `computed_at` is when it was published).
- **`GET /api/plan/timeline`** — just the latest plan's per-block rows (the chart-ready shape):

```json
[ { "t": "2026-06-23T11:30:00+00:00", "import_price": 0.12, "export_price": 0.05,
    "pv_kw": 4.1, "soc_kwh": 6.2, "charge_kw": 0.0, "discharge_kw": 1.3,
    "grid_import_kw": 0.0, "grid_export_kw": 0.0, "curtail_kw": 0.0,
    "heat_kw": {"livingroom": 0.0}, "cool_kw": {}, "hvac_heat_kw": {},
    "temp_c": {"livingroom": 21.4},
    "slot": "regular", "export_enabled": true, "inverter_on": true,
    "price_is_placeholder": false } ]
```

### Capabilities & EV

- **`GET /api/capabilities`** — what this house has, for conditional UI: `{ has_hvac, has_ev, chargers: [name…] }`.
- **`GET /api/ev`** — per-charger live state + planned charge schedule (present only with EV configured): `[{ name, status, on_our_charger, controllable_now, charging_elsewhere, soc_pct, target_pct, strategy, charger_power_kw, charged_kwh, deadline_source, deadline_hm, charge_kw:[…], solar_kw:[…], grid_kw:[…], batt_kw:[…] }]`. `status` ∈ `charging | connected | charging_away | away`; `deadline_source` ∈ `pref | learned | config` says which deadline won (`deadline_hm` is the resolved local time — see [ev.md](ev.md)).
- **`GET /api/ev/<name>/preference`** / **`POST /api/ev/<name>/preference`** / **`DELETE /api/ev/<name>/preference`** — read / merge / clear the live override (`strategy`, `max_rate_kw`, `target_pct`, `deadline`). The POST **merges per field** (any subset; omitted fields keep their stored values); DELETE reverts everything to config / the car. The **only** MPC write — to its own `MPC_EV_PREF_STORE` file, never InfluxDB/MQTT. `404` for an unknown charger. With the `MPC_API_TOKEN` env var set on the server, both mutating verbs require a matching `X-MPC-Token` header (`401` otherwise; the dashboard prompts once and remembers it) — set it if untrusted devices share the LAN. See [ev.md](ev.md).

### Accuracy & calibration

- **`GET /api/pv/backtest?days=N`** — PV forecast vs actual Growatt generation (default 7, 1–60), excluding curtailed hours, with each day's forecast source. Also `leads: [{lead_from_h, lead_to_h, all, solcast, other}]` — accuracy per lead-time bucket ([0,6),(6,12),(12,24),(24,48) h) over every stored snapshot of the last 14 days, split by source class (each score `{n, rmse_kw, bias_kw, forecast_kwh, actual_kwh}`).
- **`GET /api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=&start=&stop=`** — thermal model accuracy per zone (RMSE / bias / max error).
  - `passive` (default): free-response drift (summer). `window_hours` default 24, `warmup_hours` default 48.
  - `active`: driven by recorded heating relays; **fits** internal gains and returns `{before, after, gains_w}` (before/after = per-zone scores without/with the fitted gains). `start`/`stop` are Flux ranges (default `-{warmup+window}h` .. `now()`).
  - `x0=kalman` (passive only): measurement updates run only during the warm-up, then the window is scored as a pure open-loop prediction from the Kalman-filtered state — the held-out estimator comparison. `400` unless `estimator.mode` builds a filter at startup.
- **`GET /api/calibration/gains`** — the live internal-gain self-correction, plus each scheduled
  load's magnitude (`source` is `"measured"` when a `sensor` drives the flux from the real draw,
  `"configured"` when `power_w` is set, else `"fitted"`):

```json
{ "live": { "fitted_at": "…", "window_days": 7, "gains_w": { "livingroom": { "night": 40, "day": 60, "evening": 320 } },
            "scheduled": [{"label": "water heat-pump", "zone": "technical_room",
                           "magnitude_w": 1600, "source": "configured"}] },
  "config_baseline_w": { "livingroom": { "night": 351, "day": 351, "evening": 351 } },
  "recalibrate_hours": 24, "window_days": 7,
  "bias": { "updated_at": "…", "bias_w": { "livingroom": -85.0 },
            "raw_bias_k": { "livingroom": [0.35, 6] } } }
```

  `bias` is the fast offset-free feedback's honesty surface (`heating.bias_correction`, default
  off — `null` until enabled and first updated): the corrective per-zone flux the forward
  prediction currently carries, and the raw short-lead mean error (K) + point count it came from.

### Forward validation

- **`GET /api/forecast/validation`** — "predict now, score later". The loop snapshots its forward temperature prediction periodically (`forecast_snapshot_minutes`); this scores the most recent snapshot with ≥3 h elapsed against the measured hourly temperatures: `{anchored_at, scored_until, zones: [{zone, n, rmse_k, mean_bias_k, points:[{t, predicted_c, measured_c}]}], mean_rmse_k, leads, snapshots_scored}`. `leads` resolves accuracy by how far ahead the prediction was made — bins [0,3),(3,6),(6,12),(12,24),(24,36) h over ALL stored snapshots, each `{lead_from_h, lead_to_h, n, rmse_k, mean_bias_k, zones:[…]}` (bins with `n: 0` had no scoreable points; the store holds ~4 days).

## Configuration

`config.json5` knobs that affect the API:
- `mpc_tick_minutes` — how often the loop re-plans (also sets the `/readyz` staleness threshold).
- `internal_gain_recalibrate_hours` / `internal_gain_window_days` — the live gain re-fit cadence/window.
- `forecast_snapshot_minutes` — how often the forward prediction is snapshotted (0 disables).

Environment:
- `MPC_BIND` — bind host (`0.0.0.0` in a container).
- `MPC_FORECAST_STORE` — path to the forecast-snapshot JSON file (default `forecast_snapshots.json` in the working directory). **Bind-mount this** to persist forward-validation history across container recreation.

## Grafana

The server already runs Grafana (`loxone-db-grafana`). Because the brain is read-only it can't write
InfluxDB, so drive Grafana from these endpoints with the **Infinity** datasource
(`yesoreyeram-infinity-datasource`): type `JSON`, source `URL`, and a root selector of `data` (or
`data.timeline`) to step into the envelope. A starter dashboard is in
[`deploy/grafana/mpc-brain-dashboard.json`](../deploy/grafana/mpc-brain-dashboard.json) — import it
and point the Infinity datasource at `http://mpc-brain:3000` (on the `caddy_net` network) or the
published `127.0.0.1:3000`.
