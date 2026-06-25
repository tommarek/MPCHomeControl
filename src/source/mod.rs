//! Pluggable, **read-only** data sources.
//!
//! The MPC core (optimizer, forecasts, thermal model, loop) consumes typed inputs and never knows
//! where they come from. This module is the seam: a [`SourceLocator`] addresses one signal in
//! whatever backend a house keeps it (InfluxDB, a read-only Postgres `SELECT`, an HTTP `GET`), and a
//! [`SourceClients`] registry dispatches the read to the right backend. It generalises the EV
//! `sources` map to the whole input layer — swapping a signal's backend becomes a config edit.
//!
//! **Read-only by construction.** Every variant is a query / GET: a backend client here can *read*
//! but never write or actuate. MQTT (the house's actuation transport) is deliberately absent — it
//! reaches the MPC only via a bridge sidecar that normalises it into one of these pull stores, so the
//! MPC binary keeps its structural no-MQTT guarantee.

use std::collections::HashMap;

use serde::Deserialize;

use crate::influxdb::{InfluxDB, InfluxQuery, PriceSample, TimeSample};

/// A read-only locator for one signal, in whatever backend holds it. The config form is a
/// `{ type: "influx" | "postgres" | "http", … }` tagged object (the EV `sources` shape, generalised).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceLocator {
    /// A field in an InfluxDB measurement (the default backend).
    Influx {
        /// Which InfluxDB instance (see `data_sources.influx`); `None` = the default `db` instance.
        #[serde(default)]
        connection: Option<String>,
        bucket: String,
        measurement: String,
        field: String,
        #[serde(default)]
        tags: HashMap<String, String>,
        /// Multiplier on the raw value (e.g. `0.001` for Watts → kW).
        #[serde(default = "unit_scale")]
        scale: f64,
    },
    /// A read-only `SELECT` against a Postgres database (e.g. `teslamate-db`). The query is run as-is
    /// (no bind parameters) and must return its rows newest-last with the value in the final column,
    /// cast to `float8`; the newest row's last column is taken. Put any time window in the query itself
    /// (e.g. `where date > now() - interval '6 hours'`) — that is its freshness guard.
    Postgres {
        /// Which configured Postgres connection to use (see [`SourceClients`]); `None` = the default.
        #[serde(default)]
        connection: Option<String>,
        query: String,
        #[serde(default = "unit_scale")]
        scale: f64,
    },
    /// A read-only HTTP `GET` returning JSON; `pointer` is an RFC-6901 JSON pointer to the value.
    Http {
        url: String,
        #[serde(default)]
        pointer: String,
        #[serde(default = "unit_scale")]
        scale: f64,
    },
}

fn unit_scale() -> f64 {
    1.0
}

impl SourceLocator {
    /// A short human label for logs / the dashboard ("influx loxone/ev/ev_charging_power").
    pub fn label(&self) -> String {
        match self {
            SourceLocator::Influx {
                bucket,
                measurement,
                field,
                ..
            } => format!("influx {bucket}/{measurement}/{field}"),
            SourceLocator::Postgres { connection, .. } => {
                format!("postgres {}", connection.as_deref().unwrap_or("default"))
            }
            // Drop any query string — a house may embed an API token in the URL, and the label is
            // logged on a read failure.
            SourceLocator::Http { url, .. } => {
                format!(
                    "http {}",
                    url.split_once('?').map_or(url.as_str(), |(base, _)| base)
                )
            }
        }
    }

    /// The `scale` multiplier this locator applies to the raw value (every variant carries one).
    fn scale(&self) -> f64 {
        match self {
            SourceLocator::Influx { scale, .. }
            | SourceLocator::Postgres { scale, .. }
            | SourceLocator::Http { scale, .. } => *scale,
        }
    }
}

/// Build the default Influx locator for a signal (the built-in fallback when a house maps nothing).
fn influx_default(
    bucket: &str,
    measurement: &str,
    field: &str,
    tags: &[(&str, &str)],
) -> SourceLocator {
    SourceLocator::Influx {
        connection: None,
        bucket: bucket.to_string(),
        measurement: measurement.to_string(),
        field: field.to_string(),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        scale: 1.0,
    }
}

/// Per-house overrides for where signals are read from — the config face of the pluggable data-source
/// layer. Each field maps a signal to a [`SourceLocator`] (any backend); an unmapped signal falls back
/// to its built-in InfluxDB default, so the current house needs no config (the default reproduces
/// today's read byte-for-byte). Resolvers return the override-or-default locator.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DataSources {
    /// Growatt telemetry metric (`InputPower`, `SOC`, …) → its locator. The most house-/inverter-
    /// specific group; the live energy-flow reads resolve their metrics through here.
    #[serde(default)]
    pub growatt: HashMap<String, SourceLocator>,
    /// The open-meteo outside-temperature forecast series.
    #[serde(default)]
    pub weather_temperature: Option<SourceLocator>,
    /// The open-meteo cloud-cover forecast series.
    #[serde(default)]
    pub weather_cloud: Option<SourceLocator>,
    /// The Growatt "grid export enabled" flag series (curtailment detection in the PV backtest).
    #[serde(default)]
    pub curtailment_export: Option<SourceLocator>,
    /// The battery-SoC series used (with the export flag) to detect curtailed PV hours.
    #[serde(default)]
    pub curtailment_soc: Option<SourceLocator>,
    /// The day-ahead electricity spot-price series (the OTE feed by default).
    #[serde(default)]
    pub prices: Option<SourceLocator>,
    /// The stored PV-forecast curve (Solcast/loxone). Only the **bucket** is honored — the
    /// measurement is chosen per call (history vs current) and the `hourly_json`/`source`/
    /// `forecast_date` fields are structural to the curve format.
    #[serde(default)]
    pub pv_forecast: Option<SourceLocator>,
    /// The per-zone heating-relay series. The bucket/measurement/tags are honored; the **field** is
    /// the zone's room name (substituted per zone), so the locator's `field` is ignored.
    #[serde(default)]
    pub heating_relay: Option<SourceLocator>,
    /// Extra InfluxDB instances a house keeps some signals in, beyond the default `db` block. A
    /// `{ type: "influx", connection: "<name>", … }` locator reads from the matching instance; the
    /// token is read from `token_env` (never stored in config), like the default. Empty ⇒ one Influx.
    #[serde(default)]
    pub influx: HashMap<String, InfluxInstance>,
}

/// Connection details for a named extra InfluxDB instance (the token stays in the environment).
#[derive(Debug, Clone, Deserialize)]
pub struct InfluxInstance {
    pub host: String,
    pub org: String,
    /// Env var holding this instance's token. When omitted, the per-instance `INFLUX_<NAME>_TOKEN`
    /// is tried first, then the shared `INFLUX_TOKEN` — so several instances don't collide on one
    /// variable (mirrors the Postgres `MPC_PG_<NAME>` convention). The token is never stored here.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl InfluxInstance {
    /// The env-var names to try for this instance's token, in precedence order.
    fn token_env_candidates(&self, name: &str) -> Vec<String> {
        match &self.token_env {
            Some(explicit) => vec![explicit.clone()],
            // Per-instance var first, then the two shared names the default instance also accepts
            // (`INFLUX_TOKEN` / `INFLUXDB_TOKEN`) — so a house that sets only `INFLUXDB_TOKEN` works for
            // a named instance too, not just the default.
            None => vec![
                format!("INFLUX_{}_TOKEN", name.to_uppercase()),
                "INFLUX_TOKEN".to_string(),
                "INFLUXDB_TOKEN".to_string(),
            ],
        }
    }

    /// This instance's host: the `INFLUX_<NAME>_HOST` env override if set, else the config `host`.
    /// Mirrors the primary instance's `INFLUX_HOST` override (and the per-instance token resolution),
    /// so a secondary instance can be repointed (e.g. at a staging DB) without editing config.
    fn resolved_host(&self, name: &str) -> String {
        std::env::var(format!("INFLUX_{}_HOST", name.to_uppercase()))
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| self.host.clone())
    }
}

impl DataSources {
    /// Validate every configured locator's `scale` (finite and > 0). A typo'd scale is a config
    /// error, so fail at load rather than silently degrading that one signal to `None` at read time
    /// (`scaled_finite`'s runtime guard). The write-side adapters validate their scales likewise.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Each instance name forms an env-var name (`INFLUX_<NAME>_TOKEN` / `INFLUX_<NAME>_HOST`), so
        // it must be a valid identifier — else the lookup silently fails and looks like a missing token.
        for name in self.influx.keys() {
            anyhow::ensure!(
                env_name_safe(name),
                "data_sources.influx: instance name {name:?} must be [A-Za-z0-9_] (it forms INFLUX_<NAME>_TOKEN)"
            );
        }
        let known: std::collections::HashSet<&str> =
            self.influx.keys().map(String::as_str).collect();
        let check = |name: &str, loc: &SourceLocator| -> anyhow::Result<()> {
            let s = loc.scale();
            anyhow::ensure!(
                s.is_finite() && s > 0.0,
                "data_sources {name:?}: scale must be finite and > 0 (got {s})"
            );
            match loc {
                // A locator naming an Influx `connection` must reference a configured
                // `data_sources.influx` instance. Catch a typo at load — otherwise the read degrades
                // to `None` and looks like missing data (the default `connection: None` is always ok).
                SourceLocator::Influx {
                    connection: Some(conn),
                    ..
                } => anyhow::ensure!(
                    known.contains(conn.as_str()),
                    "data_sources {name:?}: unknown influx instance {conn:?} (configured: {known:?})"
                ),
                SourceLocator::Postgres {
                    connection, query, ..
                } => {
                    // A Postgres `connection` forms the env-var `MPC_PG_<NAME>`; reject an invalid
                    // identifier so it doesn't silently fail to resolve the DSN.
                    if let Some(conn) = connection {
                        anyhow::ensure!(
                            env_name_safe(conn),
                            "data_sources {name:?}: postgres connection {conn:?} must be [A-Za-z0-9_] (it forms MPC_PG_<NAME>)"
                        );
                    }
                    // Enforce the read-only invariant *structurally*: the query must be a `SELECT`
                    // (or a `WITH … SELECT` CTE). The extended protocol `query()` runs a single
                    // statement, so this stops a typo'd or hostile config turning the read-only
                    // Postgres source into a write/DDL path.
                    let q = query.trim_start().to_ascii_lowercase();
                    anyhow::ensure!(
                        q.starts_with("select") || q.starts_with("with"),
                        "data_sources {name:?}: postgres query must be a read-only SELECT (or WITH … SELECT), got {query:?}"
                    );
                }
                SourceLocator::Http { url, pointer, .. } => {
                    // Validate the URL and JSON pointer at load — otherwise a bad scheme or a pointer
                    // typo (missing leading `/`) only surfaces after a wasted network round-trip, per
                    // cycle, looking identical to a genuinely-absent path.
                    validate_http_url(url)
                        .map_err(|e| anyhow::anyhow!("data_sources {name:?}: http url {url:?}: {e}"))?;
                    anyhow::ensure!(
                        pointer.is_empty() || pointer.starts_with('/'),
                        "data_sources {name:?}: malformed JSON pointer {pointer:?}: must be empty or start with '/'"
                    );
                }
                _ => {}
            }
            Ok(())
        };
        for (name, loc) in &self.growatt {
            check(name, loc)?;
        }
        for (name, loc) in [
            ("weather_temperature", &self.weather_temperature),
            ("weather_cloud", &self.weather_cloud),
            ("curtailment_export", &self.curtailment_export),
            ("curtailment_soc", &self.curtailment_soc),
            ("prices", &self.prices),
            ("pv_forecast", &self.pv_forecast),
            ("heating_relay", &self.heating_relay),
        ] {
            if let Some(loc) = loc {
                check(name, loc)?;
            }
        }
        Ok(())
    }

    /// A Growatt telemetry metric's locator: override, else the `solar`-bucket field in its native unit.
    pub fn growatt_locator(&self, metric: &str) -> SourceLocator {
        self.growatt
            .get(metric)
            .cloned()
            .unwrap_or_else(|| influx_default("solar", "solar", metric, &[]))
    }

    /// The outside-temperature forecast locator (default open-meteo `weather_forecast`, `room=outside`).
    pub fn weather_temperature_locator(&self) -> SourceLocator {
        self.weather_temperature.clone().unwrap_or_else(|| {
            influx_default(
                "weather_forecast",
                "weather_forecast",
                "temperature_2m",
                &[("room", "outside"), ("type", "hour")],
            )
        })
    }

    /// The cloud-cover forecast locator (default open-meteo `weather_forecast`, `room=outside`).
    pub fn weather_cloud_locator(&self) -> SourceLocator {
        self.weather_cloud.clone().unwrap_or_else(|| {
            influx_default(
                "weather_forecast",
                "weather_forecast",
                "cloudcover",
                &[("room", "outside"), ("type", "hour")],
            )
        })
    }

    /// The "export enabled" flag locator (default `solar`-bucket `export_enabled`).
    pub fn curtailment_export_locator(&self) -> SourceLocator {
        self.curtailment_export
            .clone()
            .unwrap_or_else(|| influx_default("solar", "solar", "export_enabled", &[]))
    }

    /// The battery-SoC locator for curtailment detection (default `solar/solar/SOC`, the grott
    /// telemetry field; a house whose SoC lives elsewhere remaps it via `data_sources`).
    pub fn curtailment_soc_locator(&self) -> SourceLocator {
        self.curtailment_soc
            .clone()
            .unwrap_or_else(|| influx_default("solar", "solar", "SOC", &[]))
    }

    /// The day-ahead price locator (default OTE `ote_prices`/`electricity_prices`/`price`, EUR/MWh).
    pub fn prices_locator(&self) -> SourceLocator {
        self.prices
            .clone()
            .unwrap_or_else(|| influx_default("ote_prices", "electricity_prices", "price", &[]))
    }

    /// The PV-forecast locator (default `solar`/`solar_forecast_history`/`hourly_json`).
    pub fn pv_forecast_locator(&self) -> SourceLocator {
        self.pv_forecast.clone().unwrap_or_else(|| {
            influx_default("solar", "solar_forecast_history", "hourly_json", &[])
        })
    }

    /// The heating-relay locator (default `loxone`/`relay`, `tag1=heating`; field set per zone).
    pub fn heating_relay_locator(&self) -> SourceLocator {
        self.heating_relay
            .clone()
            .unwrap_or_else(|| influx_default("loxone", "relay", "", &[("tag1", "heating")]))
    }
}

/// The live backend clients a house's signals resolve against. InfluxDB is the default backend (and
/// the MQTT-bridge target); the read-only Postgres / HTTP adapters need no per-house client here — a
/// Postgres locator resolves its DSN from the environment (`MPC_PG_<NAME>`, secrets stay out of the
/// config), and an HTTP locator carries its own URL. The registry also carries the per-house
/// [`DataSources`] signal map, so a reader asks `db` for a signal without knowing its backend.
pub struct SourceClients {
    influx: InfluxDB,
    /// Named extra InfluxDB instances (`data_sources.influx`); a locator's `connection` selects one.
    influx_extra: HashMap<String, InfluxDB>,
    signals: DataSources,
}

impl SourceClients {
    /// Wrap the InfluxDB backend with a house's configured signal map (`config.data_sources`). Pass
    /// `DataSources::default()` for the built-in defaults (every signal at its default Influx location).
    /// Any extra InfluxDB instances in the signal map are connected here (token from each `token_env`).
    pub fn with_signals(influx: InfluxDB, signals: DataSources) -> Self {
        let influx_extra = signals
            .influx
            .iter()
            .filter_map(|(name, inst)| {
                // Resolve the token from the first set candidate var (explicit `token_env`, else
                // `INFLUX_<NAME>_TOKEN`, else the shared `INFLUX_TOKEN`).
                let candidates = inst.token_env_candidates(name);
                let token = candidates
                    .iter()
                    .find_map(|k| std::env::var(k).ok().filter(|t| !t.is_empty()))
                    .or_else(|| {
                        eprintln!(
                            "[source] influx instance {name:?}: none of {candidates:?} set — skipped"
                        );
                        None
                    })?;
                match InfluxDB::from_parts(&inst.resolved_host(name), &inst.org, &token) {
                    Ok(db) => Some((name.clone(), db)),
                    Err(e) => {
                        eprintln!("[source] influx instance {name:?}: {e}");
                        None
                    }
                }
            })
            .collect();
        Self {
            influx,
            influx_extra,
            signals,
        }
    }

    /// The InfluxDB client for a locator's `connection`: a named extra instance, or the default. A
    /// named-but-unconfigured instance ⇒ `None` (the read degrades to `None`, never silently the wrong DB).
    fn influx_for(&self, connection: &Option<String>) -> Option<&InfluxDB> {
        match connection {
            None => Some(&self.influx),
            Some(name) => self.influx_extra.get(name),
        }
    }

    // --- Configured-signal convenience reads (resolve the locator, then read) ------------------

    /// Latest value of a Growatt telemetry metric (native unit), fresh within `max_age_min`.
    pub async fn growatt_latest(&self, metric: &str, max_age_min: i64) -> Option<f64> {
        self.read_locator(&self.signals.growatt_locator(metric), max_age_min)
            .await
    }

    /// The outside-temperature forecast series.
    pub async fn weather_temperature_series(
        &self,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.read_locator_series(
            &self.signals.weather_temperature_locator(),
            start,
            stop,
            every,
        )
        .await
    }

    /// The cloud-cover forecast series.
    pub async fn weather_cloud_series(
        &self,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.read_locator_series(&self.signals.weather_cloud_locator(), start, stop, every)
            .await
    }

    /// The "export enabled" flag series (curtailment detection).
    pub async fn curtailment_export_series(
        &self,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.read_locator_series(
            &self.signals.curtailment_export_locator(),
            start,
            stop,
            every,
        )
        .await
    }

    /// The battery-SoC series (curtailment detection).
    pub async fn curtailment_soc_series(
        &self,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.read_locator_series(&self.signals.curtailment_soc_locator(), start, stop, every)
            .await
    }

    /// The InfluxDB bucket the PV-forecast curve is stored in (the configured `pv_forecast` locator's
    /// bucket, default `solar`). Only the bucket is configurable for this group (see [`DataSources`]).
    pub fn pv_forecast_bucket(&self) -> String {
        match self.signals.pv_forecast_locator() {
            SourceLocator::Influx { bucket, .. } => bucket,
            _ => "solar".to_string(),
        }
    }

    /// The hourly heating-relay series for one `room` (the per-zone field), from the configured
    /// `heating_relay` locator's bucket/measurement/tags (default `loxone`/`relay`, `tag1=heating`).
    pub async fn heating_relay_series(
        &self,
        room: &str,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        match self.signals.heating_relay_locator() {
            SourceLocator::Influx {
                connection,
                bucket,
                measurement,
                tags,
                scale,
                ..
            } => {
                let influx = self.influx_for(&connection).ok_or_else(|| {
                    anyhow::anyhow!("unknown influx instance {connection:?} for heating relay")
                })?;
                let tag_pairs: Vec<(&str, &str)> =
                    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let mut series = influx
                    .read_series(&bucket, &measurement, room, &tag_pairs, start, stop, every)
                    .await?;
                if (scale - 1.0).abs() > f64::EPSILON {
                    for s in &mut series {
                        s.value *= scale;
                    }
                }
                // Drop non-finite samples (a "NaN"/"Infinity" Influx string, or a scale overflow), like
                // the scalar reads' `scaled_finite` guard.
                series.retain(|s| s.value.is_finite());
                Ok(series)
            }
            other => anyhow::bail!(
                "heating-relay series is Influx-only (got {})",
                other.label()
            ),
        }
    }

    // --- Delegated InfluxDB read API ---------------------------------------------------------
    // The zone-mapping reads (already config-driven) and the raw-row reads stay Influx-native;
    // re-exposing them here lets every reader take `&SourceClients` in place of `&InfluxDB`.

    pub async fn read_rows(
        &self,
        query: &InfluxQuery,
    ) -> anyhow::Result<Vec<HashMap<String, String>>> {
        self.influx.read_rows(query).await
    }

    pub async fn read_zone(&self, zone: &str) -> anyhow::Result<HashMap<String, Vec<String>>> {
        self.influx.read_zone(zone).await
    }

    pub async fn read_zone_temperature_series(
        &self,
        zone: &str,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.influx
            .read_zone_temperature_series(zone, start, stop, every)
            .await
    }

    // A thin query primitive; the parameters are all distinct Influx selectors (bucket / measurement /
    // field / tags / start / stop / every), so a struct would only obscure the call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn read_series(
        &self,
        bucket: &str,
        measurement: &str,
        field: &str,
        tags: &[(&str, &str)],
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        self.influx
            .read_series(bucket, measurement, field, tags, start, stop, every)
            .await
    }

    pub fn zone_room(&self, zone: &str) -> Option<&str> {
        self.influx.zone_room(zone)
    }

    pub async fn read_prices(&self, start: &str) -> anyhow::Result<Vec<PriceSample>> {
        self.read_prices_range(start, "now()").await
    }

    /// Day-ahead spot prices from the configured `prices` locator (default OTE; Influx-only — the
    /// parse is price-specific). A house on a different market remaps `data_sources.prices`.
    pub async fn read_prices_range(
        &self,
        start: &str,
        stop: &str,
    ) -> anyhow::Result<Vec<PriceSample>> {
        match self.signals.prices_locator() {
            SourceLocator::Influx {
                connection,
                bucket,
                measurement,
                field,
                scale,
                ..
            } => {
                self.influx_for(&connection)
                    .ok_or_else(|| {
                        anyhow::anyhow!("unknown influx instance {connection:?} for prices")
                    })?
                    .read_prices_at(&bucket, &measurement, &field, scale, start, stop)
                    .await
            }
            other => anyhow::bail!("price reads are Influx-only (got {})", other.label()),
        }
    }

    // --- Backend-agnostic locator reads (the pluggable layer) ----------------------------------

    /// Latest value of a signal (scaled), if newer than `max_age_min` minutes. `None` on a stale /
    /// missing / unreachable source — every reader treats inputs as best-effort.
    pub async fn read_locator(&self, loc: &SourceLocator, max_age_min: i64) -> Option<f64> {
        match loc {
            SourceLocator::Influx {
                connection,
                bucket,
                measurement,
                field,
                tags,
                scale,
            } => {
                let Some(influx) = self.influx_for(connection) else {
                    // Log (like the Postgres/HTTP arms) instead of a silent `None`, so a typo'd
                    // `connection` surfaces as a config error rather than quietly-missing data.
                    eprintln!(
                        "[source] {} read failed: unknown influx instance {connection:?}",
                        loc.label()
                    );
                    return None;
                };
                // Bounding the range to `-{max_age}m` is the recency guard: `last()` returns a point
                // only if one exists inside the window.
                let start = format!("-{max_age_min}m");
                let q = InfluxQuery::new(bucket, &start, Some("now()"))
                    .filter("_measurement", measurement)
                    .filter("_field", field)
                    .filter_tags(tags)
                    .last();
                // Log a *query* failure (like the Postgres/HTTP arms); an empty result (no point in the
                // recency window) is the normal "no recent data" case and stays a silent `None`.
                let rows = match influx.read_rows(&q).await {
                    Ok(rows) => rows,
                    Err(e) => {
                        eprintln!("[source] {} read failed: {e}", loc.label());
                        return None;
                    }
                };
                let row = rows.into_iter().last()?;
                let v: f64 = row.get("_value")?.parse().ok()?;
                scaled_finite(v, *scale)
            }
            // Read-only `SELECT` against a Postgres DB. The DSN (with any secret) comes from the
            // environment, never the config. `max_age_min` is the query's responsibility (e.g. an
            // `ORDER BY time DESC LIMIT 1` with a time predicate); we take the newest row's value.
            SourceLocator::Postgres {
                connection,
                query,
                scale,
            } => {
                let dsn = postgres_dsn(connection)?;
                match read_postgres_latest(query, &dsn).await {
                    Ok(v) => scaled_finite(v, *scale),
                    Err(e) => {
                        eprintln!("[source] {} read failed: {e}", loc.label());
                        None
                    }
                }
            }
            SourceLocator::Http {
                url,
                pointer,
                scale,
            } => {
                let (url, pointer) = (url.clone(), pointer.clone());
                match tokio::task::spawn_blocking(move || read_http_value(&url, &pointer)).await {
                    Ok(Ok(v)) => scaled_finite(v, *scale),
                    Ok(Err(e)) => {
                        eprintln!("[source] {} read failed: {e}", loc.label());
                        None
                    }
                    Err(e) => {
                        eprintln!("[source] {} read task panicked: {e}", loc.label());
                        None
                    }
                }
            }
        }
    }

    /// A windowed mean series for a signal (scaled), aligned to `every`. Influx-only; propagates the
    /// read failure so the caller can fall back.
    pub async fn read_locator_series(
        &self,
        loc: &SourceLocator,
        start: &str,
        stop: &str,
        every: &str,
    ) -> anyhow::Result<Vec<TimeSample>> {
        match loc {
            SourceLocator::Influx {
                connection,
                bucket,
                measurement,
                field,
                tags,
                scale,
            } => {
                let influx = self.influx_for(connection).ok_or_else(|| {
                    anyhow::anyhow!("unknown influx instance {connection:?} for {}", loc.label())
                })?;
                let tag_pairs: Vec<(&str, &str)> =
                    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let mut series = influx
                    .read_series(bucket, measurement, field, &tag_pairs, start, stop, every)
                    .await?;
                if (*scale - 1.0).abs() > f64::EPSILON {
                    for s in &mut series {
                        s.value *= *scale;
                    }
                }
                // Drop non-finite samples (cf. `scaled_finite`).
                series.retain(|s| s.value.is_finite());
                Ok(series)
            }
            _ => anyhow::bail!("series reads are Influx-only (got {})", loc.label()),
        }
    }
}

/// Resolve a Postgres locator's connection to a DSN from the environment: `MPC_PG_<NAME>` for a named
/// connection, `MPC_PG_DEFAULT` otherwise. Keeping the DSN (and its password) in the environment
/// honours the "never store secrets in config" rule. `None` ⇒ unconfigured (the read is skipped).
fn postgres_dsn(connection: &Option<String>) -> Option<String> {
    let key = match connection {
        Some(name) => format!("MPC_PG_{}", name.to_uppercase()),
        None => "MPC_PG_DEFAULT".to_string(),
    };
    std::env::var(&key).ok().filter(|v| !v.is_empty())
}

/// Run a read-only query and return the newest row's last column as `f64`. The query should return
/// rows newest-last with the value in the final column, cast to `double precision` (`::float8`).
/// Bound the Postgres connect/query so an unreachable or stuck server can't hang the read for the
/// OS-level TCP timeout (minutes) and stall the planning cycle — mirroring the HTTP path's timeout.
const PG_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PG_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Bound on the graceful driver shutdown join, so a stuck socket can't hang the read after the query.
const PG_DRIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn read_postgres_latest(query: &str, dsn: &str) -> anyhow::Result<f64> {
    let (client, connection) = match tokio::time::timeout(
        PG_CONNECT_TIMEOUT,
        tokio_postgres::connect(dsn, tokio_postgres::NoTls),
    )
    .await
    {
        // Sanitize the connect error: the DSN carries the password, and a config-parse error could
        // echo part of it. Surface only a generic category so a secret never reaches the logs (the
        // call site logs this via `{e}`), matching how `label()` redacts an HTTP URL's query string.
        Ok(r) => r.map_err(|_| {
            anyhow::anyhow!(
                "postgres connect failed (check the host/port/credentials in the DSN env)"
            )
        })?,
        Err(_) => anyhow::bail!("postgres connect timed out after {PG_CONNECT_TIMEOUT:?}"),
    };
    // The connection future must be driven for the client to make progress.
    let driver = tokio::spawn(connection);
    let result = match tokio::time::timeout(PG_QUERY_TIMEOUT, client.query(query, &[])).await {
        Ok(r) => r.map_err(anyhow::Error::from),
        Err(_) => Err(anyhow::anyhow!(
            "postgres query timed out after {PG_QUERY_TIMEOUT:?}"
        )),
    };
    // Close gracefully: dropping the client ends the connection future, so the driver task finishes on
    // its own (a clean TCP shutdown) rather than being aborted mid-flight and leaking the socket.
    drop(client);
    // Bound the join too, so a stuck socket can't hang the read on the OS TCP close. On timeout we
    // `abort()` the task (dropping the join handle alone would only detach it, leaving it to spin on
    // the OS TCP close and accrue across ticks). (A clean shutdown is the triply-`Ok` arm.)
    let driver_abort = driver.abort_handle();
    match tokio::time::timeout(PG_DRIVER_TIMEOUT, driver).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => eprintln!("[source] postgres connection closed with error: {e}"),
        Ok(Err(e)) => eprintln!("[source] postgres driver panicked/cancelled: {e}"),
        Err(_) => {
            driver_abort.abort();
            eprintln!("[source] postgres driver shutdown timed out — aborted");
        }
    }
    let rows = result?;
    let row = rows
        .last()
        .ok_or_else(|| anyhow::anyhow!("query returned no rows"))?;
    let idx = row
        .len()
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("query returned no columns"))?;
    pg_column_f64(row, idx)
}

/// Coerce a Postgres column to `f64`, trying the common numeric/boolean types in turn.
fn pg_column_f64(row: &tokio_postgres::Row, idx: usize) -> anyhow::Result<f64> {
    if let Ok(v) = row.try_get::<_, f64>(idx) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<_, f32>(idx) {
        return Ok(v as f64);
    }
    if let Ok(v) = row.try_get::<_, i64>(idx) {
        return Ok(v as f64);
    }
    if let Ok(v) = row.try_get::<_, i32>(idx) {
        return Ok(v as f64);
    }
    if let Ok(v) = row.try_get::<_, bool>(idx) {
        return Ok(if v { 1.0 } else { 0.0 });
    }
    anyhow::bail!("value column is not a supported numeric type — cast it to float8 in the query")
}

/// Blocking HTTP `GET` of a JSON document; `pointer` is an RFC-6901 JSON pointer (empty = the whole
/// body, for a bare-scalar response). Booleans map to 1.0 / 0.0.
fn read_http_value(url: &str, pointer: &str) -> anyhow::Result<f64> {
    // Sanitize the request error: `ureq`'s transport-error Display embeds the full URL, which may carry
    // a token in its query string. Surface only the status / transport-kind so the secret never reaches
    // the logs (the caller logs `{e}`), matching the Postgres connect-error redaction and `label()`.
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("HTTP status {code}"),
            ureq::Error::Transport(t) => anyhow::anyhow!("transport error ({})", t.kind()),
        })?;
    let body = resp.into_string()?;
    let doc: serde_json::Value = serde_json::from_str(&body)?;
    let target = if pointer.is_empty() {
        &doc
    } else {
        // A valid RFC-6901 pointer is empty or starts with `/`. Catch a malformed one (e.g. a config
        // typo `battery/level` missing the leading slash) up front with a distinct message — otherwise
        // `pointer()` returns `None` for both a config typo and a genuinely-absent path, which look
        // identical and send an operator chasing an API change that didn't happen.
        if !pointer.starts_with('/') {
            anyhow::bail!("malformed JSON pointer {pointer:?}: must be empty or start with '/'");
        }
        doc.pointer(pointer).ok_or_else(|| {
            anyhow::anyhow!("JSON pointer {pointer:?} not found in the response (path absent)")
        })?
    };
    json_number(target)
        .ok_or_else(|| anyhow::anyhow!("value at {pointer:?} is not a number/bool/numeric-string"))
}

/// Apply a locator's `scale` and keep the result only if it is finite — guards against a misconfigured
/// (or absurdly large) scale overflowing a finite reading to `inf`, which must degrade to `None` rather
/// than propagate as a bogus value.
fn scaled_finite(v: f64, scale: f64) -> Option<f64> {
    let out = v * scale;
    out.is_finite().then_some(out)
}

/// Is `s` usable as the `<NAME>` segment of an env var (`MPC_PG_<NAME>`, `INFLUX_<NAME>_TOKEN`)? Env
/// vars are `[A-Za-z0-9_]`, so a connection / instance name with e.g. a hyphen must be rejected at
/// config load rather than silently failing the (case-folded) `std::env::var` lookup.
fn env_name_safe(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Validate an HTTP source `url` at config load: it must be an absolute `http(s)://` URL with a host
/// and no embedded `user:pass@` credentials. This catches a typo'd or non-HTTP scheme (e.g. a
/// `file://` SSRF/local-read target) before any request is made, and keeps plaintext credentials out
/// of config — secrets belong in the environment (a query-string token is tolerated but redacted from
/// logs by `read_http_value`). Pull-only `GET`; no scheme other than http/https is ever fetched.
fn validate_http_url(url: &str) -> anyhow::Result<()> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow::anyhow!("must start with http:// or https://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    anyhow::ensure!(!authority.is_empty(), "missing host");
    anyhow::ensure!(
        !authority.contains('@'),
        "must not embed credentials (user:pass@…); put secrets in the environment, not config"
    );
    Ok(())
}

/// Coerce a JSON scalar to `f64` (number, bool → 1/0, or a numeric string).
fn json_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_parses_each_backend_and_defaults_scale() {
        let influx: SourceLocator = json5::from_str(
            r#"{ type: "influx", bucket: "loxone", measurement: "ev", field: "ev_charging_power", scale: 0.001 }"#,
        )
        .unwrap();
        assert!(
            matches!(influx, SourceLocator::Influx { scale, .. } if (scale - 0.001).abs() < 1e-9)
        );

        let pg: SourceLocator = json5::from_str(
            r#"{ type: "postgres", query: "select date, val from positions order by date desc limit 1" }"#,
        )
        .unwrap();
        assert!(matches!(pg, SourceLocator::Postgres { scale, .. } if (scale - 1.0).abs() < 1e-9));

        let http: SourceLocator =
            json5::from_str(r#"{ type: "http", url: "https://api/x", pointer: "/soc" }"#).unwrap();
        assert_eq!(http.label(), "http https://api/x");
    }

    #[test]
    fn validate_enforces_read_only_postgres_query() {
        let with_pg = |q: &str| {
            let loc: SourceLocator =
                json5::from_str(&format!(r#"{{ type: "postgres", query: "{q}" }}"#)).unwrap();
            DataSources {
                curtailment_soc: Some(loc),
                ..Default::default()
            }
        };
        // SELECT (case/whitespace-insensitive) and a WITH … SELECT CTE are accepted.
        assert!(with_pg("select v from t order by ts desc limit 1")
            .validate()
            .is_ok());
        assert!(with_pg("  WITH x AS (SELECT 1) SELECT * FROM x")
            .validate()
            .is_ok());
        // A write / DDL is rejected at load — the read-only invariant is enforced, not just trusted.
        assert!(with_pg("delete from t").validate().is_err());
        assert!(with_pg("update t set v = 0").validate().is_err());
    }

    /// The no-regression guarantee for the migrated core groups: with no overrides, every default
    /// locator reproduces exactly the bucket/measurement/field/tags the old hardcoded reads used.
    #[test]
    fn default_signal_locators_match_the_legacy_reads() {
        let d = DataSources::default();
        let influx = |l: SourceLocator| match l {
            SourceLocator::Influx {
                bucket,
                measurement,
                field,
                tags,
                scale,
                ..
            } => (bucket, measurement, field, tags, scale),
            _ => panic!("default locators must be Influx"),
        };

        let (b, m, f, t, s) = influx(d.weather_temperature_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str(), s),
            (
                "weather_forecast",
                "weather_forecast",
                "temperature_2m",
                1.0
            )
        );
        assert_eq!(t.get("room").map(String::as_str), Some("outside"));
        assert_eq!(t.get("type").map(String::as_str), Some("hour"));

        let (b, m, f, _, _) = influx(d.weather_cloud_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str()),
            ("weather_forecast", "weather_forecast", "cloudcover")
        );

        let (b, m, f, t, _) = influx(d.curtailment_export_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str(), t.is_empty()),
            ("solar", "solar", "export_enabled", true)
        );

        let (b, m, f, _, _) = influx(d.curtailment_soc_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str()),
            ("solar", "solar", "SOC")
        );

        let (b, m, f, _, _) = influx(d.growatt_locator("InputPower"));
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str()),
            ("solar", "solar", "InputPower")
        );

        let (b, m, f, _, s) = influx(d.prices_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str(), s),
            ("ote_prices", "electricity_prices", "price", 1.0)
        );

        let (b, m, f, _, _) = influx(d.pv_forecast_locator());
        assert_eq!(
            (b.as_str(), m.as_str(), f.as_str()),
            ("solar", "solar_forecast_history", "hourly_json")
        );

        // The heating relay: bucket/measurement/tag are honored; the field (room) is set per zone.
        let (b, m, _, t, _) = influx(d.heating_relay_locator());
        assert_eq!((b.as_str(), m.as_str()), ("loxone", "relay"));
        assert_eq!(t.get("tag1").map(String::as_str), Some("heating"));
    }

    #[test]
    fn influx_locator_and_instances_parse() {
        // A locator can name an extra Influx instance; absent ⇒ the default.
        let named: SourceLocator = json5::from_str(
            r#"{ type: "influx", connection: "secondary", bucket: "b", measurement: "m", field: "f" }"#,
        )
        .unwrap();
        assert!(
            matches!(named, SourceLocator::Influx { connection: Some(c), .. } if c == "secondary")
        );
        let default: SourceLocator =
            json5::from_str(r#"{ type: "influx", bucket: "b", measurement: "m", field: "f" }"#)
                .unwrap();
        assert!(matches!(
            default,
            SourceLocator::Influx {
                connection: None,
                ..
            }
        ));

        // The data_sources block declares the extra instances (token from the env).
        let ds: DataSources = json5::from_str(
            r#"{ influx: { secondary: { host: "http://influx2:8086", org: "loxone", token_env: "INFLUX2_TOKEN" } } }"#,
        )
        .unwrap();
        let inst = ds.influx.get("secondary").unwrap();
        assert_eq!(
            (
                inst.host.as_str(),
                inst.org.as_str(),
                inst.token_env.as_deref()
            ),
            ("http://influx2:8086", "loxone", Some("INFLUX2_TOKEN"))
        );
        // With no explicit token_env, the per-instance var is tried before the shared default.
        let defaulted: DataSources = json5::from_str(
            r#"{ influx: { roof: { host: "http://influx3:8086", org: "loxone" } } }"#,
        )
        .unwrap();
        assert_eq!(
            defaulted.influx["roof"].token_env_candidates("roof"),
            vec![
                "INFLUX_ROOF_TOKEN".to_string(),
                "INFLUX_TOKEN".to_string(),
                "INFLUXDB_TOKEN".to_string()
            ]
        );
        // Default locators (and the legacy reads) carry no connection — the default instance.
        assert!(matches!(
            DataSources::default().growatt_locator("SOC"),
            SourceLocator::Influx {
                connection: None,
                ..
            }
        ));
    }

    #[test]
    fn scaled_finite_drops_overflow_and_nan() {
        assert_eq!(scaled_finite(7400.0, 0.001), Some(7.4));
        assert_eq!(scaled_finite(1e308, 10.0), None); // overflow → None, not a bogus inf
        assert_eq!(scaled_finite(f64::NAN, 1.0), None);
    }

    #[test]
    fn validate_rejects_non_finite_or_non_positive_scale() {
        let loc = |scale: &str| {
            format!(
                r#"{{ growatt: {{ InputPower: {{ type: "influx", bucket: "b", measurement: "m", field: "f", scale: {scale} }} }} }}"#
            )
        };
        let ok: DataSources = json5::from_str(&loc("0.001")).unwrap();
        assert!(ok.validate().is_ok());
        // `Infinity`/`NaN` are JSON5 literals that parse to a non-finite f64, so validation (not the
        // parser) must be what rejects them.
        for bad in ["0", "-1", "Infinity", "NaN"] {
            let ds: DataSources = json5::from_str(&loc(bad)).unwrap();
            assert!(ds.validate().is_err(), "scale {bad} should be rejected");
        }
    }

    #[test]
    fn validate_rejects_unknown_connection() {
        // A locator referencing a configured `influx` instance passes.
        let ok: DataSources = json5::from_str(
            r#"{ influx: { secondary: { host: "http://i2:8086", org: "o" } },
                 growatt: { SOC: { type: "influx", connection: "secondary", bucket: "b", measurement: "m", field: "f" } } }"#,
        )
        .unwrap();
        assert!(ok.validate().is_ok());
        // A typo'd / unconfigured connection fails at load rather than degrading to None at read time.
        let bad: DataSources = json5::from_str(
            r#"{ growatt: { SOC: { type: "influx", connection: "typo", bucket: "b", measurement: "m", field: "f" } } }"#,
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn validate_rejects_env_unsafe_names() {
        // An influx instance name with a hyphen forms an invalid `INFLUX_PROD-DB_TOKEN` env var.
        let bad_instance: DataSources =
            json5::from_str(r#"{ influx: { "prod-db": { host: "http://i:8086", org: "o" } } }"#)
                .unwrap();
        assert!(bad_instance.validate().is_err());
        // A postgres connection with a hyphen forms an invalid `MPC_PG_MY-DB` env var.
        let bad_pg: DataSources = json5::from_str(
            r#"{ growatt: { SOC: { type: "postgres", connection: "my-db", query: "select 1" } } }"#,
        )
        .unwrap();
        assert!(bad_pg.validate().is_err());
    }

    #[test]
    fn validate_checks_http_url_and_pointer() {
        // A well-formed http(s) URL with a valid (or empty) pointer passes.
        let ok: DataSources = json5::from_str(
            r#"{ growatt: { SOC: { type: "http", url: "https://api.local/state", pointer: "/battery/soc" } } }"#,
        )
        .unwrap();
        assert!(ok.validate().is_ok());
        // A non-HTTP scheme (file:// — an SSRF/local-read target) is rejected at load.
        let file_url: DataSources =
            json5::from_str(r#"{ growatt: { SOC: { type: "http", url: "file:///etc/passwd" } } }"#)
                .unwrap();
        assert!(file_url.validate().is_err());
        // Embedded credentials (secrets belong in the environment) are rejected.
        let creds: DataSources = json5::from_str(
            r#"{ growatt: { SOC: { type: "http", url: "http://user:pass@api.local/x" } } }"#,
        )
        .unwrap();
        assert!(creds.validate().is_err());
        // A pointer missing its leading slash is a config typo — caught at load, not after a request.
        let bad_ptr: DataSources = json5::from_str(
            r#"{ growatt: { SOC: { type: "http", url: "http://api.local/x", pointer: "battery/soc" } } }"#,
        )
        .unwrap();
        assert!(bad_ptr.validate().is_err());
    }

    #[test]
    fn http_label_redacts_query_string() {
        let l: SourceLocator =
            json5::from_str(r#"{ type: "http", url: "http://api.local/x?token=secret" }"#).unwrap();
        assert_eq!(l.label(), "http http://api.local/x");
    }

    #[test]
    fn json_number_coerces_scalars() {
        assert_eq!(json_number(&serde_json::json!(42.5)), Some(42.5));
        assert_eq!(json_number(&serde_json::json!(true)), Some(1.0));
        assert_eq!(json_number(&serde_json::json!("73.0")), Some(73.0));
        assert_eq!(json_number(&serde_json::json!({"a": 1})), None);
    }

    /// The HTTP adapter pulls a value out of a real JSON response via a pointer — a tiny one-shot
    /// server stands in for any house's HTTP API (e.g. a Tesla bridge), proving the backend is
    /// interchangeable with Influx behind the same `read_locator`.
    #[test]
    fn http_source_reads_a_json_pointer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let body = r#"{"battery":{"level":73.5}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let v = read_http_value(&format!("http://{addr}/status"), "/battery/level").unwrap();
        assert!((v - 73.5).abs() < 1e-9);
        // A pointer that doesn't resolve is an error (best-effort `None` at the caller).
        server.join().unwrap();
    }
}
