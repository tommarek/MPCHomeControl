//! Read-only monitoring + reporting HTTP API (axum).
//!
//! Exposes the running brain over JSON: liveness/readiness, version, the estimated thermal state,
//! the live dispatch plan (aggregates + a chart-ready per-block timeline), the PV / thermal
//! model-accuracy backtests, the internal-gain self-correction, a MPC-vs-loxone comparison, and
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
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uom::si::f64::Angle;

use crate::app::{
    current_plan, current_state, zone_temp_history, GainsSnapshot, PlanReport, TimestampedPlan,
};
use crate::optimize::config::ControlConfig;
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
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
    pub db: SourceClients,
    pub latitude: Angle,
    pub longitude: Angle,
    /// When the process started (for uptime reporting).
    pub started_at: DateTime<Utc>,
    /// The latest plan published by the MPC loop (`None` until the first tick completes).
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
        db: SourceClients,
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

/// A `400` for malformed/unsafe user input (e.g. a query parameter that fails validation).
fn bad_request(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
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
        // cache without bound.
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

/// Readiness: the MPC loop has published a recent plan (so the DB is reachable and planning
/// works). 503 if no plan yet or the last tick is too old.
async fn readyz(State(s): State<Shared>) -> (StatusCode, Json<Value>) {
    let last = lock(&s.latest).clone();
    // Measure freshness from the plan's **monotonic** publish instant, not wall-clock `computed_at`,
    // so an NTP step (forward or back) can't turn a fresh plan into a false not-ready. `elapsed()` is
    // monotonic and never negative.
    let age = last
        .as_ref()
        .map(|tp| tp.published.elapsed().as_secs() as i64);
    // Allow a few missed ticks before declaring not-ready (≥10 min regardless of a long tick),
    // capped at a day. Saturating math so an absurd configured tick can't overflow when scaled up.
    let max_age = s
        .config
        .mpc_tick_minutes
        .max(1)
        .saturating_mul(5)
        .max(10)
        .saturating_mul(60)
        .min(86_400) as i64;
    // Inclusive upper bound: a plan exactly `max_age` old is still ready.
    let ready = age.is_some_and(|a| a <= max_age);
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

/// Topology + live status; `/livez` + `/readyz` are the lightweight orchestration probes.
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
        { "path": "/api/state", "desc": "current per-zone air temperature (measured, model-anchored)" },
        { "path": "/api/zones/series?hours=N", "desc": "measured per-zone temperature series (comfort sparklines)" },
        { "path": "/api/plan", "desc": "on-demand whole-house plan (aggregates + timeline)" },
        { "path": "/api/plan/latest", "desc": "latest plan published by the MPC loop (no recompute)" },
        { "path": "/api/plan/timeline", "desc": "the latest plan's per-block rows (chart-ready)" },
        { "path": "/api/history?hours=N", "desc": "measured PV (kW) + battery SoC (kWh) over today so far" },
        { "path": "/api/pv/backtest?days=N", "desc": "PV forecast vs actual" },
        { "path": "/api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=&start=&stop=", "desc": "thermal model accuracy" },
        { "path": "/api/calibration/gains", "desc": "live internal gains + config baseline" },
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

async fn get_zone_series(
    State(s): State<Shared>,
    Query(p): Query<HistoryParams>,
) -> Result<Json<Value>, ApiError> {
    let hours = p.hours.unwrap_or(24).clamp(1, 48);
    cached(&s, format!("zone_series:{hours}"), || {
        zone_temp_history(&s.db, &s.net, hours)
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

/// One field of the latest published plan, in the freshness envelope (503 until the first tick). The
/// envelope carries `computed_at`, so `project` serializes just the field — never the whole
/// `TimestampedPlan` — keeping the payload from being doubly-timestamped.
fn latest_plan(
    s: &Shared,
    project: impl FnOnce(&PlanReport) -> serde_json::Result<Value>,
) -> Result<Json<Value>, ApiError> {
    match lock(&s.latest).clone() {
        Some(tp) => Ok(envelope(
            tp.computed_at,
            (Utc::now() - tp.computed_at).num_seconds().max(0) as u64,
            project(&tp.plan).map_err(|e| fail(anyhow::Error::new(e)))?,
        )),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no plan computed yet; the loop is warming up" })),
        )),
    }
}

/// `404` unless a charger named `name` is configured (shared by the EV-preference handlers).
fn require_charger(s: &Shared, name: &str) -> Result<(), ApiError> {
    if s.config.chargers.iter().any(|c| c.name == name) {
        Ok(())
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no charger named {name:?}") })),
        ))
    }
}

/// The latest plan published by the MPC loop (no recompute). 503 until the first tick.
async fn get_plan_latest(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    latest_plan(&s, |p| serde_json::to_value(p))
}

/// Feature capabilities, so the dashboard shows/hides config-driven sections (e.g. the EV nav only
/// when a charger is configured).
async fn get_capabilities(State(s): State<Shared>) -> Json<Value> {
    let chargers: Vec<&str> = s.config.chargers.iter().map(|c| c.name.as_str()).collect();
    Json(json!({
        "has_hvac": s.config.hvac.is_some(),
        "has_ev": !s.config.chargers.is_empty(),
        "chargers": chargers,
    }))
}

/// Per-EV-charger fused live state + the optimizer's charge schedule, from the latest published plan.
async fn get_ev(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    latest_plan(&s, |p| serde_json::to_value(&p.ev))
}

/// The effective live charging preference for one charger (empty object if none set; 404 for an
/// unknown charger, mirroring POST).
async fn get_ev_pref(
    State(s): State<Shared>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_charger(&s, &name)?;
    let prefs = crate::ev::prefs::load();
    let value = serde_json::to_value(prefs.get(&name).cloned().unwrap_or_default())
        .map_err(|e| fail(anyhow::Error::new(e)))?;
    Ok(Json(value))
}

/// Set a live charging preference (strategy / rate / target / deadline) for a charger, persisted to
/// the MPC's **own** store. This is the only write the MPC makes — never to the house; the wallbox is
/// driven by the (separately-gated) controller, not here.
async fn post_ev_pref(
    State(s): State<Shared>,
    Path(name): Path<String>,
    Json(pref): Json<crate::ev::EvPreference>,
) -> Result<Json<Value>, ApiError> {
    require_charger(&s, &name)?;
    pref.validate().map_err(fail)?;
    // Atomic load-modify-save (a process lock) so concurrent POSTs can't lose an update.
    crate::ev::prefs::update(name, pref).map_err(fail)?;
    Ok(Json(json!({ "ok": true })))
}

/// The per-block timeline rows of the latest plan (the chart-ready Grafana shape).
async fn get_plan_timeline(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    latest_plan(&s, |p| serde_json::to_value(&p.timeline))
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    hours: Option<i64>,
}

/// One `solar`-measurement field as a `[[rfc3339, value*scale]]` JSON series of 15-minute means over
/// `start..now`. Empty on any query error — measured history is best-effort context for the dashboard.
async fn measured_series(db: &SourceClients, field: &str, start: &str, scale: f64) -> Vec<Value> {
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
            // Read the hour-of-day in the site's local time, so the window reaches back to ~local
            // midnight ("today so far").
            let offset = chrono::FixedOffset::east_opt(s.config.site.utc_offset_hours * 3600)
                .expect("site.utc_offset_hours validated at config load");
            Utc::now().with_timezone(&offset).hour() as i64 + 1
        })
        .clamp(1, 48);
    cached(&s, format!("history:{lookback}"), || async {
        let start = format!("-{lookback}h");
        let cap = s.config.battery.capacity_kwh.max(0.1);
        anyhow::Ok(json!({
            "pv_kw": measured_series(&s.db, "InputPower", &start, 0.001).await,
            "house_kw": measured_series(&s.db, "INVPowerToLocalLoad", &start, 0.001).await,
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
    // The user-supplied range bounds are interpolated into a Flux `range()` — validate them against
    // the time-expression allow-list so they can't inject pipeline operations (Flux injection).
    for t in [p.start.as_deref(), p.stop.as_deref()]
        .into_iter()
        .flatten()
    {
        if !crate::influxdb::valid_flux_time(t) {
            return Err(bad_request(format!(
                "invalid time {t:?}: use RFC3339, a relative duration like -2d, or now()"
            )));
        }
    }
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
        let local_offset = chrono::FixedOffset::east_opt(s.config.site.utc_offset_hours * 3600)
            .expect("site.utc_offset_hours validated at config load");
        cached(&s, key, || async {
            let (before, after, fit) = calibrate_internal_gains(
                &s.db,
                &s.net,
                &s.ss,
                &s.config.heating,
                &s.config.scheduled_loads,
                local_offset,
                s.latitude,
                s.longitude,
                &cfg,
                &start,
                &stop,
            )
            .await?;
            // Scheduled-load magnitudes aren't surfaced here (dashboard display is a follow-up); the
            // backtest reports only the per-zone internal gains, unchanged.
            Ok(ActiveBacktest {
                before,
                after,
                gains_w: fit.gains,
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
// reads the JSON API above; ECharts is vendored (not a CDN), so it works fully offline.
const DASHBOARD_HTML: &str = include_str!("dashboard/index.html");
const DASHBOARD_CSS: &str = include_str!("dashboard/style.css");
const DASHBOARD_JS: &str = include_str!("dashboard/app.js");
const DASHBOARD_ECHARTS: &str = include_str!("dashboard/echarts.min.js");

/// Serve one embedded dashboard asset with its content type and an optional `Cache-Control`.
fn asset(content_type: &'static str, cache: Option<&'static str>, body: &'static str) -> Response {
    let mut resp = ([(header::CONTENT_TYPE, content_type)], body).into_response();
    if let Some(c) = cache {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    resp
}

const JS: &str = "application/javascript; charset=utf-8";
async fn dashboard_html() -> Response {
    asset("text/html; charset=utf-8", None, DASHBOARD_HTML)
}
async fn dashboard_css() -> Response {
    asset("text/css; charset=utf-8", None, DASHBOARD_CSS)
}
async fn dashboard_js() -> Response {
    asset(JS, None, DASHBOARD_JS)
}
async fn dashboard_echarts() -> Response {
    asset(JS, Some("public, max-age=86400"), DASHBOARD_ECHARTS)
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
        .route("/api/zones/series", get(get_zone_series))
        .route("/api/plan", get(get_plan))
        .route("/api/plan/latest", get(get_plan_latest))
        .route("/api/capabilities", get(get_capabilities))
        .route("/api/ev", get(get_ev))
        .route(
            "/api/ev/:name/preference",
            get(get_ev_pref).post(post_ev_pref),
        )
        .route("/api/plan/timeline", get(get_plan_timeline))
        .route("/api/history", get(get_history))
        .route("/api/pv/backtest", get(get_pv_backtest))
        .route("/api/thermal/backtest", get(get_thermal_backtest))
        .route("/api/calibration/gains", get(get_calibration_gains))
        .route("/api/forecast/validation", get(get_forecast_validation))
        .with_state(state)
}

/// Serve the monitoring API on `127.0.0.1:port` (set `MPC_BIND=0.0.0.0` to expose from a container),
/// with the MPC loop running in the background (re-planning every `tick` and publishing to
/// `/api/plan/latest`), until terminated.
pub async fn serve(state: AppState, port: u16, tick: Duration) -> Result<()> {
    let shared: Shared = Arc::new(state);
    tokio::spawn(crate::mpc_loop::run(shared.clone(), tick));
    let app = router(shared);
    let bind_host = std::env::var("MPC_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{bind_host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!(
        "Dashboard + monitoring API on http://{addr}/  (GET /api for the endpoint index); MPC loop every {} min",
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
