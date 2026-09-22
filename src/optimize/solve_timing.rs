//! Acceptance criterion 3 (item F): a catch-up scenario (a zone seeded well below a just-tightened
//! floor) must still produce a feasible, INTEGRAL plan within budget, on the REAL house model/config
//! — not a synthetic toy — and on the DEFAULT multi-rate grid (`config.horizon`). This is exactly the
//! instance shape that stalled the old unbounded `microlp` MILP tick after tick
//! (`memory/mpchc-band-swap-planner-timeout.md`), and that the first HiGHS/MILP attempt (item A alone,
//! commit 974fa3f) still could not solve in time on the uniform 144-block grid (measured ~94-98 s,
//! see this module's git history) — the multi-rate grid (item F) is what actually fixes it, by making
//! the LP ~4x smaller.
//!
//! Both scenarios run [`crate::app::fix_and_round`] — the EXACT function a live tick calls (via
//! `solve_bounded`'s strict closure) — so this test cannot silently drift from what production runs.
//! `Instant` is placed strictly around that call; kernel-building (the one-time, x0-independent dense
//! linear algebra `build_kernel_cache` does at live startup, never repeated per tick) happens first
//! and is NOT timed, matching what a live tick actually pays.
//!
//! Reference point, measured release / dev box: winter 13.3 s, September 12.5 s — both comfortably
//! under the 16 s budget below. **Release-only**: in a debug build this same test costs on the order
//! of 5 minutes PER SCENARIO (unoptimized surrounding Rust/`good_lp` glue), so the test itself is
//! `#[cfg_attr(debug_assertions, ignore)]`'d — plain `cargo test`/tarpaulin skip it, and CI enforces
//! the timed criterion directly with its own `cargo test --release
//! catch_up_demand_solves_within_budget` step (`.github/workflows/ci.yml`'s `test` job).

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};
use nalgebra::DVector;

use crate::app::{
    battery_spec, build_kernel_cache, default_pv_array, fix_and_round, pv_arrays, SolveJob,
};
use crate::forecast::consumption::ConsumptionModel;
use crate::model::Model;
use crate::optimize::config::ControlConfig;
use crate::optimize::coordinator::ForecastContext;
use crate::optimize::grid::BlockGrid;
use crate::optimize::unified::SolveBudget;
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::tools::c_to_k;

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

/// Build one catch-up [`SolveJob`] on the DEFAULT multi-rate grid (`config.horizon.{hours,
/// fine_hours}`) — `start` is on a quarter-hour that is NOT on the hour, so `BlockGrid::multi_rate`'s
/// hour-alignment rounding rule is genuinely exercised (a live tick almost never starts exactly on
/// the hour either). Every zone is seeded at `base_c` except the guestroom's air node, seeded at
/// `guestroom_seed_c` — well below `guestroom_floor` (a just-tightened comfort floor, the shape that
/// stalled the old planner).
#[allow(clippy::too_many_arguments)]
fn catch_up_job(
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    kernels: Arc<crate::optimize::thermal::KernelSet>,
    start: DateTime<Utc>,
    outside_c: f64,
    cloud_cover: f64,
    guestroom_floor: f64,
    guestroom_seed_c: f64,
    base_c: f64,
) -> SolveJob {
    let grid = BlockGrid::multi_rate(
        start,
        config.horizon.hours,
        config.horizon.fine_hours,
        900.0,
    );
    let n_fine = grid.n_fine();

    // Tighten the guestroom's floor — a catch-up scenario, not today's live band. Also raise t_max
    // so the band stays sane (an internally-contradictory band would make the soft-comfort slack
    // degenerate rather than exercising the catch-up shape this test is after).
    let mut heating = config.heating.clone();
    {
        let z = heating
            .zones
            .get_mut("guestroom")
            .expect("guestroom is a configured heated zone in config.json5");
        z.t_min = guestroom_floor;
        z.t_max = z.t_max.max(z.t_min + 1.0);
    }

    let mut x0 = DVector::from_element(ss.n_states(), c_to_k(base_c));
    let guestroom_node = net.zone_indices["guestroom"];
    let guestroom_row = ss
        .state_index(guestroom_node)
        .expect("guestroom has a state row");
    x0[guestroom_row] = c_to_k(guestroom_seed_c);

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
        scheduled_w: Vec::new(),
        import_price,
        export_price,
        export_allowed: vec![true; n_fine],
        inverter_on: vec![true; n_fine],
        battery_amortisation: 0.0,
        terminal_value: 0.05,
        min_final_soc_kwh: None,
        price_is_placeholder: Vec::new(),
        max_import_kw: None,
        max_export_kw: None,
        pv_kw_override: None,
        load_scale: 1.0,
        outlook: None,
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
        kernels: Some(kernels),
    }
}

/// Run `fix_and_round` on `job`, print the timing, and assert every acceptance-3 property:
/// - release-only, `< 16s` wall clock for the FULL fix-and-round path (both LPs + rounding);
/// - unconditional (every profile): a feasible plan, graded `Rounded` (the pinned re-solve
///   succeeded — genuinely integral, not just the advisory relaxed plan), every heat decision
///   inside its zone's physical envelope.
fn assert_catch_up_solves_in_budget(label: &str, job: &SolveJob) {
    let solve_budget = SolveBudget {
        time_limit_s: Some(14.0), // matches app::PER_LP_HIGHS_TIME_LIMIT_S, the live per-LP budget
    };

    let started = Instant::now();
    let result = fix_and_round(job, solve_budget);
    let elapsed = started.elapsed();
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
        -5.0,
        0.9,
        23.2,
        20.2,
        20.0,
    );
    assert_catch_up_solves_in_budget("winter catch-up", &winter);

    // (b) September: the live incident this brief exists to fix (spec.md's "Examples" — sensor
    // 20.0 °C, floor 21.5 °C after the room_1/guestroom band swap). Milder outside temperature;
    // same `:15` offset as winter (see the kernel-cache comment above).
    let september = catch_up_job(
        &config,
        &net,
        &ss,
        Arc::clone(&kernels),
        "2026-09-22T00:15:00Z".parse().unwrap(),
        12.0,
        0.5,
        21.5,
        20.0,
        22.0,
    );
    assert_catch_up_solves_in_budget("September catch-up", &september);
}
