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
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uom::si::f64::Angle;
use uom::si::{angle::degree, heat_flux_density::watt_per_square_meter};

use crate::app::{
    current_plan, current_state, zone_temp_history, GainsSnapshot, PlanExtras, PlanReport,
    TimestampedPlan,
};
use crate::optimize::config::ControlConfig;
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::sun::{sun_azimuth_elevation, tilted_irradiance_components, SolarInput};
use crate::topology::ModelTopology;
use crate::validate::{
    backtest_passive_detail, calibrate_internal_gains, BacktestConfig, ZoneBacktest,
};

/// How long a computed response stays fresh before it is recomputed.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// TTL for `/api/live` — long enough to collapse concurrent dashboard pollers onto one read, far
/// shorter than the Growatt feed's own cadence, so the "live" view stays live.
const LIVE_TTL: Duration = Duration::from_secs(5);

/// Hard ceiling on a single computation, so a slow/stuck DB can't pin a request open. Covers
/// `/api/plan`'s full solve path (the strict fix-and-round pipeline's 32 s + the fallback's 15 s,
/// see `app::STRICT_SOLVE_TIMEOUT`/`FALLBACK_SOLVE_TIMEOUT`) plus headroom for the pre-solve DB reads.
const COMPUTE_TIMEOUT: Duration = Duration::from_secs(55);

/// Everything the handlers need, shared (read-only) across requests.
pub struct AppState {
    pub net: RcNetwork,
    pub ss: StateSpace,
    /// Rc-free snapshot of the building envelope (zones + boundaries), for `/api/model/topology`.
    pub topology: ModelTopology,
    pub config: ControlConfig,
    pub db: SourceClients,
    pub latitude: Angle,
    pub longitude: Angle,
    /// The startup-built thermal kernel cache (x0-independent), shared by the loop and the
    /// on-demand plan path — see [`crate::optimize::thermal::KernelSet`].
    pub kernels: Arc<crate::optimize::thermal::KernelSet>,
    /// The Kalman filter for `estimator.mode: kalman`. Built in a BACKGROUND thread (the
    /// Riccati solve is seconds on the real ~500-state model — tens of seconds in the static-musl
    /// release — and must never block the HTTP server / MPC loop from starting). Empty until the
    /// build finishes (readers then behave as anchor); never populated in anchor mode.
    pub kalman: Arc<std::sync::OnceLock<Arc<crate::kalman::KalmanFilter>>>,
    /// Set when the background Kalman build FAILED (as opposed to still running). Without it an
    /// empty `kalman` slot is ambiguous forever, so `x0=kalman` kept telling callers to "retry
    /// shortly" for a filter that will never arrive.
    pub kalman_failed: Arc<std::sync::atomic::AtomicBool>,
    /// When the process started (for uptime reporting).
    pub started_at: DateTime<Utc>,
    /// The latest plan published by the MPC loop (`None` until the first tick completes).
    pub latest: Mutex<Option<TimestampedPlan>>,
    /// The latest internal-gain re-fit published by the loop (`None` until the first fit lands).
    pub gains: Mutex<Option<GainsSnapshot>>,
    /// Per-endpoint TTL cache of the last computed value, with the wall-clock instant it was made.
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// Single-flight gates, one per cache key: the SECOND caller for a key whose entry is cold or
    /// just expired waits for the first instead of starting its own computation. Without this, a
    /// dashboard reload or a second tab landing on a cold `/api/thermal/backtest` ran the multi-day
    /// Influx read and the full drive+fit once PER caller.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// A cached response: the monotonic instant and wall-clock time it was computed, plus the value.
type CacheEntry = (Instant, DateTime<Utc>, Value);

impl AppState {
    pub fn new(
        net: RcNetwork,
        ss: StateSpace,
        topology: ModelTopology,
        config: ControlConfig,
        db: SourceClients,
        latitude: Angle,
        longitude: Angle,
    ) -> Self {
        let kernels = Arc::new(crate::app::build_kernel_cache(&config, &net, &ss));
        // The Kalman filter (config `estimator.mode`). Anchor mode never builds it. Otherwise
        // build it in a DETACHED thread: the Riccati solve depends only on the model + noise
        // config, so it is a one-shot, but it takes seconds (tens in static-musl) on the real
        // ~500-state model and must NOT block the server + loop from starting. Until it lands the
        // OnceLock is empty and every reader behaves as plain anchor (a safe degradation — the
        // kalman x0 simply isn't available for the first minute (anchor is used).
        let kalman: Arc<std::sync::OnceLock<Arc<crate::kalman::KalmanFilter>>> =
            Arc::new(std::sync::OnceLock::new());
        let kalman_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if config.estimator.mode != crate::optimize::config::EstimatorMode::Anchor {
            let (net_c, ss_c, cfg_c, zones) = (
                net.clone(),
                ss.clone(),
                config.estimator.clone(),
                db.mapped_zones(),
            );
            let slot = Arc::clone(&kalman);
            let failed = Arc::clone(&kalman_failed);
            std::thread::spawn(move || {
                // A drop guard, so ANY exit that leaves the slot empty — including a PANIC in the
                // Riccati/augmentation sizing arithmetic — is reported. Setting the flag only in the
                // `Err` arm meant a panicking build unwound this detached thread silently and
                // `estimator_status()` reported `building: true` for the life of the process:
                // permanently "about to be ready", never ready, on a house configured for kalman.
                struct ReportOnExit(
                    Arc<std::sync::OnceLock<Arc<crate::kalman::KalmanFilter>>>,
                    Arc<std::sync::atomic::AtomicBool>,
                );
                impl Drop for ReportOnExit {
                    fn drop(&mut self) {
                        if self.0.get().is_none() {
                            self.1.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                let _guard = ReportOnExit(Arc::clone(&slot), Arc::clone(&failed));
                let t = std::time::Instant::now();
                match crate::kalman::KalmanFilter::build(&net_c, &ss_c, &cfg_c, &zones) {
                    Ok(f) => {
                        let _ = slot.set(Arc::new(f));
                        println!(
                            "[kalman] filter built in {:.1}s — kalman estimate now active",
                            t.elapsed().as_secs_f64()
                        );
                    }
                    Err(e) => {
                        failed.store(true, std::sync::atomic::Ordering::Relaxed);
                        eprintln!("[kalman] filter build failed ({e:#}); staying on anchor");
                    }
                }
            });
        }
        Self {
            net,
            ss,
            topology,
            config,
            db,
            latitude,
            longitude,
            kernels,
            kalman,
            kalman_failed,
            started_at: Utc::now(),
            latest: Mutex::new(None),
            gains: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
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

/// A clone of the latest published plan, for the loop's commitment-latch seeding across respawns.
pub(crate) fn lock_latest(state: &AppState) -> Option<TimestampedPlan> {
    lock(&state.latest).clone()
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
/// cache and return it — wrapped in the freshness envelope. The cache lock (a std mutex) is never
/// held across an `await`; the single-flight gate is a separate async mutex.
async fn cached<T, F, Fut>(state: &Shared, key: String, compute: F) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
    T: Serialize,
{
    cached_for(state, key, CACHE_TTL, compute).await
}

/// The cached value for `key` if it is still within `ttl`, wrapped in the freshness envelope.
fn cache_hit(state: &Shared, key: &str, ttl: Duration) -> Option<Json<Value>> {
    let cache = lock(&state.cache);
    let (at, computed_at, value) = cache.get(key)?;
    (at.elapsed() < ttl).then(|| envelope(*computed_at, at.elapsed().as_secs(), value.clone()))
}

/// [`cached`] with an explicit TTL, for endpoints whose freshness contract differs from the default
/// (e.g. `/api/live`, which wants single-flight sharing but must stay seconds-fresh).
async fn cached_for<T, F, Fut>(
    state: &Shared,
    key: String,
    ttl: Duration,
    compute: F,
) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
    T: Serialize,
{
    let fresh = |state: &Shared| -> Option<Json<Value>> { cache_hit(state, &key, ttl) };
    if let Some(hit) = fresh(state) {
        return Ok(hit);
    }
    // Cold or expired: take this key's gate so overlapping callers queue instead of each running the
    // whole computation. The waiter re-checks the cache on the way in and normally returns the value
    // the first caller just stored.
    let gate = {
        let mut inflight = lock(&state.inflight);
        // Nobody else holds a handle → the entry is finished work, not an in-flight computation.
        inflight.retain(|_, g| Arc::strong_count(g) > 1);
        Arc::clone(
            inflight
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    // Bound the WAIT as well as the computation. `compute()` is capped at COMPUTE_TIMEOUT, but an
    // unbounded `lock().await` in front of it reintroduced unbounded latency in exactly the degraded
    // state the timeout exists for: with a wedged DB each waiter serially runs its own COMPUTE_TIMEOUT
    // attempt, so the Nth queued caller blocked for ~N×COMPUTE_TIMEOUT with no 504. There is no
    // request-timeout layer on the router to catch it.
    let Ok(_guard) = tokio::time::timeout(COMPUTE_TIMEOUT, gate.lock()).await else {
        return Err(timeout_error());
    };
    if let Some(hit) = fresh(state) {
        return Ok(hit);
    }
    let computed = tokio::time::timeout(COMPUTE_TIMEOUT, compute())
        .await
        .map_err(|_| timeout_error())?
        // Contention on the backtest gate is overload, not a server fault — report it like the
        // sibling single-flight gate timeout above (504), not as a 500.
        .map_err(|e| {
            if e.downcast_ref::<BacktestBusy>().is_some() {
                timeout_error()
            } else {
                fail(e)
            }
        })?;
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

/// Run one backtest under the global one-at-a-time gate, supervised so its lifetime and its
/// result both outlive an impatient caller. Called from INSIDE `cached`'s compute closure — i.e.
/// only after the per-key single-flight gate is held and the cache re-checked, so identical
/// concurrent requests dedupe instead of each spawning a run.
///
/// The drive + `fit_gains` work runs under `spawn_blocking` (validate.rs), which a timeout can
/// abandon but never cancel, so two things must survive a 504'd (or disconnected) caller:
/// - the PERMIT: owned by the detached supervisor task and released only when the work truly
///   finishes — a handler-scoped permit was released on timeout while the blocking drive kept
///   burning a thread, letting pollers stack unbounded concurrent runs on an endpoint that
///   needs no token and binds 0.0.0.0 (same pattern as `app.rs`'s solver permits);
/// - the RESULT: the supervisor writes the TTL cache ITSELF on success — via the caller alone, a
///   run longer than `COMPUTE_TIMEOUT` was computed to completion, discarded, and recomputed on
///   every poll: a permanent 504 loop that never once served the answer. With the supervisor
///   write, the first poll after the run lands gets the cache hit.
///
/// A caller that cannot take the gate within the compute budget errors out ("still running");
/// its single-flight waiters see the cache once the supervisor stores it.
/// Marker for "the gate is busy" — `cached_for` maps it to a 504 instead of the 500 a genuine
/// compute failure gets, so monitoring can tell transient contention from a server fault.
#[derive(Debug)]
struct BacktestBusy;

impl std::fmt::Display for BacktestBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another backtest is still running — retry shortly")
    }
}
impl std::error::Error for BacktestBusy {}

async fn supervised_backtest<T: Serialize + Send + 'static>(
    state: Shared,
    key: String,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<Value> {
    static GATE: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let permit = tokio::time::timeout(COMPUTE_TIMEOUT, Arc::clone(&GATE).acquire_owned())
        .await
        .map_err(|_| anyhow::Error::new(BacktestBusy))??;
    // Re-check the cache AFTER winning the permit: a caller that queued here while a LONG run
    // (> COMPUTE_TIMEOUT) was in flight wakes exactly when that run's supervisor has just stored
    // its result — spawning unconditionally would immediately re-run the identical backtest and
    // hold the gate for its whole duration (the redundant-run failure this function exists to
    // prevent, one layer deeper than cached_for's pre-compute re-check can see).
    if let Some(hit) = {
        let cache = lock(&state.cache);
        cache
            .get(&key)
            .filter(|(at, _, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, _, v)| v.clone())
    } {
        return Ok(hit);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let log_key = key.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let res: Result<Value> = match work.await {
            Ok(v) => serde_json::to_value(&v).map_err(anyhow::Error::new),
            Err(e) => Err(e),
        };
        match &res {
            Ok(value) => {
                let mut cache = lock(&state.cache);
                cache.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
                cache.insert(key, (Instant::now(), Utc::now(), value.clone()));
            }
            // Log here, not just via the oneshot: when the caller already 504'd (the designed
            // long-run case) rx is gone, and a failure against e.g. a wedged DB would otherwise
            // vanish without a trace while every poll restarts the same doomed run.
            Err(e) => eprintln!("[web] backtest {log_key:?} failed: {e}"),
        }
        let _ = tx.send(res);
    });
    rx.await
        .map_err(|_| anyhow::anyhow!("backtest supervisor dropped"))?
}

/// Hard ceiling on the active backtest's total loaded range, `warmup + window` (30 days). The two
/// knobs are each clamped to 720 h individually; this bounds their SUM, since the loaded span —
/// an hourly grid of ~500-state vectors, driven once per fit probe — is what actually costs.
const MAX_BACKTEST_SPAN_HOURS: i64 = 720;

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
async fn version(State(s): State<Shared>) -> Json<Value> {
    // Computed ONCE: the files are read at boot and cannot change without a restart, so hashing
    // both (65 KB) on every request only put blocking file IO on the async runtime — and
    // `/api/version` is exactly what a monitor polls.
    static FINGERPRINTS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    let (config, model) = FINGERPRINTS.get_or_init(|| {
        (
            file_fingerprint("config.json5"),
            file_fingerprint("model.json5"),
        )
    });
    Json(json!({
        "git_sha": env!("MPC_GIT_SHA"),
        "built_at": env!("MPC_BUILT_AT"),
        "config_fingerprint": config,
        "model_fingerprint": model,
        "estimator": estimator_status(&s),
    }))
}

/// Which state estimator is actually running, for `/api/version`. A background Kalman build that
/// FAILS was previously observable only as one stderr line that scrolls away: the brain then runs on
/// the anchor estimator indefinitely while every probe and endpoint looks perfectly normal.
fn estimator_status(s: &Shared) -> Value {
    use std::sync::atomic::Ordering;
    let configured = format!("{:?}", s.config.estimator.mode).to_lowercase();
    let built = s.kalman.get().is_some();
    let failed = s.kalman_failed.load(Ordering::Relaxed);
    json!({
        "configured": configured,
        "active": if built { "kalman" } else { "anchor" },
        "build_failed": failed,
        // True while a configured filter is neither built nor known-failed: still warming up.
        "building": !built && !failed && configured != "anchor",
    })
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
    // Project the two scalars we need UNDER the guard rather than cloning the plan. The clone copied
    // the whole 144-block timeline — five HashMaps per block — for one `Instant` and one `is_some`,
    // on the endpoint every orchestrator probes and every open dashboard tab polls every 10 s.
    //
    // Measure freshness from the plan's **monotonic** publish instant, not wall-clock `computed_at`,
    // so an NTP step (forward or back) can't turn a fresh plan into a false not-ready. `elapsed()` is
    // monotonic and never negative.
    let age = {
        let guard = lock(&s.latest);
        guard
            .as_ref()
            .map(|tp| tp.published.elapsed().as_secs() as i64)
    };
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
            "plan_available": age.is_some(),
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
        { "path": "/api/model/topology", "desc": "building envelope: zones + boundaries (area, orientation, layers, U-value)" },
        { "path": "/api/model/solar", "desc": "live per-surface clear-sky solar gain (W) + the sun's position" },
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
        { "path": "/api/thermal/backtest?mode=passive|active&window_hours=&warmup_hours=&detail=1", "desc": "thermal model accuracy (range is -(warmup+window)h..now); detail=1 adds the hourly per-zone series + drive inputs (passive)" },
        { "path": "/api/calibration/gains", "desc": "live internal gains + config baseline" },
        { "path": "/api/forecast/validation", "desc": "forward-prediction scorecard (predict now, score later)" },
        { "path": "/api/capabilities", "desc": "what this house has (has_hvac, has_ev, chargers) — drives conditional UI" },
        { "path": "/api/ev", "desc": "per-charger live state + planned charge schedule (EV only)" },
        { "path": "/api/ev/<name>/preference", "desc": "GET / POST (merge) / DELETE the live charging override — the only mutating route" },
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
            &s.config,
            s.kalman.get().map(|a| a.as_ref()),
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
        // On-demand/advisory: no cache, no block-0 commitment (may differ from the published plan
        // at block 0 — the loop's latch applies only there), but the kernel cache still applies.
        current_plan(
            &s.db,
            &s.net,
            &s.ss,
            &s.config,
            s.latitude,
            s.longitude,
            PlanExtras {
                kernels: Some(s.kernels.clone()),
                kalman: s.kalman.get().cloned(),
                ..Default::default()
            },
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
    // Bind the clone first: a temporary guard in the match scrutinee lives to the end of the
    // match, so `match lock(..).clone()` would hold the mutex across the whole plan
    // serialization below — blocking every concurrent poller AND the MPC loop's publish.
    let latest = lock(&s.latest).clone();
    match latest {
        // Age from the MONOTONIC publish instant (like /readyz), not the wall clock: the armed
        // publisher's staleness gate keys on this value, and a backward clock step during a
        // wedged loop would otherwise shrink the reported age and blind the gate.
        Some(tp) => Ok(envelope(
            tp.computed_at,
            tp.published.elapsed().as_secs(),
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
    // The preference store is plain synchronous file IO; keep it off the async worker threads
    // (a slow/contended bind-mounted volume would otherwise stall unrelated requests).
    let prefs = tokio::task::spawn_blocking(crate::ev::prefs::load)
        .await
        .map_err(|e| fail(anyhow::anyhow!("ev preference read task failed: {e}")))?;
    let value = serde_json::to_value(prefs.get(&name).cloned().unwrap_or_default())
        .map_err(|e| fail(anyhow::Error::new(e)))?;
    Ok(Json(value))
}

/// Optional write-protection for the mutating EV routes: when `MPC_API_TOKEN` is set, they require
/// a matching `X-MPC-Token` header (the dashboard prompts once and remembers it). Unset ⇒ open, the
/// LAN-trusted default — but these prefs steer real charging money, so a token keeps an arbitrary
/// LAN device (guest phone, compromised IoT) from POSTing `charge_now` at the evening peak.
fn require_api_token(headers: &header::HeaderMap) -> Result<(), ApiError> {
    let Ok(expected) = std::env::var("MPC_API_TOKEN") else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    let presented = headers.get("x-mpc-token").and_then(|v| v.to_str().ok());
    // Constant-time comparison (length check + XOR fold): a short-circuiting `==` on a secret is
    // a byte-position timing side-channel. Low stakes on a LAN token, but the fix is free.
    let ok = presented.is_some_and(|p| {
        p.len() == expected.len()
            && p.bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if ok {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or wrong X-MPC-Token" })),
        ))
    }
}

/// Set a live charging preference (strategy / rate / target / deadline) for a charger, persisted to
/// the MPC's **own** store. This is the only write the MPC makes — never to the house; the wallbox is
/// driven by the (separately-gated) controller, not here.
async fn post_ev_pref(
    State(s): State<Shared>,
    Path(name): Path<String>,
    headers: header::HeaderMap,
    Json(pref): Json<crate::ev::EvPreference>,
) -> Result<Json<Value>, ApiError> {
    require_api_token(&headers)?;
    require_charger(&s, &name)?;
    // Client-supplied values only — a bad body is the caller's error (400), not a server fault.
    pref.validate().map_err(|e| bad_request(e.to_string()))?;
    // Atomic load-modify-save (a process lock) so concurrent POSTs can't lose an update. Fields
    // absent from the body keep their stored values (merge semantics — "set any subset").
    tokio::task::spawn_blocking(move || crate::ev::prefs::update(name, pref))
        .await
        .map_err(|e| fail(anyhow::anyhow!("ev preference write task failed: {e}")))?
        .map_err(fail)?;
    Ok(Json(json!({ "ok": true })))
}

/// Clear a charger's live preference entirely — every override reverts to config / the car's own
/// limit. The counterpart to the POST's merge semantics (which can only set fields, not unset them).
async fn delete_ev_pref(
    State(s): State<Shared>,
    Path(name): Path<String>,
    headers: header::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_api_token(&headers)?;
    require_charger(&s, &name)?;
    tokio::task::spawn_blocking(move || crate::ev::prefs::clear(&name))
        .await
        .map_err(|e| fail(anyhow::anyhow!("ev preference clear task failed: {e}")))?
        .map_err(fail)?;
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
async fn measured_series(db: &SourceClients, metric: &str, start: &str, scale: f64) -> Vec<Value> {
    // Through the growatt locator (like /api/live), not a hardcoded bucket/measurement — a house
    // remapping its telemetry via `data_sources` would otherwise get a live energy-flow but a
    // silently-empty history overlay. `scale` is the dashboard unit conversion (W→kW, %→kWh),
    // layered on top of any locator-configured scale.
    db.growatt_series(metric, start, "now()", "15m")
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
            let offset = s.config.site.offset_at(Utc::now());
            Utc::now().with_timezone(&offset).hour() as i64 + 1
        })
        .clamp(1, 48);
    cached(&s, format!("history:{lookback}"), || async {
        let start = format!("-{lookback}h");
        // No/zero battery ⇒ no SoC series at all — /api/live deliberately reports None for the
        // same condition ("rather than a misleading 0"); a curve scaled by a clamped 0.1 kWh
        // would contradict it with fabricated near-zero points.
        let cap = s.config.battery.capacity_kwh;
        let soc_kwh = if cap > 0.0 {
            measured_series(&s.db, "SOC", &start, cap / 100.0).await
        } else {
            Vec::new()
        };
        anyhow::Ok(json!({
            "pv_kw": measured_series(&s.db, "InputPower", &start, 0.001).await,
            "house_kw": measured_series(&s.db, "INVPowerToLocalLoad", &start, 0.001).await,
            "soc_kwh": soc_kwh,
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
        backtest_pv(&s.db, &s.config.site, days)
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
    /// `x0=kalman` scores an open-loop window from the Kalman-filtered warm-up state instead of
    /// the seed+drive path — the held-out estimator comparison. Needs `estimator.mode` ≠ anchor
    /// (the filter is built at startup).
    x0: Option<String>,
    /// `detail=1` (passive only): return the HOURLY per-zone predicted/measured series and the
    /// drive inputs (outside °C, GHI, cloud) alongside the scores — the diagnostic behind a
    /// zone's aggregate bias (solar-shaped? diurnal? flat?). Larger payload; separately cached.
    detail: Option<String>,
}

/// The active backtest's before/after accuracy plus the gains it fitted.
#[derive(Serialize)]
struct ActiveBacktest {
    before: Vec<ZoneBacktest>,
    after: Vec<ZoneBacktest>,
    gains_w: HashMap<String, crate::optimize::config::GainProfile>,
}

async fn get_thermal_backtest(
    State(s): State<Shared>,
    Query(p): Query<ThermalParams>,
) -> Result<Json<Value>, ApiError> {
    // Allow-list, not a fallback: an unrecognised value used to return 200 with a PASSIVE
    // scorecard, silently answering a different question than asked — on the endpoint whose whole
    // job is judging model accuracy. It also bounded the TTL cache key, which interpolates `mode`.
    let mode = match p.mode.as_deref() {
        None | Some("passive") => "passive",
        Some("active") => "active",
        Some(other) => {
            return Err(bad_request(format!(
                "invalid mode {other:?}: use \"passive\" or \"active\""
            )))
        }
    };
    let window = p.window_hours.unwrap_or(24).clamp(1, 720);
    let warmup = p.warmup_hours.unwrap_or(48).clamp(0, 720);
    // Clamp the PAIR too, BEFORE the scorer sees it. Each is individually legal at 720 h, but the
    // active path can only load `MAX_BACKTEST_SPAN_HOURS` of data — so `?window_hours=720&
    // warmup_hours=48` loaded 720 h, then scored with the unclamped 720 h window and left ZERO
    // warm-up: the scorecard silently answered a different question than the one asked.
    // `warmup` is capped one short of the span so `window` always keeps ≥ 1 h WITHIN the cap —
    // the previous `.max(1)` rescue could push the pair to cap + 1.
    let warmup = warmup.min(MAX_BACKTEST_SPAN_HOURS - 1);
    let window = window.min(MAX_BACKTEST_SPAN_HOURS - warmup);
    let cfg = BacktestConfig {
        warmup_hours: warmup,
        window_hours: window,
        ground_temperature_c: s.config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    // `start`/`stop` are no longer accepted at all. The explicit-range path needed its own
    // validator (`flux_span_hours`) to bound an unauthenticated CPU-heavy request, and that one
    // helper produced SIX defects across four review rounds — a reachable panic, a false 400 on the
    // endpoint's own defaults, validator/resolver disagreements, dead RFC3339 parsing, abs() hiding
    // inverted ranges, future-dated bounds — while the real protection was always the one-permit
    // gate plus the clamps. The derived `-(warmup+window)h..now()` range expresses every supported
    // question (window and warmup are the knobs); an arbitrary historical window was surface, not
    // capability, and it is gone.
    if p.start.is_some() || p.stop.is_some() {
        return Err(bad_request(
            "start/stop are not supported; use window_hours and warmup_hours (the range is \
             -(warmup+window)h..now)",
        ));
    }
    // Allow-list, same reason as `mode` above: an unrecognised value silently fell through to the
    // seed path and answered a different question than asked.
    let x0_kalman = match p.x0.as_deref() {
        None => false,
        Some("kalman") => true,
        Some("seed") => false,
        Some(other) => {
            return Err(bad_request(format!(
                "invalid x0 {other:?}: use \"seed\" or \"kalman\""
            )))
        }
    };
    // `mode=active` fits gains over its own internally-seeded drive and has no x0 seam, so honouring
    // `x0` there is impossible — reject it rather than return a silently non-Kalman result.
    if x0_kalman && mode == "active" {
        return Err(bad_request(
            "x0=kalman applies only to mode=passive (the active fit seeds its own state)",
        ));
    }
    if x0_kalman && s.kalman.get().is_none() {
        // Distinguish "not configured" from "configured but the background build hasn't finished"
        // — the filter takes seconds (tens under static-musl) to solve the Riccati at startup.
        return Err(bad_request(
            if s.config.estimator.mode == crate::optimize::config::EstimatorMode::Anchor {
                "x0=kalman needs estimator.mode: kalman (this server runs the anchor estimator)"
            } else if s.kalman_failed.load(std::sync::atomic::Ordering::Relaxed) {
                // Terminal: the one-shot build already failed, so retrying never helps.
                "x0=kalman: the Kalman filter FAILED to build at startup (see the logs); the \
                 server is running on the anchor estimator until it is restarted"
            } else {
                "x0=kalman: the Kalman filter is still building at startup — retry shortly"
            },
        ));
    }
    let detail = matches!(p.detail.as_deref(), Some("1") | Some("true"));
    if detail && mode == "active" {
        return Err(bad_request("detail=1 applies only to mode=passive"));
    }
    let key = format!("thermal:{mode}:{window}:{warmup}:{x0_kalman}:{detail}");
    if mode == "active" {
        // The range is DERIVED, `-(warmup+window)h .. now()` — bounded by construction, since both
        // knobs are clamped and their sum capped above. No user-supplied range ever reaches Flux.
        let derived_h = warmup + window;
        let start = format!("-{derived_h}h");
        let stop = "now()".to_string();
        let local_offset = s.config.site.offset_at(Utc::now());
        // Cache FIRST, permit second: a fresh cached answer must never queue behind (and then 504
        // on) another caller's long-running compute, which the permit serializes.
        if let Some(hit) = cache_hit(&s, &key, CACHE_TTL) {
            return Ok(hit);
        }
        let sup = Arc::clone(&s);
        let key2 = key.clone();
        cached(&s, key, || async move {
            let state = Arc::clone(&sup);
            supervised_backtest(state, key2, async move {
                let (before, after, fit) = calibrate_internal_gains(
                    &sup.db,
                    &sup.net,
                    &sup.ss,
                    &sup.config.heating,
                    &sup.config.scheduled_loads,
                    local_offset,
                    sup.latitude,
                    sup.longitude,
                    &cfg,
                    &start,
                    &stop,
                )
                .await?;
                // Scheduled-load magnitudes aren't surfaced here (dashboard display is a follow-up);
                // the backtest reports only the per-zone internal gains, unchanged.
                Ok(ActiveBacktest {
                    before,
                    after,
                    gains_w: fit.gains,
                })
            })
            .await
        })
        .await
    } else {
        if let Some(hit) = cache_hit(&s, &key, CACHE_TTL) {
            return Ok(hit);
        }
        let sup = Arc::clone(&s);
        let key2 = key.clone();
        cached(&s, key, || async move {
            let state = Arc::clone(&sup);
            supervised_backtest(state, key2, async move {
                let kalman = if x0_kalman {
                    sup.kalman.get().map(|a| a.as_ref())
                } else {
                    None
                };
                let full = backtest_passive_detail(
                    &sup.db,
                    &sup.net,
                    &sup.ss,
                    sup.latitude,
                    sup.longitude,
                    &cfg,
                    kalman,
                )
                .await?;
                // One shape per key: the scores alone (the historical response) or the full detail.
                Ok(if detail {
                    serde_json::to_value(full)?
                } else {
                    serde_json::to_value(full.scores)?
                })
            })
            .await
        })
        .await
    }
}

/// The live internal gains + the config baseline they're refining.
async fn get_calibration_gains(State(s): State<Shared>) -> Json<Value> {
    let live = lock(&s.gains).clone();
    // The envelope must describe the GAINS, not this request. The fit runs on its own slow cadence
    // (`internal_gain_recalibrate_hours`, and it retains the last-good result across failures), so
    // stamping `computed_at = now, age = 0` claimed a day-old fit was fresh — exactly the staleness
    // the envelope exists to expose. Fall back to now() only when no fit has landed yet.
    let (computed_at, age) = match live.as_ref().map(|g| g.fitted_at) {
        Some(at) => (at, (Utc::now() - at).num_seconds().max(0) as u64),
        None => (Utc::now(), 0),
    };
    let data = json!({
        "live": live,
        "config_baseline_w": s.config.heating.internal_gains(),
        "recalibrate_hours": s.config.internal_gain_recalibrate_hours,
        "window_days": s.config.internal_gain_window_days,
    });
    envelope(computed_at, age, data)
}

async fn get_forecast_validation(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    cached(&s, "forecast_validation".into(), || {
        crate::forecast_validation::validate(&s.db)
    })
    .await
}

/// Measured current telemetry (PV / grid / house / battery / SoC / outside temp) for the dashboard's
/// live energy flow. Cached for a short TTL (LIVE_TTL, 5 s) behind the single-flight gate — collapsing concurrent pollers onto one read — and timeout-bounded like the rest.
async fn get_live(State(s): State<Shared>) -> Result<Json<Value>, ApiError> {
    // Behind the shared cache with a SHORT TTL. It is the one "live" endpoint, so it must not be
    // stale — but it was also the only one bypassing the single-flight gate, and it is polled every
    // 10 s by four of the seven screens with no re-entrancy guard: several open tabs each ran their
    // own eight sequential Growatt reads. A few seconds of sharing costs nothing at a feed that
    // updates every few seconds, and collapses N concurrent pollers onto one read.
    cached_for(&s, "live".into(), LIVE_TTL, || {
        crate::live::read_live(&s.db, &s.config)
    })
    .await
}

/// Per-zone comfort band + heater limit + internal gain — the static house definition the dashboard
/// needs to shade comfort bands and label heating. From `config.heating` (no secrets).
async fn get_zones(State(s): State<Shared>) -> Json<Value> {
    build_zones(&s.config, Utc::now())
}

/// Pure builder behind [`get_zones`], split out so it's testable from a fixture config with no
/// `Shared`/DB/network needed.
fn build_zones(config: &ControlConfig, now: DateTime<Utc>) -> Json<Value> {
    // The band the schedule makes effective RIGHT NOW, resolved server-side in the site's local time
    // with the very same `band_at` the optimizer uses. Clients were shipping the static `t_min`
    // only, so a bedroom correctly gliding to its night-setback floor was labelled "cold" and
    // sorted to the top of the comfort list — the optimizer honouring the schedule, reported as a
    // violation. Reimplementing the window semantics (later-wins, wrap past midnight, DST) in JS
    // would have been a second source of truth; this cannot drift.
    let minute_now = {
        let local = now.with_timezone(&config.site.offset_at(now));
        local.hour() * 60 + local.minute()
    };
    // Heated ∪ HVAC-served, exactly like the LP's `controlled` set. Iterating `heating.zones` alone
    // made an HVAC-only cooling room invisible to the whole dashboard (the comfort grid, band bars,
    // sparklines and the heating screen all read this array), and gave a heat+HVAC room the heating
    // `t_max` as its ceiling instead of the `t_cool` the optimizer actually constrains.
    let hvac = config.hvac.as_ref();
    let mut names: Vec<&String> = config.heating.zones.keys().collect();
    names.extend(hvac.iter().flat_map(|h| h.comfort.keys()));
    // `served_zones()` (units-based) ALSO covers a zone with no `hvac.comfort` entry of its own
    // that relies entirely on `hvac.default_comfort` — `comfort.keys()` alone would hide it, and
    // `h.comfort[zone]` below would panic on it. Bound to a `let` so its borrow outlives `names`.
    let served: Vec<String> = hvac.map(|h| h.served_zones()).unwrap_or_default();
    names.extend(served.iter());
    names.sort();
    names.dedup();
    let mut zones: Vec<Value> = names
        .into_iter()
        .map(|zone| {
            let heated = config.heating.zones.get(zone);
            // The one place that resolves per-zone HVAC comfort (entry + `default_comfort` +
            // underfloor `t_heat` fallback, field by field) — everything below reads from THIS,
            // never `hvac.comfort[zone]` directly, so a default_comfort-only zone can't panic here.
            let resolved = hvac.and_then(|h| h.effective_comfort(zone, &config.heating));
            let hvac_served = resolved.is_some();
            let (t_min_now, t_max_now) = crate::optimize::config::comfort_band(
                &config.heating,
                hvac,
                zone,
                minute_now,
                heated.is_some(),
                hvac_served,
            )
            .unwrap_or((f64::NAN, f64::NAN));
            // The STATIC band a client falls back to: the heating limits for a heated zone, the HVAC
            // deadband for an HVAC-only one.
            let (t_min, t_max) = match (heated, &resolved) {
                (Some(c), None) => (c.t_min, c.t_max),
                (Some(c), Some(hc)) => (c.t_min, hc.t_cool),
                (None, Some(hc)) => (hc.t_heat, hc.t_cool),
                (None, None) => (f64::NAN, f64::NAN),
            };
            let c = heated;
            // The overheat tier only applies to underfloor-heated zones (validated at load: never
            // set on an HVAC-served one), so an absent/non-heated zone reports 0 — today's
            // single-tier band exactly, matching `overheat_c`'s own "0 ⇒ today's band" default.
            let overheat_c = c.map_or(0.0, |c| c.overheat_c);
            json!({
                "zone": zone,
                "t_min": t_min,
                "t_max": t_max,
                "heated": c.is_some(),
                "hvac": hvac_served,
                /* The scheduled band in force at `computed_at` (equal to t_min/t_max outside any
                   window) — what a client should shade and judge comfort against. */
                "t_min_now": t_min_now,
                "t_max_now": t_max_now,
                // Extra K of slab-heat headroom above t_max_now this zone may bank into (0 when
                // unset) — see `ZoneComfort::overheat_c`. `t_max_boost_now` is the derived ceiling
                // so a client never has to re-implement schedule/overheat resolution itself.
                "overheat_c": overheat_c,
                "t_max_boost_now": t_max_now + overheat_c,
                // Daily band-override windows (night setback etc.), so a client can shade the
                // SCHEDULED band — a zone gliding below the static t_min inside a setback window
                // is the optimizer honoring the schedule, not a comfort violation.
                "windows": c.map(|c| c.windows.iter().map(|w| json!({
                    "start": w.start,
                    "end": w.end,
                    "t_min": w.t_min,
                    "t_max": w.t_max,
                })).collect::<Vec<_>>()).unwrap_or_default(),
                "max_heat_kw": c.map(|c| c.max_heat_kw),
                "internal_gain_w": c.map(|c| c.internal_gain_w),
            })
        })
        .collect();
    zones.sort_by(|a, b| a["zone"].as_str().cmp(&b["zone"].as_str()));
    envelope(now, 0, Value::Array(zones))
}

/// The building **envelope** — zones + the boundaries between them with area, orientation,
/// construction (layer stack), and U-value. Static (built once at startup from the model), so it is
/// served straight from state with no DB or recompute.
async fn get_topology(State(s): State<Shared>) -> Json<Value> {
    let mut data = json!(s.topology);
    // The configured slab/ground boundary temperature, so the dashboard's ground-loss ΔT uses the
    // real value rather than a hardcoded constant.
    data["ground_temperature_c"] = json!(s.config.site.ground_temperature_c);
    envelope(s.started_at, 0, data)
}

#[derive(Debug, Deserialize)]
struct SolarParams {
    /// `now` scales the clear-sky model by the current cloud fraction from the live weather
    /// forecast (the same feed the planner reads). Anything else, including absent, is `clear` —
    /// today's unchanged clear-sky behaviour.
    sky: Option<String>,
}

/// The `weather_cloud_series` feed reports PERCENT (0..100), not a 0..1 fraction — mirrors
/// `live_inputs.rs::forecast_series` and `estimate.rs::seed_state`'s identical `pct / 100.0`
/// (rework cycle 5, item 5 / refuter finding 5: `current_cloud_fraction` previously clamped the raw
/// percent straight to `0.0..=1.0`, so any cloud reading of 1% or more rendered as fully overcast —
/// `beam_w: 0.0` even at a real 82% cloud fraction, live on the shadow brain).
fn cloud_pct_to_fraction(pct: f64) -> f64 {
    (pct / 100.0).clamp(0.0, 1.0)
}

/// Best-effort current cloud fraction (0..1) from the live weather forecast, for `?sky=now`.
/// `None` on any DB/parse hiccup or an empty series — the caller then falls back to clear-sky, so a
/// dead weather feed degrades `?sky=now` to `?sky=clear` rather than erroring the request.
async fn current_cloud_fraction(db: &SourceClients) -> Option<f64> {
    let now = Utc::now();
    let start = (now - ChronoDuration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let stop = (now + ChronoDuration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let series = db.weather_cloud_series(&start, &stop, "1h").await.ok()?;
    series.last().map(|s| cloud_pct_to_fraction(s.value))
}

/// Live per-surface **solar gain**: for each oriented exterior boundary, the irradiance now (W/m²,
/// split into `beam`/`diffuse`, plus `total_w` = today's `solar_w`) and the heat it injects (W),
/// plus the sun's position. Opaque `Layered` surfaces ABSORB (irradiance × absorptance × area, at
/// the outer surface); `Simple` panes TRANSMIT (irradiance × g × area, into the zone — the RC
/// network's `WindowSurface` path, typically the house's dominant solar gain). Each row is tagged
/// with its `mode`. Clear-sky by default (`?sky=clear`, cloud not applied) so it reads the
/// orientation effect — which faces are catching sun; `?sky=now` scales by the live cloud fraction.
async fn get_solar(State(s): State<Shared>, Query(q): Query<SolarParams>) -> Json<Value> {
    let now = Utc::now();
    let (az, el) = sun_azimuth_elevation(s.latitude, s.longitude, &now);
    // `sky_now` is only true when the caller asked for `now` AND the live cloud feed actually
    // answered — an unavailable feed silently degrades to `clear`, reported honestly in the
    // response's `sky` field rather than claiming a cloud model that didn't run.
    let cloud_now = if q.sky.as_deref() == Some("now") {
        current_cloud_fraction(&s.db).await
    } else {
        None
    };
    let sky_now = cloud_now.is_some();
    let cloud = cloud_now.unwrap_or(0.0);
    let boundaries: Vec<Value> = s
        .topology
        .boundaries
        .iter()
        .filter_map(|b| {
            let (azimuth, tilt) = (b.azimuth_deg?, b.tilt_deg?);
            // Absorbed at an opaque surface, or transmitted through glazing — a boundary with
            // neither coefficient (or g = 0) injects nothing, matching the RC network.
            let (factor, mode) = match (b.solar_absorptance, b.solar_g) {
                (Some(a), _) => (a, "absorbed"),
                (None, Some(g)) if g > 0.0 => (g, "transmitted"),
                _ => return None,
            };
            // And, like the RC network, only surfaces that actually face `outside` receive solar — not
            // an oriented ground/interior surface (inert today, but keeps the rule identical).
            if b.zone_a != "outside" && b.zone_b != "outside" {
                return None;
            }
            let c = tilted_irradiance_components(
                s.latitude,
                s.longitude,
                &now,
                SolarInput::Cloud { cloud },
                Angle::new::<degree>(tilt),
                Angle::new::<degree>(azimuth),
            );
            let beam_wm2 = c.beam.get::<watt_per_square_meter>();
            let diffuse_wm2 = c.diffuse.get::<watt_per_square_meter>();
            let irradiance =
                (beam_wm2 + diffuse_wm2 + c.reflected.get::<watt_per_square_meter>()).max(0.0);
            let solar_w = irradiance * factor * b.area_m2;
            Some(json!({
                "id": b.id,
                "irradiance_wm2": irradiance,
                "solar_w": solar_w,
                "beam_w": beam_wm2 * factor * b.area_m2,
                "diffuse_w": diffuse_wm2 * factor * b.area_m2,
                "total_w": solar_w,
                "cos_incidence": c.cos_incidence,
                "mode": mode,
            }))
        })
        .collect();
    envelope(
        now,
        0,
        json!({
            "sky": if sky_now { "now" } else { "clear" },
            "sun": { "azimuth_deg": az, "elevation_deg": el, "up": el > 0.0 },
            "boundaries": boundaries,
        }),
    )
}

// The dashboard is a self-contained single-page app embedded in the binary (no extra mounts). It
// reads the JSON API above; ECharts is vendored (not a CDN), so it works fully offline.
const DASHBOARD_HTML: &str = include_str!("dashboard/index.html");
const DASHBOARD_CSS: &str = include_str!("dashboard/style.css");
const DASHBOARD_JS: &str = include_str!("dashboard/app.js");
const DASHBOARD_ECHARTS: &str = include_str!("dashboard/echarts.min.js");

/// Serve one dashboard asset. If `MPC_DASHBOARD_DIR` is set and holds `name`, that file wins —
/// UI iteration without restarting the brain (bind-mount the dir read-only and copy files in);
/// otherwise the embedded copy ships. `name` is one of four fixed literals below, never
/// request-derived, so there is no traversal surface.
async fn asset(
    name: &str,
    content_type: &'static str,
    cache: Option<&'static str>,
    embedded: &'static str,
) -> Response {
    let body = match std::env::var("MPC_DASHBOARD_DIR") {
        Ok(dir) => tokio::fs::read_to_string(std::path::Path::new(&dir).join(name))
            .await
            .unwrap_or_else(|_| embedded.to_string()),
        Err(_) => embedded.to_string(),
    };
    let mut resp = ([(header::CONTENT_TYPE, content_type)], body).into_response();
    if let Some(c) = cache {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    resp
}

const JS: &str = "application/javascript; charset=utf-8";
async fn dashboard_html() -> Response {
    asset(
        "index.html",
        "text/html; charset=utf-8",
        None,
        DASHBOARD_HTML,
    )
    .await
}
async fn dashboard_css() -> Response {
    asset("style.css", "text/css; charset=utf-8", None, DASHBOARD_CSS).await
}
async fn dashboard_js() -> Response {
    asset("app.js", JS, None, DASHBOARD_JS).await
}
async fn dashboard_echarts() -> Response {
    asset(
        "echarts.min.js",
        JS,
        Some("public, max-age=86400"),
        DASHBOARD_ECHARTS,
    )
    .await
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
        .route("/api/model/topology", get(get_topology))
        .route("/api/model/solar", get(get_solar))
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
            get(get_ev_pref).post(post_ev_pref).delete(delete_ev_pref),
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
    // Supervised: a panic anywhere in a loop tick (the solver runs under spawn_blocking, but the
    // estimator/forecast/gain-fit code runs on the task itself) would otherwise kill re-planning
    // silently and permanently while the web server — and /livez — stay green. Respawn with a
    // backoff; each incarnation re-publishes to the same `latest` store, so nothing is lost.
    let loop_state = shared.clone();
    tokio::spawn(async move {
        loop {
            match tokio::spawn(crate::mpc_loop::run(loop_state.clone(), tick)).await {
                // run() loops forever; a clean return would mean deliberate shutdown.
                Ok(()) => break,
                Err(e) => {
                    eprintln!("[mpc] LOOP TASK DIED ({e}); restarting in 60 s");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
        }
    });
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

    /// Rework cycle 5, item 5 (refuter finding 5): `weather_cloud_series` reports PERCENT (0..100),
    /// the same feed `live_inputs.rs`/`estimate.rs` both divide by 100 — `cloud_pct_to_fraction`
    /// must do the same, not clamp the raw percent straight to a 0..1 fraction (the cycle-4 bug,
    /// which rendered any cloud reading of 1% or more as fully overcast).
    #[test]
    fn cloud_pct_to_fraction_divides_by_100_not_clamps() {
        assert!((cloud_pct_to_fraction(82.0) - 0.82).abs() < 1e-9);
        assert_eq!(cloud_pct_to_fraction(0.0), 0.0);
        assert_eq!(cloud_pct_to_fraction(100.0), 1.0);
        // Out-of-range inputs still clamp, same as before.
        assert_eq!(cloud_pct_to_fraction(150.0), 1.0);
        assert_eq!(cloud_pct_to_fraction(-10.0), 0.0);
    }

    /// The practical consequence of the bug above, at the exact reading the refuter found live
    /// (82% cloud, 10:00): fed through the OLD (buggy) conversion, `SolarInput::Cloud { cloud: 82.0
    /// clamped to 1.0 }` is fully overcast and zeroes `beam`; fed through the FIXED conversion
    /// (`cloud: 0.82`), a sun well above the horizon must still show a nonzero, merely attenuated
    /// beam component — "beam scaled, not zero".
    #[test]
    fn sky_now_at_82_percent_cloud_scales_beam_instead_of_zeroing_it() {
        use crate::tools::sun::{tilted_irradiance_components, SolarInput};
        use chrono::DateTime;
        use uom::si::angle::degree;
        use uom::si::f64::Angle;
        use uom::si::heat_flux_density::watt_per_square_meter;

        // Well above the horizon, roughly south-facing surface, summer midday — a scenario with
        // real beam irradiance to attenuate.
        let when = DateTime::parse_from_rfc3339("2026-06-23T11:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let tilt = Angle::new::<degree>(30.0);
        let azimuth = Angle::new::<degree>(180.0);
        let lat = Angle::new::<degree>(49.5);
        let lon = Angle::new::<degree>(17.4);

        let fixed = tilted_irradiance_components(
            lat,
            lon,
            &when,
            SolarInput::Cloud {
                cloud: cloud_pct_to_fraction(82.0),
            },
            tilt,
            azimuth,
        );
        let buggy = tilted_irradiance_components(
            lat,
            lon,
            &when,
            SolarInput::Cloud {
                cloud: 82.0_f64.clamp(0.0, 1.0), // the cycle-4 bug: raw percent clamped, not divided
            },
            tilt,
            azimuth,
        );

        assert_eq!(
            buggy.beam.get::<watt_per_square_meter>(),
            0.0,
            "sanity: the OLD conversion must fully zero beam at any cloud >= 1%"
        );
        assert!(
            fixed.beam.get::<watt_per_square_meter>() > 1.0,
            "the FIXED conversion must leave a genuinely scaled (nonzero) beam component: {}",
            fixed.beam.get::<watt_per_square_meter>()
        );
    }

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

    /// `/api/zones` reports `overheat_c`/`t_max_boost_now` for a zone that has it configured, and
    /// 0/`t_max_now` (no boost) for one that doesn't — built straight from a fixture config, no
    /// `Shared`/DB needed (`build_zones` is the pure part of the handler).
    #[test]
    fn zones_report_overheat_allowance_only_where_configured() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut f,
            br#"{
                site: { latitude: 49.5, longitude: 17.4, utc_offset_hours: 2 },
                heating: {
                    cop: 1.0,
                    comfort_penalty: 5.0,
                    zones: {
                        livingroom: { max_heat_kw: 3.0, t_min: 21.0, t_max: 24.0, overheat_c: 1.0 },
                        office: { max_heat_kw: 0.82, t_min: 21.0, t_max: 24.0 },
                    },
                },
            }"#,
        )
        .unwrap();
        let config = ControlConfig::load(f.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-06-23T11:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let Json(v) = build_zones(&config, now);
        let by_zone = |z: &str| {
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["zone"] == z)
                .unwrap()
                .clone()
        };

        let lr = by_zone("livingroom");
        assert_eq!(lr["overheat_c"], 1.0);
        assert_eq!(
            lr["t_max_boost_now"].as_f64().unwrap(),
            lr["t_max_now"].as_f64().unwrap() + 1.0
        );

        let office = by_zone("office");
        assert_eq!(office["overheat_c"], 0.0);
        assert_eq!(
            office["t_max_boost_now"].as_f64().unwrap(),
            office["t_max_now"].as_f64().unwrap()
        );
    }

    /// Brief K / `hvac.default_comfort`: a unit-served zone with NO entry of its own in
    /// `hvac.comfort` — relying entirely on `default_comfort` (+ its own underfloor `t_min` for
    /// `t_heat`, since it's dual-served here) — must show up with a real band, not be silently
    /// dropped (the old `comfort.keys()`-only zone list) or panic (the old `h.comfort[zone]`
    /// direct index).
    #[test]
    fn zones_reports_a_default_comfort_only_zone() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut f,
            br#"{
                site: { latitude: 49.5, longitude: 17.4, utc_offset_hours: 2 },
                heating: {
                    cop: 1.0,
                    comfort_penalty: 5.0,
                    zones: {
                        guestroom: { max_heat_kw: 2.0, t_min: 20.5, t_max: 22.5 },
                    },
                },
                hvac: {
                    default_comfort: { t_cool_min: 23.0, t_cool: 25.0 },
                    units: {
                        guestroom_ac: {
                            zones: ["guestroom"],
                            max_cool_kw: 2.5, max_heat_kw: 0.0,
                            cooling_cop: 3.2, heating_cop: 1.0,
                        },
                    },
                },
            }"#,
        )
        .unwrap();
        let config = ControlConfig::load(f.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-06-23T11:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let Json(v) = build_zones(&config, now); // must not panic
        let guestroom = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["zone"] == "guestroom")
            .unwrap();
        assert_eq!(guestroom["hvac"], true);
        assert_eq!(
            guestroom["t_min"], 20.5,
            "t_heat falls back to underfloor t_min"
        );
        assert_eq!(guestroom["t_max"], 25.0, "t_cool from default_comfort");
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
