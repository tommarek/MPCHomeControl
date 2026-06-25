# Monitoring & reporting API

The shadow server (`cargo run -- serve`) exposes a **read-only** JSON API on `:3000`
(`MPC_BIND=0.0.0.0` to expose from a container). It never writes InfluxDB (only its own
forecast-snapshot file) and never actuates.

`GET /` serves the **dashboard** — a self-contained multi-screen web app (Home + Energy, Heating,
Model, System), embedded in the binary (ECharts vendored, works offline), driven entirely
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

### Live & state

- **`GET /api/live`** — measured **current** telemetry for the energy-flow view (not cached; best-effort per field, `null` if a feed is stale): `{ at, solar_kw, grid_kw (+=import), house_kw, battery_kw (+=charge), soc_pct, soc_kwh, outside_temp_c }`.
- **`GET /api/history?hours=N`** — measured PV power and battery SoC over the recent part of the day, for the dashboard's history-vs-forecast overlay. 15-minute means of the live Growatt telemetry (`solar` bucket): `InputPower` → **kW**, `SOC` → **kWh** (via the configured battery capacity). `hours` defaults to "since ~local midnight" (clamped 1–48); empty arrays when a series has no data. `{ pv_kw: [[iso, kW], …], soc_kwh: [[iso, kWh], …] }`.
- **`GET /api/zones`** — per-zone comfort band + heater limit + internal gain (from `config.heating`): `[{ zone, t_min, t_max, max_heat_kw, internal_gain_w }]`.
- **`GET /api/state`** — estimated current per-zone air temperature: `{ zones: [{zone, temp_c}] }`.
- **`GET /api/plan`** — on-demand whole-house plan (recomputes). Aggregates (cost EUR/CZK, grid/heating/cooling/HVAC-heating/battery kWh, PV curtailed, calibration scale, `placeholder_inputs`), the immediate `first_step`, and the per-block `timeline` (below). HVAC fields (`cooling_kwh`, `hvac_heating_kwh`, and the per-block `cool_kw`/`hvac_heat_kw` maps) are `0`/empty unless an `hvac` block is configured.
- **`GET /api/plan/latest`** — the latest plan published by the shadow loop (no recompute; `503` while warming up). `data` is the same plan shape as `/api/plan` (the envelope's `computed_at` is when it was published).
- **`GET /api/plan/timeline`** — just the latest plan's per-block rows (the chart-ready shape):

```json
[ { "t": "2026-06-23T11:30:00+00:00", "import_price": 0.12, "export_price": 0.05,
    "pv_kw": 4.1, "soc_kwh": 6.2, "charge_kw": 0.0, "discharge_kw": 1.3,
    "grid_import_kw": 0.0, "grid_export_kw": 0.0, "curtail_kw": 0.0,
    "heat_kw": {"livingroom": 0.0}, "cool_kw": {}, "hvac_heat_kw": {},
    "temp_c": {"livingroom": 21.4},
    "slot": "regular", "export_enabled": true, "inverter_on": true } ]
```

### Capabilities & EV

- **`GET /api/capabilities`** — what this house has, for conditional UI: `{ has_hvac, has_ev, chargers: [name…] }`.
- **`GET /api/ev`** — per-charger live state + planned charge schedule (present only with EV configured): `[{ name, status, on_our_charger, controllable_now, charging_elsewhere, soc_pct, target_pct, strategy, charger_power_kw, charged_kwh, charge_kw:[…], solar_kw:[…], grid_kw:[…], batt_kw:[…] }]`. `status` ∈ `charging | connected | charging_away | away`.
- **`GET /api/ev/<name>/preference`** / **`POST /api/ev/<name>/preference`** — read / set the live override (`strategy`, `max_rate_kw`, `target_pct`, `deadline`; any subset). The **only** MPC write — to its own `MPC_EV_PREF_STORE` file, never InfluxDB/MQTT. `404` for an unknown charger. See [ev.md](ev.md).

### Accuracy & calibration

- **`GET /api/pv/backtest?days=N`** — PV forecast vs actual Growatt generation (default 7, 1–60), excluding curtailed hours, with each day's forecast source.
- **`GET /api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=&start=&stop=`** — thermal model accuracy per zone (RMSE / bias / max error).
  - `passive` (default): free-response drift (summer). `window_hours` default 24, `warmup_hours` default 48.
  - `active`: driven by recorded heating relays; **fits** internal gains and returns `{before, after, gains_w}` (before/after = per-zone scores without/with the fitted gains). `start`/`stop` are Flux ranges (default `-{warmup+window}h` .. `now()`).
- **`GET /api/calibration/gains`** — the live internal-gain self-correction:

```json
{ "live": { "fitted_at": "…", "window_days": 7, "gains_w": {"livingroom": 83, …} },
  "config_baseline_w": {"livingroom": 351, …},
  "recalibrate_hours": 24, "window_days": 7 }
```

### Forward validation

- **`GET /api/forecast/validation`** — "predict now, score later". The loop snapshots its forward temperature prediction periodically (`forecast_snapshot_minutes`); this scores the most recent snapshot with ≥3 h elapsed against the measured hourly temperatures: `{anchored_at, scored_until, zones: [{zone, n, rmse_k, mean_bias_k, points:[{t, predicted_c, measured_c}]}], mean_rmse_k}`.

## Configuration

`config.json5` knobs that affect the API:
- `mpc_tick_minutes` — how often the loop re-plans (also sets the `/readyz` staleness threshold).
- `internal_gain_recalibrate_hours` / `internal_gain_window_days` — the live gain re-fit cadence/window.
- `forecast_snapshot_minutes` — how often the forward prediction is snapshotted (0 disables).

Environment:
- `MPC_BIND` — bind host (`0.0.0.0` in a container).
- `MPC_FORECAST_STORE` — path to the forecast-snapshot JSON file (default `forecast_snapshots.json` in the working directory). **Bind-mount this** to persist forward-validation history across container recreation.

## Grafana

The server already runs Grafana (`loxone-db-grafana`). Because the shadow is read-only it can't write
InfluxDB, so drive Grafana from these endpoints with the **Infinity** datasource
(`yesoreyeram-infinity-datasource`): type `JSON`, source `URL`, and a root selector of `data` (or
`data.timeline`) to step into the envelope. A starter dashboard is in
[`deploy/grafana/mpc-shadow-dashboard.json`](../deploy/grafana/mpc-shadow-dashboard.json) — import it
and point the Infinity datasource at `http://mpc-shadow:3000` (on the `caddy_net` network) or the
published `127.0.0.1:3000`.
