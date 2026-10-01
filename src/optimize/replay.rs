//! Replay a planned dispatch under the Growatt's real PV-dark export restriction.
//!
//! `optimize_unified` books whatever revenue a `batt_to_grid` decision earns on paper; the real
//! inverter refuses battery-sourced grid export while its PV input reads (near) 0 W, so a plan
//! computed WITHOUT `export_needs_pv` (or evaluated before the gate existed) books export revenue
//! in dark blocks that never materialises. [`replay_dark_export`] walks such a plan block by block
//! and re-derives what would ACTUALLY happen: the blocked export stays stored in the battery
//! (`retained`) and is greedily reused later — first to displace planned grid-charging, then to
//! cover grid-supplied load within the battery's remaining discharge headroom (only while the
//! inverter is on) — with any energy still retained beyond the battery's capacity displacing solar
//! charging instead (`excess/η_c` kWh of AC solar can no longer enter the battery and is exported
//! directly when the block allows it, else curtailed).
//!
//! This is a read-only, pure-function EVALUATION of a plan already computed elsewhere — it changes
//! no decision and re-solves nothing. It is deliberately greedy in time order (use retained energy
//! as soon as a later block can take it), which is feasible but not necessarily optimal: a
//! receding-horizon re-solve would likely do somewhat better, so this OVERSTATES the old plan's
//! realized cost slightly. It only accounts for the battery/grid terms of the objective (grid cash,
//! wear, terminal SoC value) — heating, comfort and EV-target terms are unaffected by the export
//! gate and excluded here.

use super::battery::{BatterySpec, DispatchInputs};
use super::unified::{FlowParams, UnifiedPlan, PV_PRESENT_KW};

const REPLAY_EPS: f64 = 1e-9;

/// Everything [`replay_dark_export`] needs to evaluate a plan: the plan itself and the exact LP
/// inputs it was solved from. `pub(crate)` — an internal hook for read-only tooling (`export_audit`),
/// never part of the public/API plan shape.
#[derive(Debug, Clone)]
pub(crate) struct ReplayInputs {
    pub(crate) plan: UnifiedPlan,
    pub(crate) inputs: DispatchInputs,
    pub(crate) flow: FlowParams,
    pub(crate) battery: BatterySpec,
    pub(crate) dt_hours: Vec<f64>,
}

/// The battery/grid economics of one plan, before and after replaying it under the real PV-dark
/// export restriction. All fields are EUR (or CZK, whatever price-units `inputs`/`flow` carry) or
/// kWh as named; see [`replay_dark_export`]'s module doc for the accounting rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayOutcome {
    /// The battery/grid part of the plan's own objective, computed from its AS-PLANNED
    /// `grid_import_kw` / `grid_export_kw` / `discharge_kw` / final `soc_kwh`: `Σ dt·(import_price·
    /// grid_import − export_price·grid_export) + amortisation·Σ dt·discharge −
    /// terminal_value·η_d·final_soc`. Excludes heating/comfort/EV terms.
    pub planned_grid_cost: f64,
    /// The same quantity re-derived after blocking every dark-block `batt_to_grid` decision and
    /// greedily reusing the retained energy (displaced grid-charging, then grid-supplied load,
    /// then overflow export/curtailment) — what the plan would actually have cost the house.
    pub realized_grid_cost: f64,
    /// Total battery→grid energy (kWh) the plan booked in blocks with forecast `pv_kw` at/below
    /// [`PV_PRESENT_KW`] — the export that never materialises.
    pub blocked_export_kwh: f64,
    /// Gross export revenue (price-units) the plan booked for `blocked_export_kwh`, before any
    /// wear saved or later reuse of the retained energy — the raw size of the fiction.
    pub blocked_revenue: f64,
    /// Retained kWh (stored-energy units) put to productive use later — displacing a planned
    /// grid-charge or covering grid-supplied load — rather than sitting unused or overflowing.
    pub retained_used_kwh: f64,
    /// Retained kWh that pushed the battery over its capacity at some block (`soc_kwh[i] +
    /// retained > max_soc_kwh`) and had to leave the battery immediately, exported when the block
    /// allowed it or curtailed otherwise.
    pub overflow_kwh: f64,
    /// Retained kWh still unspent at the end of the horizon, valued at `terminal_value·η_d` in
    /// `realized_grid_cost` exactly as the LP values leftover SoC.
    pub leftover_kwh: f64,
}

/// Replay `plan` (from [`super::unified::optimize_unified`]) under the real device constraint:
/// battery→grid export cannot happen while the inverter's PV input is (near) 0 W, REGARDLESS of
/// whether the plan itself was computed with `flow.export_needs_pv` on. See the module doc for the
/// block-by-block rule and its bias. `dt_hours[i]` must be the same per-block length the plan was
/// solved on (`ThermalContext::grid.dt_hours_vec()`).
pub fn replay_dark_export(
    plan: &UnifiedPlan,
    inputs: &DispatchInputs,
    flow: &FlowParams,
    battery: &BatterySpec,
    dt_hours: &[f64],
) -> ReplayOutcome {
    let eta_c = battery.charge_efficiency;
    let eta_d = battery.discharge_efficiency;

    let planned_grid_cost = grid_cost(
        &plan.grid_import_kw,
        &plan.grid_export_kw,
        &plan.discharge_kw,
        inputs,
        flow,
        dt_hours,
        plan.soc_kwh
            .last()
            .copied()
            .unwrap_or(battery.initial_soc_kwh),
        eta_d,
    );

    let mut retained = 0.0;
    let mut blocked_export_kwh = 0.0;
    let mut blocked_revenue = 0.0;
    let mut wear_saved = 0.0;
    let mut saved_grid_charge_cost = 0.0;
    let mut saved_load_cost = 0.0;
    let mut wear_booked_load = 0.0;
    let mut overflow_export_credit = 0.0;
    let mut overflow_kwh = 0.0;
    let mut retained_used_kwh = 0.0;

    for (i, &dt) in dt_hours.iter().enumerate() {
        // 1. The plan's dark-block battery→grid decision cannot happen: it never leaves the
        // battery, so the stored energy it would have consumed stays retained.
        let b = plan.batt_to_grid_kw[i];
        if inputs.pv_kw[i] <= PV_PRESENT_KW && b > REPLAY_EPS {
            blocked_export_kwh += b * dt;
            blocked_revenue += inputs.export_price[i] * b * dt;
            wear_saved += flow.amortisation * b * dt;
            retained += b * dt / eta_d;
        }

        // 2. Retained energy first displaces the plan's own grid-charging: no need to buy from the
        // grid what is already sitting in the battery.
        let g = plan.batt_grid_charge_kw[i];
        if g > REPLAY_EPS && retained > REPLAY_EPS {
            let displaced_stored = (g * dt * eta_c).min(retained);
            saved_grid_charge_cost += inputs.import_price[i] * displaced_stored / eta_c;
            retained -= displaced_stored;
            retained_used_kwh += displaced_stored;
        }

        // 3. Remaining retained energy covers grid-supplied load, within the battery's spare
        // discharge headroom AS PLANNED (a conservative choice — it does not credit the headroom
        // freed by step 1's removed export; part of this function's documented overstatement).
        // Skipped when the inverter is off this block: the LP itself bans battery→load there
        // (`leg(off(i))` in `unified.rs`), so the battery cannot physically cover the load either.
        if flow.inverter_on[i] {
            let ev_grid_sum: f64 = plan
                .ev_grid_kw
                .values()
                .map(|v| v.get(i).copied().unwrap_or(0.0))
                .sum();
            let grid_to_load =
                (plan.grid_import_kw[i] - plan.batt_grid_charge_kw[i] - ev_grid_sum).max(0.0);
            let headroom_kw = (battery.max_discharge_kw - plan.discharge_kw[i]).max(0.0);
            let covered = (grid_to_load * dt)
                .min(headroom_kw * dt)
                .min(retained * eta_d);
            if covered > REPLAY_EPS {
                saved_load_cost += inputs.import_price[i] * covered;
                wear_booked_load += flow.amortisation * covered;
                retained -= covered / eta_d;
                retained_used_kwh += covered / eta_d;
            }
        }

        // 4. Retained energy that would push this block's SoC past capacity has to leave the
        // battery right away: it is standing in for solar that would otherwise have charged the
        // battery, so `excess/η_c` kWh of that solar can no longer be stored and is exported
        // directly instead (the block allowing it), else curtailed. Only when there is retained
        // energy to overflow — a plan's SoC sitting fractionally (solver tolerance) above capacity
        // with nothing retained must not manufacture spurious overflow/leftover.
        if retained > REPLAY_EPS {
            let excess = (plan.soc_kwh[i] + retained - battery.max_soc_kwh)
                .max(0.0)
                .min(retained);
            if excess > REPLAY_EPS {
                retained -= excess;
                overflow_kwh += excess;
                if flow.export_allowed[i] && flow.inverter_on[i] {
                    overflow_export_credit += inputs.export_price[i] * excess / eta_c;
                }
            }
        }
    }

    let leftover_kwh = retained;
    let terminal_leftover_credit = flow.terminal_value * eta_d * leftover_kwh;
    let realized_grid_cost =
        planned_grid_cost + blocked_revenue - wear_saved - saved_grid_charge_cost - saved_load_cost
            + wear_booked_load
            - overflow_export_credit
            - terminal_leftover_credit;

    ReplayOutcome {
        planned_grid_cost,
        realized_grid_cost,
        blocked_export_kwh,
        blocked_revenue,
        retained_used_kwh,
        overflow_kwh,
        leftover_kwh,
    }
}

/// `Σ dt·(import_price·grid_import − export_price·grid_export) + amortisation·Σ dt·discharge −
/// terminal_value·η_d·final_soc` — the battery/grid part of `optimize_unified`'s objective (see
/// `unified.rs`'s `grid_cash` + wear term + terminal-value term), evaluated from concrete per-block
/// vectors rather than LP expressions.
#[allow(clippy::too_many_arguments)]
fn grid_cost(
    grid_import_kw: &[f64],
    grid_export_kw: &[f64],
    discharge_kw: &[f64],
    inputs: &DispatchInputs,
    flow: &FlowParams,
    dt_hours: &[f64],
    final_soc_kwh: f64,
    discharge_efficiency: f64,
) -> f64 {
    let n = dt_hours.len();
    let grid_cash: f64 = (0..n)
        .map(|i| {
            (inputs.import_price[i] * grid_import_kw[i]
                - inputs.export_price[i] * grid_export_kw[i])
                * dt_hours[i]
        })
        .sum();
    let wear: f64 = (0..n)
        .map(|i| flow.amortisation * discharge_kw[i] * dt_hours[i])
        .sum();
    grid_cash + wear - flow.terminal_value * discharge_efficiency * final_soc_kwh
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn battery_spec() -> BatterySpec {
        BatterySpec {
            max_charge_kw: 5.0,
            max_discharge_kw: 5.0,
            charge_efficiency: 0.95,
            discharge_efficiency: 0.95,
            min_soc_kwh: 0.0,
            max_soc_kwh: 10.0,
            initial_soc_kwh: 5.0,
        }
    }

    fn permissive_flow(n: usize, amortisation: f64, terminal_value: f64) -> FlowParams {
        FlowParams {
            export_allowed: vec![true; n],
            inverter_on: vec![true; n],
            price_placeholder: Vec::new(),
            amortisation,
            terminal_value,
            terminal_heat_value: 0.0,
            terminal_heat_value_by_zone: HashMap::new(),
            terminal_heat_budget_kwh: HashMap::new(),
            max_import_kw: None,
            max_export_kw: None,
            export_needs_pv: false,
        }
    }

    fn inputs(n: usize, import: f64, export: f64, pv_kw: Vec<f64>) -> DispatchInputs {
        DispatchInputs {
            dt_hours: 0.25,
            import_price: vec![import; n],
            export_price: vec![export; n],
            pv_kw,
            load_kw: vec![0.0; n],
            min_final_soc_kwh: None,
        }
    }

    fn bare_plan(n: usize) -> UnifiedPlan {
        UnifiedPlan {
            charge_kw: vec![0.0; n],
            discharge_kw: vec![0.0; n],
            grid_import_kw: vec![0.0; n],
            batt_grid_charge_kw: vec![0.0; n],
            batt_to_grid_kw: vec![0.0; n],
            grid_export_kw: vec![0.0; n],
            curtail_kw: vec![0.0; n],
            soc_kwh: vec![0.0; n],
            load_kw: vec![0.0; n],
            heat_kw: HashMap::new(),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            zone_temp_c: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            ev_solar_kw: HashMap::new(),
            ev_grid_kw: HashMap::new(),
            ev_batt_kw: HashMap::new(),
            ev_bonus_block: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            total_cost: 0.0,
            terminal_heat_credit: HashMap::new(),
            export_pv_gated_blocks: 0,
        }
    }

    /// Invariant: a plan with no dark export (every `batt_to_grid` block has real PV, or is 0)
    /// replays to EXACTLY its own planned cost — nothing for the rules to change.
    #[test]
    fn no_dark_export_replays_to_planned_cost() {
        let n = 4;
        let dt = vec![0.25; n];
        let flow = permissive_flow(n, 0.05, 0.1);
        let inputs = inputs(n, 0.30, 0.20, vec![2.0; n]); // PV present every block
        let mut plan = bare_plan(n);
        plan.grid_import_kw = vec![0.1, 0.0, 0.3, 0.0];
        plan.grid_export_kw = vec![0.0, 1.0, 0.0, 0.5];
        plan.batt_to_grid_kw = vec![0.0, 1.0, 0.0, 0.5]; // exported, but PV is present: not dark
        plan.discharge_kw = vec![0.0, 1.0, 0.0, 0.5];
        plan.soc_kwh = vec![4.5, 4.0, 4.0, 3.5];
        let bat = battery_spec();

        let outcome = replay_dark_export(&plan, &inputs, &flow, &bat, &dt);
        assert!((outcome.realized_grid_cost - outcome.planned_grid_cost).abs() < 1e-9);
        assert_eq!(outcome.blocked_export_kwh, 0.0);
        assert_eq!(outcome.blocked_revenue, 0.0);
        assert_eq!(outcome.overflow_kwh, 0.0);
        assert_eq!(outcome.leftover_kwh, 0.0);
    }

    /// Hand-built 3-block plan: block 0 books a dark battery→grid export that can't happen, block
    /// 1 plans a grid charge the retained energy displaces, block 2 has grid-supplied load the
    /// retained energy covers. Every number checked by hand.
    #[test]
    fn dark_export_reused_for_grid_charge_then_load() {
        let n = 3;
        let dt = vec![0.25; n]; // 15-minute blocks
        let flow = permissive_flow(n, 0.02, 0.0);
        let import = vec![0.30, 0.30, 0.30];
        let export = vec![0.28, 0.28, 0.28];
        let inputs = inputs(n, 0.0, 0.0, vec![0.0, 0.0, 0.0]); // dark all 3 blocks
        let inputs = DispatchInputs {
            import_price: import,
            export_price: export,
            ..inputs
        };
        let mut plan = bare_plan(n);
        // Block 0: plan discharges 2 kW to the grid for 0.25 h — 0.5 kWh export that can't happen.
        plan.batt_to_grid_kw[0] = 2.0;
        plan.discharge_kw[0] = 2.0;
        plan.grid_export_kw[0] = 2.0;
        plan.grid_import_kw[0] = 0.0;
        plan.soc_kwh[0] = 5.0 - 2.0 * 0.25 / 0.95; // initial 5.0 minus the planned discharge
                                                   // Block 1: plan grid-charges the battery at 1 kW for 0.25 h — the retained energy replaces it.
        plan.batt_grid_charge_kw[1] = 1.0;
        plan.grid_import_kw[1] = 1.0;
        plan.soc_kwh[1] = plan.soc_kwh[0] + 1.0 * 0.25 * 0.95;
        // Block 2: plan imports 0.5 kW purely for the house load (no charge, no EV).
        plan.grid_import_kw[2] = 0.5;
        plan.soc_kwh[2] = plan.soc_kwh[1];
        let bat = battery_spec();

        let outcome = replay_dark_export(&plan, &inputs, &flow, &bat, &dt);

        // retained after block 0: 2.0 * 0.25 / 0.95 = 0.526315789...
        let retained0 = 2.0 * 0.25 / 0.95;
        assert!((outcome.blocked_export_kwh - 0.5).abs() < 1e-9);
        assert!((outcome.blocked_revenue - 0.28 * 0.5).abs() < 1e-9);

        // Block 1: displaced_stored = min(1.0 * 0.25 * 0.95, retained0) = min(0.2375, 0.5263) = 0.2375
        let displaced = (1.0_f64 * 0.25 * 0.95).min(retained0);
        let retained1 = retained0 - displaced;
        let saved_charge = 0.30 * displaced / 0.95;

        // Block 2: grid_to_load = 0.5 - 0 - 0 = 0.5; headroom = 5.0 - 0.0 = 5.0;
        // covered = min(0.5*0.25, 5.0*0.25, retained1*0.95)
        let covered = (0.5_f64 * 0.25).min(5.0 * 0.25).min(retained1 * 0.95);
        let retained2 = retained1 - covered / 0.95;
        let saved_load = 0.30 * covered;

        assert!((outcome.retained_used_kwh - (displaced + covered / 0.95)).abs() < 1e-9);
        assert_eq!(outcome.overflow_kwh, 0.0);
        assert!((outcome.leftover_kwh - retained2).abs() < 1e-9);

        let expected_realized = outcome.planned_grid_cost + outcome.blocked_revenue
            - flow.amortisation * 0.5 // wear saved on the blocked 0.5 kWh
            - saved_charge
            - saved_load
            + flow.amortisation * covered // wear booked on the load-covering discharge
            - flow.terminal_value * 0.95 * retained2;
        assert!((outcome.realized_grid_cost - expected_realized).abs() < 1e-6);
    }

    /// Overflow: the battery is already at capacity, so retained energy from a blocked export has
    /// nowhere to go this block and is exported immediately (the block allows it) rather than
    /// accumulating past `max_soc_kwh`.
    #[test]
    fn overflow_above_capacity_is_exported_when_allowed() {
        let n = 2;
        let dt = vec![1.0, 1.0];
        let flow = permissive_flow(n, 0.0, 0.0);
        // Block 0: dark, battery at a high but plausible SoC, plans a 3 kWh export. Block 1: PV
        // present (a solar-charging block), plan ends it exactly AT capacity — so the retained
        // energy from block 0's blocked export has nowhere to go and overflows there.
        let inputs = inputs(n, 0.30, 0.0, vec![0.0, 2.0]);
        let inputs = DispatchInputs {
            export_price: vec![0.25, 0.20],
            ..inputs
        };
        let bat = battery_spec(); // max_soc_kwh 10.0, η_c = η_d = 0.95
        let mut plan = bare_plan(n);
        plan.batt_to_grid_kw[0] = 3.0; // 3 kWh planned export this block
        plan.discharge_kw[0] = 3.0;
        plan.grid_export_kw[0] = 3.0;
        plan.soc_kwh[0] = 8.0 - 3.0 / bat.discharge_efficiency; // physically consistent post-discharge SoC
        plan.soc_kwh[1] = bat.max_soc_kwh; // solar charges the battery to exactly full

        let outcome = replay_dark_export(&plan, &inputs, &flow, &bat, &dt);
        // retained after block 0 = 3.0 / 0.95; block 1's planned SoC is already at capacity, so
        // ALL of it overflows there (none fits).
        let retained = 3.0 / bat.discharge_efficiency;
        assert!((outcome.overflow_kwh - retained).abs() < 1e-9);
        assert!(outcome.leftover_kwh.abs() < 1e-9);

        // The overflow is displaced SOLAR charging, not battery discharge: `excess/η_c` kWh of AC
        // solar can no longer enter the battery and is exported directly instead, at BLOCK 1's
        // price — credited at `export_price[1] * excess / η_c`, not `* η_d`.
        let overflow_credit = inputs.export_price[1] * retained / bat.charge_efficiency;
        let expected_realized =
            outcome.planned_grid_cost + outcome.blocked_revenue - overflow_credit;
        assert!((outcome.realized_grid_cost - expected_realized).abs() < 1e-9);
    }

    /// The inverter-off gate: step 3 (retained energy covering grid-supplied load) must not apply
    /// while the inverter is off — the LP itself bans battery→load there (`leg(off(i))`).
    #[test]
    fn step_3_skips_load_coverage_when_inverter_is_off() {
        let n = 2;
        let dt = vec![1.0, 1.0];
        let mut flow = permissive_flow(n, 0.0, 0.0);
        flow.inverter_on[1] = false;
        let inputs = inputs(n, 0.30, 0.28, vec![0.0, 0.0]); // dark both blocks
        let mut plan = bare_plan(n);
        plan.batt_to_grid_kw[0] = 2.0;
        plan.discharge_kw[0] = 2.0;
        plan.grid_export_kw[0] = 2.0;
        plan.soc_kwh[0] = 5.0 - 2.0 / 0.95;
        // Block 1: inverter off, but the plan still shows a grid import for the load — the battery
        // must NOT be credited with covering it despite having retained energy and spare headroom.
        plan.grid_import_kw[1] = 1.0;
        plan.soc_kwh[1] = plan.soc_kwh[0];
        let bat = battery_spec();

        let outcome = replay_dark_export(&plan, &inputs, &flow, &bat, &dt);
        let retained = 2.0 / 0.95;
        assert!((outcome.leftover_kwh - retained).abs() < 1e-9); // untouched by step 3
        assert_eq!(outcome.retained_used_kwh, 0.0);
    }

    /// No dark export, but the plan's SoC sits fractionally (solver tolerance) ABOVE capacity
    /// with nothing retained — must not manufacture spurious overflow/leftover.
    #[test]
    fn no_retained_energy_yields_no_spurious_overflow() {
        let n = 1;
        let dt = vec![1.0];
        let flow = permissive_flow(n, 0.0, 0.0);
        let inputs = inputs(n, 0.30, 0.25, vec![2.0]); // PV present: not dark
        let mut plan = bare_plan(n);
        let mut bat = battery_spec();
        bat.max_soc_kwh = 10.0;
        plan.soc_kwh[0] = 10.0 + 1e-7; // solver-tolerance overshoot, no dark export at all

        let outcome = replay_dark_export(&plan, &inputs, &flow, &bat, &dt);
        assert_eq!(outcome.overflow_kwh, 0.0);
        assert_eq!(outcome.leftover_kwh, 0.0);
        assert!((outcome.realized_grid_cost - outcome.planned_grid_cost).abs() < 1e-12);
    }
}
