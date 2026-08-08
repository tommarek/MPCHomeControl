//! The rolling-horizon MPC loop.
//!
//! On a fixed schedule it re-plans the whole house from the **current measured state** (the
//! receding horizon comes from re-planning with `start = now`; there is no model-state to carry
//! forward — each tick re-estimates from measurements and reads the live battery SoC). It logs the
//! decisions it *would* apply for the coming hour and publishes the latest plan for the web API.
//!
//! **Read-only loop.** It never actuates or writes InfluxDB itself — it only publishes the plan to the
//! API. Downstream, the controllers (growatt battery, loxone heating/EV) consume that plan and
//! drive the house; `loxone_smart_home` keeps the domains not yet cut over.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::app::{
    build_cache, current_plan, fit_live_internal_gains, GainsSnapshot, PlanCache, PlanExtras,
    PlanReport, ScheduledFit, TimestampedPlan,
};
use crate::forecast_validation::{append_snapshot, Snapshot};
use crate::optimize::config::GainProfile;
use crate::tools::sort_desc_by_key;
use crate::web::AppState;

/// How long the cached slow inputs (consumption model, PV calibration) stay fresh before a rebuild —
/// they're trained from days of history, so the per-minute re-plans reuse them.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Retry cadence for a cache that was built on fallbacks (neutral calibration / flat consumption
/// after a DB blip) — degraded inputs shouldn't be honored for the full [`CACHE_TTL`].
const DEGRADED_CACHE_RETRY: Duration = Duration::from_secs(2 * 60);

/// After a failed internal-gain re-fit, wait at least this long before retrying — short enough to
/// recover quickly from a transient DB blip, long enough not to hammer the DB during a real outage.
const GAIN_REFIT_RETRY: Duration = Duration::from_secs(15 * 60);
/// Consecutive fast retries allowed while the slow inputs stay degraded, before accepting the
/// condition and resuming the normal [`CACHE_TTL`] (a persistent fallback is not fixable by retrying).
const MAX_DEGRADED_RETRIES: usize = 3;

/// Run the loop forever: every `tick`, re-plan and publish. Planning failures are logged and the
/// loop continues (the previous published plan stays available).
pub async fn run(state: Arc<AppState>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    // A tick that overruns (degraded DB, solver timeout) must NOT be followed by a burst of
    // queued back-to-back re-plans against the already-struggling backend — one tick per period.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cache: Option<(Instant, PlanCache)> = None;
    // Consecutive degraded slow-input rebuilds; see the `cache_ttl` comment below.
    let mut degraded_retries: usize = 0;
    // The heating relays decided at the current 15-min block's start, held for its 15 minutes so the
    // relays don't flip mid-block under the per-minute re-planning (a minimum on/off time).
    // Seeded from the already-published plan so a supervisor respawn (loop panic) inside a block
    // resumes the same hold instead of re-deciding the relays mid-block.
    let mut committed: Option<(DateTime<Utc>, HashMap<String, f64>)> = {
        let latest = crate::web::lock_latest(&state);
        latest
            .filter(|tp| !tp.plan.degraded && !tp.plan.relaxed)
            .map(|tp| {
                (
                    tp.plan.first_step.hour_start,
                    tp.plan.first_step.heat_kw.clone(),
                )
            })
    };

    // Per controllable load: hours already run inside the window occurrence in progress, and the
    // block that tally belongs to. Only block 0 is ever actuated and the loop re-plans every minute,
    // so without this the LP would re-decide the whole occurrence from scratch each tick — either
    // dropping the requirement for the current window entirely or re-running an appliance that has
    // already had its hours. Reset when the load leaves its window (a fresh occurrence starts) and,
    // like `committed`, advanced only when the block moves FORWARD, so a within-block re-plan or a
    // backward clock step cannot double-count the same block. In-memory: a restart mid-window
    // forgets, which lets the load run its target again — the safe direction (never starved).
    let mut load_run: HashMap<String, f64> = HashMap::new();
    let mut load_run_block: Option<DateTime<Utc>> = None;

    // Live internal-gain self-correction: re-fit from a trailing window on a slow cadence (the gains
    // drift only as occupant behaviour does), seeded from the calibrated config values until the
    // first fit lands. `internal_gain_recalibrate_hours == 0` pins them to the config values. The same
    // fit learns each scheduled load's magnitude (W), held alongside the gains and stamped into the
    // cache so the plan applies it.
    // Seed from the last published snapshot when there is one: the loop can be respawned within a
    // live process (supervisor restart after a panic), and starting from the CONFIG baseline threw
    // away a landed fit — the plan would then run on stale-by-months gains for up to a full
    // `internal_gain_recalibrate_hours` before the next re-fit. `gains_at` stays `None` so the first
    // tick still re-fits; this only decides what the plan uses in the meantime.
    let published = state
        .gains
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut gains: HashMap<String, GainProfile> = match &published {
        Some(snap) => snap.gains_w.clone(),
        None => state.config.heating.internal_gains(),
    };
    // Seed with the configured magnitudes (a fixed `power_w` is used as-is; a fitted load starts at 0)
    // so the plan applies the known draws even before the first re-fit lands.
    // Same for the scheduled-load magnitudes; the snapshot is aligned to `config.scheduled_loads`
    // by construction, so only adopt it when the lengths still agree (a config edit invalidates it).
    let mut scheduled_w: Vec<f64> = state
        .config
        .scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
        .collect();
    if let Some(snap) = &published {
        if snap.scheduled.len() == scheduled_w.len() {
            scheduled_w = snap.scheduled.iter().map(|f| f.magnitude_w).collect();
        }
    }
    let mut gains_at: Option<Instant> = None; // last *successful* re-fit
    let mut last_attempt: Option<Instant> = None; // last attempt (gates the failure back-off)
    let gain_interval = Duration::from_secs(
        state
            .config
            .internal_gain_recalibrate_hours
            .saturating_mul(3600),
    );

    // Forward-prediction snapshots for the validation scorecard, captured on their own cadence.
    let mut last_snapshot: Option<Instant> = None;
    let snapshot_interval =
        Duration::from_secs(state.config.forecast_snapshot_minutes.saturating_mul(60));

    loop {
        interval.tick().await; // fires immediately, then every `tick`

        // Re-fit the internal gains on their own (slow) cadence, independent of the plan cache. After
        // a failure, retry on a short back-off (not every tick — the DB may be down — and not the
        // full interval); keep the last-good gains meanwhile. An empty fit is trusted. A zero
        // `internal_gain_recalibrate_hours` pins the gains to the config values.
        let due = gains_at.is_none_or(|t| t.elapsed() >= gain_interval);
        let retry_ok = last_attempt.is_none_or(|t| t.elapsed() >= GAIN_REFIT_RETRY);
        if !gain_interval.is_zero() && due && retry_ok {
            last_attempt = Some(Instant::now());
            if let Some(fitted) = fit_live_internal_gains(
                &state.db,
                &state.net,
                &state.ss,
                &state.config,
                state.latitude,
                state.longitude,
            )
            .await
            {
                log_gains(&fitted.gains);
                gains = fitted.gains;
                // Align defensively to the configured load count (the fit returns exactly that). On a
                // length mismatch, fall back to the configured magnitudes (fixed used as-is, fitted 0).
                scheduled_w = if fitted.scheduled_w.len() == state.config.scheduled_loads.len() {
                    fitted.scheduled_w
                } else {
                    state
                        .config
                        .scheduled_loads
                        .iter()
                        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
                        .collect()
                };
                gains_at = Some(Instant::now());
                // Surface each scheduled-load magnitude in use, tagged configured vs fitted, for
                // `/api/calibration/gains` → `live.scheduled`.
                let scheduled: Vec<ScheduledFit> = state
                    .config
                    .scheduled_loads
                    .iter()
                    .zip(&scheduled_w)
                    .map(|(load, &w)| ScheduledFit {
                        label: if load.label.is_empty() {
                            load.zone.clone()
                        } else {
                            load.label.clone()
                        },
                        zone: load.zone.clone(),
                        // A sensor-driven load's flux is the *measured* draw (not in `scheduled_w`, which
                        // the fit leaves untouched for it); report the configured forecast magnitude.
                        magnitude_w: if load.sensor.is_some() {
                            load.power_w.unwrap_or(0.0) * load.power_factor.unwrap_or(1.0)
                        } else {
                            w
                        },
                        source: if load.sensor.is_some() {
                            "measured".to_string()
                        } else if load.power_w.is_some() {
                            "configured".to_string()
                        } else {
                            "fitted".to_string()
                        },
                    })
                    .collect();
                *state.gains.lock().unwrap_or_else(|e| e.into_inner()) = Some(GainsSnapshot {
                    fitted_at: Utc::now(),
                    window_days: state.config.internal_gain_window_days,
                    gains_w: gains.clone(),
                    scheduled,
                });
            }
        }

        // Refresh the slow inputs periodically; the per-minute re-plans reuse them and re-read only
        // the fast state (zone temps, SoC) and the horizon forecasts. A cache built on fallbacks
        // (a DB blip at refresh time → neutral calibration / flat consumption) retries on a short
        // back-off instead of serving degraded inputs for the full TTL.
        //
        // The fast retry is BOUNDED: not every fallback is transient. "PV calibration (scored hours
        // < 24; neutral)" and friends persist indefinitely on a house with thin history, and an
        // unbounded fast path would then rebuild the slow inputs every 2 min instead of every 15 —
        // ~7.5x the DB load, forever, for a condition no retry can fix. After
        // MAX_DEGRADED_RETRIES consecutive degraded rebuilds we accept it and resume the full TTL;
        // any clean rebuild resets the budget.
        let cache_ttl = |c: &PlanCache, retries: usize| {
            if c.fallbacks.is_empty() || retries >= MAX_DEGRADED_RETRIES {
                CACHE_TTL
            } else {
                DEGRADED_CACHE_RETRY
            }
        };
        if cache
            .as_ref()
            .is_none_or(|(t, c)| t.elapsed() >= cache_ttl(c, degraded_retries))
        {
            // Hand the outgoing cache in so a component that fails to refresh keeps its last good
            // value rather than collapsing to a fabricated fallback the loop would then actuate.
            let fresh = build_cache(
                &state.db,
                &state.net,
                &state.config,
                cache.as_ref().map(|(_, c)| c),
            )
            .await;
            if fresh.fallbacks.is_empty() {
                degraded_retries = 0;
            } else {
                degraded_retries += 1;
                if degraded_retries == MAX_DEGRADED_RETRIES {
                    eprintln!(
                        "[mpc] slow inputs still degraded after {MAX_DEGRADED_RETRIES} fast \
                         retries ({:?}) — backing off to the normal {}s refresh",
                        fresh.fallbacks,
                        CACHE_TTL.as_secs()
                    );
                }
            }
            cache = Some((Instant::now(), fresh));
        }
        // Stamp the current live gains + scheduled-load magnitudes into the cache so the plan uses
        // them (cheap clones).
        if let Some((_, c)) = cache.as_mut() {
            c.internal_gains = gains.clone();
            c.scheduled_w = scheduled_w.clone();
        }
        let cached = cache.as_ref().map(|(_, c)| c);

        match current_plan(
            &state.db,
            &state.net,
            &state.ss,
            &state.config,
            state.latitude,
            state.longitude,
            PlanExtras {
                cache: cached,
                loop_caller: true,
                // The current block's committed relays are fixed INTO the LP (current_plan
                // forwards them only when its block 0 matches), so first_step, the timeline and
                // both armed controllers agree by construction — no post-hoc patch.
                committed_heat: committed.clone(),
                kernels: Some(state.kernels.clone()),
                kalman: state.kalman.get().cloned(),
                load_run_hours: load_run.clone(),
            },
        )
        .await
        {
            Ok(plan) => {
                // Latch the relays for the current block: decided fresh at the block start, then
                // held for the rest of the block so the minute re-plans can't sub-cycle them. Re-latch
                // only when the block moves *forward* (`block > b`); a same-or-earlier block start — a
                // within-block re-plan, or a backward wall-clock step (NTP) — holds the committed
                // relays rather than recomputing them. The commitment is enforced inside the LP
                // (see PlanExtras::committed_heat), so nothing is patched here.
                let block = plan.first_step.hour_start;
                match &committed {
                    Some((b, _)) if block <= *b => {}
                    // Never latch from a degraded or relaxed plan: the publisher refused to
                    // actuate it, so its (possibly fictional / fractional) relays are NOT what the
                    // house is holding — pinning them into the next strict solve would be wrong.
                    _ if plan.degraded || plan.relaxed => {}
                    _ => committed = Some((block, plan.first_step.heat_kw.clone())),
                }
                // Bank the block we are about to actuate, once per block — and ONLY from a plan the
                // publisher will actually send, exactly like the relay latch above. A degraded or
                // relaxed plan is skipped wholesale downstream, so banking its (often fractional)
                // `on` credited run-time the appliance never got. Since `already_run_hours` sets the
                // occurrence's upper CAP as well as its demand, over-banking forces the load OFF for
                // the rest of the night with no shortfall to signal it: a degraded stretch at the
                // start of a boiler window silently cost the whole night's hot water. Leaving
                // `load_run_block` unadvanced is right — the first strict plan in the same block
                // banks it.
                let dt_h = 1.0 / crate::app::BLOCKS_PER_HOUR as f64;
                if !plan.degraded && !plan.relaxed && load_run_block.is_none_or(|b| block > b) {
                    load_run_block = Some(block);
                    for (name, &kw) in &plan.first_step.controllable_load_kw {
                        // The publisher only switches a load on above its own threshold; a
                        // near-zero relaxed draw is not a run.
                        if kw > 0.5 * rated_kw(&state.config, name) {
                            *load_run.entry(name.clone()).or_insert(0.0) += dt_h;
                        }
                    }
                }
                // A load that is no longer inside a window has finished that occurrence: forget
                // the tally, so the NEXT one starts from its full target. Evaluated with the load's
                // own `unit_profile` at the block's local time — the same rule that built the LP's
                // window mask.
                let local = block.with_timezone(&state.config.site.offset_at(block));
                let minute = local.hour() * 60 + local.minute();
                for l in &state.config.scheduled_loads {
                    if l.controllable && l.unit_profile(local.month(), minute) == 0.0 {
                        load_run.remove(&crate::optimize::coordinator::load_name(l));
                    }
                }
                log_decision(&plan);
                // Snapshot the forward temperature prediction on its own cadence (for the
                // validation scorecard) before the plan is moved into the published store.
                // Only strict, fully-fed plans enter the validation history: a degraded/relaxed
                // plan predicts from fallback inputs and is never actuated, so scoring it would
                // charge input-outage error to the thermal model (same rationale as the relay
                // latch above).
                if !snapshot_interval.is_zero()
                    && !plan.degraded
                    && !plan.relaxed
                    && last_snapshot.is_none_or(|t| t.elapsed() >= snapshot_interval)
                {
                    // `append_snapshot` reads, parses, re-serializes and rewrites the whole ~90 KB
                    // history; the store is a bind-mounted file, so on a contended volume that
                    // blocks a tokio worker and stalls unrelated HTTP handlers. Off-thread it goes,
                    // as `web.rs` already does for the far smaller EV-preference file.
                    let written = match Snapshot::from_plan(&plan) {
                        Some(snap) => tokio::task::spawn_blocking(move || append_snapshot(snap))
                            .await
                            .unwrap_or_else(|e| Err(anyhow::anyhow!("snapshot task: {e}"))),
                        None => Ok(()), // empty plan: nothing to snapshot
                    };
                    // Only advance the clock on a real write, so a transient failure retries.
                    match written {
                        Ok(()) => last_snapshot = Some(Instant::now()),
                        Err(e) => eprintln!("[mpc] forecast snapshot write failed: {e}"),
                    }
                }
                *state.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(TimestampedPlan {
                    computed_at: Utc::now(),
                    published: Instant::now(),
                    plan,
                });
            }
            Err(e) => eprintln!("[mpc] planning failed: {e}"),
        }
    }
}

/// Log the controls the optimizer chose for the coming hour (what a controller would apply).
fn log_decision(plan: &PlanReport) {
    let fs = &plan.first_step;
    let heat_kw: f64 = fs.heat_kw.values().sum();
    let battery_kw = fs.battery_discharge_kw - fs.battery_charge_kw; // + = discharging
    println!(
        "[mpc] {}: mode {} (export {}, inverter {}), heat {heat_kw:.1} kW, battery {battery_kw:+.1} kW, grid import {:.1} / export {:.1} kW \
         ({}h cost {:.2} EUR / {:.0} CZK){}",
        fs.hour_start.format("%Y-%m-%d %H:%M UTC"),
        fs.mode.slot,
        if fs.mode.export_enabled { "on" } else { "off" },
        if fs.mode.inverter_on { "on" } else { "off" },
        fs.grid_import_kw,
        fs.grid_export_kw,
        plan.horizon_hours,
        plan.total_cost_eur,
        plan.total_cost_czk,
        if plan.placeholder_inputs.is_empty() {
            String::new()
        } else {
            format!("  [fallbacks: {}]", plan.placeholder_inputs.join("; "))
        },
    );
}

/// A controllable load's rated draw (kW) by plan name, or 0 when it is not configured (then any
/// positive draw counts as a run).
fn rated_kw(config: &crate::optimize::config::ControlConfig, name: &str) -> f64 {
    config
        .scheduled_loads
        .iter()
        .find(|l| crate::optimize::coordinator::load_name(l) == name)
        .and_then(|l| l.power_w)
        .unwrap_or(0.0)
        / 1000.0
}

/// Log the freshly re-fitted per-zone internal gains (the live self-correction), strongest first.
fn log_gains(gains: &HashMap<String, GainProfile>) {
    if gains.is_empty() {
        println!("[mpc] internal-gain re-fit: no extra gain needed in any zone");
        return;
    }
    let mut items: Vec<(&String, &GainProfile)> = gains.iter().collect();
    sort_desc_by_key(&mut items, |it| it.1.evening.max(it.1.day).max(it.1.night));
    let list = items
        .iter()
        .map(|(z, p)| format!("{z} n{:.0}/d{:.0}/e{:.0} W", p.night, p.day, p.evening))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "[mpc] internal-gain re-fit: {list} (evening total {:.0} W)",
        gains.values().map(|p| p.evening).sum::<f64>(),
    );
}
