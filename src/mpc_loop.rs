//! The shadow rolling-horizon MPC loop.
//!
//! On a fixed schedule it re-plans the whole house from the **current measured state** (the
//! receding horizon comes from re-planning with `start = now`; there is no model-state to carry
//! forward — each tick re-estimates from measurements and reads the live battery SoC). It logs the
//! decisions it *would* apply for the coming hour and publishes the latest plan for the web API.
//!
//! **Shadow only.** It never actuates and never writes InfluxDB — the live `loxone_smart_home`
//! still operates the house. This is a confidence-building observer, not a controller.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::app::{
    build_cache, current_plan, fit_live_internal_gains, GainsSnapshot, PlanCache, PlanReport,
    TimestampedPlan,
};
use crate::forecast_validation::{append_snapshot, Snapshot};
use crate::web::AppState;

/// How long the cached slow inputs (consumption model, PV calibration) stay fresh before a rebuild —
/// they're trained from days of history, so the per-minute re-plans reuse them.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// After a failed internal-gain re-fit, wait at least this long before retrying — short enough to
/// recover quickly from a transient DB blip, long enough not to hammer the DB during a real outage.
const GAIN_REFIT_RETRY: Duration = Duration::from_secs(15 * 60);

/// Run the loop forever: every `tick`, re-plan and publish. Planning failures are logged and the
/// loop continues (the previous published plan stays available).
pub async fn run(state: Arc<AppState>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    let mut cache: Option<(Instant, PlanCache)> = None;
    // The heating relays decided at the current 15-min block's start, held for its 15 minutes so the
    // relays don't flip mid-block under the per-minute re-planning (a min on/off time, shadow-side).
    let mut committed: Option<(DateTime<Utc>, HashMap<String, f64>)> = None;

    // Live internal-gain self-correction: re-fit from a trailing window on a slow cadence (the gains
    // drift only as occupant behaviour does), seeded from the calibrated config values until the
    // first fit lands. `internal_gain_recalibrate_hours == 0` pins them to the config values.
    let mut gains: HashMap<String, f64> = state.config.heating.internal_gains();
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
                log_gains(&fitted);
                gains = fitted;
                gains_at = Some(Instant::now());
                *state.gains.lock().unwrap_or_else(|e| e.into_inner()) = Some(GainsSnapshot {
                    fitted_at: Utc::now(),
                    window_days: state.config.internal_gain_window_days,
                    gains_w: gains.clone(),
                });
            }
        }

        // Refresh the slow inputs periodically; the per-minute re-plans reuse them and re-read only
        // the fast state (zone temps, SoC) and the horizon forecasts.
        if cache.as_ref().is_none_or(|(t, _)| t.elapsed() >= CACHE_TTL) {
            cache = Some((Instant::now(), build_cache(&state.db, &state.config).await));
        }
        // Stamp the current live gains into the cache so the plan uses them (cheap map clone).
        if let Some((_, c)) = cache.as_mut() {
            c.internal_gains = gains.clone();
        }
        let cached = cache.as_ref().map(|(_, c)| c);

        match current_plan(
            &state.db,
            &state.net,
            &state.ss,
            &state.config,
            state.latitude,
            state.longitude,
            cached,
        )
        .await
        {
            Ok(mut plan) => {
                // Latch the relays for the current block: decided fresh at the block start, then
                // held for the rest of the block so the minute re-plans can't sub-cycle them.
                let block = plan.first_step.hour_start;
                match &committed {
                    Some((b, relays)) if *b == block => plan.first_step.heat_kw = relays.clone(),
                    _ => committed = Some((block, plan.first_step.heat_kw.clone())),
                }
                log_decision(&plan);
                // Snapshot the forward temperature prediction on its own cadence (for the
                // validation scorecard) before the plan is moved into the published store.
                if !snapshot_interval.is_zero()
                    && last_snapshot.is_none_or(|t| t.elapsed() >= snapshot_interval)
                {
                    match Snapshot::from_plan(&plan).map(append_snapshot) {
                        // Only advance the clock on a real write, so a transient failure retries.
                        Some(Ok(())) => last_snapshot = Some(Instant::now()),
                        Some(Err(e)) => {
                            eprintln!("[mpc shadow] forecast snapshot write failed: {e}")
                        }
                        None => last_snapshot = Some(Instant::now()), // empty plan: nothing to snapshot
                    }
                }
                *state.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(TimestampedPlan {
                    computed_at: Utc::now(),
                    plan,
                });
            }
            Err(e) => eprintln!("[mpc shadow] planning failed: {e}"),
        }
    }
}

/// Log the controls the optimizer chose for the coming hour (what a controller would apply).
fn log_decision(plan: &PlanReport) {
    let fs = &plan.first_step;
    let heat_kw: f64 = fs.heat_kw.values().sum();
    let battery_kw = fs.battery_discharge_kw - fs.battery_charge_kw; // + = discharging
    println!(
        "[mpc shadow] {}: mode {} (export {}, inverter {}), heat {heat_kw:.1} kW, battery {battery_kw:+.1} kW, grid import {:.1} / export {:.1} kW \
         (24 h cost {:.2} EUR / {:.0} CZK){}",
        fs.hour_start.format("%Y-%m-%d %H:%M UTC"),
        fs.mode.slot,
        if fs.mode.export_enabled { "on" } else { "off" },
        if fs.mode.inverter_on { "on" } else { "off" },
        fs.grid_import_kw,
        fs.grid_export_kw,
        plan.total_cost_eur,
        plan.total_cost_czk,
        if plan.placeholder_inputs.is_empty() {
            String::new()
        } else {
            format!("  [fallbacks: {}]", plan.placeholder_inputs.join("; "))
        },
    );
}

/// Log the freshly re-fitted per-zone internal gains (the live self-correction), strongest first.
fn log_gains(gains: &HashMap<String, f64>) {
    if gains.is_empty() {
        println!("[mpc shadow] internal-gain re-fit: no extra gain needed in any zone");
        return;
    }
    let mut items: Vec<(&String, &f64)> = gains.iter().collect();
    items.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    let list = items
        .iter()
        .map(|(z, w)| format!("{z} {w:.0} W"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "[mpc shadow] internal-gain re-fit: {list} (total {:.0} W)",
        gains.values().sum::<f64>(),
    );
}
