# Scrapers — modular data collection

`scrapers/` holds the workspace's data-collection crates. The long-term plan is to move **all** of
`loxone_smart_home`'s scrapers here, one crate per source, each writing to the **same InfluxDB
series the house has always used** — so the brain's readers (and any legacy writer still running)
never notice the handover. The read-only property of the root `mpc_home_control` crate is
untouched: scrapers are separate crates, like the controllers, and the root never depends on them.

## Design rules (every scraper)

- **One crate per source**, `scrapers/<source>/`, a small binary: fetch → pure record extraction →
  line protocol → one batched write. The pure parts are unit-tested; the IO layer stays thin.
- **Same series as production**: identical bucket / measurement / tag set / field names, so the new
  writer merges idempotently with the old one (same series + timestamp ⇒ last write per field) and
  cut-over is just "stop the old one".
- **Honest nulls**: a field the source returns as `null` is *skipped*, never 0-coerced — absent
  must read as "unknown" downstream, not as a real zero.
- **Token via env only** (`INFLUXDB_TOKEN`), sourced at run time from the loxone `.env`; never in a
  config file, never logged.
- **No arm gate**: scrapers only write forecast/telemetry data (idempotent, low-consequence), so
  they run without the controllers' two-key arming. They still deploy as bind-mount-config Docker
  containers via `deploy/controller.Dockerfile` + a `deploy/run-scraper-<source>.sh`.

## `scrapers/openmeteo`

Fetches the open-meteo hourly forecast (the full field list the loxone scraper requests, including
`shortwave/direct/diffuse_radiation`) and writes **every future hour as its own future-dated
point** — the piece the loxone scraper never did (it stores only "now"), and what the MPC's
radiation-driven solar model (`SolarInput`) and 36 h horizon consume.

- Series: bucket `weather_forecast`, measurement `weather_forecast`, tags
  `room=outside, type=hour, source=openmeteo` — exactly the house's existing series.
- Config `scraper.json5`: `influx {url, org, bucket}`, `site {latitude, longitude}`,
  `interval_minutes` (default 30), `horizon_hours` (default 72).

```bash
cargo run -p mpc-scraper-openmeteo -- scraper.json5 --once --dry-run   # print the lines, write nothing
cargo run -p mpc-scraper-openmeteo -- scraper.json5 --once             # one scrape + write, then exit
cargo run -p mpc-scraper-openmeteo -- scraper.json5                    # the service loop
```

Deploy: build static-musl like the controllers, image via
`docker build -f controller.Dockerfile --build-arg BIN=mpc-scraper-openmeteo -t mpc-scraper-openmeteo .`,
run with `deploy/run-scraper-openmeteo.sh` (needs `LOXONE_ENV` pointing at the `.env` with the
token). Verify with the flux query below — `direct_radiation`/`diffuse_radiation` should appear
future-dated within one interval:

```flux
from(bucket:"weather_forecast") |> range(start: now(), stop: 48h)
  |> filter(fn:(r)=> r._measurement=="weather_forecast" and r.type=="hour")
  |> keep(columns:["_field"]) |> distinct(column:"_field")
```

## `scrapers/solcast`

Fetches the Solcast rooftop forecast (30-min `pv_estimate`/`pv_estimate10`/`pv_estimate90`, kW,
period-ending) and writes **one snapshot point per covered `forecast_date`** into the exact series
the brain reads: measurement `solar_forecast_history`, tag `forecast_date=YYYY-MM-DD`, string
fields `hourly_json` (local **hour-ending** keys) + `hourly_json_p10`/`p90` (emitted only when
every contributing record has the percentile — no mixed curves) + `source="solcast"`, all on one
point timestamp (the brain joins p10 to p50 by `(forecast_date, _time)`). The p10 curve activates
the already-shipped curtailment-risk metric and `battery.p10_precharge_guard`.

- Config `scraper.json5`: `influx {url, org, bucket: "solar"}`, `solcast {site_ids, hours,
  fetch_hours_local}`, `timezone`. Secrets via env: `SOLCAST_API_KEY` + `INFLUXDB_TOKEN`.
- **Budget guard** (the free tier is ~9 API calls/day, ONE owner): a cron-like local-hour
  schedule (`fetch_hours_local` — restart-safe, unlike an interval loop); config load rejects
  `hours × sites > 9`; before each fetch the scraper checks its own newest `source="solcast"`
  snapshot and skips the slot if it is <45 min old; HTTP 429 skips the slot.
- `--once` fetches immediately (verification); `--dry-run` prints the line protocol.

### Cut-over checklist (replacing the loxone Solcast fetcher)

1. Deploy with a **reduced** `fetch_hours_local` while loxone's fetcher still runs — the combined
   daily calls must stay within the budget, so temporarily shrink one side.
2. Verify: `--once --dry-run` lines look right; then live, and flux-query
   `solar_forecast_history` for `source="solcast"` points at the new snapshot times; check
   `/api/pv/backtest` day rows report `source: "solcast"` and that `hourly_json_p10` presence
   turns `p10_surplus_kwh`/`curtailment_risk_kwh` non-null on the plan.
3. Disable the loxone Solcast API fetch (remove its key/flag) — this scraper is the sole budget
   owner from here; raise `fetch_hours_local` to the full schedule.
4. **Keep loxone's `model+api` fallback writer running indefinitely**: it is the gap-filler when
   Solcast is down or the budget is spent — the brain's snapshot pick automatically prefers
   `solcast`-sourced snapshots when present (`supersedes` in `src/solar_forecast.rs`).
5. Watch `/api/pv/backtest` — `incomplete_forecast_days` and the per-source `leads` bins — for a
   week.

## Roadmap (migrating the rest)

| Source | Today (loxone_smart_home) | Here |
|---|---|---|
| open-meteo weather | writes only "now" + "today" | ✅ `scrapers/openmeteo` (future-dated hourly) |
| Solcast PV forecast | `solar_forecast_history` `hourly_json` | ✅ `scrapers/solcast` (p50 + p10/p90; replaces the loxone fetcher per the cut-over checklist above; loxone's `model+api` fallback stays as the gap-filler) |
| OTE day-ahead prices | `ote_prices` | future crate |
| Growatt telemetry | MQTT → telegraf bridge | stays (not a scraper) |
