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

## Roadmap (migrating the rest)

| Source | Today (loxone_smart_home) | Here |
|---|---|---|
| open-meteo weather | writes only "now" + "today" | ✅ `scrapers/openmeteo` (future-dated hourly) |
| Solcast PV forecast | `solar_forecast_history` `hourly_json` | future crate — must **replace** (not run beside) the loxone fetcher: the free tier is ~9 fetches/day, one owner only. Brings `hourly_json_p10`/`p90` with it (the brain's curtailment-risk metric and `p10_precharge_guard` are already wired and dormant). |
| OTE day-ahead prices | `ote_prices` | future crate |
| Growatt telemetry | MQTT → telegraf bridge | stays (not a scraper) |
