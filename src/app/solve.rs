//! The bounded solve pipeline: the kernel cache, fix-and-round (relaxed LP → rounding → pinned
//! re-solve), the relaxed fallback, and the wall-clock/concurrency guards around them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use anyhow::Result;
use nalgebra::DVector;

use super::{BLOCK_SECONDS, HORIZON_BLOCKS};
use crate::forecast::consumption::ConsumptionModel;
use crate::forecast::solar::PvArray;
use crate::optimize::battery::BatterySpec;
use crate::optimize::config::ControlConfig;
use crate::optimize::coordinator::{kernel_inputs, plan_unified, ForecastContext, PlanOptions};
use crate::optimize::thermal::{build_kernels, KernelSet};
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;

/// Build the kernel cache for the live serve paths — the expensive, state-independent half of the
/// thermal condensation, computed once at startup (see [`KernelSet`]).
pub fn build_kernel_cache(config: &ControlConfig, net: &RcNetwork, ss: &StateSpace) -> KernelSet {
    let (hvac_zones, load_sources) = kernel_inputs(config);
    build_kernels(
        ss,
        net,
        BLOCK_SECONDS,
        HORIZON_BLOCKS,
        &hvac_zones,
        &load_sources,
    )
}

/// Everything one solver run needs, owned — `spawn_blocking` requires `'static`.
/// Everything one LP solve (or a fix-and-round pipeline of them) needs, bundled so it can be
/// `Arc`-shared across the strict/fallback blocking-thread closures in [`solve_bounded`] — and,
/// via [`fix_and_round`], reused verbatim by `solve_timing`'s acceptance test so it cannot drift
/// from what a live tick actually runs.
pub(crate) struct SolveJob {
    pub(crate) pv: PvArray,
    pub(crate) consumption: ConsumptionModel,
    pub(crate) battery: BatterySpec,
    pub(crate) heating: crate::optimize::config::HeatingConfig,
    pub(crate) hvac: crate::optimize::config::HvacConfig,
    pub(crate) ss: StateSpace,
    pub(crate) net: RcNetwork,
    pub(crate) ctx: ForecastContext,
    pub(crate) x0: DVector<f64>,
    pub(crate) ev_specs: Vec<crate::optimize::unified::EvSpec>,
    pub(crate) ev_monitored: Vec<f64>,
    pub(crate) committed: Option<HashMap<String, f64>>,
    /// Block 1's frozen relays (see `PlanOptions::held_next_heat`).
    pub(crate) held_next_heat: Option<HashMap<String, f64>>,
    /// The frozen battery decision carried across the mark (see `PlanOptions::committed_battery`).
    pub(crate) committed_battery: Option<crate::optimize::unified::CommittedBattery>,
    /// Relay state of the block before block 0 (see `PlanOptions::prev_heat_kw`).
    pub(crate) prev_heat: Option<HashMap<String, f64>>,
    pub(crate) kernels: Option<Arc<KernelSet>>,
}

/// Run one LP solve of `job` — `fixed` pins every binary (fix-and-round's pinned re-solve) or
/// leaves them all free (the relaxed pass and the plain fallback).
pub(crate) fn run_solve(
    job: &SolveJob,
    fixed: Option<&crate::optimize::unified::FixedBinaries>,
    solve_budget: crate::optimize::unified::SolveBudget,
) -> Result<crate::optimize::unified::UnifiedPlan> {
    plan_unified(
        &job.pv,
        &job.consumption,
        &job.battery,
        &job.heating,
        &job.hvac,
        &job.ss,
        &job.net,
        &job.ctx,
        &job.x0,
        &job.ev_specs,
        &job.ev_monitored,
        PlanOptions {
            kernels: job.kernels.as_deref(),
            committed_heat: job.committed.as_ref(),
            held_next_heat: job.held_next_heat.as_ref(),
            committed_battery: job.committed_battery,
            prev_heat_kw: job.prev_heat.as_ref(),
            fixed_binaries: fixed,
            solve_budget,
        },
    )
}

/// The fix-and-round pipeline: a relaxed LP, deterministic rounding, then a
/// fully-pinned re-solve — the sole integrality mechanism now that HiGHS never runs branch-and-
/// bound, and the ONLY solve path a normal tick takes (see [`solve_bounded`]'s strict closure,
/// which is exactly this function). Extracted so `solve_timing`'s acceptance test runs the REAL
/// production pipeline rather than a hand-rolled copy that could silently drift from it.
///
/// `Ok((plan, Rounded))` on a successful pinned re-solve (or when the relaxed solve was already
/// integral — see below); `Ok((relaxed_plan, Relaxed))` if the re-solve itself fails (the relaxed
/// plan is still returned, advisory); `Err` only if even the first (relaxed) solve fails.
///
/// `deadline` is when the caller's outer budget runs out (`None` = no outer budget): the SoC-guard
/// retry, a third LP, is skipped when it could not finish before it.
///
/// `salvage` is filled with the relaxed plan as soon as it succeeds, BEFORE the pinned re-solve
/// starts: the pinned re-solve is the slower of the two LPs
/// to go wrong (it starts from a harder, pinned-integral feasible region), so if `solve_bounded`'s
/// outer timeout fires while this function is still stuck in it, the relaxed plan already sitting
/// in `salvage` is a real, freshly-computed answer — worth publishing (graded `Relaxed`) instead of
/// starting a brand-new fallback LP from scratch.
pub(crate) fn fix_and_round(
    job: &SolveJob,
    budget: crate::optimize::unified::SolveBudget,
    deadline: Option<Instant>,
    salvage: &Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>>,
) -> Result<(crate::optimize::unified::UnifiedPlan, SolveGrade)> {
    fix_and_round_inner(job, budget, deadline, salvage, false)
}

/// The real body of [`fix_and_round`], with the "already integral" skip check overridable so a
/// timing test can force the pinned re-solve to actually run — otherwise a scenario whose relaxed
/// LP happens to land on integral values (as both of `solve_timing`'s catch-up scenarios do, since
/// `HEAT_COOL_PIN_BLOCKS` covers blocks 0 AND 1) skips the second LP entirely and the timed region
/// silently stops exercising it, leaving no test that times a two-LP tick.
/// `fix_and_round` itself always passes `false`, so production behaviour is unaffected.
pub(crate) fn fix_and_round_inner(
    job: &SolveJob,
    budget: crate::optimize::unified::SolveBudget,
    deadline: Option<Instant>,
    salvage: &Arc<Mutex<Option<crate::optimize::unified::UnifiedPlan>>>,
    force_pinned_resolve: bool,
) -> Result<(crate::optimize::unified::UnifiedPlan, SolveGrade)> {
    let relaxed_plan = run_solve(job, None, budget)?;
    *salvage.lock().unwrap_or_else(|e| e.into_inner()) = Some(relaxed_plan.clone());
    let loads = crate::optimize::coordinator::controllable_load_specs(&job.ctx);
    // Skip the pinned re-solve entirely when the relaxed LP already settled on integral values
    // (see `relaxed_plan_is_already_integral`'s doc) — a second LP that can only reproduce numbers
    // already in hand costs a full solve for nothing, and on the live tick budget every skipped
    // one is roughly half a tick's wall-clock cost.
    if !force_pinned_resolve
        && crate::optimize::unified::relaxed_plan_is_already_integral(
            &relaxed_plan,
            &job.heating,
            &job.hvac,
            &job.ev_specs,
            &loads,
        )
        && crate::optimize::unified::dispatch_legs_integral(&relaxed_plan, job.ctx.min_dispatch_kw)
    {
        eprintln!("[solve] relaxed plan already integral; skipping the pinned re-solve");
        return Ok((relaxed_plan, SolveGrade::Rounded));
    }
    let dt = job.ctx.grid.dt_hours_vec();
    let fixed = crate::optimize::unified::round_binaries(
        &relaxed_plan,
        &job.heating,
        &job.hvac,
        &job.ev_specs,
        &loads,
        &dt,
    );
    // The dispatch-floor pins (batt_to_grid/grid_charge: 0 or >= min_dispatch_kw) and the
    // routing-loophole caps (batt_to_load/solar_to_batt, capped at the relaxed plan's own real
    // house deficit/surplus — see `round_dispatch_legs`) are rounded separately from
    // `round_binaries`' per-zone/per-charger families — a single flat battery, not keyed by name —
    // and merged into a COPY of its result rather than threading parameters through
    // `round_binaries`' ~6 call sites (test fixtures included). Both caps are identical in STAGE 1
    // and STAGE 2 (they depend only on `relaxed_plan`, not on `guard`), so each stage merges them
    // from its own `round_dispatch_legs` call.
    //
    // STAGE 1 is the UN-GUARDED pin (`guard: false`, every sub-floor leg pinned OFF
    // unconditionally): the SoC-guard's trajectory check is only a SUFFICIENT feasibility
    // condition, not a necessary one (the pinned LP can itself absorb retained energy by
    // curtailing later solar charging or serving load from the battery instead), so trying it
    // first freezes far more legs than are actually infeasible. STAGE 2 (the SoC guard, `guard:
    // true`) is the RETRY, run only if stage 1's pinned re-solve itself fails.
    let load_shift = crate::optimize::unified::pinned_load_shift_kw(
        &relaxed_plan,
        &fixed,
        &job.heating,
        &job.ev_specs,
        &loads,
    );
    let stage1_pins = crate::optimize::unified::round_dispatch_legs(
        &relaxed_plan,
        &job.battery,
        job.ctx.min_dispatch_kw,
        &dt,
        &load_shift,
        false,
    );
    let mut stage1 = fixed.clone();
    stage1.batt_to_grid_on = stage1_pins.batt_to_grid_on;
    stage1.grid_charge_on = stage1_pins.grid_charge_on;
    stage1.batt_to_load_cap = stage1_pins.batt_to_load_cap;
    stage1.solar_to_batt_cap = stage1_pins.solar_to_batt_cap;

    match run_solve(job, Some(&stage1), budget) {
        Ok(p) => Ok((p, SolveGrade::Rounded)),
        Err(e1) => {
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if let Some(why) = stage2_skip_reason(&e1, remaining, budget) {
                eprintln!(
                    "[solve] pinned re-solve failed ({e1}); skipping the SoC-guard retry ({why}); \
                     publishing the relaxed plan"
                );
                return Ok((relaxed_plan, SolveGrade::Relaxed));
            }
            let stage2_pins = crate::optimize::unified::round_dispatch_legs(
                &relaxed_plan,
                &job.battery,
                job.ctx.min_dispatch_kw,
                &dt,
                &load_shift,
                true,
            );
            if stage2_pins.guard_freed == 0 {
                // The guard would free nothing beyond what stage 1 already pinned off — stage 2
                // would re-solve the IDENTICAL LP and fail again, so skip straight to the relaxed
                // fallback (same message/behaviour as a single-stage failure).
                eprintln!("[solve] pinned re-solve failed ({e1}); publishing the relaxed plan");
                return Ok((relaxed_plan, SolveGrade::Relaxed));
            }
            let guard_freed = stage2_pins.guard_freed;
            eprintln!(
                "[solve] dispatch floor: un-guarded pin failed ({e1}); retrying with the SoC \
                 guard ({guard_freed} block(s) freed)"
            );
            let mut stage2 = fixed;
            stage2.batt_to_grid_on = stage2_pins.batt_to_grid_on;
            stage2.grid_charge_on = stage2_pins.grid_charge_on;
            stage2.batt_to_load_cap = stage2_pins.batt_to_load_cap;
            stage2.solar_to_batt_cap = stage2_pins.solar_to_batt_cap;
            match run_solve(job, Some(&stage2), budget) {
                Ok(p) => Ok((p, SolveGrade::Rounded)),
                Err(e) => {
                    eprintln!("[solve] pinned re-solve failed ({e}); publishing the relaxed plan");
                    Ok((relaxed_plan, SolveGrade::Relaxed))
                }
            }
        }
    }
}

/// Why the SoC-guard retry (stage 2 of [`fix_and_round_inner`]) is not worth running after the
/// un-guarded pinned re-solve failed with `stage1_err`, if it is not. A solver-limit failure is not
/// the infeasibility the guard frees legs for, and a retry that cannot finish inside the caller's
/// `remaining` budget would only be cut off by the outer timeout.
fn stage2_skip_reason(
    stage1_err: &anyhow::Error,
    remaining: Option<StdDuration>,
    budget: crate::optimize::unified::SolveBudget,
) -> Option<&'static str> {
    if hit_solver_limit(stage1_err) {
        return Some("the un-guarded re-solve hit the solver time limit");
    }
    match (remaining, budget.time_limit_s) {
        (Some(left), Some(limit)) if left.as_secs_f64() < limit => {
            Some("the strict budget left is below one LP's time limit")
        }
        _ => None,
    }
}

/// HiGHS's own wall-clock limit for EACH LP solve (the strict fix-and-round pipeline's and the
/// fallback's) — leaves headroom for model build + presolve, which sit outside HiGHS's own
/// time-limit check.
pub(super) const PER_LP_HIGHS_TIME_LIMIT_S: f64 = 14.0;
/// The strict (fix-and-round) pipeline's outer budget: a relaxed LP, a cheap deterministic
/// rounding pass, and a fully-pinned re-solve — each LP capped at `PER_LP_HIGHS_TIME_LIMIT_S`,
/// with headroom for the rounding pass and model build between them. It fits two LPs, not the
/// SoC-guard retry's third: `fix_and_round` runs that one only when a whole per-LP limit is left.
pub(super) const STRICT_SOLVE_TIMEOUT: StdDuration = StdDuration::from_secs(32);
/// The fallback's outer budget: ONE plain relaxed LP (no rounding/re-solve) when the strict
/// pipeline itself times out or its permit is busy. strict + fallback + the on-demand path's
/// pre-solve DB reads must fit inside the web layer's `COMPUTE_TIMEOUT` (55 s) with headroom.
pub(super) const FALLBACK_SOLVE_TIMEOUT: StdDuration = StdDuration::from_secs(15);
/// The fallback's OWN per-LP HiGHS limit: unlike the strict pipeline (sequential LPs sharing
/// `STRICT_SOLVE_TIMEOUT`), the fallback runs a single LP inside `FALLBACK_SOLVE_TIMEOUT`, so it
/// can use nearly all of it — 1 s of headroom for model build + presolve outside HiGHS's own
/// time-limit check (same reasoning as [`PER_LP_HIGHS_TIME_LIMIT_S`]). Computed from
/// `FALLBACK_SOLVE_TIMEOUT` rather than reusing `PER_LP_HIGHS_TIME_LIMIT_S`, whose 1 s of
/// headroom would only hold by numeric coincidence, so the two can never silently drift apart.
pub(super) const FALLBACK_PER_LP_HIGHS_TIME_LIMIT_S: f64 =
    FALLBACK_SOLVE_TIMEOUT.as_secs_f64() - 1.0;

/// How a plan's solve concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolveGrade {
    /// Fix-and-round: relaxed LP → deterministic rounding → fully-pinned re-solve. INTEGRAL and
    /// self-consistent — the NORMAL result now that HiGHS never runs branch-and-bound; actuated,
    /// latched and snapshotted.
    Rounded,
    /// The plain relaxed LP alone (the strict pipeline timed out, its permit was busy, or its own
    /// pinned re-solve failed) — advisory only; the publisher skips it and the loop neither
    /// latches nor snapshots from it.
    Relaxed,
}

/// Did HiGHS stop on its time limit? good_lp 1.15 reports that as `Other("NoSolutionFound")` when
/// no primal-feasible point exists, and `optimize_unified` rejects a feasible-but-unproven
/// `TimeLimit` status; infeasible/unbounded come back as their own `ResolutionError` variants.
fn hit_solver_limit(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("TimeLimit") || msg.contains("NoSolutionFound")
}

/// Run the bounded FALLBACK: a single plain relaxed LP on one blocking thread. Its own one-permit
/// gate (same detached-supervisor pattern as the strict path) stops abandoned fallback threads
/// piling up if a pathological LP outlives its timeout tick after tick.
async fn run_fallback<T, G>(fallback: G, timeout: StdDuration, loop_caller: bool) -> Result<T>
where
    T: Send + 'static,
    G: FnOnce() -> Result<T> + Send + 'static,
{
    // Split loop/web permits for the same reason `solve_bounded` splits the strict ones: a web
    // caller holding the only fallback permit would make the loop's fallback error out, and a
    // loop tick with no plan at all is worse than the relaxed plan it was trying to produce.
    static LOOP_FALLBACK: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    static WEB_FALLBACK: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let gate: Arc<tokio::sync::Semaphore> = if loop_caller {
        Arc::clone(&LOOP_FALLBACK)
    } else {
        Arc::clone(&WEB_FALLBACK)
    };
    let permit = gate.try_acquire_owned().map_err(|_| {
        anyhow::anyhow!("previous fallback solve still running — keeping the last plan")
    })?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _permit = permit; // released only when the blocking thread truly finishes
        let _ = tx.send(tokio::task::spawn_blocking(fallback).await);
    });
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(joined)) => {
            joined.map_err(|e| anyhow::anyhow!("fallback solver task failed: {e}"))?
        }
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "fallback solver supervisor dropped its channel"
        )),
        Err(_) => Err(anyhow::anyhow!("fallback solve also timed out")),
    }
}

/// The on-demand (non-loop) strict-solve permit — see [`solve_bounded`].
static WEB_SOLVER: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

/// Whether an on-demand solve would get the strict-solve permit right now. A caller that does not
/// want the relaxed fallback a busy permit degrades into (the off-path counterfactual plans) checks
/// this first; the answer is advisory — the permit can still be taken between check and solve.
pub(crate) fn on_demand_solver_idle() -> bool {
    WEB_SOLVER.available_permits() > 0
}

/// Run `strict` (the fix-and-round pipeline) off the async runtime with a timeout; on expiry (or
/// when a previous strict solve still holds the permit) run `fallback` (a single plain relaxed LP)
/// instead. Returns the plan, its grade, and — when the fallback path was used — why.
///
/// A pure LP under HiGHS's own `time_limit_s` either finishes or errors well inside the outer
/// timeouts in practice, but a stuck strict thread still **cannot be killed** (blocking C++ FFI),
/// so the same belt-and-suspenders structure as before stays:
/// - a detached SUPERVISOR task owns the one-permit semaphore's permit for the blocking thread's
///   full lifetime — the caller may itself be cancelled (the web layer's `COMPUTE_TIMEOUT` drops
///   the whole future) without releasing the permit early, so strict solves can never pile up no
///   matter how the caller ends;
/// - while the permit is held by a stuck solve, callers fall back to the bounded relaxed LP
///   (flagged) instead of erroring — fresh advisory plans keep flowing.
pub(super) async fn solve_bounded<T, F, G>(
    strict: F,
    fallback: G,
    strict_timeout: StdDuration,
    fallback_timeout: StdDuration,
    loop_caller: bool,
    salvage: Arc<Mutex<Option<T>>>,
) -> Result<(T, SolveGrade, Option<String>)>
where
    T: Send + 'static,
    F: FnOnce() -> Result<(T, SolveGrade)> + Send + 'static,
    G: FnOnce() -> Result<T> + Send + 'static,
{
    // Separate permits so an on-demand /api/plan solve can never hold the loop's: a displaced
    // loop tick would fall to the relaxed fallback, which the publisher refuses to actuate — a
    // dashboard viewer would silently pause actuation. Worst case two strict solves overlap
    // (~seconds of CPU on separate blocking threads), which is fine.
    static LOOP_SOLVER: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let solver: Arc<tokio::sync::Semaphore> = if loop_caller {
        Arc::clone(&LOOP_SOLVER)
    } else {
        Arc::clone(&WEB_SOLVER)
    };
    match solver.try_acquire_owned() {
        Ok(permit) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _permit = permit; // released only when the blocking thread truly finishes
                let _ = tx.send(tokio::task::spawn_blocking(strict).await);
            });
            match tokio::time::timeout(strict_timeout, rx).await {
                // The blocking task finished (no panic) inside the outer timeout — but "finished"
                // still splits into the strict pipeline's own Ok/Err: a strict `Err` (HiGHS
                // `TimeLimit`/`NoSolutionFound`, or anything else `fix_and_round` can return) is
                // reachable BELOW every outer timeout. A salvaged relaxed plan is published
                // instead (graded `Relaxed`).
                Ok(Ok(Ok(Ok((plan, grade))))) => Ok((plan, grade, None)),
                Ok(Ok(Ok(Err(e)))) => {
                    // `salvage` is not necessarily empty when `fix_and_round` returns `Err`: the
                    // relaxed plan may have landed before the pinned re-solve failed.
                    if let Some(plan) = salvage.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "fix-and-round error: {e}; salvaged the relaxed plan"
                            )),
                        ));
                    }
                    // An empty salvage means the strict pipeline's FIRST (unpinned relaxed) LP
                    // failed and the fallback is that identical LP. A deterministic failure
                    // (infeasible, unbounded) would only repeat, so it propagates; a HiGHS time
                    // limit can be CPU contention on the shared host, so that one gets the retry.
                    if hit_solver_limit(&e) {
                        let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "relaxed LP hit the time limit ({e}); re-solved once"
                            )),
                        ));
                    }
                    Err(e.context(
                        "fix-and-round error in the relaxed LP; skipped the fallback (it would \
                         re-solve the identical LP)",
                    ))
                }
                // A PANIC inside the strict closure (`fix_and_round`) — a bug, not an ordinary
                // solver failure — must not surface as a hard `Err` ("planning failed, no plan
                // published"). Publish the salvaged relaxed plan if the panic came after it, else
                // run the fallback, logged loudly (a panic here always deserves investigation,
                // unlike a routine HiGHS TimeLimit).
                Ok(Ok(Err(join_err))) => {
                    if let Some(plan) = salvage.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        eprintln!(
                            "[mpc] PANIC in strict solve: {join_err} — publishing the salvaged \
                             relaxed plan"
                        );
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "strict solve panicked: {join_err}; salvaged the relaxed plan"
                            )),
                        ));
                    }
                    eprintln!(
                        "[mpc] PANIC in strict solve: {join_err} — falling back to the relaxed LP"
                    );
                    let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                    Ok((
                        plan,
                        SolveGrade::Relaxed,
                        Some(format!("strict solve panicked: {join_err}")),
                    ))
                }
                Ok(Err(_)) => Err(anyhow::anyhow!("solver supervisor dropped its channel")),
                Err(_) => {
                    // Outer STRICT_SOLVE_TIMEOUT fired with the blocking task still running
                    // (detached; it keeps going and will eventually release the permit). If the
                    // relaxed LP already succeeded and stored itself in `salvage` — the common
                    // shape, since the PINNED re-solve is the slower/harder of the two LPs — publish
                    // that real, freshly-solved plan instead of paying for a brand-new fallback LP.
                    if let Some(plan) = salvage.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        return Ok((
                            plan,
                            SolveGrade::Relaxed,
                            Some(format!(
                                "fix-and-round timeout after {}s; salvaged the relaxed plan",
                                strict_timeout.as_secs()
                            )),
                        ));
                    }
                    let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
                    Ok((
                        plan,
                        SolveGrade::Relaxed,
                        Some(format!(
                            "fix-and-round timeout after {}s",
                            strict_timeout.as_secs()
                        )),
                    ))
                }
            }
        }
        Err(_) => {
            let plan = run_fallback(fallback, fallback_timeout, loop_caller).await?;
            Ok((
                plan,
                SolveGrade::Relaxed,
                Some("previous fix-and-round still running".to_string()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn solve_bounded_falls_back_to_relaxed_on_timeout() {
        // Millisecond-scale stand-ins (never test with the real 30 s under single-threaded CI).
        let strict_fast = || Ok::<_, anyhow::Error>((1, SolveGrade::Rounded));
        let fallback = || Ok::<_, anyhow::Error>(2);
        let (v, grade, cause) = solve_bounded(
            strict_fast,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 1);
        assert_eq!(grade, SolveGrade::Rounded);
        assert!(cause.is_none());

        // A stuck strict solve times out and the relaxed fallback answers instead.
        let strict_stuck = || {
            std::thread::sleep(StdDuration::from_millis(300));
            Ok::<_, anyhow::Error>((1, SolveGrade::Rounded))
        };
        let fallback = || Ok::<_, anyhow::Error>(2);
        let (v, grade, cause) = solve_bounded(
            strict_stuck,
            fallback,
            StdDuration::from_millis(20),
            StdDuration::from_millis(500),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 2);
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("fix-and-round timeout"));

        // While the stuck strict thread still holds the permit, a concurrent caller is served by
        // the relaxed fallback instead of erroring (fresh plans keep flowing).
        let (v, grade, cause) = solve_bounded(
            || Ok::<_, anyhow::Error>((1, SolveGrade::Rounded)),
            || Ok::<_, anyhow::Error>(3),
            StdDuration::from_millis(200),
            StdDuration::from_millis(500),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 3);
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("still running"));
        // Give the detached stuck thread time to release the permit for later tests.
        tokio::time::sleep(StdDuration::from_millis(350)).await;

        // A strict closure that returns `Err` with nothing salvaged failed in its relaxed LP —
        // the very LP the fallback would re-solve — so the error propagates without running it.
        let strict_err = || {
            Err::<(i32, SolveGrade), anyhow::Error>(anyhow::Error::new(
                good_lp::ResolutionError::Infeasible,
            ))
        };
        let fallback_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fallback_ran_in = Arc::clone(&fallback_ran);
        let fallback = move || {
            fallback_ran_in.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, anyhow::Error>(9)
        };
        let err = solve_bounded(
            strict_err,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("Infeasible"));
        assert!(format!("{err:#}").contains("skipped the fallback"));
        assert!(!fallback_ran.load(std::sync::atomic::Ordering::SeqCst));

        // A HiGHS time limit may be CPU contention: that one still gets the single retry.
        let strict_timelimit = || {
            Err::<(i32, SolveGrade), anyhow::Error>(anyhow::anyhow!(
                "HiGHS did not reach optimality (status TimeLimit)"
            ))
        };
        let (v, grade, cause) = solve_bounded(
            strict_timelimit,
            || Ok::<_, anyhow::Error>(7),
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!((v, grade), (7, SolveGrade::Relaxed));
        assert!(cause.unwrap().contains("re-solved once"));
        assert!(!fallback_ran.load(std::sync::atomic::Ordering::SeqCst));

        // good_lp reports a time limit with no primal-feasible point as `Other("NoSolutionFound")`.
        let strict_no_solution = || {
            Err::<(i32, SolveGrade), anyhow::Error>(
                anyhow::Error::new(good_lp::ResolutionError::Other("NoSolutionFound"))
                    .context("unified LP"),
            )
        };
        let (v, grade, cause) = solve_bounded(
            strict_no_solution,
            || Ok::<_, anyhow::Error>(8),
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!((v, grade), (8, SolveGrade::Relaxed));
        assert!(cause.unwrap().contains("re-solved once"));

        // A strict closure that PANICS (a JoinError, not a normal `Err`)
        // must also run the fallback instead of propagating a hard error. `spawn_blocking` catches
        // the panic (no `panic = "abort"` profile is set) and reports it as a `JoinError`; the
        // default panic hook still prints the panic message to stderr, which is expected noise for
        // this one test, not a failure.
        let strict_panics = || -> Result<(i32, SolveGrade), anyhow::Error> { panic!("boom") };
        let fallback = || Ok::<_, anyhow::Error>(7);
        let (v, grade, cause) = solve_bounded(
            strict_panics,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            Arc::new(Mutex::new(None)),
        )
        .await
        .unwrap();
        assert_eq!(v, 7, "the fallback's answer, not a propagated panic");
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("panicked"));

        // A panic AFTER the relaxed plan landed in `salvage` publishes that plan, not a fresh
        // fallback LP.
        let salvage: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let salvage_for_strict = Arc::clone(&salvage);
        let strict_salvages_then_panics = move || -> Result<(i32, SolveGrade), anyhow::Error> {
            *salvage_for_strict.lock().unwrap() = Some(5);
            panic!("rounding boom")
        };
        let fallback = || Ok::<_, anyhow::Error>(99);
        let (v, grade, cause) = solve_bounded(
            strict_salvages_then_panics,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(200),
            false,
            salvage,
        )
        .await
        .unwrap();
        assert_eq!(
            v, 5,
            "the salvaged relaxed plan, not the fallback's fresh answer"
        );
        assert_eq!(grade, SolveGrade::Relaxed);
        let cause = cause.unwrap();
        assert!(cause.contains("panicked") && cause.contains("salvaged"));

        // Salvage: a strict closure that stores a relaxed plan in
        // `salvage` as soon as it has one, then keeps running past the outer timeout, must have
        // that STORED plan returned (graded `Relaxed`) rather than a brand-new fallback LP.
        let salvage: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let salvage_for_strict = Arc::clone(&salvage);
        let strict_salvages_then_hangs = move || {
            *salvage_for_strict.lock().unwrap() = Some(42);
            std::thread::sleep(StdDuration::from_millis(300));
            Ok::<_, anyhow::Error>((1, SolveGrade::Rounded))
        };
        let fallback = || Ok::<_, anyhow::Error>(99);
        let (v, grade, cause) = solve_bounded(
            strict_salvages_then_hangs,
            fallback,
            StdDuration::from_millis(20),
            StdDuration::from_millis(500),
            false,
            salvage,
        )
        .await
        .unwrap();
        assert_eq!(
            v, 42,
            "the salvaged relaxed plan, not the fallback's fresh answer"
        );
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("salvaged"));
        // Give the detached stuck thread time to release the permit for later tests.
        tokio::time::sleep(StdDuration::from_millis(350)).await;

        // The strict-`Err` arm (as opposed to the
        // outer-timeout arm above) must ALSO prefer a salvaged relaxed plan over paying for a fresh
        // fallback LP — and, critically, over losing the plan entirely if that fresh fallback then
        // also errors. Kept in THIS test function (not a separate `#[tokio::test]`) deliberately:
        // `solve_bounded` gates on a module-level `static` semaphore shared by every call in the
        // process, so a standalone test risks racing this file's OTHER `solve_bounded` tests for the
        // same permit under `cargo test`'s default parallel runner (observed: it intermittently took
        // the "previous fix-and-round still running" branch instead of the one under test) — exactly
        // why every other multi-scenario check here already lives in one sequential test.
        let salvage: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let salvage_for_strict = Arc::clone(&salvage);
        let strict_salvages_then_errors = move || {
            *salvage_for_strict.lock().unwrap() = Some(7);
            Err::<(i32, SolveGrade), anyhow::Error>(anyhow::anyhow!("pinned re-solve blew up"))
        };
        // A fallback that would be an obviously WRONG answer if ever reached, so the assertion
        // below proves the salvage path won rather than merely matching by coincidence.
        let fallback = || Ok::<_, anyhow::Error>(99);
        let (v, grade, cause) = solve_bounded(
            strict_salvages_then_errors,
            fallback,
            StdDuration::from_millis(200),
            StdDuration::from_millis(500),
            false,
            salvage,
        )
        .await
        .unwrap();
        assert_eq!(
            v, 7,
            "the salvaged relaxed plan, not the fallback's fresh answer"
        );
        assert_eq!(grade, SolveGrade::Relaxed);
        assert!(cause.unwrap().contains("salvaged"));
    }

    #[test]
    fn stage2_retry_runs_only_for_a_fast_non_limit_failure_with_a_full_lp_left() {
        let budget = crate::optimize::unified::SolveBudget {
            time_limit_s: Some(PER_LP_HIGHS_TIME_LIMIT_S),
        };
        let infeasible = anyhow::anyhow!("solve failed: Infeasible");
        let plenty = Some(StdDuration::from_secs(20));
        assert_eq!(stage2_skip_reason(&infeasible, plenty, budget), None);
        assert_eq!(stage2_skip_reason(&infeasible, None, budget), None);
        for limit in ["status TimeLimit", "Other(\"NoSolutionFound\")"] {
            let e = anyhow::anyhow!("solve failed").context(limit);
            assert!(stage2_skip_reason(&e, plenty, budget).is_some(), "{limit}");
        }
        let short = Some(StdDuration::from_secs(4));
        assert!(stage2_skip_reason(&infeasible, short, budget).is_some());
        let unlimited = crate::optimize::unified::SolveBudget { time_limit_s: None };
        assert_eq!(stage2_skip_reason(&infeasible, short, unlimited), None);
    }
}
