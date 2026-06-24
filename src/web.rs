//! Read-only monitoring + reporting HTTP API (axum).
//!
//! Exposes the running brain over JSON: liveness/readiness, version, the estimated thermal state,
//! the live dispatch plan (aggregates + a chart-ready per-block timeline), the PV / thermal
//! model-accuracy backtests, the internal-gain self-correction, a shadow-vs-loxone comparison, and
//! the forward-prediction validation scorecard. The network and state-space are plain `Send + Sync`
//! data, so they are shared across the multi-threaded server without copies. Strictly read-only —
//! it never writes InfluxDB (only its own forecast-snapshot file) and never actuates.
//!
//! Every data endpoint returns a uniform envelope `{computed_at, age_seconds, data}` so a dashboard
//! can show freshness. The heavier endpoints (DB queries + the estimator/optimizer) are cached for a
//! short TTL and bounded by a timeout, so rapid or concurrent polling reuses the cached result.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uom::si::f64::Angle;

use crate::app::{current_plan, current_state, GainsSnapshot, TimestampedPlan};
use crate::influxdb::InfluxDB;
use crate::optimize::config::ControlConfig;
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::validate::{backtest_passive, calibrate_internal_gains, BacktestConfig, ZoneBacktest};

/// How long a computed response stays fresh before it is recomputed.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// Hard ceiling on a single computation, so a slow/stuck DB can't pin a request open.
const COMPUTE_TIMEOUT: Duration = Duration::from_secs(45);

/// Everything the handlers need, shared (read-only) across requests.
pub struct AppState {
    pub net: RcNetwork,
    pub ss: StateSpace,
    pub config: ControlConfig,
    pub db: InfluxDB,
    pub latitude: Angle,
    pub longitude: Angle,
    /// When the process started (for uptime reporting).
    pub started_at: DateTime<Utc>,
    /// The latest plan published by the shadow MPC loop (`None` until the first tick completes).
    pub latest: Mutex<Option<TimestampedPlan>>,
    /// The latest internal-gain re-fit published by the loop (`None` until the first fit lands).
    pub gains: Mutex<Option<GainsSnapshot>>,
    /// Per-endpoint TTL cache of the last computed value, with the wall-clock instant it was made.
    cache: Mutex<HashMap<String, CacheEntry>>,
}

/// A cached response: the monotonic instant and wall-clock time it was computed, plus the value.
type CacheEntry = (Instant, DateTime<Utc>, Value);

impl AppState {
    pub fn new(
        net: RcNetwork,
        ss: StateSpace,
        config: ControlConfig,
        db: InfluxDB,
        latitude: Angle,
        longitude: Angle,
    ) -> Self {
        Self {
            net,
            ss,
            config,
            db,
            latitude,
            longitude,
            started_at: Utc::now(),
            latest: Mutex::new(None),
            gains: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

type Shared = Arc<AppState>;
type ApiError = (StatusCode, Json<Value>);

/// Map an internal error to a 500 JSON body.
fn fail(e: anyhow::Error) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
}

/// Wrap a data value in the uniform freshness envelope.
fn envelope(computed_at: DateTime<Utc>, age_seconds: u64, data: Value) -> Json<Value> {
    Json(json!({
        "computed_at": computed_at.to_rfc3339(),
        "age_seconds": age_seconds,
        "data": data,
    }))
}

/// Lock a shared mutex, recovering from poisoning — a panicked handler shouldn't break the rest.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `504` returned when a bounded computation exceeds its timeout.
fn timeout_error() -> ApiError {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(json!({ "error": "computation timed out" })),
    )
}

/// Return the cached value for `key` if still fresh, otherwise run `compute` (bounded by a timeout),
/// cache and return it — wrapped in the freshness envelope. The cache lock is never held across the
/// `await`.
async fn cached<T, F, Fut>(state: &Shared, key: String, compute: F) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
    T: Serialize,
{
    {
        let cache = lock(&state.cache);
        if let Some((at, computed_at, value)) = cache.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(envelope(
                    *computed_at,
                    at.elapsed().as_secs(),
                    value.clone(),
                ));
            }
        }
    }
    let computed = tokio::time::timeout(COMPUTE_TIMEOUT, compute())
        .await
        .map_err(|_| timeout_error())?
        .map_err(fail)?;
    let value = serde_json::to_value(&computed).map_err(|e| fail(anyhow::Error::new(e)))?;
    let now = Utc::now();
    {
        let mut cache = lock(&state.cache);
        // Drop expired entries so parameterized keys (e.g. arbitrary backtest windows) can't grow the
        // cache without bound — anything older than the TTL would be recomputed anyway.
        cache.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
        cache.insert(key, (Instant::now(), now, value.clone()));
    }
    Ok(envelope(now, 0, value))
}

/// A non-cryptographic fingerprint of a file's bytes, so the deployed config/model can be matched to
/// a known version. `"missing"` if the file can't be read.
fn file_fingerprint(path: &str) -> String {
    use std::hash::{Hash, Hasher};
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            format!("{:016x}", h.finish())
        }
        Err(_) => "missing".to_string(),
    }
}

/// Build version + identity: the git commit and build time stamped at compile, plus runtime
/// fingerprints of the config/model files actually loaded.
async fn version() -> Json<Value> {
    Json(json!({
        "git_sha": env!("MPC_GIT_SHA"),
        "built_at": env!("MPC_BUILT_AT"),
        "config_fingerprint": file_fingerprint("config.json5"),
        "model_fingerprint": file_fingerprint("model.json5"),
    }))
}

/// Liveness: the process is up. Always 200 (used by orchestrators to decide *restart*).
async fn livez(State(s): State<Shared>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "uptime_seconds": (Utc::now() - s.started_at).num_seconds().max(0),
    }))
}

/// Readiness: the shadow loop has published a recent plan (so the DB is reachable and planning
/// works). 503 if no plan yet or the last tick is too old (used to decide *route traffic*).
async fn readyz(State(s): State<Shared>) -> (StatusCode, Json<Value>) {
    let last = lock(&s.latest).clone();
    let age = last
        .as_ref()
        .map(|tp| (Utc::now() - tp.computed_at).num_seconds());
    // Allow a few missed ticks before declaring not-ready (≥10 min regardless of a long tick),
    // capped at a day. Saturating math so an absurd configured tick can't overflow into a negative.
    let max_age = s
        .config
        .mpc_tick_minutes
        .max(1)
        .saturating_mul(5)
        .max(10)
        .saturating_mul(60)
        .min(86_400) as i64;
    let ready = age.is_some_and(|a| (0..max_age).contains(&a));
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({
            "ready": ready,
            "plan_available": last.is_some(),
            "last_tick_age_seconds": age,
            "max_tick_age_seconds": max_age,
        })),
    )
}

/// Topology + liveness (kept for back-compat; `/livez` + `/readyz` are the orchestration probes).
async fn health(State(s): State<Shared>) -> Json<Value> {
    let mut heated: Vec<String> = s
        .net
        .marker_indices
        .keys()
        .filter(|(_, marker)| marker == "heating")
        .map(|(zone, _)| zone.clone())
        .collect();
    heated.sort();
    heated.dedup();
    Json(json!({
        "status": "ok",
        "git_sha": env!("MPC_GIT_SHA"),
        "uptime_seconds": (Utc::now() - s.started_at).num_seconds().max(0),
        "thermal_states": s.ss.n_states(),
        "heated_zones": heated,
    }))
}

/// A machine-readable index of the API (for discovery and contract tests).
async fn api_index() -> Json<Value> {
    Json(json!({ "endpoints": [
        { "path": "/", "desc": "the monitoring dashboard (HTML)" },
        { "path": "/api/live", "desc": "measured current telemetry (PV/grid/house/battery/SoC/outside)" },
        { "path": "/api/zones", "desc": "per-zone comfort band + heater limit + internal gain" },
        { "path": "/health", "desc": "topology + liveness" },
        { "path": "/livez", "desc": "process liveness (always 200)" },
        { "path": "/readyz", "desc": "readiness: recent plan published" },
        { "path": "/api/version", "desc": "git sha, build time, config/model fingerprints" },
        { "path": "/api/state", "desc": "estimated current per-zone air temperature" },
        { "path": "/api/plan", "desc": "on-demand whole-house plan (aggregates + timeline)" },
        { "path": "/api/plan/latest", "desc": "latest plan published by the shadow loop (no recompute)" },
        { "path": "/api/plan/timeline", "desc": "the latest plan's per-block rows (chart-ready)" },
        { "path": "/api/history?hours=N", "desc": "measured PV (kW) + battery SoC (kWh) over today so far" },
        { "path": "/api/pv/backtest?days=N", "desc": "PV forecast vs actual" },
        { "path": "/api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=&start=&stop=", "desc": "thermal model accuracy" },
        { "path": "/api/calibration/gains", "desc": "live internal gains + config baseline" },
        { "path": "/api/compare", "desc": "shadow recommendation vs loxone live actuals" },
        { "path": "/api/forecast/validation", "desc": "forward-prediction scorecard (predict now, score later)" },
    ]}))
}

async fn get_state(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    cached(&s, "state".into(), || {
        current_state(
            &s.db,
            &s.net,
            &s.ss,
            s.latitude,
            s.longitude,
            s.config.site.ground_temperature_c,
        )
    })
    .await
}

async fn get_plan(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    cached(&s, "plan".into(), || {
        current_plan(
            &s.db,
            &s.net,
            &s.ss,
            &s.config,
            s.latitude,
            s.longitude,
            None,
        )
    })
    .await
}

/// The latest plan published by the shadow MPC loop (no recompute). 503 until the first tick.
async fn get_plan_latest(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    match lock(&s.latest).clone() {
        Some(tp) => Ok(envelope(
            tp.computed_at,
            (Utc::now() - tp.computed_at).num_seconds().max(0) as u64,
            serde_json::to_value(&tp).map_err(|e| fail(anyhow::Error::new(e)))?,
        )),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no plan computed yet; the loop is warming up" })),
        )),
    }
}

/// Just the per-block timeline rows of the latest plan (the chart-ready Grafana shape).
async fn get_plan_timeline(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    match lock(&s.latest).clone() {
        Some(tp) => Ok(envelope(
            tp.computed_at,
            (Utc::now() - tp.computed_at).num_seconds().max(0) as u64,
            serde_json::to_value(&tp.plan.timeline).map_err(|e| fail(anyhow::Error::new(e)))?,
        )),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no plan computed yet; the loop is warming up" })),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    hours: Option<i64>,
}

/// One `solar`-measurement field as a `[[rfc3339, value*scale]]` JSON series of 15-minute means over
/// `start..now`. Empty on any query error — measured history is best-effort context for the dashboard.
async fn measured_series(db: &InfluxDB, field: &str, start: &str, scale: f64) -> Vec<Value> {
    db.read_series("solar", "solar", field, &[], start, "now()", "15m")
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|t| json!([t.time.to_rfc3339(), t.value * scale]))
                .collect()
        })
        .unwrap_or_default()
}

/// Measured PV power (kW) and battery SoC (kWh) over the recent part of the day, for the dashboard's
/// history-vs-forecast overlay. Reads live Growatt telemetry from the `solar` bucket: `InputPower`
/// (W → kW) and `SOC` (% → kWh via the configured battery capacity), as 15-minute means. The default
/// window reaches back to ~local midnight so the chart covers "today so far".
async fn get_history(
    State(s): State<Shared>,
    Query(p): Query<HistoryParams>,
) -> Result<Json<Value>, ApiError> {
    let lookback = p
        .hours
        .unwrap_or_else(|| {
            let local = Utc::now() + chrono::Duration::hours(s.config.site.utc_offset_hours as i64);
            local.hour() as i64 + 1
        })
        .clamp(1, 48);
    cached(&s, format!("history:{lookback}"), || async {
        let start = format!("-{lookback}h");
        let cap = s.config.battery.capacity_kwh.max(0.1);
        anyhow::Ok(json!({
            "pv_kw": measured_series(&s.db, "InputPower", &start, 0.001).await,
            "soc_kwh": measured_series(&s.db, "SOC", &start, cap / 100.0).await,
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct PvParams {
    days: Option<i64>,
}

async fn get_pv_backtest(
    State(s): State<Shared>,
    Query(p): Query<PvParams>,
) -> Result<Json<Value>, ApiError> {
    let days = p.days.unwrap_or(7).clamp(1, 60);
    cached(&s, format!("pv_backtest:{days}"), || {
        backtest_pv(&s.db, s.config.site.utc_offset_hours, days)
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ThermalParams {
    mode: Option<String>,
    window_hours: Option<i64>,
    warmup_hours: Option<i64>,
    start: Option<String>,
    stop: Option<String>,
}

/// The active backtest's before/after accuracy plus the gains it fitted.
#[derive(Serialize)]
struct ActiveBacktest {
    before: Vec<ZoneBacktest>,
    after: Vec<ZoneBacktest>,
    gains_w: HashMap<String, f64>,
}

async fn get_thermal_backtest(
    State(s): State<Shared>,
    Query(p): Query<ThermalParams>,
) -> Result<Json<Value>, ApiError> {
    let mode = p.mode.as_deref().unwrap_or("passive");
    let window = p.window_hours.unwrap_or(24).clamp(1, 720);
    let warmup = p.warmup_hours.unwrap_or(48).clamp(0, 720);
    let cfg = BacktestConfig {
        warmup_hours: warmup,
        window_hours: window,
        ground_temperature_c: s.config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    let key = format!(
        "thermal:{mode}:{window}:{warmup}:{:?}:{:?}",
        p.start, p.stop
    );
    if mode == "active" {
        let start = p
            .start
            .clone()
            .unwrap_or_else(|| format!("-{}h", warmup + window));
        let stop = p.stop.clone().unwrap_or_else(|| "now()".to_string());
        cached(&s, key, || async {
            let (before, after, gains_w) = calibrate_internal_gains(
                &s.db,
                &s.net,
                &s.ss,
                &s.config.heating,
                s.latitude,
                s.longitude,
                &cfg,
                &start,
                &stop,
            )
            .await?;
            Ok(ActiveBacktest {
                before,
                after,
                gains_w,
            })
        })
        .await
    } else {
        cached(&s, key, || {
            backtest_passive(&s.db, &s.net, &s.ss, s.latitude, s.longitude, &cfg)
        })
        .await
    }
}

/// The live internal gains + the config baseline they're refining.
async fn get_calibration_gains(State(s): State<Shared>) -> Json<Value> {
    let live = lock(&s.gains).clone();
    let data = json!({
        "live": live,
        "config_baseline_w": s.config.heating.internal_gains(),
        "recalibrate_hours": s.config.internal_gain_recalibrate_hours,
        "window_days": s.config.internal_gain_window_days,
    });
    envelope(Utc::now(), 0, data)
}

async fn get_compare(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    let latest = lock(&s.latest).clone();
    // Key on plan *presence*, not its timestamp: a warm-up result (no plan yet) is abandoned the
    // moment a plan exists (the key flips once), while the steady-state key stays stable so the TTL
    // cache actually hits — rather than recomputing the loxone reads on every poll (the plan
    // republishes each tick, so a timestamp-keyed cache would never hit).
    let key = if latest.is_some() {
        "compare:plan"
    } else {
        "compare:warmup"
    };
    cached(&s, key.into(), || {
        crate::compare::compare(&s.db, &s.config, &s.net, latest.as_ref())
    })
    .await
}

async fn get_forecast_validation(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    cached(&s, "forecast_validation".into(), || {
        crate::forecast_validation::validate(&s.db)
    })
    .await
}

/// Measured current telemetry (PV / grid / house / battery / SoC / outside temp) for the dashboard's
/// live energy flow. Not TTL-cached — it's the "live" view — but timeout-bounded like the rest.
async fn get_live(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    let data = tokio::time::timeout(COMPUTE_TIMEOUT, crate::live::read_live(&s.db, &s.config))
        .await
        .map_err(|_| timeout_error())?
        .map_err(fail)?;
    let value = serde_json::to_value(&data).map_err(|e| fail(anyhow::Error::new(e)))?;
    Ok(envelope(Utc::now(), 0, value))
}

/// Per-zone comfort band + heater limit + internal gain — the static house definition the dashboard
/// needs to shade comfort bands and label heating. From `config.heating` (no secrets).
async fn get_zones(State(s): State<Shared>) -> Json<Value> {
    let mut zones: Vec<Value> = s
        .config
        .heating
        .zones
        .iter()
        .map(|(zone, c)| {
            json!({
                "zone": zone,
                "t_min": c.t_min,
                "t_max": c.t_max,
                "max_heat_kw": c.max_heat_kw,
                "internal_gain_w": c.internal_gain_w,
            })
        })
        .collect();
    zones.sort_by(|a, b| a["zone"].as_str().cmp(&b["zone"].as_str()));
    envelope(Utc::now(), 0, Value::Array(zones))
}

// The dashboard is a self-contained single-page app embedded in the binary (no extra mounts). It
// reads the JSON API above; Chart.js is loaded from a CDN by the page (the browser, not the server).
const DASHBOARD_HTML: &str = include_str!("dashboard/index.html");
const DASHBOARD_CSS: &str = include_str!("dashboard/style.css");
const DASHBOARD_JS: &str = include_str!("dashboard/app.js");
// ECharts is vendored (not a CDN) so the dashboard works fully offline — it's a home product.
const DASHBOARD_ECHARTS: &str = include_str!("dashboard/echarts.min.js");

async fn dashboard_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
}
async fn dashboard_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        DASHBOARD_CSS,
    )
}
async fn dashboard_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        DASHBOARD_JS,
    )
}
async fn dashboard_echarts() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        DASHBOARD_ECHARTS,
    )
}

/// Build the router over a shared (already-`Arc`'d) state.
pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(dashboard_html))
        .route("/static/style.css", get(dashboard_css))
        .route("/static/app.js", get(dashboard_js))
        .route("/static/echarts.min.js", get(dashboard_echarts))
        .route("/health", get(health))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/api", get(api_index))
        .route("/api/zones", get(get_zones))
        .route("/api/live", get(get_live))
        .route("/api/version", get(version))
        .route("/api/state", get(get_state))
        .route("/api/plan", get(get_plan))
        .route("/api/plan/latest", get(get_plan_latest))
        .route("/api/plan/timeline", get(get_plan_timeline))
        .route("/api/history", get(get_history))
        .route("/api/pv/backtest", get(get_pv_backtest))
        .route("/api/thermal/backtest", get(get_thermal_backtest))
        .route("/api/calibration/gains", get(get_calibration_gains))
        .route("/api/compare", get(get_compare))
        .route("/api/forecast/validation", get(get_forecast_validation))
        .with_state(state)
}

/// Serve the monitoring API on `127.0.0.1:port` (set `MPC_BIND=0.0.0.0` to expose from a container),
/// with the shadow MPC loop running in the background (re-planning every `tick` and publishing to
/// `/api/plan/latest`), until terminated.
pub async fn serve(state: AppState, port: u16, tick: Duration) -> Result<()> {
    let shared: Shared = Arc::new(state);
    tokio::spawn(crate::mpc_loop::run(shared.clone(), tick));
    let app = router(shared);
    let bind_host = std::env::var("MPC_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{bind_host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!(
        "Dashboard + monitoring API on http://{addr}/  (GET /api for the endpoint index); shadow MPC loop every {} min",
        tick.as_secs() / 60
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_wraps_with_freshness_fields() {
        let when = DateTime::parse_from_rfc3339("2026-06-23T11:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let Json(v) = envelope(when, 7, json!({ "x": 1 }));
        assert_eq!(v["computed_at"], "2026-06-23T11:30:00+00:00");
        assert_eq!(v["age_seconds"], 7);
        assert_eq!(v["data"]["x"], 1);
    }

    #[test]
    fn file_fingerprint_is_stable_and_flags_missing() {
        assert_eq!(file_fingerprint("/no/such/file/at/all"), "missing");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"hello").unwrap();
        let p = path.to_str().unwrap();
        let a = file_fingerprint(p);
        assert_eq!(a.len(), 16, "16 hex chars");
        assert_eq!(a, file_fingerprint(p), "deterministic for the same bytes");
        std::fs::write(&path, b"hello!").unwrap();
        assert_ne!(a, file_fingerprint(p), "changes when bytes change");
    }
}
