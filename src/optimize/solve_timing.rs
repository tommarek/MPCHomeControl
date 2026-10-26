//! Planner solve-time and feasibility regressions on the REAL house model/config (not a synthetic
//! toy), on the default multi-rate grid (`config.horizon`).
//!
//! The core scenario is a **catch-up**: one zone seeded well below a just-tightened floor, every
//! other zone under winter (or September) conditions. That shape forces a large deviation from the
//! free response and is the hardest instance the live planner meets; it must still produce a
//! feasible, INTEGRAL plan within the live per-tick budget (≤16 s for the whole fix-and-round path).
//!
//! Every scenario runs [`crate::app::fix_and_round_inner`] — the same body
//! [`crate::app::fix_and_round`] runs live (via `solve_bounded`'s strict closure) — so the test
//! cannot drift from production.
//! `Instant` is placed strictly around that call; kernel-building (the one-time, x0-independent dense
//! linear algebra `build_kernel_cache` does at live startup, never repeated per tick) happens first
//! and is NOT timed, matching what a live tick actually pays.
//!
//! When the relaxed LP is already integral (common once the near-term heat/cool pin covers blocks 0
//! and 1), fix-and-round skips the second (pinned) LP, so the plain catch-up test may time a one-LP
//! tick. `catch_up_two_lp_tick_solves_within_budget` re-runs the same scenarios with
//! `force_pinned_resolve: true` to keep the genuine two-LP path timed against the same budget.
//!
//! **Release-only timing**: in a debug build each scenario costs on the order of minutes
//! (unoptimized surrounding Rust/`good_lp` glue), so the timed tests are
//! `#[cfg_attr(debug_assertions, ignore)]`'d — plain `cargo test`/tarpaulin skip them, and CI enforces
//! the timed criteria with their own `cargo test --release` steps for
//! `catch_up_demand_solves_within_budget` and `catch_up_two_lp_tick_solves_within_budget`
//! (`.github/workflows/ci.yml`'s `test` job). A cheap, unconditional companion on a small grid
//! (`catch_up_feasible_on_a_small_grid`) covers feasibility/integrality/temperature sanity in every
//! profile.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};
use nalgebra::DVector;
use petgraph::graph::NodeIndex;

use crate::app::{
    battery_spec, build_kernel_cache, default_pv_array, fix_and_round_inner, pv_arrays, SolveJob,
};
use crate::forecast::consumption::ConsumptionModel;
use crate::model::Model;
use crate::optimize::config::ControlConfig;
use crate::optimize::coordinator::{known_thermal_inputs, ForecastContext};
use crate::optimize::grid::BlockGrid;
use crate::optimize::thermal::KERNEL_BUILD_COUNT;
use crate::optimize::unified::SolveBudget;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::tools::{c_to_k, k_to_c};

/// Every STATE ROW physically part of `zone`: its own air node, plus every internal boundary-layer
/// mass (wall/slab layers) reachable from it without crossing into another zone's air node or a
/// reserved (infinite-capacity) boundary. `rc_network.rs`'s `add_boundary_node` leaves every
/// internal Layered-boundary node's `zone_name: None` — only a zone's own air node ever carries its
/// name — so filtering on `zone_name` alone (as `StateSpace::labels` reports it) finds just the air
/// node; this walks the graph instead. Internal chains never cross-connect except at their two zone
/// endpoints (each Layered boundary builds its own independent chain — see `rc_network.rs`'s module
/// doc), so stopping at any node that DOES carry a name (a real zone, or a reserved `outside`/
/// `ground` boundary, which is also modelled as a named — infinite-capacity — zone) is exactly the
/// right place to stop.
///
/// A catch-up seed must cover all of these: seeding only the air node leaves the zone's own slab/wall
/// mass at the warm base seed, and the free response drifts back toward it before block 0 even
/// closes — not a real catch-up at all.
pub(crate) fn zone_state_rows(net: &RcNetwork, ss: &StateSpace, zone: &str) -> Vec<usize> {
    let start = net.zone_indices[zone];
    let mut rows = Vec::new();
    let mut visited: HashSet<NodeIndex> = HashSet::from([start]);
    let mut frontier = vec![start];
    if let Some(row) = ss.state_index(start) {
        rows.push(row);
    }
    while let Some(node) = frontier.pop() {
        for neighbor in net.graph.neighbors(node) {
            if !visited.insert(neighbor) {
                continue;
            }
            if net.graph[neighbor].zone_name.is_some() {
                continue; // another zone's own air node, or a reserved boundary — stop here
            }
            if let Some(row) = ss.state_index(neighbor) {
                rows.push(row);
                frontier.push(neighbor);
            }
        }
    }
    rows
}

/// The unheated free-response air temperature (°C) of `zone` at the END of block 0 — reconstructed
/// the same way [`crate::optimize::thermal::ThermalContext::free_response`] is (a plain `simulate`
/// with every actuator off), without needing the full condensed-prediction machinery. Used to prove
/// a catch-up scenario is a REAL one: if the free response alone already
/// clears the floor by block 0, the scenario exercises no real catch-up pressure regardless of how
/// the LP then behaves.
fn free_response_c_at_block0(job: &SolveJob, zone: &str) -> f64 {
    let n_fine = job.ctx.grid.n_fine();
    let u_known = known_thermal_inputs(&job.ss, &job.net, &job.ctx, n_fine);
    let traj = job
        .ss
        .simulate(&job.x0, &u_known, job.ctx.step_seconds)
        .expect("free-response simulate");
    let row = job
        .ss
        .state_index(job.net.zone_indices[zone])
        .expect("zone has a state row");
    let end = job.ctx.grid.fine_range(0).end;
    k_to_c(traj[end][row])
}

/// A real day/night price shape (cheap night, expensive evening) on the FINE (15-min) lattice, by
/// each fine step's own UTC hour — so the LP has an actual pre-heat-vs-cost tradeoff to solve, not a
/// trivial "heat whenever" one. `ForecastContext`'s price vectors are fine-lattice (`plan_unified`
/// aggregates them onto the block grid internally); indexing by block would misalign once the grid's
/// hourly section is reached.
fn day_night_prices(start: DateTime<Utc>, n_fine: usize) -> (Vec<f64>, Vec<f64>) {
    let import: Vec<f64> = (0..n_fine)
        .map(|f| {
            let t = start + ChronoDuration::seconds(900 * f as i64);
            match t.hour() {
                17..=20 => 0.35,
                1..=5 => 0.08,
                _ => 0.15,
            }
        })
        .collect();
    let export: Vec<f64> = import.iter().map(|p| p * 0.2).collect();
    (import, export)
}

/// Build one catch-up [`SolveJob`] on a multi-rate grid (`horizon_hours`/`fine_hours` — the live
/// acceptance test passes `config.horizon.{hours, fine_hours}`; the cheap small-grid regression
/// test passes a smaller pair so it stays fast in debug too) — `start` is on a quarter-hour that is
/// NOT on the hour, so `BlockGrid::multi_rate`'s hour-alignment rounding rule is genuinely exercised
/// (a live tick almost never starts exactly on the hour either). Every zone is seeded at `base_c`
/// except the guestroom's air node, seeded at `guestroom_seed_c` — well below `guestroom_floor` (a
/// just-tightened comfort floor, the hardest shape for the planner).
#[allow(clippy::too_many_arguments)]
fn catch_up_job(
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    kernels: Arc<crate::optimize::thermal::KernelSet>,
    start: DateTime<Utc>,
    horizon_hours: usize,
    fine_hours: usize,
    outside_c: f64,
    cloud_cover: f64,
    guestroom_floor: f64,
    guestroom_seed_c: f64,
    base_c: f64,
) -> SolveJob {
    let grid = BlockGrid::multi_rate(start, horizon_hours, fine_hours, 900.0);
    let n_fine = grid.n_fine();

    // Tighten the guestroom's floor — a catch-up scenario, not the live band: the heat target
    // becomes the tightened floor (no dip allowed below it) with at least 1 K of headroom above, so
    // the band stays sane (an internally-contradictory band would make the soft-comfort slack
    // degenerate rather than exercising the catch-up shape this test is after).
    let mut heating = config.heating.clone();
    {
        let room = heating
            .comfort
            .rooms
            .get_mut("guestroom")
            .expect("guestroom is a configured comfort room in config.json5");
        let ceiling = room.heat.target + room.heat.above_k();
        room.heat = crate::optimize::config::BandSpec {
            target: guestroom_floor,
            delta: None,
            below: Some(0.0),
            above: Some((ceiling - guestroom_floor).max(1.0)),
        };
    }

    // Seed EVERY state row belonging to the guestroom — air node AND its internal boundary-layer
    // mass (wall/slab layers) — not just the air node: see `zone_state_rows`'s doc for why seeding
    // the air node alone understates a real catch-up.
    let mut x0 = DVector::from_element(ss.n_states(), c_to_k(base_c));
    for row in zone_state_rows(net, ss, "guestroom") {
        x0[row] = c_to_k(guestroom_seed_c);
    }

    let local_offset = config.site.offset_at(start);
    let (import_price, export_price) = day_night_prices(start, n_fine);

    let mut consumption = ConsumptionModel::new();
    for h in 0..24u32 {
        consumption.add_sample(outside_c, h, false, 0.4);
    }
    consumption.build();

    let ctx = ForecastContext {
        latitude: uom::si::f64::Angle::new::<uom::si::angle::degree>(config.site.latitude),
        longitude: uom::si::f64::Angle::new::<uom::si::angle::degree>(config.site.longitude),
        start,
        step_seconds: 900.0,
        grid,
        local_offset,
        temperature_c: vec![outside_c; n_fine],
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover: vec![cloud_cover; n_fine],
        solar: Vec::new(),
        internal_gain_w: heating.internal_gains(),
        scheduled_loads: Vec::new(),
        load_run_hours: Default::default(),
        room_mode_prior: Default::default(),
        scheduled_w: Vec::new(),
        import_price,
        export_price,
        export_allowed: vec![true; n_fine],
        inverter_on: vec![true; n_fine],
        battery_amortisation: 0.0,
        export_needs_pv: false,
        min_dispatch_kw: 0.0,
        terminal_value: 0.05,
        terminal_heat_basis: 0.05,
        min_final_soc_kwh: None,
        price_is_placeholder: Vec::new(),
        max_import_kw: None,
        max_export_kw: None,
        pv_kw_override: None,
        load_kw_override: None,
        load_scale: 1.0,
        outlook: None,
        price_history: Vec::new(),
        public_holidays: Vec::new(),
        easter_holidays: false,
        distribution_eur_by_local_hour: [0.0; 24],
    };

    let battery = battery_spec(&config.battery);
    let pv = pv_arrays(&config.pv)
        .first()
        .copied()
        .unwrap_or_else(default_pv_array);
    let hvac = config.hvac.clone().unwrap_or_default();

    SolveJob {
        pv,
        consumption,
        battery,
        heating,
        hvac,
        ss: ss.clone(),
        net: net.clone(),
        ctx,
        x0,
        ev_specs: Vec::new(),
        ev_monitored: Vec::new(),
        committed: None,
        held_next_heat: None,
        prev_heat: None,
        kernels: Some(kernels),
    }
}

/// Run `fix_and_round` on `job`, print the timing, and assert every catch-up property:
/// - release-only, `< 16s` wall clock for the FULL fix-and-round path (both LPs + rounding);
/// - unconditional (every profile): a feasible plan, graded `Rounded` (the pinned re-solve
///   succeeded — genuinely integral, not just the advisory relaxed plan), every heat decision
///   inside its zone's physical envelope;
/// - unconditional: NO kernel cache rebuild happens inside the timed region — the shared startup
///   cache built once outside it (see the caller) must serve both LPs via `kernel_set_matches`'
///   `>=` match, exactly like a live tick's cache always does.
///
/// `force_pinned_resolve`: when `true`, skips the "relaxed plan already integral" shortcut and always
/// runs the SECOND (pinned) LP too — see [`crate::app::fix_and_round_inner`]'s doc. `false`
/// reproduces a live tick exactly (`fix_and_round` itself always passes `false`).
fn assert_catch_up_solves_in_budget(label: &str, job: &SolveJob, force_pinned_resolve: bool) {
    let solve_budget = SolveBudget {
        time_limit_s: Some(14.0), // matches app::PER_LP_HIGHS_TIME_LIMIT_S, the live per-LP budget
    };

    let salvage = std::sync::Arc::new(std::sync::Mutex::new(None));
    let builds_before = KERNEL_BUILD_COUNT.with(|c| c.get());
    let started = Instant::now();
    let result = fix_and_round_inner(job, solve_budget, &salvage, force_pinned_resolve);
    let elapsed = started.elapsed();
    let builds_after = KERNEL_BUILD_COUNT.with(|c| c.get());
    assert_eq!(
        builds_after,
        builds_before,
        "{label}: fix-and-round rebuilt the kernel cache {} time(s) inside the timed region — the \
         startup cache built before `Instant::now()` should have matched both LPs",
        builds_after - builds_before
    );
    eprintln!(
        "{label}: fix-and-round took {elapsed:?} ({} profile, grid: {} blocks / {} fine steps)",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        job.ctx.grid.len(),
        job.ctx.grid.n_fine(),
    );

    // Release-only: HiGHS is always compiled optimized, but `cargo test`'s default `dev` profile
    // leaves the surrounding Rust/good_lp glue unoptimized. The debug run still exercises
    // feasibility/integrality below, just without the tight wall-clock bound.
    #[cfg(not(debug_assertions))]
    assert!(
        elapsed < std::time::Duration::from_secs(16),
        "{label}: catch-up fix-and-round took {elapsed:?}, over the 16 s budget"
    );

    let (plan, grade) = result.unwrap_or_else(|e| {
        panic!("{label}: the catch-up scenario must still yield a feasible plan: {e}")
    });
    assert_eq!(
        grade,
        crate::app::SolveGrade::Rounded,
        "{label}: the pinned re-solve must succeed — a feasible INTEGRAL plan, not just the \
         advisory relaxed one"
    );

    // Every heat decision stays within [0, max_heat_kw] for its zone (feasibility — unconditional
    // in every profile).
    for (zone, series) in &plan.heat_kw {
        let max = job
            .heating
            .zones
            .get(zone)
            .map(|z| z.max_heat_kw)
            .unwrap_or(0.0);
        for &kw in series {
            assert!(
                (-1e-6..=max + 1e-6).contains(&kw),
                "{label}: {zone} heat {kw} kW outside [0, {max}]"
            );
        }
    }

    // Temperature consistency (unconditional): every reported zone temperature is finite and
    // within a generously sane physical range — a catch-up scenario pushes the affine prediction
    // and the LP's own auxiliary temperature variables into unusual territory (a large deviation
    // from the free response), where a sign error or an unhinged slack could otherwise silently
    // produce a NaN/huge value that still "solves".
    for (zone, series) in &plan.zone_temp_c {
        for &t in series {
            assert!(
                t.is_finite() && (-40.0..=60.0).contains(&t),
                "{label}: {zone} predicted temperature {t} °C outside a sane range"
            );
        }
    }
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release-only: the fix-and-round timing criterion; run `cargo test --release \
              catch_up_demand_solves_within_budget`"
)]
fn catch_up_demand_solves_within_budget() {
    let model = Model::load("model.json5").expect("model.json5 loads");
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load("config.json5").expect("config.json5 loads");

    // Kernel-building is the one-time, x0-independent dense linear algebra a live tick does ONCE at
    // startup and never repeats — build it here, outside the timed region, exactly like
    // `app::build_kernel_cache`'s own doc says a live tick does. Both scenarios share it (a live
    // brain builds exactly one): same model, same `config.horizon`, and the SAME `:15`-past-the-hour
    // offset below, so both grids have the same fine-step count and the cache matches for both —
    // building it once here and NOT inside the timed calls is only representative of a live tick if
    // it actually matches every time, the way it always does in production.
    let kernels = Arc::new(build_kernel_cache(&config, &net, &ss));

    // (a) Winter: cold and mostly overcast, every heated zone under real thermal pressure — not
    // just the guestroom. `:15` start (not on the hour) exercises BlockGrid::multi_rate's
    // hour-alignment rounding rule.
    let winter = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-01-15T00:15:00Z".parse().unwrap(),
        config.horizon.hours,
        config.horizon.fine_hours,
        -5.0,
        0.9,
        23.2,
        20.2,
        20.0,
    );
    assert_catch_up_solves_in_budget("winter catch-up", &winter, false);

    // (b) September: sensor 20.0 °C under a 21.5 °C floor after a band swap. Milder outside
    // temperature; same `:15` offset as winter (see the kernel-cache comment above).
    let september = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-09-22T00:15:00Z".parse().unwrap(),
        config.horizon.hours,
        config.horizon.fine_hours,
        12.0,
        0.5,
        21.5,
        20.0,
        22.0,
    );
    // Prove this is a REAL catch-up: the unheated free response at block 0 must still sit at least
    // 1 K under the 21.5 °C floor (see `zone_state_rows` for why every guestroom state row is
    // seeded, not just the air node).
    let september_free_c = free_response_c_at_block0(&september, "guestroom");
    eprintln!(
        "September catch-up: guestroom free response at block 0 = {september_free_c:.3} °C \
         (floor 21.5 °C, margin {:.3} K)",
        21.5 - september_free_c
    );
    assert!(
        21.5 - september_free_c >= 1.0,
        "September catch-up must be a REAL catch-up: guestroom's unheated free response at block 0 \
         ({september_free_c:.3} °C) must sit at least 1 K under the 21.5 °C floor, margin {:.3} K",
        21.5 - september_free_c
    );
    assert_catch_up_solves_in_budget("September catch-up", &september, false);
}

/// Cheap companion to [`catch_up_demand_solves_within_budget`]: the SAME two catch-up scenarios, on
/// a small 12 h / 2 h-fine grid instead of the live 36 h / 6 h-fine one, so it runs un-ignored in a
/// plain debug `cargo test` (including tarpaulin). No wall-clock assertion (a small grid's timing
/// says nothing about the live budget); `assert_catch_up_solves_in_budget` still runs every
/// unconditional check (feasibility, integrality, heat envelope, temperature sanity).
///
/// The kernel cache here is sized to THIS test's own small grid (`small_n_fine`, ~47 fine steps for
/// 12h/2h-fine) rather than the live horizon. The debug-mode cost (~80 s) is dominated not by the
/// fine-step count but by the ONE-TIME `StateSpace::discretize` call inside `build_kernels` — a dense
/// van Loan block-matrix exponential over the real house model's ~536 thermal states, paid once
/// regardless of `n`; `Cargo.toml`'s `[profile.dev.package.nalgebra]` override (opt-level 3 for just
/// that dependency) keeps it bearable. `kernel_set_matches`' `>=` rule only requires the cache to
/// cover AT LEAST what a build asks for, so sizing it to the smaller grid still matches both
/// scenarios below — `catch_up_job` always starts at `:15` past the hour, so winter and September
/// share the same `n_fine()`.
#[test]
fn catch_up_feasible_on_a_small_grid() {
    let model = Model::load("model.json5").expect("model.json5 loads");
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load("config.json5").expect("config.json5 loads");

    let small_horizon_hours = 12;
    let small_fine_hours = 2;
    let small_n_fine = BlockGrid::multi_rate(
        "2026-01-15T00:15:00Z".parse().unwrap(),
        small_horizon_hours,
        small_fine_hours,
        900.0,
    )
    .n_fine();
    let (hvac_zones, load_sources) = crate::optimize::coordinator::kernel_inputs(&config);
    let kernels = Arc::new(crate::optimize::thermal::build_kernels(
        &ss,
        &net,
        crate::app::BLOCK_SECONDS,
        small_n_fine,
        &hvac_zones,
        &load_sources,
    ));

    let winter = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-01-15T00:15:00Z".parse().unwrap(),
        small_horizon_hours,
        small_fine_hours,
        -5.0,
        0.9,
        23.2,
        20.2,
        20.0,
    );
    assert_catch_up_solves_in_budget("winter catch-up (small grid)", &winter, false);

    let september = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-09-22T00:15:00Z".parse().unwrap(),
        small_horizon_hours,
        small_fine_hours,
        12.0,
        0.5,
        21.5,
        20.0,
        22.0,
    );
    assert_catch_up_solves_in_budget("September catch-up (small grid)", &september, false);
}

/// The SAME two catch-up scenarios as [`catch_up_demand_solves_within_budget`], but with
/// `force_pinned_resolve: true`: both scenarios' relaxed LPs land on integral values already (the
/// near-term heat/cool pin covers blocks 0 and 1), so the sibling test times a ONE-LP tick. Forcing
/// the pinned re-solve keeps the genuine two-LP fix-and-round path — what a live tick pays whenever
/// the relaxed solve ISN'T already integral — measured against the same live ≤16 s budget.
/// Release-gated like the sibling test, for the same reason.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release-only: run `cargo test --release catch_up_two_lp_tick_solves_within_budget`"
)]
fn catch_up_two_lp_tick_solves_within_budget() {
    let model = Model::load("model.json5").expect("model.json5 loads");
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load("config.json5").expect("config.json5 loads");

    // Same reasoning as catch_up_demand_solves_within_budget: build the kernel cache once, outside
    // the timed region, exactly like a live tick's one startup build.
    let kernels = Arc::new(build_kernel_cache(&config, &net, &ss));

    let winter = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-01-15T00:15:00Z".parse().unwrap(),
        config.horizon.hours,
        config.horizon.fine_hours,
        -5.0,
        0.9,
        23.2,
        20.2,
        20.0,
    );
    assert_catch_up_solves_in_budget("winter catch-up (forced two-LP)", &winter, true);

    let september = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-09-22T00:15:00Z".parse().unwrap(),
        config.horizon.hours,
        config.horizon.fine_hours,
        12.0,
        0.5,
        21.5,
        20.0,
        22.0,
    );
    assert_catch_up_solves_in_budget("September catch-up (forced two-LP)", &september, true);
}

/// MEASURES (does not gate on a target) how much wall-clock a full live-sized plan tick (winter
/// catch-up, the SAME scenario/pipeline as [`catch_up_demand_solves_within_budget`]) costs with NO
/// outlook versus the same tick with an outlook attached at 36 h (the default), 168 h (7 days), and
/// 336 h (14 days, the configured validation ceiling) — printed as a percentage added at each length,
/// for a human to compare against the current baseline. Release-gated for the
/// same reason as the sibling timing tests (the LP solve itself dominates in an unoptimized `dev`
/// build, swamping the signal this test is after).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release-only: run `cargo test --release outlook_cost_at_168h_and_336h`"
)]
fn outlook_cost_at_168h_and_336h() {
    let model = Model::load("model.json5").expect("model.json5 loads");
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load("config.json5").expect("config.json5 loads");
    let kernels = Arc::new(build_kernel_cache(&config, &net, &ss));

    // A fresh job per measurement (a solve mutates nothing persistent, but keeps each timing run
    // independent of any other's LP state) — `outlook_hours: None` means no outlook at all.
    let job_with_outlook = |outlook_hours: Option<usize>| -> SolveJob {
        let mut job = catch_up_job(
            &config,
            &net,
            &ss,
            Arc::clone(&kernels),
            "2026-01-15T00:15:00Z".parse().unwrap(),
            config.horizon.hours,
            config.horizon.fine_hours,
            -5.0,
            0.9,
            23.2,
            20.2,
            20.0,
        );
        job.ctx.outlook = outlook_hours.map(|h| {
            let fine_steps = h * 4; // hourly outlook onto the 15-min fine lattice
            crate::optimize::coordinator::Outlook {
                temperature_c: vec![-5.0; fine_steps],
                cloud_cover: vec![0.9; fine_steps],
                solar: Vec::new(),
            }
        });
        job
    };

    let solve_budget = SolveBudget {
        time_limit_s: Some(14.0),
    };
    let time_it = |job: &SolveJob| -> std::time::Duration {
        let salvage = std::sync::Arc::new(std::sync::Mutex::new(None));
        let started = Instant::now();
        fix_and_round_inner(job, solve_budget, &salvage, false).expect("scenario must still solve");
        started.elapsed()
    };

    let builds_before = KERNEL_BUILD_COUNT.with(|c| c.get());

    let without_elapsed = time_it(&job_with_outlook(None));
    let with_36h_elapsed = time_it(&job_with_outlook(Some(36))); // the live default
    let with_168h_elapsed = time_it(&job_with_outlook(Some(168)));
    let with_336h_elapsed = time_it(&job_with_outlook(Some(336))); // the 14-day ceiling

    let builds_after = KERNEL_BUILD_COUNT.with(|c| c.get());
    assert_eq!(
        builds_after, builds_before,
        "the outlook must never trigger a kernel-cache rebuild (it doesn't feed the LP's own \
         grid) — a rebuild would swamp this measurement with the one-time discretize cost"
    );

    let pct = |with: std::time::Duration| {
        100.0 * (with.as_secs_f64() - without_elapsed.as_secs_f64()) / without_elapsed.as_secs_f64()
    };
    eprintln!(
        "outlook cost: no-outlook tick {without_elapsed:?}; 36h (default) {with_36h_elapsed:?} \
         ({:+.1}% added); 168h {with_168h_elapsed:?} ({:+.1}% added); 336h {with_336h_elapsed:?} \
         ({:+.1}% added)",
        pct(with_36h_elapsed),
        pct(with_168h_elapsed),
        pct(with_336h_elapsed),
    );
}

/// The mild no-AC ceiling tier is CAPPED (`comfort.overheat_max_c`). The real house under sustained
/// NEGATIVE prices (heat is paid for): at EVERY block, no room may sit more than a small tolerance
/// above `max(its heat ceiling + overheat_max_c, its own unheated temperature at that block)` — the
/// cap bounds what HEATING adds, passive warmth (sun, a warm seed) is judged against the free
/// response block by block. A whole-horizon peak bound would hide a mid-horizon overshoot in a room
/// that is passively hot only later. Scenarios: a hot summer day, a cool summer evening (rooms start
/// BELOW the ceilings, so heating alone could run them up), winter with negative prices all horizon
/// / only the first 4 h / only the last 12 h of the fine grid. An uncapped tier runs the first
/// scenario's house to ~32 C.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release-only (real-house 36 h solves); run `cargo test --release \
              sustained_negative_price_keeps_every_real_room_within_the_capped_ceiling`"
)]
fn sustained_negative_price_keeps_every_real_room_within_the_capped_ceiling() {
    /// Slack on the per-block bound: the cap is a soft tier and the solve a MIP-gap incumbent.
    const TOL_C: f64 = 0.3;
    struct Scenario {
        label: &'static str,
        start: &'static str,
        outside_c: f64,
        cloud: f64,
        seed_c: f64,
        /// Fine steps priced negative from the start (`usize::MAX` = all); `0` = only the last 12 h.
        neg_fine_steps: usize,
        terminal_heat_basis: f64,
    }
    let scenarios = [
        Scenario {
            label: "summer-all-neg",
            start: "2026-06-14T09:15:00Z",
            outside_c: 26.0,
            cloud: 0.0,
            seed_c: 25.0,
            neg_fine_steps: usize::MAX,
            terminal_heat_basis: 0.05,
        },
        Scenario {
            label: "evening-cool-all-neg",
            start: "2026-06-14T18:15:00Z",
            outside_c: 15.0,
            cloud: 1.0,
            seed_c: 21.0,
            neg_fine_steps: usize::MAX,
            terminal_heat_basis: 0.05,
        },
        Scenario {
            label: "winter-4h-neg",
            start: "2026-01-14T09:15:00Z",
            outside_c: 0.0,
            cloud: 0.9,
            seed_c: 22.5,
            neg_fine_steps: 16,
            terminal_heat_basis: 0.5,
        },
        Scenario {
            label: "winter-all-neg",
            start: "2026-01-14T09:15:00Z",
            outside_c: 0.0,
            cloud: 0.9,
            seed_c: 22.5,
            neg_fine_steps: usize::MAX,
            terminal_heat_basis: 0.05,
        },
        Scenario {
            label: "winter-tail-neg",
            start: "2026-01-14T09:15:00Z",
            outside_c: 0.0,
            cloud: 0.9,
            seed_c: 22.5,
            neg_fine_steps: 0,
            terminal_heat_basis: 0.05,
        },
    ];
    let model = Model::load("model.json5").expect("model.json5 loads");
    let net: RcNetwork = (&model).into();
    let ss: StateSpace = (&net).into();
    let config = ControlConfig::load("config.json5").expect("config.json5 loads");
    let kernels = Arc::new(build_kernel_cache(&config, &net, &ss));
    for sc in scenarios {
        let start: DateTime<Utc> = sc.start.parse().unwrap();
        let mut job = catch_up_job(
            &config,
            &net,
            &ss,
            Arc::clone(&kernels),
            start,
            config.horizon.hours,
            config.horizon.fine_hours,
            sc.outside_c,
            sc.cloud,
            18.0,
            sc.seed_c,
            sc.seed_c,
        );
        job.ctx.terminal_heat_basis = sc.terminal_heat_basis;
        let n_fine = job.ctx.grid.n_fine();
        job.ctx.import_price = (0..n_fine)
            .map(|f| {
                let negative = if sc.neg_fine_steps == 0 {
                    f + 48 >= n_fine
                } else {
                    f < sc.neg_fine_steps
                };
                if negative {
                    -1.0
                } else {
                    0.15
                }
            })
            .collect();
        job.ctx.export_price = job
            .ctx
            .import_price
            .iter()
            .map(|&p| if p < 0.0 { p } else { p * 0.2 })
            .collect();
        let salvage = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (plan, _) = fix_and_round_inner(
            &job,
            SolveBudget {
                time_limit_s: Some(60.0),
            },
            &salvage,
            false,
        )
        .unwrap_or_else(|e| panic!("{}: the scenario yields a plan: {e}", sc.label));
        let u_known = known_thermal_inputs(&job.ss, &job.net, &job.ctx, n_fine);
        let passive = job
            .ss
            .simulate(&job.x0, &u_known, job.ctx.step_seconds)
            .expect("free-response simulate");
        let cap = job.heating.comfort.overheat_max_c;
        assert_eq!(cap, 1.5, "the shipped config carries the owner's cap");
        let mut checked = 0;
        for (zone, room) in &job.heating.comfort.rooms {
            let Some(temps) = plan.zone_temp_c.get(zone) else {
                continue;
            };
            let ceiling = room.heat.target + room.heat.above_k();
            let row = job
                .ss
                .state_index(job.net.zone_indices[zone.as_str()])
                .expect("zone has a state row");
            for (k, &t) in temps.iter().enumerate().take(job.ctx.grid.len()) {
                // `temps[k]` is the end of block k = the state after its last fine step.
                let unheated = k_to_c(passive[job.ctx.grid.fine_range(k).end][row]);
                let bound = (ceiling + cap).max(unheated) + TOL_C;
                assert!(
                    t <= bound,
                    "{}: {zone} block {k}: {t:.2} C exceeds max(ceiling {ceiling} + cap {cap}, \
                     unheated {unheated:.2}) + {TOL_C}",
                    sc.label
                );
            }
            checked += 1;
        }
        assert!(
            checked > 10,
            "{}: the check covered the house: {checked} rooms",
            sc.label
        );
    }
}
