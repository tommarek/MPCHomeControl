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
    PlanReport, ScheduledFit, TimelineBlock, TimestampedPlan,
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

/// item 3 (rework cycle 2, findings 5/2): how long before block 1's own start (`mark`) the loop
/// begins FREEZING its heating/cool decision — chosen so at least one publisher poll (30 s cadence)
/// reliably lands inside the window before the mark, while only the last couple of ticks before a
/// mark are affected. See [`freeze_committed_next`].
const FREEZE_WINDOW_SECONDS: i64 = 120;

/// The freeze window's commitment for an upcoming mark (block 1's start) — block 1's heating/cool
/// decision, pinned to whatever the FIRST tick inside `[mark - FREEZE_WINDOW_SECONDS, mark)` decided.
/// Every later tick targeting the SAME mark keeps repeating it (see [`freeze_committed_next`]),
/// regardless of what a fresh solve says for block 1 — this is what makes what the controllers apply
/// at the mark (via the publisher's frozen-gated next command) identical to what the loop itself
/// latches at rollover (see [`rollover_heat_kw`]), closing the brain/publisher divergence finding 5
/// found. `cool_kw`/`hvac_heat_kw` are the reversible-HVAC mirror of `heat_kw`; empty when no `hvac`
/// unit is configured.
#[derive(Clone)]
struct CommittedNext {
    /// The mark (block 1's start) this commitment targets — must equal the new block at rollover
    /// (a skipped tick, or a plan computed before an earlier rollover, makes it stale).
    mark: DateTime<Utc>,
    heat_kw: HashMap<String, f64>,
    cool_kw: HashMap<String, f64>,
    hvac_heat_kw: HashMap<String, f64>,
}

/// Whether `now` is inside the pre-mark freeze window for `mark`. A tick already PAST `mark` but not
/// yet rolled over (a late/slow tick) is treated the same way — freeze, never un-freeze, once inside
/// the window for a given mark (true by construction for a monotonically increasing clock: `mark`
/// stays fixed while `now` only grows, so `mark - now` only shrinks).
fn in_freeze_window(mark: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    mark - now <= chrono::Duration::seconds(FREEZE_WINDOW_SECONDS)
}

/// item 3: this tick's freeze-window commitment for `mark` (block 1's start), given the PREVIOUS
/// tick's commitment (`previous`) and THIS tick's own fresh `block1`. Outside the window, `previous`
/// is returned untouched (stale/unused until a future window opens for a matching mark). Inside the
/// window: the first CLEAN (`clean == true`, i.e. not degraded/relaxed) tick to observe it commits
/// `block1`'s decision and every later tick for the SAME mark keeps that commitment (`previous.mark
/// == mark`); a degraded/relaxed tick inside the window with nothing committed yet leaves `previous`
/// as-is (still `None`, or stale from an earlier mark) and keeps waiting for a clean one. Pure, so
/// it's directly unit-testable without a live loop/DB.
fn freeze_committed_next(
    previous: Option<CommittedNext>,
    mark: DateTime<Utc>,
    block1: &TimelineBlock,
    now: DateTime<Utc>,
    clean: bool,
) -> Option<CommittedNext> {
    if !in_freeze_window(mark, now) {
        return previous;
    }
    match previous {
        Some(c) if c.mark == mark => Some(c),
        _ if clean => Some(CommittedNext {
            mark,
            heat_kw: block1.heat_kw.clone(),
            cool_kw: block1.cool_kw.clone(),
            hvac_heat_kw: block1.hvac_heat_kw.clone(),
        }),
        _ => previous,
    }
}

/// item 3: apply `committed_next` onto `next_step`, setting `frozen: true`, when it targets `next_step`'s
/// own block start AND we're inside its freeze window — a no-op (returns `next_step` unchanged, still
/// `frozen: false`) otherwise: nothing committed yet, a stale commitment for a different mark, or a
/// commitment that exists but whose window hasn't opened (defensive; `freeze_committed_next` only ever
/// commits from inside the window, so this should already hold whenever `committed_next` matches, but
/// checking it here keeps the two functions independently correct). Pure, so directly unit-testable.
fn apply_freeze_to_next_step(
    next_step: Option<TimelineBlock>,
    committed_next: Option<&CommittedNext>,
    now: DateTime<Utc>,
) -> Option<TimelineBlock> {
    let mut ns = next_step?;
    if let Some(c) = committed_next {
        if c.mark == ns.t && in_freeze_window(c.mark, now) {
            ns.heat_kw = c.heat_kw.clone();
            ns.cool_kw = c.cool_kw.clone();
            ns.hvac_heat_kw = c.hvac_heat_kw.clone();
            ns.frozen = true;
        }
    }
    Some(ns)
}

/// Run the loop forever: every `tick`, re-plan and publish. Planning failures are logged and the
/// loop continues (the previous published plan stays available).
pub async fn run(state: Arc<AppState>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    // A tick that overruns (degraded DB, solver timeout) must NOT be followed by a burst of
    // queued back-to-back re-plans against the already-struggling backend — one tick per period.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // item 8 (rework cycle 2, finding 8): with a 1-minute cadence, re-anchor to the wall-clock
    // second-`:20` mark on EVERY tick (not once at startup — see the loop below), so :20 puts the
    // LAST tick before every quarter-hour mark at mark − 40 s, comfortably inside the item-3 freeze
    // window (mark − 120 s), and an overrun on any one tick can never permanently shift the phase:
    // the very next tick still targets the true next :20 mark, not "last actual tick + tick" the
    // way `tokio::time::interval`'s own `MissedTickBehavior::Delay` computes it (rework cycle 1
    // shipped a ONE-SHOT re-anchor via `interval_at`, which self-corrected only at startup — a
    // later overrun then drifted the phase for good). Only for `mpc_tick_minutes == 1` — no
    // equivalent 10–20-s-before-the-mark target is defined for another cadence.
    // See `delay_to_next_second20`.
    let realign_every_tick = tick == Duration::from_secs(60);
    // The very first tick always fires immediately via `interval.tick()` (unchanged startup/respawn
    // latency); every tick after that uses the explicit wall-clock re-anchor below when
    // `realign_every_tick`, or `interval.tick()` unchanged for any other cadence.
    let mut first_tick = true;
    let mut cache: Option<(Instant, PlanCache)> = None;
    // Consecutive degraded slow-input rebuilds; see the `cache_ttl` comment below.
    let mut degraded_retries: usize = 0;
    // Seed both the within-block relay latch (`committed`) and the freeze-window commitment
    // (`committed_next`, item 3) from the same already-published plan, so a supervisor respawn (loop
    // panic) resumes correctly whether it lands mid-block or right before a rollover.
    let seed_plan = crate::web::lock_latest(&state).map(|tp| tp.plan);
    // The heating relays decided at the current 15-min block's start, held for its 15 minutes so the
    // relays don't flip mid-block under the per-minute re-planning (a minimum on/off time).
    let mut committed: Option<(DateTime<Utc>, HashMap<String, f64>)> = seed_plan
        .as_ref()
        .filter(|plan| !plan.degraded && !plan.relaxed)
        .map(|plan| (plan.first_step.hour_start, plan.first_step.heat_kw.clone()));
    // item 3: seed the freeze-window commitment from the last published plan's `next_step`, but only
    // when it was already FROZEN (a respawn landing mid-freeze-window) and clean — otherwise `None`,
    // so the next freeze window simply commits fresh (a respawn between windows, or one before this
    // field existed on an older published plan, loses nothing: nothing was frozen to resume).
    let mut committed_next: Option<CommittedNext> = seed_plan.as_ref().and_then(|plan| {
        plan.next_step
            .as_ref()
            .filter(|ns| ns.frozen && !plan.degraded && !plan.relaxed)
            .map(|ns| CommittedNext {
                mark: ns.t,
                heat_kw: ns.heat_kw.clone(),
                cool_kw: ns.cool_kw.clone(),
                hvac_heat_kw: ns.hvac_heat_kw.clone(),
            })
    });

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
        if first_tick {
            interval.tick().await; // fires immediately — unchanged startup/respawn latency
            first_tick = false;
        } else if realign_every_tick {
            // item 8: recompute the wall-clock delay to the next :20 mark EVERY tick, rather than
            // consuming a persistent `tokio::time::interval`'s own (potentially phase-drifted) next
            // deadline — this is what makes an overrun on any one tick self-heal on the very next one.
            tokio::time::sleep(delay_to_next_second20(Utc::now())).await;
        } else {
            interval.tick().await;
        }

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
                // forwards them when the committed block is its block 0 or one block later — the
                // bounded backward-clock hold), so first_step, the timeline and both armed
                // controllers agree by construction — no post-hoc patch.
                committed_heat: committed.clone(),
                kernels: Some(state.kernels.clone()),
                kalman: state.kalman.get().cloned(),
                load_run_hours: load_run.clone(),
            },
        )
        .await
        {
            Ok(mut plan) => {
                // Latch the relays for the current block: decided fresh at the block start, then
                // held for the rest of the block so the minute re-plans can't sub-cycle them.
                // Re-latch when the block moves *forward* (`block > b`) OR when the anchor sits
                // MORE than one block ahead (a large backward wall-clock step — the LP is no
                // longer honoring that commitment, see below). A same-block re-plan holds; a
                // small backward step (≤ one block) holds AND re-bases the anchor so the hold
                // expires after one block of real time. The commitment is enforced inside the LP
                // (see PlanExtras::committed_heat; `current_plan` accepts a committed block equal
                // to its block 0 or exactly one block later), so nothing is patched here.
                let block = plan.first_step.hour_start;
                // Block 0 is ALWAYS a fine (15-min) block by construction (item F: `horizon.
                // fine_hours >= 1`), so its real duration is what the within-block latch and the
                // run-hours tally below are keyed on — read from the plan itself (`TimelineBlock::
                // dt_minutes`) rather than assuming, with a debug assertion the invariant still
                // holds. `.unwrap_or(15)` only matters for a plan with an empty timeline (nothing
                // to latch/bank against anyway).
                let block0_minutes = plan.timeline.first().map_or(15, |b| b.dt_minutes);
                debug_assert_eq!(
                    block0_minutes, 15,
                    "block 0 must always be a fine (15-min) block"
                );
                let block0_seconds = i64::from(block0_minutes) * 60;
                match &committed {
                    // Bounded like current_plan's acceptance window: a latch more than one block
                    // ahead of the planned block (a large backward clock step) is NOT being
                    // honored by the LP anymore, so fall through and re-latch from this plan
                    // rather than believing relays held that aren't. Within the window, RE-BASE
                    // the anchor to the plan's own block (keeping the relay values): without the
                    // re-base a small backward step that crossed a block edge kept the original
                    // anchor, so the hold lasted until wall-clock re-passed it — up to two blocks
                    // of real time — instead of expiring after one.
                    Some((b, relays))
                        if block < *b && (*b - block).num_seconds() <= block0_seconds =>
                    {
                        committed = Some((block, relays.clone()));
                    }
                    Some((b, _)) if block == *b => {}
                    // Never latch from a degraded or relaxed plan: the publisher refused to
                    // actuate it, so its (possibly fictional / fractional) relays are NOT what the
                    // house is holding — pinning them into the next strict solve would be wrong.
                    _ if plan.degraded || plan.relaxed => {}
                    // The block moved forward (a rollover, or startup with no prior commitment):
                    // item 3 rollover adoption — prefer `committed_next` (the value FROZEN by the
                    // first clean tick inside the pre-mark freeze window, item 3) over re-deciding
                    // block 0 fresh here, which the LP would otherwise do independently ~60 s into
                    // the new block — a second, possibly different relay command the mechanical
                    // relays must never see. This is also exactly what the publisher promoted as the
                    // next command (frozen-gated), so the loop's own latch and the controllers'
                    // applied value can never diverge (finding 5). Falls back to today's behaviour
                    // (this plan's own block 0) when no committed value covers the new block. See
                    // `rollover_heat_kw`'s doc.
                    _ => {
                        committed = Some((
                            block,
                            rollover_heat_kw(
                                block,
                                committed_next.as_ref(),
                                &plan.first_step.heat_kw,
                            ),
                        ));
                    }
                }
                // item 3: advance the freeze-window commitment for THIS tick's own block 1 (a mark
                // still ahead of `block`), then mirror it onto `plan.next_step` once the window has
                // opened and a commitment for that exact mark exists — see `freeze_committed_next`'s
                // doc. `None` when the plan has no block 1 at all (a degenerate/very short horizon).
                let now = Utc::now();
                committed_next = match plan.timeline.get(1) {
                    Some(block1) => freeze_committed_next(
                        committed_next,
                        block1.t,
                        block1,
                        now,
                        !plan.degraded && !plan.relaxed,
                    ),
                    None => None,
                };
                plan.next_step =
                    apply_freeze_to_next_step(plan.next_step, committed_next.as_ref(), now);
                // Bank the block we are about to actuate, once per block — and ONLY from a plan the
                // publisher will actually send, exactly like the relay latch above. A degraded or
                // relaxed plan is skipped wholesale downstream, so banking its (often fractional)
                // `on` credited run-time the appliance never got. Since `already_run_hours` sets the
                // occurrence's upper CAP as well as its demand, over-banking forces the load OFF for
                // the rest of the night with no shortfall to signal it: a degraded stretch at the
                // start of a boiler window silently cost the whole night's hot water. Leaving
                // `load_run_block` unadvanced is right — the first strict plan in the same block
                // banks it.
                let dt_h = f64::from(block0_minutes) / 60.0;
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

/// item 3 rollover adoption: decide the new block's relay commitment when the loop's block moves
/// forward. Adopts `committed_next`'s heat_kw when it targets the new block (`mark == new_block` —
/// `committed_next` is only ever populated from a clean, non-degraded/non-relaxed tick, see
/// `freeze_committed_next`); otherwise falls back to `fresh_block0` — today's behaviour of deciding
/// fresh from the first post-boundary plan (no commitment covers the new block: startup, every tick
/// inside the freeze window was degraded/relaxed, or a stale commitment whose mark isn't this new
/// block, e.g. after a skipped tick jumped more than one block).
///
/// Pure (no I/O), so it is directly unit-testable without a live loop/DB — see the tests below.
fn rollover_heat_kw(
    new_block: DateTime<Utc>,
    committed_next: Option<&CommittedNext>,
    fresh_block0: &HashMap<String, f64>,
) -> HashMap<String, f64> {
    match committed_next {
        Some(c) if c.mark == new_block => c.heat_kw.clone(),
        _ => fresh_block0.clone(),
    }
}

/// Item G tick phase: the [`Duration`] from `now` to the next wall-clock second-`:20` mark of its
/// minute (0 if `now` already sits exactly there). Pure, so it's directly unit-testable without a
/// live clock/interval.
fn delay_to_next_second20(now: DateTime<Utc>) -> Duration {
    let this_minute = now
        .with_second(20)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    let target = if this_minute >= now {
        this_minute
    } else {
        this_minute + chrono::Duration::minutes(1)
    };
    (target - now).to_std().unwrap_or(Duration::ZERO)
}

/// Log the controls the optimizer chose for the coming hour (what a controller would apply).
fn log_decision(plan: &PlanReport) {
    let fs = &plan.first_step;
    let heat_kw: f64 = fs.heat_kw.values().sum();
    let battery_kw = fs.battery_discharge_kw - fs.battery_charge_kw; // + = discharging
                                                                     // Transparency-only suffix: which safety-critical inputs fell back to a placeholder.
    let mut suffix = String::new();
    if !plan.placeholder_inputs.is_empty() {
        suffix.push_str(&format!(
            "  [fallbacks: {}]",
            plan.placeholder_inputs.join("; ")
        ));
    }
    println!(
        "[mpc] {}: mode {} (export {}, inverter {}), heat {heat_kw:.1} kW, battery {battery_kw:+.1} kW, grid import {:.1} / export {:.1} kW \
         ({}h cost {:.2} EUR / {:.0} CZK){suffix}",
        fs.hour_start.format("%Y-%m-%d %H:%M UTC"),
        fs.mode.slot,
        if fs.mode.export_enabled { "on" } else { "off" },
        if fs.mode.inverter_on { "on" } else { "off" },
        fs.grid_import_kw,
        fs.grid_export_kw,
        plan.horizon_hours,
        plan.total_cost_eur,
        plan.total_cost_czk,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339 instant")
    }

    fn kw(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|&(z, v)| (z.to_string(), v)).collect()
    }

    fn committed_next(mark: DateTime<Utc>, heat: &[(&str, f64)]) -> CommittedNext {
        CommittedNext {
            mark,
            heat_kw: kw(heat),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
        }
    }

    // Acceptance G2 (item 3 rework): "with a committed_next whose mark matches the new block, the
    // latch for the new block is its heat_kw".
    #[test]
    fn rollover_adopts_committed_next_at_the_matching_mark() {
        let new_block = utc("2026-01-15T00:15:00Z");
        let committed = committed_next(new_block, &[("A", 2.0), ("B", 0.0)]);
        // What a fresh post-boundary re-decide picked — deliberately the OPPOSITE, so the
        // assertion proves adoption actually won rather than merely matching by coincidence.
        let fresh_block0 = kw(&[("A", 0.0), ("B", 2.0)]);

        let latch = rollover_heat_kw(new_block, Some(&committed), &fresh_block0);

        assert_eq!(latch.get("A").copied(), Some(2.0), "zone A should latch ON");
        assert_eq!(
            latch.get("B").copied(),
            Some(0.0),
            "zone B should latch OFF"
        );
    }

    // Acceptance G2: "no committed_next exists -> today's behaviour" — covers BOTH startup (no prior
    // tick ran at all) and every tick inside the freeze window having been degraded/relaxed
    // (`freeze_committed_next` never commits from an unclean tick, so `committed_next` stays `None`).
    #[test]
    fn rollover_falls_back_when_no_committed_next_exists() {
        let new_block = utc("2026-01-15T00:15:00Z");
        let fresh_block0 = kw(&[("A", 0.0)]);

        let latch = rollover_heat_kw(new_block, None, &fresh_block0);

        assert_eq!(latch, fresh_block0);
    }

    // Acceptance G2: "a committed_next whose mark is not the new block (stale) -> today's
    // behaviour" — e.g. a skipped tick that jumped more than one block.
    #[test]
    fn rollover_falls_back_when_committed_next_is_stale() {
        let new_block = utc("2026-01-15T00:15:00Z");
        let committed = committed_next(utc("2026-01-15T00:00:00Z"), &[("A", 2.0)]); // NOT the new block
        let fresh_block0 = kw(&[("A", 0.0)]);

        let latch = rollover_heat_kw(new_block, Some(&committed), &fresh_block0);

        assert_eq!(
            latch, fresh_block0,
            "a stale commitment must not be adopted"
        );
    }

    // ---- item 3: the freeze window itself ----

    #[test]
    fn in_freeze_window_bounds() {
        let mark = utc("2026-01-15T00:15:00Z");
        assert!(
            in_freeze_window(mark, mark - chrono::Duration::seconds(120)),
            "exactly 120s before the mark is inside the window"
        );
        assert!(
            !in_freeze_window(mark, mark - chrono::Duration::seconds(121)),
            "121s before the mark is still outside the window"
        );
        assert!(
            in_freeze_window(mark, mark),
            "at the mark itself is inside the window"
        );
        assert!(
            in_freeze_window(mark, mark + chrono::Duration::seconds(5)),
            "a late tick past the mark stays frozen (never un-freezes)"
        );
    }

    /// A minimal, otherwise-zeroed [`TimelineBlock`] at `t` with the given `heat_kw`, for the freeze
    /// tests below (which only care about `t`/`heat_kw`/`cool_kw`/`hvac_heat_kw`/`frozen`).
    fn block1_at(t: DateTime<Utc>, heat: &[(&str, f64)]) -> TimelineBlock {
        TimelineBlock {
            t,
            dt_minutes: 15,
            import_price: 0.0,
            export_price: 0.0,
            price_is_placeholder: false,
            pv_kw: 0.0,
            load_kw: 0.0,
            soc_kwh: 0.0,
            charge_kw: 0.0,
            discharge_kw: 0.0,
            grid_import_kw: 0.0,
            grid_export_kw: 0.0,
            curtail_kw: 0.0,
            heat_kw: kw(heat),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            temp_c: HashMap::new(),
            slot: "regular".to_string(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    /// The freeze window pins block 1's decision to the FIRST clean tick that observes it — a later
    /// tick inside the SAME window with a DIFFERENT fresh solve must not change the commitment.
    #[test]
    fn freeze_committed_next_pins_to_the_first_clean_tick_inside_the_window() {
        let mark = utc("2026-01-15T00:15:00Z");
        let tick1 = mark - chrono::Duration::seconds(100);
        let tick2 = mark - chrono::Duration::seconds(40);

        let after_tick1 =
            freeze_committed_next(None, mark, &block1_at(mark, &[("A", 2.0)]), tick1, true);
        let a1 = after_tick1.as_ref().unwrap();
        assert_eq!(a1.mark, mark);
        assert_eq!(a1.heat_kw.get("A").copied(), Some(2.0));

        // Tick 2's fresh solve disagrees (0.0 instead of 2.0) -- must NOT overwrite the commitment.
        let after_tick2 = freeze_committed_next(
            after_tick1,
            mark,
            &block1_at(mark, &[("A", 0.0)]),
            tick2,
            true,
        );
        let a2 = after_tick2.unwrap();
        assert_eq!(
            a2.heat_kw.get("A").copied(),
            Some(2.0),
            "the SECOND tick inside the window must not change the FIRST tick's commitment"
        );
    }

    /// Outside the window, the previous commitment is returned untouched — including `None`.
    #[test]
    fn freeze_committed_next_is_a_no_op_outside_the_window() {
        let mark = utc("2026-01-15T00:15:00Z");
        let well_before = mark - chrono::Duration::seconds(121);

        let result = freeze_committed_next(
            None,
            mark,
            &block1_at(mark, &[("A", 2.0)]),
            well_before,
            true,
        );
        assert!(
            result.is_none(),
            "outside the window a clean tick must not commit anything yet"
        );
    }

    /// A degraded/relaxed tick inside the window with nothing committed yet must keep waiting for a
    /// clean one — not commit a possibly-fictional/fractional decision.
    #[test]
    fn freeze_committed_next_waits_for_a_clean_tick_when_unclean() {
        let mark = utc("2026-01-15T00:15:00Z");
        let tick1 = mark - chrono::Duration::seconds(100);
        let tick2 = mark - chrono::Duration::seconds(40);

        let after_dirty = freeze_committed_next(
            None,
            mark,
            &block1_at(mark, &[("A", 2.0)]),
            tick1,
            false, // degraded/relaxed
        );
        assert!(
            after_dirty.is_none(),
            "an unclean tick must not commit anything"
        );

        let after_clean = freeze_committed_next(
            after_dirty,
            mark,
            &block1_at(mark, &[("A", 3.0)]),
            tick2,
            true,
        );
        assert_eq!(
            after_clean.unwrap().heat_kw.get("A").copied(),
            Some(3.0),
            "the first CLEAN tick inside the window commits"
        );
    }

    // ---- item 3: mirroring the commitment onto next_step ----

    #[test]
    fn next_step_is_frozen_only_when_a_matching_commitment_exists_inside_the_window() {
        let mark = utc("2026-01-15T00:15:00Z");
        let raw_next_step = block1_at(mark, &[("A", 0.0)]); // the tick's own (possibly stale) solve
        let committed = committed_next(mark, &[("A", 2.0)]);

        // Inside the window with a matching commitment: overridden and flagged frozen.
        let frozen = apply_freeze_to_next_step(
            Some(raw_next_step.clone()),
            Some(&committed),
            mark - chrono::Duration::seconds(30),
        )
        .unwrap();
        assert!(frozen.frozen);
        assert_eq!(frozen.heat_kw.get("A").copied(), Some(2.0));

        // No commitment at all: untouched, not frozen.
        let untouched = apply_freeze_to_next_step(
            Some(raw_next_step.clone()),
            None,
            mark - chrono::Duration::seconds(30),
        )
        .unwrap();
        assert!(!untouched.frozen);
        assert_eq!(untouched.heat_kw.get("A").copied(), Some(0.0));

        // A commitment for a DIFFERENT mark: untouched, not frozen.
        let other_mark = committed_next(mark + chrono::Duration::minutes(15), &[("A", 9.0)]);
        let stale = apply_freeze_to_next_step(
            Some(raw_next_step.clone()),
            Some(&other_mark),
            mark - chrono::Duration::seconds(30),
        )
        .unwrap();
        assert!(!stale.frozen);

        // No next_step at all (a very short horizon): stays None.
        assert!(apply_freeze_to_next_step(None, Some(&committed), mark).is_none());
    }

    // Item G tick phase.
    #[test]
    fn delay_to_next_second20_before_the_mark_in_this_minute() {
        let now = utc("2026-01-15T00:03:07.5Z");
        let delay = delay_to_next_second20(now);
        assert!(
            (delay.as_secs_f64() - 12.5).abs() < 1e-9,
            "{delay:?} (expected 12.5s to 00:03:20)"
        );
    }

    #[test]
    fn delay_to_next_second20_past_the_mark_rolls_to_next_minute() {
        let now = utc("2026-01-15T00:03:45.1Z");
        let delay = delay_to_next_second20(now);
        // Next :20 mark is 00:04:20, i.e. 34.9s away.
        assert!(
            (delay.as_secs_f64() - 34.9).abs() < 1e-6,
            "{delay:?} (expected 34.9s to 00:04:20)"
        );
    }

    #[test]
    fn delay_to_next_second20_exactly_at_the_mark_is_zero() {
        let now = utc("2026-01-15T00:03:20Z");
        assert_eq!(delay_to_next_second20(now), Duration::ZERO);
    }
}
