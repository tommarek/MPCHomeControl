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

use chrono::{DateTime, Utc};

use crate::app::{
    build_cache, current_plan, fit_live_internal_gains, BiasSnapshot, GainsSnapshot, PlanCache,
    PlanExtras, PlanReport, ScheduledFit, TimestampedPlan,
};
use crate::estimate::hour_key;
use crate::forecast_validation::{append_snapshot, load_snapshots, short_lead_bias, Snapshot};
use crate::optimize::config::{BiasCorrectionConfig, GainProfile};
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

/// Run the loop forever: every `tick`, re-plan and publish. Planning failures are logged and the
/// loop continues (the previous published plan stays available).
pub async fn run(state: Arc<AppState>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    // A tick that overruns (degraded DB, solver timeout) must NOT be followed by a burst of
    // queued back-to-back re-plans against the already-struggling backend — one tick per period.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cache: Option<(Instant, PlanCache)> = None;
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

    // Live internal-gain self-correction: re-fit from a trailing window on a slow cadence (the gains
    // drift only as occupant behaviour does), seeded from the calibrated config values until the
    // first fit lands. `internal_gain_recalibrate_hours == 0` pins them to the config values. The same
    // fit learns each scheduled load's magnitude (W), held alongside the gains and stamped into the
    // cache so the plan applies it.
    let mut gains: HashMap<String, GainProfile> = state.config.heating.internal_gains();
    // Seed with the configured magnitudes (a fixed `power_w` is used as-is; a fitted load starts at 0)
    // so the plan applies the known draws even before the first re-fit lands.
    let mut scheduled_w: Vec<f64> = state
        .config
        .scheduled_loads
        .iter()
        .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
        .collect();
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

    // Fast offset-free bias feedback (config `heating.bias_correction`, default OFF): a per-zone
    // leaky integrator on the mean signed short-lead prediction error, injected into the forward
    // prediction as a small corrective air-node flux. Loop-local state — a restart relearns from
    // zero (deliberate: bounded, fast, and never persisted as if it were calibration).
    let bias_cfg = state.config.heating.bias_correction.clone();
    let mut bias_w: HashMap<String, f64> = HashMap::new();
    let mut last_bias: Option<Instant> = None;
    // Shadow-estimator sample cadence (only used when estimator.mode != anchor).
    let mut last_shadow: Option<Instant> = None;

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
                // A fresh re-fit re-absorbs the steady error the integrator was covering; keeping
                // both would double-count it. Relearn from zero.
                if bias_cfg.enabled && bias_w.values().any(|w| w.abs() > 1e-9) {
                    println!("[mpc] bias feedback reset (internal-gain re-fit landed)");
                    bias_w.clear();
                }
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
        let cache_ttl = |c: &PlanCache| {
            if c.fallbacks.is_empty() {
                CACHE_TTL
            } else {
                DEGRADED_CACHE_RETRY
            }
        };
        if cache
            .as_ref()
            .is_none_or(|(t, c)| t.elapsed() >= cache_ttl(c))
        {
            cache = Some((
                Instant::now(),
                build_cache(&state.db, &state.net, &state.config).await,
            ));
        }
        // Stamp the current live gains + scheduled-load magnitudes into the cache so the plan uses
        // them (cheap clones).
        if let Some((_, c)) = cache.as_mut() {
            c.internal_gains = gains.clone();
            c.scheduled_w = scheduled_w.clone();
            c.bias_w = bias_w.clone();
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
                    match Snapshot::from_plan(&plan).map(append_snapshot) {
                        // Only advance the clock on a real write, so a transient failure retries.
                        Some(Ok(())) => last_snapshot = Some(Instant::now()),
                        Some(Err(e)) => {
                            eprintln!("[mpc] forecast snapshot write failed: {e}")
                        }
                        None => last_snapshot = Some(Instant::now()), // empty plan: nothing to snapshot
                    }
                }
                // Persist a shadow-estimator sample on the snapshot cadence (the Experiments
                // page charts these to judge the shadow period). Same cadence gate as the
                // forecast snapshots; only plans that actually carried a diff.
                if let Some(diff) = &plan.kalman_diff_k {
                    if !snapshot_interval.is_zero()
                        && last_shadow.is_none_or(|t: Instant| t.elapsed() >= snapshot_interval)
                    {
                        let sample = crate::kalman::ShadowSample {
                            t: Utc::now(),
                            diff_k: diff.clone(),
                            disturbance_w: plan.disturbance_w.clone(),
                        };
                        match crate::kalman::append_shadow_sample(sample) {
                            Ok(()) => last_shadow = Some(Instant::now()),
                            Err(e) => eprintln!("[kalman] shadow store write failed: {e}"),
                        }
                    }
                }
                // Update the bias integrator on the snapshot cadence, from strict plans only (a
                // degraded/relaxed plan predicts from fallback inputs — its error is not model
                // bias). Zones need >= 2 scored short-lead points and a fresh sensor hour.
                if bias_cfg.enabled
                    && !plan.degraded
                    && !plan.relaxed
                    && !snapshot_interval.is_zero()
                    && last_bias.is_none_or(|t| t.elapsed() >= snapshot_interval)
                {
                    let now = Utc::now();
                    let lead_min = (bias_cfg.max_lead_hours * 60.0) as i64 + 60;
                    let start = (now - chrono::Duration::minutes(lead_min)).to_rfc3339();
                    let stop = now.to_rfc3339();
                    let mut measured: HashMap<String, HashMap<i64, f64>> = HashMap::new();
                    for zone in state.config.heating.zones.keys() {
                        if let Ok(series) = state
                            .db
                            .read_zone_temperature_series(zone, &start, &stop, "1h")
                            .await
                        {
                            measured.insert(
                                zone.clone(),
                                series.iter().map(|s| (hour_key(s.time), s.value)).collect(),
                            );
                        }
                    }
                    let raw =
                        short_lead_bias(&load_snapshots(), &measured, now, bias_cfg.max_lead_hours);
                    // Integrate over the real elapsed interval (capped: after a long stall, one
                    // update must not apply hours of accumulation at once).
                    let dt_h = last_bias
                        .map(|t| t.elapsed().as_secs_f64() / 3600.0)
                        .unwrap_or_else(|| snapshot_interval.as_secs_f64() / 3600.0)
                        .min(1.0);
                    let mut changed = false;
                    for zone in state.config.heating.zones.keys() {
                        let fresh = measured.get(zone).is_some_and(|m| {
                            m.contains_key(&hour_key(now)) || m.contains_key(&(hour_key(now) - 1))
                        });
                        if let Some(&(mean_k, n)) = raw.get(zone) {
                            if n >= 2 && fresh {
                                let prev = bias_w.get(zone).copied().unwrap_or(0.0);
                                let next = step_bias(prev, mean_k, dt_h, &bias_cfg);
                                changed |= (next - prev).abs() > 1.0;
                                bias_w.insert(zone.clone(), next);
                            }
                        }
                    }
                    if changed {
                        let list = bias_w
                            .iter()
                            .map(|(z, w)| format!("{z} {w:+.0} W"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        println!("[mpc] bias feedback: {list}");
                    }
                    *state.bias.lock().unwrap_or_else(|e| e.into_inner()) = Some(BiasSnapshot {
                        updated_at: now,
                        bias_w: bias_w.clone(),
                        raw_bias_k: raw,
                    });
                    last_bias = Some(Instant::now());
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

/// One update of the leaky-integrator corrective flux (W). `bias_k` is predicted − measured, so a
/// model that runs WARM (positive bias) accumulates a NEGATIVE flux. Decays toward zero with the
/// configured half-life, integrates only the error beyond the deadband, clamps at ±`max_w`
/// (anti-windup). Pure.
pub(crate) fn step_bias(prev_w: f64, bias_k: f64, dt_h: f64, cfg: &BiasCorrectionConfig) -> f64 {
    let decayed = prev_w * 0.5_f64.powf(dt_h / cfg.half_life_hours);
    let excess = (bias_k.abs() - cfg.deadband_k).max(0.0) * bias_k.signum();
    (decayed - cfg.gain_w_per_k_h * excess * dt_h).clamp(-cfg.max_w, cfg.max_w)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BiasCorrectionConfig {
        BiasCorrectionConfig {
            enabled: true,
            gain_w_per_k_h: 60.0,
            max_w: 300.0,
            deadband_k: 0.2,
            half_life_hours: 6.0,
            max_lead_hours: 3.0,
        }
    }

    #[test]
    fn step_bias_signs_deadband_and_clamp() {
        let c = cfg();
        // Model predicts 1 K too WARM -> negative (cooling) correction; only the error beyond the
        // deadband integrates: -(60 W/K/h) * (1.0 - 0.2) K * 0.25 h = -12 W (minus a little decay of 0).
        let w = step_bias(0.0, 1.0, 0.25, &c);
        assert!((w - (-12.0)).abs() < 1e-9, "{w}");
        // Symmetric for a too-cold prediction.
        let w = step_bias(0.0, -1.0, 0.25, &c);
        assert!((w - 12.0).abs() < 1e-9, "{w}");
        // Inside the deadband nothing integrates; the existing value only decays.
        let w = step_bias(100.0, 0.1, 6.0, &c);
        assert!((w - 50.0).abs() < 1e-9, "one half-life halves it, {w}");
        // Anti-windup: a huge persistent error saturates at max_w.
        let mut w = 0.0;
        for _ in 0..100 {
            w = step_bias(w, -10.0, 1.0, &c);
        }
        assert!((w - c.max_w).abs() < 1e-9, "{w}");
    }

    #[test]
    fn step_bias_decays_to_zero_without_error() {
        let c = cfg();
        let mut w = -200.0;
        for _ in 0..10 {
            w = step_bias(w, 0.0, 6.0, &c); // 10 half-lives
        }
        assert!(w.abs() < 0.5, "{w}");
    }
}
