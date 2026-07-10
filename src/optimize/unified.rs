//! Unified economic + thermal optimizer (energy-flow model).
//!
//! Co-optimizes battery dispatch and **price-responsive heating** over the horizon as a single LP.
//! The battery/PV side mirrors the `loxone_smart_home` energy-flow model: each block's solar is
//! split across house / battery / grid / curtailment, the house load (including the heat-pump
//! electricity, `Σ heat / COP`) is met from solar / battery / grid, and the objective is the net
//! grid cash plus battery **wear** on discharge, a tiny curtailment penalty, and a terminal value
//! for energy left in the battery. Per-block **export-disabled** and **inverter-off** gates (from
//! the spot price vs the tariff thresholds) are baked into the variable bounds. Heating stays a
//! soft (slack-penalized) comfort constraint via the affine [`ThermalContext`] prediction, so the
//! problem always returns a best-effort plan rather than going infeasible.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use good_lp::{
    constraint, microlp, variable, variables, Expression, Solution, SolverModel, Variable,
};

use super::battery::{BatterySpec, DispatchInputs};
use super::config::{EvStrategy, HeatingConfig, HvacConfig};
use super::thermal::ThermalContext;

const KELVIN_OFFSET: f64 = 273.15;
/// A tiny penalty per kWh curtailed, so the optimizer prefers banking free solar over dropping it,
/// without distorting any real price decision (well below the smallest tariff spread). ~0.01 CZK.
const CURTAIL_PENALTY: f64 = 0.0004;
/// Penalty (price-units per kWh) on grid import above the configured connection cap — far above
/// any real price, so the LP only exceeds the cap when the exogenous base load forces it (the
/// alternative was a hard-infeasible plan and a wedged planning loop).
const IMPORT_OVERLOAD_PENALTY: f64 = 1_000.0;
/// Floor on the battery-wear coefficient in the objective: the no-simultaneous-charge/discharge
/// property is enforced ECONOMICALLY by the wear term, so a configured `battery_amortisation: 0`
/// would let a negative-price block schedule charge+discharge in the same block (paid to burn
/// energy through the round-trip loss) — physically impossible for the inverter. Far below any
/// real price, it only ever breaks that tie.
const WEAR_EPSILON: f64 = 1e-4;
/// Direct-electric heating is a relay (on/off), so the near-term blocks are a binary full-power-or-
/// off decision (a 15-minute minimum on/off time by block granularity — the relay can't sub-cycle).
/// Only the near-term is made integer; distant blocks stay continuous (advisory, re-binarized as
/// they approach), bounding the integer count so the MILP stays fast.
const BINARY_HEAT_BLOCKS: usize = 8;
/// Penalty (price-units per kWh) on energy still missing at an EV charger's deadline — large enough
/// to dominate price arbitrage, so the target is met whenever physically feasible, but soft so the
/// problem never goes infeasible (an unreachable deadline just charges as much as it can).
const EV_SHORTFALL_PENALTY: f64 = 100.0;
/// A tiny bias (price-units per kWh) toward solar over grid for the `solar_preferred` strategy —
/// below any real tariff spread, so it only breaks ties the economics leave open.
const EV_SOLAR_PREFERENCE: f64 = 0.001;
/// Penalty (price-units per kWh) on a controllable load's run-time still missing within its window —
/// large enough to dominate price arbitrage so the `run_hours` target is met whenever the window
/// allows, but soft so the problem never goes infeasible (a window too short just runs as much as it
/// can). Mirrors [`EV_SHORTFALL_PENALTY`].
const LOAD_SHORTFALL_PENALTY: f64 = 100.0;

/// One controllable (deferrable) scheduled load's inputs to the LP — a boiler / hot-water tank the
/// optimizer switches on/off within its window to run for `run_hours` at the cheapest blocks. Built
/// from a `controllable` [`crate::optimize::config::ScheduledLoad`].
#[derive(Debug, Clone)]
pub struct ControllableLoadSpec {
    /// Stable identifier (the load's label, or its zone) — the schedule key and the kernel key.
    pub name: String,
    /// The zone whose air node the load's heat couples into.
    pub zone: String,
    /// Rated electrical draw when on (kW) — added to the house load and priced at the import tariff.
    pub rated_kw: f64,
    /// Signed air-node heat when on (kW): `+` for a source (warms the room), `−` for a sink (cools it).
    pub heat_kw: f64,
    /// Per-block: is the load inside one of its windows (allowed to run)? Out-of-window ⇒ forced off.
    pub window: Vec<bool>,
    /// Total run time required within the window (hours) — the soft target.
    pub run_hours: f64,
}

/// One controllable EV charger's per-block inputs to the LP. `monitored` chargers carry no decision
/// and are folded into `DispatchInputs::load_kw` upstream, so they never appear here.
#[derive(Debug, Clone)]
pub struct EvSpec {
    pub name: String,
    /// On/off only (charges at the rated power, a near-term binary) vs. continuous modulation.
    pub on_off: bool,
    pub strategy: EvStrategy,
    /// Effective maximum charge power (kW) — the rate cap.
    pub max_kw: f64,
    /// Minimum modulation power (kW) for a charging block — most chargers can't go below ~6 A
    /// (~1.4 kW). `0` = no floor. Enforced with the near-term on/off binary: a block either rests
    /// or charges in `[min_kw, max_kw]`, so the LP can't plan sub-6 A setpoints the wallbox would
    /// round to nothing. Ignored for `on_off` chargers (they only know rated-or-off anyway).
    pub min_kw: f64,
    /// AC→DC charging efficiency (0..1): energy into the car battery per kWh drawn from the house.
    pub efficiency: f64,
    /// May the home battery charge the car? (Off ⇒ `battery→EV` is bounded to 0.)
    pub allow_battery_to_ev: bool,
    /// Per-block: is the car controllable on our wallbox this block (the plug-in window)?
    pub plugged: Vec<bool>,
    /// Energy to deliver to reach the target (kWh) — the soft goal at `deadline_block`.
    pub target_energy_kwh: f64,
    /// Opportunistic headroom ABOVE the target (kWh, up to the car's own charge limit), fillable
    /// only in **bonus blocks** — curtailment-regime PV (export disabled with PV present) or
    /// negative import prices — where the energy is otherwise wasted or we are paid to take it.
    /// `0` disables the feature (SoC or car limit unknown).
    pub bonus_energy_kwh: f64,
    /// The block by which the target should be met.
    pub deadline_block: usize,
    /// Fraction (0..1] of `deadline_block` actually usable before the deadline: a `HH:MM` deadline
    /// has minute granularity, so it can land *partway* through the 15-min block that contains it.
    /// The rate cap in that final block is scaled by this, so the LP can't schedule a full block of
    /// charge to "complete" by a deadline only seconds into it. `1.0` when the deadline aligns to a
    /// block boundary, rolls past the horizon, or for `charge_now` (which uses whole blocks).
    pub deadline_frac: f64,
}

/// The optimized whole-house plan: battery dispatch plus the per-zone heating schedule.
#[derive(Debug, Clone)]
pub struct UnifiedPlan {
    pub charge_kw: Vec<f64>,
    pub discharge_kw: Vec<f64>,
    pub grid_import_kw: Vec<f64>,
    /// Battery AC-charge from the grid ONLY (no EV leg) — what `classify_mode` needs to decide
    /// `charge_from_grid`: the total `grid_import_kw` also carries EV grid charging, and a
    /// solar-charging-battery block with concurrent EV import would otherwise actuate the
    /// inverter into forced AC charge.
    pub batt_grid_charge_kw: Vec<f64>,
    /// Battery→grid export ONLY (no solar leg, no EV) — the `discharge_to_grid` signal for
    /// `classify_mode`; `discharge_kw` also carries battery→EV and `grid_export_kw` carries solar.
    pub batt_to_grid_kw: Vec<f64>,
    pub grid_export_kw: Vec<f64>,
    /// PV curtailed (kW) per block — solar neither used, stored, nor exported.
    pub curtail_kw: Vec<f64>,
    pub soc_kwh: Vec<f64>,
    /// Forecast BASE house load (kW) per block — the consumption the optimizer planned around,
    /// EXCLUDING heating and EV electricity (those are decision variables added on top). NOT the
    /// same quantity as `/api/live`'s measured `house_kw` total; charting them together needs the
    /// distinction labeled.
    pub load_kw: Vec<f64>,
    /// Underfloor-heating power (kW) per heated zone, per step.
    pub heat_kw: HashMap<String, Vec<f64>>,
    /// HVAC cooling power (kW) per HVAC zone, per step.
    pub cool_kw: HashMap<String, Vec<f64>>,
    /// HVAC air-side heating power (kW) per HVAC zone, per step.
    pub hvac_heat_kw: HashMap<String, Vec<f64>>,
    /// Predicted air temperature (°C) per controlled zone, for steps `1..=horizon`.
    pub zone_temp_c: HashMap<String, Vec<f64>>,
    /// EV charge power (kW) per controllable charger, per step (total over its source legs).
    pub ev_charge_kw: HashMap<String, Vec<f64>>,
    /// EV charge from solar / grid / battery (kW) per charger, per step — the source breakdown.
    pub ev_solar_kw: HashMap<String, Vec<f64>>,
    pub ev_grid_kw: HashMap<String, Vec<f64>>,
    pub ev_batt_kw: HashMap<String, Vec<f64>>,
    /// Per controllable load: its draw (kW) per block when on (`on · rated_kw`, so 0 when off). Keyed
    /// by the load name; empty when none are configured. The load-shift schedule the boiler controller
    /// reads.
    pub controllable_load_kw: HashMap<String, Vec<f64>>,
    /// Total electricity cost over the horizon (grid import minus export; includes heating + EV).
    pub total_cost: f64,
}

/// Battery + grid economics the single-bus [`DispatchInputs`] doesn't carry: the per-block
/// export / inverter-off gates and the battery-wear and terminal-SoC values.
#[derive(Debug, Clone)]
pub struct FlowParams {
    /// Per-block: may the inverter export to the grid? (false below the export-floor spot price.)
    pub export_allowed: Vec<bool>,
    /// Per-block: is the inverter powered on? (false in deeply-negative-price blocks.)
    pub inverter_on: Vec<bool>,
    /// Battery wear charged per kWh discharged (same price-units as the prices).
    pub amortisation: f64,
    /// Value of one kWh left in the battery at the horizon end (stops draining at the edge).
    pub terminal_value: f64,
    /// Value of one kWh of SLAB heat delivered at the final block (price-units per kWh thermal) —
    /// the thermal twin of `terminal_value`. Heat delivered near the horizon end yields most of
    /// its comfort benefit AFTER the horizon (slab lag), which a finite-horizon objective can't
    /// see: without this credit every plan under-preheats before cheap-night ends and lets zones
    /// glide to the band floor at the edge. Credited on a linear ramp over the last ~6 h (the
    /// slab time constant). `0` = off.
    pub terminal_heat_value: f64,
    /// Main-breaker / contracted import limit (kW) on `grid→load + grid→battery + grid→EV` per
    /// block; `None` ⇒ unconstrained. Without it the LP stacks every flexible draw into the single
    /// cheapest block, past what the service connection can physically deliver.
    pub max_import_kw: Option<f64>,
    /// Export limit (kW) on `solar→grid + battery→grid` per block; `None` ⇒ unconstrained.
    pub max_export_kw: Option<f64>,
}

impl FlowParams {
    /// Permissive defaults for `n` blocks: no gates, no wear, no terminal value, no grid caps (the
    /// plain economic-dispatch behaviour). Used by the tests.
    #[cfg(test)]
    pub fn permissive(n: usize) -> Self {
        Self {
            export_allowed: vec![true; n],
            inverter_on: vec![true; n],
            amortisation: 0.0,
            terminal_value: 0.0,
            terminal_heat_value: 0.0,
            max_import_kw: None,
            max_export_kw: None,
        }
    }
}

/// Solve the unified battery + heating + HVAC dispatch as an energy-flow model.
///
/// `outdoor_temp_c` is the per-block outdoor-air forecast (°C), used to evaluate each HVAC unit's
/// COP curve per block; because it is a *known* input the per-block COP is a constant, so the
/// problem stays a (mixed-integer) linear program.
///
/// `committed_heat` pins each listed zone's **block-0 relay binary** to on/off (the MPC loop's
/// within-block latch, fed INTO the optimization so the whole plan — battery, grid, timeline —
/// is consistent with the relays actually held; a post-hoc patch of just `heat_kw` left them
/// contradicting each other). The relay *binary* is pinned rather than the kW so a `max_heat_kw`
/// config edit or solver dust in the committed value can't make the tie constraint infeasible.
///
/// `relax_binaries` swaps every `.binary()` for its `[0, 1]` LP interval — the solve-timeout
/// fallback (a valid advisory plan with fractional relays beats no plan; flagged upstream).
#[allow(clippy::too_many_arguments)] // battery / heating / hvac / thermal / inputs / flow / temps / loads are distinct
pub fn optimize_unified(
    battery: &BatterySpec,
    heating: &HeatingConfig,
    hvac: &HvacConfig,
    thermal: &ThermalContext,
    inputs: &DispatchInputs,
    flow: &FlowParams,
    outdoor_temp_c: &[f64],
    ev: &[EvSpec],
    loads: &[ControllableLoadSpec],
    committed_heat: Option<&HashMap<String, f64>>,
    relax_binaries: bool,
    block_local_minutes: &[u32],
) -> Result<UnifiedPlan> {
    battery.validate()?;
    inputs.validate()?;
    hvac.validate()?;
    let n = inputs.import_price.len();
    for e in ev {
        ensure!(
            e.plugged.len() == n,
            "EV charger {:?}: plugged window length ({}) must match the horizon ({n})",
            e.name,
            e.plugged.len()
        );
    }
    for l in loads {
        ensure!(
            l.window.len() == n,
            "controllable load {:?}: window length ({}) must match the horizon ({n})",
            l.name,
            l.window.len()
        );
    }
    ensure!(
        thermal.horizon == n,
        "thermal horizon ({}) must match the price horizon ({n})",
        thermal.horizon
    );
    ensure!(heating.cop > 0.0, "heat-pump COP must be positive");
    ensure!(
        flow.export_allowed.len() == n && flow.inverter_on.len() == n,
        "flow gate vectors must match the horizon ({n})"
    );
    ensure!(
        outdoor_temp_c.len() == n,
        "outdoor_temp_c length ({}) must match the horizon ({n})",
        outdoor_temp_c.len()
    );
    ensure!(
        block_local_minutes.is_empty() || block_local_minutes.len() == n,
        "block_local_minutes length ({}) must be 0 (no schedules) or the horizon ({n}) — a short \
         vector would silently apply the midnight band everywhere",
        block_local_minutes.len()
    );
    let dt = inputs.dt_hours;
    ensure!(
        (thermal.dt - dt * 3600.0).abs() < 1e-6,
        "thermal grid step ({} s) must match the dispatch step ({dt} h)",
        thermal.dt
    );

    // Underfloor-heated zones (a `"heating"` slab marker + a comfort spec + a thermal state row).
    let heat_zones: Vec<String> = thermal
        .heated_zones
        .iter()
        .filter(|z| heating.zones.contains_key(*z) && thermal.free_response.contains_key(*z))
        .cloned()
        .collect();
    // HVAC-served zones (an air actuator + a comfort deadband + a thermal state row).
    let hvac_zones: Vec<String> = thermal
        .hvac_zones
        .iter()
        .filter(|z| hvac.comfort.contains_key(*z) && thermal.free_response.contains_key(*z))
        .cloned()
        .collect();
    // Controlled zones = heated ∪ HVAC (each gets a soft comfort band), in deterministic order. A
    // controllable load's zone is NOT here unless it is also heated/HVAC — the load is scheduled for
    // its run-hours, not to hold a comfort band of its own (a load-only zone has no band to hold).
    let mut controlled: Vec<String> = heat_zones
        .iter()
        .chain(hvac_zones.iter())
        .cloned()
        .collect();
    controlled.sort();
    controlled.dedup();
    // Zones whose air temperature we report: the controlled zones plus any controllable-load zone with
    // a thermal state row (so its load's effect on the room is visible even with no comfort actuator).
    let mut predicted: Vec<String> = controlled.clone();
    predicted.extend(
        loads
            .iter()
            .map(|l| l.zone.clone())
            .filter(|z| thermal.free_response.contains_key(z)),
    );
    predicted.sort();
    predicted.dedup();
    let is_heat = |z: &str| heat_zones.iter().any(|h| h == z);
    let is_hvac = |z: &str| hvac_zones.iter().any(|h| h == z);
    // Per-zone comfort band: heat below the lower edge, cool above the upper. A heated zone uses
    // its heating `t_min`; an HVAC zone uses its `t_cool` ceiling (and `t_heat` floor if not heated).
    // The effective band per (zone, block): heated zones follow their daily schedule windows
    // (night setback etc.) evaluated at the block's local minute; HVAC bands stay static.
    let band = |z: &str, i: usize| -> (f64, f64) {
        let minute = block_local_minutes.get(i).copied().unwrap_or(0);
        match (is_heat(z), is_hvac(z)) {
            (true, false) => heating.zones[z].band_at(minute),
            (true, true) => (heating.zones[z].band_at(minute).0, hvac.comfort[z].t_cool),
            (false, _) => (hvac.comfort[z].t_heat, hvac.comfort[z].t_cool),
        }
    };
    let penalty = |z: &str| {
        if is_hvac(z) {
            hvac.comfort_penalty
        } else {
            heating.comfort_penalty
        }
    };

    // HVAC equipment in deterministic order; each zone's serving unit (the first, for the damper
    // default) and each unit's served (controllable) zones.
    let mut unit_names: Vec<String> = hvac.units.keys().cloned().collect();
    unit_names.sort();
    let mut zone_unit: HashMap<String, String> = HashMap::new();
    for uname in &unit_names {
        for z in &hvac.units[uname].zones {
            zone_unit.entry(z.clone()).or_insert_with(|| uname.clone());
        }
    }
    // Every HVAC comfort zone must be served by some unit; the `zone_unit[z]` lookups below would
    // otherwise panic. This holds by construction (hvac_zones ⊆ the served zones), but a mismatched
    // ThermalContext + HvacConfig should fail cleanly here rather than panic.
    for z in &hvac_zones {
        anyhow::ensure!(
            zone_unit.contains_key(z),
            "HVAC zone {z:?} has a comfort band but is not served by any unit"
        );
    }
    let unit_served: Vec<(String, Vec<String>)> = unit_names
        .iter()
        .map(|uname| {
            let served: Vec<String> = hvac.units[uname]
                .zones
                .iter()
                .filter(|z| hvac_zones.contains(z))
                .cloned()
                .collect();
            (uname.clone(), served)
        })
        .filter(|(_, served)| !served.is_empty())
        .collect();

    let mut vars = variables!();
    // A non-negative variable, capped at 0 when its leg is gated off this block (export / inverter).
    let leg = |off: bool| {
        if off {
            variable().min(0.0).max(0.0)
        } else {
            variable().min(0.0)
        }
    };
    // Energy-flow split of each block's solar and the grid/battery legs (all kW). Inverter-off
    // zeroes every leg except grid→load (so the load is served from the grid and all PV curtails);
    // export-off additionally zeroes the two export legs.
    let off = |i: usize| !flow.inverter_on[i];
    let export_off = |i: usize| !flow.inverter_on[i] || !flow.export_allowed[i];
    let solar_to_load: Vec<_> = (0..n).map(|i| vars.add(leg(off(i)))).collect();
    let solar_to_batt: Vec<_> = (0..n).map(|i| vars.add(leg(off(i)))).collect();
    let solar_to_grid: Vec<_> = (0..n).map(|i| vars.add(leg(export_off(i)))).collect();
    let curtail: Vec<_> = (0..n).map(|_| vars.add(variable().min(0.0))).collect();
    let grid_to_load: Vec<_> = (0..n).map(|_| vars.add(variable().min(0.0))).collect();
    let grid_charge: Vec<_> = (0..n).map(|i| vars.add(leg(off(i)))).collect();
    let batt_to_load: Vec<_> = (0..n).map(|i| vars.add(leg(off(i)))).collect();
    let batt_to_grid: Vec<_> = (0..n).map(|i| vars.add(leg(export_off(i)))).collect();

    let binary_blocks = BINARY_HEAT_BLOCKS.min(n);

    // Every on/off decision goes through this factory: strict solves get a true binary, the
    // timeout-fallback relaxation gets its [0, 1] LP interval (see `relax_binaries`).
    let bin = || {
        if relax_binaries {
            variable().min(0.0).max(1.0)
        } else {
            variable().binary()
        }
    };
    // The block-0 relay commitment (the loop's within-block latch): on/off per the same 0.05 kW
    // threshold the publisher actuates with. Pinning the BINARY (min = max) keeps the `heat ==
    // max × relay` tie exactly satisfiable even if `max_heat_kw` changed since the commitment.
    // NOTE: need not match the publisher's configurable `on_threshold_kw` — strict-solve heat
    // values are exactly 0 or max_heat_kw (the relay tie), so any threshold in (0, min max_heat_kw)
    // classifies identically; relaxed plans (the only source of fractional values) are never
    // actuated or latched.
    const COMMIT_ON_THRESHOLD_KW: f64 = 0.05;
    let committed_on = |z: &str| {
        committed_heat
            .and_then(|m| m.get(z))
            .map(|&kw| f64::from(kw > COMMIT_ON_THRESHOLD_KW))
    };

    // Underfloor heating per heated zone (continuous, capped at the circuit power) + a near-term
    // binary relay (full power or off — a resistive relay can't sub-cycle).
    let mut heat: HashMap<String, Vec<Variable>> = HashMap::new();
    let mut heat_relay: HashMap<String, Vec<Variable>> = HashMap::new();
    for z in &heat_zones {
        let max = heating.zones[z].max_heat_kw;
        heat.insert(
            z.clone(),
            (0..n)
                .map(|_| vars.add(variable().min(0.0).max(max)))
                .collect(),
        );
        heat_relay.insert(
            z.clone(),
            (0..binary_blocks)
                .map(|b| match committed_on(z).filter(|_| b == 0) {
                    // Block 0 pinned to the committed on/off (held even under relaxation, so the
                    // fallback plan can't sub-cycle the live relays either).
                    Some(on) => vars.add(variable().min(on).max(on)),
                    None => vars.add(bin()),
                })
                .collect(),
        );
    }

    // HVAC per served zone: cooling and air-heating (continuous — inverter heat pumps modulate),
    // each bounded by the per-zone damper cap (default: the serving unit's total).
    let mut cool: HashMap<String, Vec<Variable>> = HashMap::new();
    let mut air_heat: HashMap<String, Vec<Variable>> = HashMap::new();
    for z in &hvac_zones {
        let unit = &hvac.units[&zone_unit[z]];
        // The per-zone damper caps a single room, never above the unit's shared total.
        let cool_cap = unit
            .per_zone_max_kw
            .get(z)
            .copied()
            .unwrap_or(unit.max_cool_kw)
            .min(unit.max_cool_kw);
        let heat_cap = unit
            .per_zone_max_kw
            .get(z)
            .copied()
            .unwrap_or(unit.max_heat_kw)
            .min(unit.max_heat_kw);
        cool.insert(
            z.clone(),
            (0..n)
                .map(|_| vars.add(variable().min(0.0).max(cool_cap)))
                .collect(),
        );
        air_heat.insert(
            z.clone(),
            (0..n)
                .map(|_| vars.add(variable().min(0.0).max(heat_cap)))
                .collect(),
        );
    }

    // Near-term cooling-mode binary: forces heat XOR cool per unit. Originally only for
    // single-compressor (ducted) units — a physical constraint — but applied to EVERY unit, since
    // without it a negative-price block lets the LP "burn" energy by heating and cooling the same
    // zone simultaneously (the thermal effects cancel; the electricity is paid for). Far-horizon
    // blocks stay relaxed as usual and re-binarize as they approach.
    let mut cool_mode: HashMap<String, Vec<Variable>> = HashMap::new();
    for (uname, _served) in &unit_served {
        cool_mode.insert(
            uname.clone(),
            (0..binary_blocks).map(|_| vars.add(bin())).collect(),
        );
    }

    // Soft comfort slack for every controlled zone (below the lower edge / above the upper).
    let mut slack_lo: HashMap<String, Vec<Variable>> = HashMap::new();
    let mut slack_hi: HashMap<String, Vec<Variable>> = HashMap::new();
    // One auxiliary predicted-temperature variable per (zone, block): the dense affine prediction
    // is written ONCE as an equality (t == free + Σ kernel·decision) and the two band constraints
    // use `t` — instead of cloning the O(sources × k) expression into both rows, which doubled the
    // constraint matrix's dominant nonzero family. Unbounded (it's a Kelvin temperature).
    let mut t_pred_var: HashMap<String, Vec<Variable>> = HashMap::new();
    for z in &controlled {
        slack_lo.insert(
            z.clone(),
            (0..n).map(|_| vars.add(variable().min(0.0))).collect(),
        );
        slack_hi.insert(
            z.clone(),
            (0..n).map(|_| vars.add(variable().min(0.0))).collect(),
        );
        t_pred_var.insert(z.clone(), (0..n).map(|_| vars.add(variable())).collect());
    }

    // EV chargers (controllable only; monitored ones are folded into `load_kw` upstream). Each
    // charger's charge is split across solar / grid / battery legs, gated to the plug-in window and
    // the strategy: `solar_only` zeroes the grid + battery legs; `battery→EV` also needs
    // `allow_battery_to_ev` and the inverter on. An on/off charger adds a near-term binary; the
    // soft target-by-deadline uses a `shortfall` slack.
    let ev_leg = |allowed: bool, max: f64| {
        if allowed {
            variable().min(0.0).max(max)
        } else {
            variable().min(0.0).max(0.0)
        }
    };
    // Bonus-block predicates (above-target charging): PV that would otherwise CURTAIL (export
    // disabled while the sun shines) may reach the car via the solar leg; NEGATIVE-price grid
    // energy via the grid leg. Both need the inverter... only the solar path does — grid→EV
    // bypasses the PV inverter, but a negative price with the inverter commanded off is the
    // deep-negative regime where we deliberately go dark; keep the grid bonus there anyway
    // (being paid to charge the car is exactly why). The home battery never bonus-charges the
    // car (wear for zero target value).
    // Bonus blocks only widen the PRICE/EXPORT gates — never the plug window: `plugged` is the
    // spec's own model of when the car is still on the wallbox (the deadline), and planning bonus
    // absorption after it re-creates the phantom-undeliverable-energy problem the target cap
    // fixed (curtailment "absorbed" by a car that already left would distort battery/grid plans).
    let bonus_solar_ok = |e: &EvSpec, i: usize| {
        e.bonus_energy_kwh > 0.0
            && e.plugged[i]
            && !flow.export_allowed[i]
            && inputs.pv_kw[i] > 0.05
            && flow.inverter_on[i]
    };
    let bonus_grid_ok = |e: &EvSpec, i: usize| {
        e.bonus_energy_kwh > 0.0 && e.plugged[i] && inputs.import_price[i] < 0.0
    };
    let ev_solar: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            (0..n)
                // PV reaches the car through the inverter, so — like `ev_batt` and the other solar
                // legs — it is gated on `inverter_on`: when the inverter is off (deeply-negative
                // prices) all PV curtails rather than flowing to the EV.
                //
                // `solar_only` additionally caps the leg at the block's PV SURPLUS over the house
                // base load: without it the LP diverts *gross* PV to the car and backfills the
                // house from the grid (the shortfall penalty dwarfs any import price), which is
                // net-meter-identical to grid-charging — exactly what the strategy promises not
                // to do. The base load is exogenous, so the cap is a plain variable bound.
                .map(|i| {
                    let cap = if e.strategy == EvStrategy::SolarOnly {
                        e.max_kw.min((inputs.pv_kw[i] - inputs.load_kw[i]).max(0.0))
                    } else {
                        e.max_kw
                    };
                    vars.add(ev_leg(
                        (e.plugged[i] && flow.inverter_on[i]) || bonus_solar_ok(e, i),
                        cap,
                    ))
                })
                .collect()
        })
        .collect();
    let ev_grid: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            let allow_grid = e.strategy != EvStrategy::SolarOnly;
            (0..n)
                .map(|i| {
                    vars.add(ev_leg(
                        allow_grid && (e.plugged[i] || bonus_grid_ok(e, i)),
                        e.max_kw,
                    ))
                })
                .collect()
        })
        .collect();
    let ev_batt: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            let allow_batt = e.allow_battery_to_ev && e.strategy != EvStrategy::SolarOnly;
            (0..n)
                .map(|i| {
                    vars.add(ev_leg(
                        e.plugged[i] && allow_batt && flow.inverter_on[i],
                        e.max_kw,
                    ))
                })
                .collect()
        })
        .collect();
    let ev_on: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            // On/off chargers need the binary for rated-or-off; modulating chargers with a
            // minimum-modulation floor need it for rest-or-[min, max] (both near-term only).
            if e.on_off || e.min_kw > 0.0 {
                (0..binary_blocks).map(|_| vars.add(bin())).collect()
            } else {
                Vec::new()
            }
        })
        .collect();
    let ev_shortfall: Vec<Variable> = ev.iter().map(|_| vars.add(variable().min(0.0))).collect();

    // Credited tail-heat variables for the terminal slab-heat value: the credit must NOT apply to
    // unlimited heat — tail-block slab pulses barely move in-horizon air temperature (slab lag),
    // so the comfort ceiling cannot bound them and every cheap-tailed plan would saturate all
    // heaters in the final blocks regardless of zone temperature. Crediting a separate variable
    // `credited ≤ heat` with a per-zone energy budget (~one full-power hour, what a slab absorbs
    // within a fraction of a kelvin) keeps the banked-heat incentive bounded.
    let terminal_ramp = ((6.0 / dt).round() as usize).clamp(1, n);
    let credited_heat: HashMap<String, Vec<Variable>> = if flow.terminal_heat_value > 0.0 {
        heat_zones
            .iter()
            .map(|z| {
                let max = heating.zones[z].max_heat_kw;
                (
                    z.clone(),
                    (0..terminal_ramp)
                        .map(|_| vars.add(variable().min(0.0).max(max)))
                        .collect(),
                )
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Per-block soft-overload slack for the grid-import cap (empty when no cap is configured);
    // penalized far above any price in the objective — see the constraint site below.
    let import_overload: Vec<Variable> = if flow.max_import_kw.is_some() {
        (0..n).map(|_| vars.add(variable().min(0.0))).collect()
    } else {
        Vec::new()
    };

    // Controllable loads: a per-block on/off relay (a true binary in-window, forced to 0 out-of-window)
    // — the boiler runs at its rated power or not at all. Binary across the whole horizon (not just the
    // near-term) so the cheapest-blocks load-shift is an integral schedule rather than a fractional one;
    // there are few such loads, so the extra binaries stay cheap. A soft `run_hours` target uses a
    // `shortfall` slack, mirroring the EV deadline.
    let load_on: Vec<Vec<Variable>> = loads
        .iter()
        .map(|l| {
            (0..n)
                .map(|i| {
                    if l.window[i] {
                        vars.add(bin())
                    } else {
                        vars.add(variable().min(0.0).max(0.0)) // out-of-window ⇒ forced off
                    }
                })
                .collect()
        })
        .collect();
    let load_shortfall: Vec<Variable> = loads
        .iter()
        .map(|_| vars.add(variable().min(0.0)))
        .collect();

    let ev_solar_sum =
        |i: usize| -> Expression { ev_solar.iter().map(|c| Expression::from(c[i])).sum() };
    let ev_grid_sum =
        |i: usize| -> Expression { ev_grid.iter().map(|c| Expression::from(c[i])).sum() };
    let ev_batt_sum =
        |i: usize| -> Expression { ev_batt.iter().map(|c| Expression::from(c[i])).sum() };

    // Running state of charge after each block (charging stores net of losses; discharging — to the
    // house, the grid, or the car — draws extra to cover them), as affine expressions reused for the
    // bounds, terminal value and report.
    let mut soc = Expression::from(battery.initial_soc_kwh);
    let mut soc_after = Vec::with_capacity(n);
    for i in 0..n {
        soc += (battery.charge_efficiency * (grid_charge[i] + solar_to_batt[i])
            - (batt_to_load[i] + batt_to_grid[i] + ev_batt_sum(i)) / battery.discharge_efficiency)
            * dt;
        soc_after.push(soc.clone());
    }

    // Reported electricity cost: grid import paid at the import price, export credited at the export
    // price (both already include the tariff). Import = to-load + charge; export = solar + battery.
    let grid_cash: Expression = (0..n)
        .map(|i| {
            (inputs.import_price[i] * (grid_to_load[i] + grid_charge[i] + ev_grid_sum(i))
                - inputs.export_price[i] * (solar_to_grid[i] + batt_to_grid[i]))
                * dt
        })
        .sum();

    // Full objective: grid cash + battery wear (on discharge) + a tiny curtailment penalty + comfort
    // slack penalty − the value of the energy left in the battery at the horizon end.
    let mut objective = grid_cash.clone();
    for i in 0..n {
        objective += flow.amortisation.max(WEAR_EPSILON)
            * (batt_to_load[i] + batt_to_grid[i] + ev_batt_sum(i))
            * dt;
        objective += CURTAIL_PENALTY * curtail[i] * dt;
        if let Some(&overload) = import_overload.get(i) {
            objective += IMPORT_OVERLOAD_PENALTY * overload * dt;
        }
    }
    // EV: a large penalty on energy still missing at each charger's deadline (soft target), plus a
    // tiny solar-over-grid bias for the `solar_preferred` strategy.
    for (c, e) in ev.iter().enumerate() {
        objective += EV_SHORTFALL_PENALTY * ev_shortfall[c];
        if e.strategy == EvStrategy::SolarPreferred {
            for g in &ev_grid[c] {
                // `* dt`: the constant is per-kWh, so bias the grid *energy* (like every other term).
                objective += EV_SOLAR_PREFERENCE * *g * dt;
            }
        }
    }
    // Controllable loads: a large penalty on run-time still missing within the window (soft target).
    for &slack in &load_shortfall {
        objective += LOAD_SHORTFALL_PENALTY * slack;
    }
    for z in &controlled {
        let pen = penalty(z);
        for k in 0..n {
            objective += pen * (slack_lo[z][k] + slack_hi[z][k]);
        }
    }
    if let Some(final_soc) = soc_after.last() {
        objective -= flow.terminal_value * battery.discharge_efficiency * final_soc.clone();
    }
    // Terminal SLAB-heat credit (see FlowParams::terminal_heat_value): heat in block i of the
    // final ramp keeps `(1 - lag/ramp)` of its post-horizon value — a linear proxy for how much
    // of a slab pulse's comfort benefit falls outside the horizon. The comfort ceiling still
    // bounds it (a credit can't push zones past t_max profitably: the slack penalty dwarfs it).
    if flow.terminal_heat_value > 0.0 {
        for (z, credited) in &credited_heat {
            let _ = z;
            for (k, &c) in credited.iter().enumerate() {
                // k = 0 is the earliest tail block (n - ramp), k = ramp-1 the final block.
                let frac = (k + 1) as f64 / terminal_ramp as f64;
                objective -= flow.terminal_heat_value * frac * c * dt;
            }
        }
    }

    let mut problem = vars.minimise(objective).using(microlp);

    // Per-block energy balances, battery power caps and SoC bounds (the gates are in the bounds).
    for i in 0..n {
        // Flexible electrical load this block: underfloor heating (Σ heat / COP) plus each HVAC
        // unit's cooling and air-heating at its per-block COP (from the outdoor-temp forecast).
        let mut flexible_elec: Expression = heat_zones
            .iter()
            .map(|z| Expression::from(heat[z][i]))
            .sum::<Expression>()
            * (1.0 / heating.cop);
        for (uname, served) in &unit_served {
            let unit = &hvac.units[uname];
            let cool_cop = unit.cooling_cop.cop_at(outdoor_temp_c[i]);
            let heat_cop = unit.heating_cop.cop_at(outdoor_temp_c[i]);
            let cool_sum: Expression = served.iter().map(|z| Expression::from(cool[z][i])).sum();
            let heat_sum: Expression = served
                .iter()
                .map(|z| Expression::from(air_heat[z][i]))
                .sum();
            flexible_elec += cool_sum * (1.0 / cool_cop) + heat_sum * (1.0 / heat_cop);
        }
        // Each controllable load draws its rated power when on — a flexible electrical load met from
        // solar/battery/grid like the heat-pump electricity, so it is priced at the import tariff and
        // the optimizer shifts it to the cheapest in-window blocks.
        for (c, l) in loads.iter().enumerate() {
            flexible_elec += l.rated_kw * load_on[c][i];
        }
        // Solar is split across house, battery, grid, the EV legs and curtailment.
        problem = problem.with(constraint!(
            solar_to_load[i] + solar_to_batt[i] + solar_to_grid[i] + ev_solar_sum(i) + curtail[i]
                == inputs.pv_kw[i]
        ));
        // The house load (incl. heating + HVAC electricity) is met by solar, battery and grid.
        problem = problem.with(constraint!(
            solar_to_load[i] + batt_to_load[i] + grid_to_load[i]
                == inputs.load_kw[i] + flexible_elec
        ));
        // Battery charge / discharge power caps (a positive wear cost keeps the two from coexisting).
        problem = problem.with(constraint!(
            grid_charge[i] + solar_to_batt[i] <= battery.max_charge_kw
        ));
        problem = problem.with(constraint!(
            batt_to_load[i] + batt_to_grid[i] + ev_batt_sum(i) <= battery.max_discharge_kw
        ));
        // Grid-connection limits (main breaker / contracted power): total import and export per
        // block. Without the import cap the LP stacks base load + battery grid-charge + EV into
        // the single cheapest block far past what the service can physically deliver.
        // The import cap is SOFT (heavily-penalized overload slack, created with the variables
        // above): the base load is exogenous, so in an inverter-off / battery-empty block
        // `grid_to_load == load_kw` is forced — a measured spike above a hard cap would make the
        // whole LP infeasible and wedge the planning loop on exactly the kind of morning it
        // matters. The penalty (≫ any price) keeps the overload at zero whenever the flexible
        // loads can yield instead.
        if let Some(cap) = flow.max_import_kw {
            problem = problem.with(constraint!(
                grid_to_load[i] + grid_charge[i] + ev_grid_sum(i) <= cap + import_overload[i]
            ));
        }
        if let Some(cap) = flow.max_export_kw {
            problem = problem.with(constraint!(solar_to_grid[i] + batt_to_grid[i] <= cap));
        }
        problem = problem.with(constraint!(soc_after[i].clone() >= battery.min_soc_kwh));
        problem = problem.with(constraint!(soc_after[i].clone() <= battery.max_soc_kwh));
    }
    if let Some(target) = inputs.min_final_soc_kwh {
        if let Some(final_soc) = soc_after.last() {
            problem = problem.with(constraint!(final_soc.clone() >= target));
        }
    }

    // Terminal-credit coupling: credited tail heat is real heat, and each zone's credited energy
    // is capped at ~one full-power hour (the slab bank the credit is allowed to value).
    if flow.terminal_heat_value > 0.0 {
        for (z, credited) in &credited_heat {
            for (k, &c) in credited.iter().enumerate() {
                let i = n - terminal_ramp + k;
                problem = problem.with(constraint!(c <= heat[z][i]));
            }
            let banked: Expression = credited.iter().map(|&c| Expression::from(c) * dt).sum();
            problem = problem.with(constraint!(banked <= heating.zones[z].max_heat_kw * 1.0));
        }
    }

    // Relay heating: tie the near-term blocks to a binary on/off (0 or full power per zone), so the
    // recommended heating switches in 15-minute units and the relay isn't sub-cycled.
    for z in &heat_zones {
        let max = heating.zones[z].max_heat_kw;
        for b in 0..binary_blocks {
            problem = problem.with(constraint!(heat[z][b] == max * heat_relay[z][b]));
        }
    }

    // HVAC units: shared cooling/air-heating capacity across the served zones, and (single-compressor
    // ducted units) a near-term mode gate so the unit can't heat and cool in the same block.
    for (uname, served) in &unit_served {
        let unit = &hvac.units[uname];
        for i in 0..n {
            let cool_sum: Expression = served.iter().map(|z| Expression::from(cool[z][i])).sum();
            let heat_sum: Expression = served
                .iter()
                .map(|z| Expression::from(air_heat[z][i]))
                .sum();
            problem = problem.with(constraint!(cool_sum.clone() <= unit.max_cool_kw));
            problem = problem.with(constraint!(heat_sum.clone() <= unit.max_heat_kw));
            if let Some(mode) = cool_mode.get(uname).filter(|_| i < binary_blocks) {
                // mode = 1 ⇒ cooling allowed (heating forced to 0); mode = 0 ⇒ the reverse.
                problem = problem.with(constraint!(cool_sum <= unit.max_cool_kw * mode[i]));
                problem = problem.with(constraint!(
                    heat_sum + unit.max_heat_kw * mode[i] <= unit.max_heat_kw
                ));
            }
        }
    }

    // Soft comfort: the affine-predicted temperature stays within the zone's [lower, upper] band,
    // slack-penalized. Underfloor heating and HVAC air-heating raise it; HVAC cooling lowers it.
    //
    // Sparsification, LP-side only (ThermalContext::predict stays exact for reporting/tests):
    // a term whose |kernel| × the source's max power moves the prediction under TERM_SKIP_K is
    // physically negligible — dominated by long-lag and weak cross-zone entries, which otherwise
    // make this the LP's dominant nonzero family (O(zones × sources × horizon²) terms). The
    // worst-case omission over a 96-block horizon is bounded by 96 × TERM_SKIP_K ≈ 0.01 K, far
    // inside the comfort band and the model's own accuracy.
    const TERM_SKIP_K: f64 = 1e-4;
    for z in &controlled {
        let free = &thermal.free_response[z];
        for k in 1..=n {
            let (lo, hi) = band(z, k - 1);
            let (lo_k, hi_k) = (lo + KELVIN_OFFSET, hi + KELVIN_OFFSET);
            let mut t_pred = Expression::from(free[k - 1]);
            for source in &heat_zones {
                if let Some(kernel) = thermal.kernels.get(&(z.clone(), source.clone())) {
                    let max_kw = heating.zones[source].max_heat_kw;
                    for j in 0..k {
                        if kernel[k - j - 1].abs() * max_kw < TERM_SKIP_K {
                            continue;
                        }
                        t_pred += kernel[k - j - 1] * heat[source][j];
                    }
                }
            }
            for source in &hvac_zones {
                if let Some(kernel) = thermal.air_kernels.get(&(z.clone(), source.clone())) {
                    let unit = &hvac.units[&zone_unit[source]];
                    let max_kw = unit.max_cool_kw.max(unit.max_heat_kw);
                    for j in 0..k {
                        if kernel[k - j - 1].abs() * max_kw < TERM_SKIP_K {
                            continue;
                        }
                        t_pred += kernel[k - j - 1] * (air_heat[source][j] - cool[source][j]);
                    }
                }
            }
            // Each controllable load: its signed per-kW heat, through the air-node kernel keyed by the
            // load name, applied in the blocks it runs (`on=1`). This is the heat-when-on coupling.
            for (c, l) in loads.iter().enumerate() {
                if let Some(kernel) = thermal.load_kernels.get(&(z.clone(), l.name.clone())) {
                    for j in 0..k {
                        if (kernel[k - j - 1] * l.heat_kw).abs() < TERM_SKIP_K {
                            continue;
                        }
                        t_pred += kernel[k - j - 1] * l.heat_kw * load_on[c][j];
                    }
                }
            }
            // One equality carries the dense expression; the band rows are 2 nonzeros each.
            let t = t_pred_var[z][k - 1];
            problem = problem.with(constraint!(t_pred == t));
            problem = problem.with(constraint!(t + slack_lo[z][k - 1] >= lo_k));
            problem = problem.with(constraint!(t - slack_hi[z][k - 1] <= hi_k));
        }
    }

    // EV chargers: the per-block rate cap (total charge over the source legs ≤ max, or = rated × the
    // near-term on/off binary), and a soft target-by-deadline (delivered energy + shortfall ≥ target).
    for (c, e) in ev.iter().enumerate() {
        let deadline = e.deadline_block.min(n.saturating_sub(1));
        for i in 0..n {
            let total: Expression = ev_solar[c][i] + ev_grid[c][i] + ev_batt[c][i];
            // The deadline block is only usable for `deadline_frac` of its duration (a mid-block
            // `HH:MM` deadline), so cap its average power proportionally — otherwise the LP could
            // "deliver" a full block of energy by a deadline only seconds into the block. This applies
            // equally to the on/off binary (the relay runs only the usable fraction of the block).
            let cap = if i == deadline {
                e.max_kw * e.deadline_frac
            } else {
                e.max_kw
            };
            // On/off is enforced as a true binary (0 or rated) only in the near-term `binary_blocks`
            // window — the part that actually gets actuated, since the loop re-plans every tick and
            // applies just the first block. Beyond that window it is relaxed to the continuous cap to
            // keep the MILP small (an LP-relaxed look-ahead), the same near-term-binary treatment as
            // the heating/HVAC single-mode gates. A far-horizon block always re-solves as binary before
            // it becomes "now".
            if e.on_off && i < binary_blocks {
                problem = problem.with(constraint!(total == cap * ev_on[c][i]));
            } else if e.min_kw > 0.0 && i < binary_blocks {
                // Minimum-modulation floor (~6 A): a near-term block either rests or charges in
                // [min_kw, cap] — the wallbox can't hold a 0.3 kW setpoint, so a sub-minimum plan
                // would silently round to nothing at the hardware. Far blocks stay LP-relaxed
                // (they re-solve as binary before they're actuated).
                let floor = e.min_kw.min(cap);
                problem = problem.with(constraint!(total.clone() <= cap * ev_on[c][i]));
                problem = problem.with(constraint!(total >= floor * ev_on[c][i]));
            } else {
                problem = problem.with(constraint!(total <= cap));
            }
        }
        // The deadline block needs no extra `deadline_frac` scaling here: `total` is the block-*average*
        // power, already capped above to `max_kw * deadline_frac`, so `total * dt` is exactly the energy
        // deliverable in the partial window (max_kw running for `deadline_frac * dt`). Scaling it again
        // would double-count (frac²) and under-credit the charge — and break energy-balance consistency,
        // since the source legs use `total` unscaled.
        let delivered: Expression = (0..=deadline)
            .map(|i| (ev_solar[c][i] + ev_grid[c][i] + ev_batt[c][i]) * (e.efficiency * dt))
            .sum();
        problem = problem.with(constraint!(
            delivered.clone() + ev_shortfall[c] >= e.target_energy_kwh
        ));
        // …and bounded from above: the target is already capped at the car's own charge limit, so
        // energy past it is physically refused — an "over-delivering" plan (free surplus-PV or
        // negative-price blocks) would just diverge from what the car accepts. The one-block
        // allowance keeps the final partial block feasible where the on/off equality (or the
        // min-modulation floor) can't express a fractional-block charge.
        let allowance = if e.on_off {
            e.max_kw * e.efficiency * dt
        } else if e.min_kw > 0.0 {
            e.min_kw * e.efficiency * dt
        } else {
            0.0
        };
        // Whole-horizon sum, bounded by target + the BONUS headroom (car's own limit): bonus
        // blocks may fill past the target with otherwise-wasted energy.
        let delivered_all: Expression = (0..n)
            .map(|i| (ev_solar[c][i] + ev_grid[c][i] + ev_batt[c][i]) * (e.efficiency * dt))
            .sum();
        problem = problem.with(constraint!(
            delivered_all <= e.target_energy_kwh + e.bonus_energy_kwh + allowance
        ));
        // NON-bonus energy alone still can't exceed the target: above-target charging must come
        // from curtailment-regime PV or negative-price blocks only, never plain paid grid/solar.
        if e.bonus_energy_kwh > 0.0 {
            // Per-LEG exemption: only the leg a bonus block legitimises escapes the target cap —
            // the battery leg never does (a block-level exemption let battery→EV dump above
            // target profit from freeing curtailment headroom at epsilon wear).
            let delivered_normal: Expression = (0..n)
                .map(|i| {
                    let mut leg: Expression = Expression::from(ev_batt[c][i]);
                    if !bonus_grid_ok(e, i) {
                        leg += ev_grid[c][i];
                    }
                    if !bonus_solar_ok(e, i) {
                        leg += ev_solar[c][i];
                    }
                    leg * (e.efficiency * dt)
                })
                .sum();
            problem = problem.with(constraint!(
                delivered_normal <= e.target_energy_kwh + allowance
            ));
        }
    }

    // Two or more `solar_only` chargers: each one's variable bound allows the full block surplus,
    // so jointly they could draw 2× it — one shared row per block keeps the strategy's promise.
    let solar_only: Vec<usize> = ev
        .iter()
        .enumerate()
        .filter(|(_, e)| e.strategy == EvStrategy::SolarOnly)
        .map(|(c, _)| c)
        .collect();
    if solar_only.len() > 1 {
        for (i, (&pv, &base)) in inputs.pv_kw.iter().zip(&inputs.load_kw).enumerate() {
            let joint: Expression = solar_only
                .iter()
                .map(|&c| Expression::from(ev_solar[c][i]))
                .sum();
            problem = problem.with(constraint!(joint <= (pv - base).max(0.0)));
        }
    }

    // Controllable loads: the soft run-hours target — total on-time (Σ on·dt) plus a shortfall slack
    // must reach `run_hours`. Out-of-window blocks are forced off, so the load can only accumulate
    // run-time inside its window; if the window is too short the shortfall absorbs the gap.
    for (c, l) in loads.iter().enumerate() {
        let run: Expression = (0..n).map(|i| Expression::from(load_on[c][i]) * dt).sum();
        problem = problem.with(constraint!(run.clone() + load_shortfall[c] >= l.run_hours));
        // …and bounded above (rounded up to whole blocks): `run_hours` is the NEEDED run time, and
        // without a ceiling the LP happily runs the load extra hours in free-surplus/negative
        // blocks — energy the appliance doesn't need and the plan then mispredicts.
        let cap_hours = (l.run_hours / dt).ceil() * dt;
        problem = problem.with(constraint!(run <= cap_hours));
    }

    let solution = problem.solve()?;

    let values =
        |vs: &[Variable]| -> Vec<f64> { vs.iter().map(|v| solution.value(*v).max(0.0)).collect() };
    // Aggregate the split legs back into the reported charge / discharge / grid flows.
    let agg = |a: &[Variable], b: &[Variable]| -> Vec<f64> {
        (0..n)
            .map(|i| (solution.value(a[i]) + solution.value(b[i])).max(0.0))
            .collect()
    };
    let heat_kw: HashMap<String, Vec<f64>> = heat_zones
        .iter()
        .map(|z| (z.clone(), values(&heat[z])))
        .collect();
    let cool_kw: HashMap<String, Vec<f64>> = hvac_zones
        .iter()
        .map(|z| (z.clone(), values(&cool[z])))
        .collect();
    let hvac_heat_kw: HashMap<String, Vec<f64>> = hvac_zones
        .iter()
        .map(|z| (z.clone(), values(&air_heat[z])))
        .collect();
    // Net signed air power per HVAC zone (air-heating − cooling) for the temperature prediction.
    let air_net: HashMap<String, Vec<f64>> = hvac_zones
        .iter()
        .map(|z| {
            (
                z.clone(),
                (0..n).map(|i| hvac_heat_kw[z][i] - cool_kw[z][i]).collect(),
            )
        })
        .collect();
    // Controllable loads: the per-block draw (`on · rated_kw`) reported in the plan, and the signed
    // heat schedule (`on · heat_kw`, keyed by name) fed to `predict` for the temperature it produced.
    let on_value = |c: usize| -> Vec<f64> {
        (0..n)
            .map(|i| solution.value(load_on[c][i]).clamp(0.0, 1.0))
            .collect()
    };
    let controllable_load_kw: HashMap<String, Vec<f64>> = loads
        .iter()
        .enumerate()
        .map(|(c, l)| {
            let on = on_value(c);
            (l.name.clone(), on.iter().map(|&o| o * l.rated_kw).collect())
        })
        .collect();
    let load_heat_net: HashMap<String, Vec<f64>> = loads
        .iter()
        .enumerate()
        .map(|(c, l)| {
            let on = on_value(c);
            (l.name.clone(), on.iter().map(|&o| o * l.heat_kw).collect())
        })
        .collect();
    let zone_temp_c: HashMap<String, Vec<f64>> = predicted
        .iter()
        .map(|z| {
            let temps = (1..=n)
                .map(|k| thermal.predict(z, k, &heat_kw, &air_net, &load_heat_net) - KELVIN_OFFSET)
                .collect();
            (z.clone(), temps)
        })
        .collect();

    // EV per-charger flows and the solar / grid / battery source breakdown.
    let ev_charge_kw: HashMap<String, Vec<f64>> = ev
        .iter()
        .enumerate()
        .map(|(c, e)| {
            let v = (0..n)
                .map(|i| {
                    (solution.value(ev_solar[c][i])
                        + solution.value(ev_grid[c][i])
                        + solution.value(ev_batt[c][i]))
                    .max(0.0)
                })
                .collect();
            (e.name.clone(), v)
        })
        .collect();
    let ev_legs = |legs: &[Vec<Variable>]| -> HashMap<String, Vec<f64>> {
        ev.iter()
            .enumerate()
            .map(|(c, e)| (e.name.clone(), values(&legs[c])))
            .collect()
    };

    Ok(UnifiedPlan {
        charge_kw: agg(&grid_charge, &solar_to_batt),
        // Discharge includes the battery→EV leg — the SoC recursion, discharge cap and wear term
        // all do, so omitting it here would report SoC dropping with zero discharge, understate
        // the wear cost, and let classify_mode label a discharging block `battery_hold` for the
        // armed Growatt controller.
        discharge_kw: (0..n)
            .map(|i| {
                (solution.value(batt_to_load[i])
                    + solution.value(batt_to_grid[i])
                    + ev_batt
                        .iter()
                        .map(|leg| solution.value(leg[i]))
                        .sum::<f64>())
                .max(0.0)
            })
            .collect(),
        // Grid import includes EV grid charging (as the cost term does), so the reported metric and
        // classify_mode see the true import.
        grid_import_kw: (0..n)
            .map(|i| {
                (solution.value(grid_to_load[i])
                    + solution.value(grid_charge[i])
                    + ev_grid
                        .iter()
                        .map(|leg| solution.value(leg[i]))
                        .sum::<f64>())
                .max(0.0)
            })
            .collect(),
        grid_export_kw: agg(&solar_to_grid, &batt_to_grid),
        batt_grid_charge_kw: values(&grid_charge),
        batt_to_grid_kw: values(&batt_to_grid),
        curtail_kw: values(&curtail),
        soc_kwh: soc_after.iter().map(|e| e.eval_with(&solution)).collect(),
        load_kw: inputs.load_kw.clone(),
        heat_kw,
        cool_kw,
        hvac_heat_kw,
        zone_temp_c,
        ev_charge_kw,
        ev_solar_kw: ev_legs(&ev_solar),
        ev_grid_kw: ev_legs(&ev_grid),
        ev_batt_kw: ev_legs(&ev_batt),
        controllable_load_kw,
        total_cost: grid_cash.eval_with(&solution),
    })
}

#[cfg(test)]
mod tests {
    use super::super::config::{CopPoint, CopSpec, HvacComfort, HvacConfig, HvacUnit, ZoneComfort};
    use super::super::thermal::build_context;
    use super::*;
    use crate::model::Model;
    use crate::rc_network::RcNetwork;
    use crate::state_space::StateSpace;
    use nalgebra::DVector;
    use uom::si::{
        f64::ThermodynamicTemperature,
        thermodynamic_temperature::{degree_celsius, kelvin},
    };

    /// One realistic insulated zone with an underfloor-heating slab. The exterior wall is
    /// insulated, so a moderate heat input holds the comfort band — leaving the optimizer slack
    /// to shift heating in time. The slab gives the multi-hour storage that makes pre-heating pay.
    fn thermal_for(outside_c: f64, ground_c: f64, x0_c: f64, n: usize) -> ThermalContext {
        thermal_for_inner(outside_c, ground_c, x0_c, n, &[])
    }

    /// As [`thermal_for`] but with zone `"a"` also served by an HVAC air-node actuator.
    fn thermal_for_hvac(outside_c: f64, ground_c: f64, x0_c: f64, n: usize) -> ThermalContext {
        thermal_for_inner(outside_c, ground_c, x0_c, n, &["a".to_string()])
    }

    fn thermal_for_inner(
        outside_c: f64,
        ground_c: f64,
        x0_c: f64,
        n: usize,
        hvac_zones: &[String],
    ) -> ThermalContext {
        let model = Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [
                        { material: "concrete", thickness: 0.05 },
                        { marker: "heating" },
                        { material: "concrete", thickness: 0.05 },
                    ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { a: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["a", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["a", "outside"], area: 25 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let dt = 3600.0;
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(outside_c),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(ground_c),
        );
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(x0_c).get::<kelvin>(),
        );
        build_context(&ss, &net, &x0, &vec![u0; n], dt, hvac_zones, &[], None).unwrap()
    }

    fn no_battery() -> BatterySpec {
        BatterySpec {
            max_charge_kw: 0.0,
            max_discharge_kw: 0.0,
            charge_efficiency: 1.0,
            discharge_efficiency: 1.0,
            min_soc_kwh: 0.0,
            max_soc_kwh: 0.0,
            initial_soc_kwh: 0.0,
        }
    }

    fn battery(capacity: f64, power: f64, initial: f64) -> BatterySpec {
        BatterySpec {
            max_charge_kw: power,
            max_discharge_kw: power,
            charge_efficiency: 1.0,
            discharge_efficiency: 1.0,
            min_soc_kwh: 0.0,
            max_soc_kwh: capacity,
            initial_soc_kwh: initial,
        }
    }

    fn heating_cfg(max_heat_kw: f64, t_min: f64, t_max: f64) -> HeatingConfig {
        HeatingConfig {
            cop: 3.0,
            comfort_penalty: 100.0,
            zones: HashMap::from([(
                "a".to_string(),
                ZoneComfort {
                    max_heat_kw,
                    t_min,
                    t_max,
                    internal_gain_w: 0.0,
                    windows: Vec::new(),
                },
            )]),
        }
    }

    /// No heated zones (battery/PV only), so the thermal side is inert.
    fn no_heating() -> HeatingConfig {
        HeatingConfig {
            cop: 3.0,
            comfort_penalty: 100.0,
            zones: HashMap::new(),
        }
    }

    fn flat_inputs(price: f64, n: usize) -> DispatchInputs {
        DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![price; n],
            export_price: vec![0.0; n],
            pv_kw: vec![0.0; n],
            load_kw: vec![0.0; n],
            min_final_soc_kwh: None,
        }
    }

    fn solve(
        battery: &BatterySpec,
        heating: &HeatingConfig,
        thermal: &ThermalContext,
        inputs: &DispatchInputs,
    ) -> UnifiedPlan {
        let n = inputs.import_price.len();
        optimize_unified(
            battery,
            heating,
            &HvacConfig::default(),
            thermal,
            inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap()
    }

    /// A single reversible HVAC unit serving zone `"a"` (constant COPs), with a `[t_heat, t_cool]`
    /// deadband — the analogue of [`heating_cfg`] for the air-side actuator.
    fn hvac_cfg(max_cool_kw: f64, max_heat_kw: f64, t_heat: f64, t_cool: f64) -> HvacConfig {
        HvacConfig {
            comfort_penalty: 100.0,
            comfort: HashMap::from([("a".to_string(), HvacComfort { t_heat, t_cool })]),
            units: HashMap::from([(
                "ac".to_string(),
                HvacUnit {
                    zones: vec!["a".to_string()],
                    max_cool_kw,
                    max_heat_kw,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.5),
                },
            )]),
        }
    }

    #[test]
    fn warm_zone_needs_no_heating() {
        let n = 6;
        let thermal = thermal_for(22.0, 21.0, 22.0, n); // stays inside [20, 24]
        let plan = solve(
            &no_battery(),
            &heating_cfg(5.0, 20.0, 24.0),
            &thermal,
            &flat_inputs(0.2, n),
        );
        let heat = &plan.heat_kw["a"];
        assert!(
            heat.iter().all(|&h| h < 1e-6),
            "no heating needed: {heat:?}"
        );
        assert!(plan.total_cost.abs() < 1e-6);
    }

    #[test]
    fn comfort_floor_is_held_when_feasible() {
        let n = 12;
        // Mild winter: the free response drifts below the 20 °C floor, so the optimizer heats.
        let thermal = thermal_for(0.0, 12.0, 20.0, n);
        let plan = solve(
            &no_battery(),
            &heating_cfg(10.0, 20.0, 24.0),
            &thermal,
            &flat_inputs(0.5, n),
        );
        assert!(
            plan.heat_kw["a"].iter().sum::<f64>() > 0.0,
            "expected heating"
        );
        let coldest = plan.zone_temp_c["a"]
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        assert!(
            coldest > 20.0 - 0.3,
            "comfort floor should be held, coldest = {coldest}"
        );
    }

    #[test]
    fn heating_electricity_enters_power_balance() {
        let n = 4;
        let thermal = thermal_for(0.0, 12.0, 20.0, n);
        let cfg = heating_cfg(10.0, 21.0, 24.0); // floor above the drift forces heating
        let plan = solve(&no_battery(), &cfg, &thermal, &flat_inputs(0.2, n));
        assert!(
            plan.heat_kw["a"].iter().sum::<f64>() > 0.0,
            "expected heating"
        );
        // No PV, no load, no battery → grid import must equal the heating electricity (Σ heat / COP).
        for t in 0..n {
            assert!((plan.grid_import_kw[t] - plan.heat_kw["a"][t] / cfg.cop).abs() < 1e-6);
        }
    }

    #[test]
    fn heating_shifts_to_cheaper_hours() {
        let n = 12;
        let thermal = thermal_for(0.0, 12.0, 20.0, n);
        let cfg = heating_cfg(10.0, 20.0, 24.0);

        // Flat prices: heat just-in-time to hold the floor.
        let flat = solve(&no_battery(), &cfg, &thermal, &flat_inputs(0.5, n));
        // Cheap first half, expensive second half: pre-heat the slab in the cheap window, then coast.
        let mut cheap_early = flat_inputs(0.5, n);
        cheap_early.import_price = (0..n).map(|t| if t < n / 2 { 0.1 } else { 0.9 }).collect();
        let shifted = solve(&no_battery(), &cfg, &thermal, &cheap_early);

        let early = |p: &UnifiedPlan| p.heat_kw["a"][0..n / 2].iter().sum::<f64>();
        let late = |p: &UnifiedPlan| p.heat_kw["a"][n / 2..].iter().sum::<f64>();
        assert!(
            early(&shifted) > early(&flat) + 1.0,
            "cheap-early prices should pull heating into the cheap window ({} vs {})",
            early(&shifted),
            early(&flat)
        );
        assert!(
            late(&shifted) < late(&flat),
            "the expensive window should be served by stored slab heat, not fresh heating"
        );
        // Comfort is still respected while shifting.
        let coldest = shifted.zone_temp_c["a"]
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        assert!(
            coldest > 20.0 - 0.3,
            "comfort floor held while shifting, coldest = {coldest}"
        );
    }

    #[test]
    fn infeasible_comfort_returns_best_effort() {
        let n = 6;
        let thermal = thermal_for(-25.0, -10.0, 18.0, n); // brutally cold, starting below band
                                                          // A tiny heater can't hold the band, but the soft formulation must still return a plan.
        let plan = solve(
            &no_battery(),
            &heating_cfg(0.1, 21.0, 24.0),
            &thermal,
            &flat_inputs(0.2, n),
        );
        let coldest = plan.zone_temp_c["a"]
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        assert!(
            coldest < 21.0,
            "comfort cannot be met here, so the band is violated (best effort)"
        );
    }

    /// Surplus PV is split (used / stored / exported / curtailed) and the split conserves energy.
    #[test]
    fn solar_split_conserves_energy() {
        let n = 3;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert (no heated zones)
        let mut inputs = flat_inputs(0.20, n);
        inputs.export_price = vec![0.10; n]; // worthwhile to export
        inputs.pv_kw = vec![5.0; n]; // surplus
        inputs.load_kw = vec![1.0; n];
        // Any positive wear stops the lossless degenerate optimum that would route solar through
        // the battery to grid (cost-neutral at η=1), so charge/discharge reflect real flows.
        let mut flow = FlowParams::permissive(n);
        flow.amortisation = 0.02;
        let plan = optimize_unified(
            &battery(10.0, 3.0, 0.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        for t in 0..n {
            // Surplus solar (no import needed) → house(1) + stored + exported + curtailed == PV(5).
            assert!(
                plan.grid_import_kw[t] < 1e-4,
                "no import with surplus solar at t={t}"
            );
            let split = 1.0 + plan.charge_kw[t] + plan.grid_export_kw[t] + plan.curtail_kw[t];
            assert!((split - 5.0).abs() < 1e-3, "solar split at t={t}: {split}");
        }
        // With a positive export price and surplus, it should export and/or store, not curtail all.
        assert!(plan.grid_export_kw.iter().sum::<f64>() + plan.charge_kw.iter().sum::<f64>() > 0.0);
    }

    fn ev_spec(strategy: EvStrategy, n: usize) -> EvSpec {
        EvSpec {
            name: "garage".to_string(),
            on_off: false,
            strategy,
            max_kw: 11.0,
            min_kw: 0.0,
            efficiency: 1.0,
            allow_battery_to_ev: false,
            plugged: vec![true; n],
            target_energy_kwh: 5.0,
            bonus_energy_kwh: 0.0,
            deadline_block: n - 1,
            deadline_frac: 1.0,
        }
    }

    /// A controllable EV charger meets its energy target by the deadline, in the cheapest block, from
    /// the grid — and the home battery is *not* tapped for the car by default.
    #[test]
    fn ev_meets_target_from_grid_in_cheap_block() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.20, n);
        inputs.import_price = vec![0.30, 0.10, 0.30, 0.30]; // block 1 is cheapest
        let ev = vec![ev_spec(EvStrategy::CostOptimized, n)];
        let plan = optimize_unified(
            &battery(10.0, 3.0, 8.0), // a charged battery that must stay out of the car
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &ev,
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let charge = &plan.ev_charge_kw["garage"];
        let delivered: f64 = charge.iter().sum::<f64>() * inputs.dt_hours; // η = 1
        assert!(
            (delivered - 5.0).abs() < 0.05,
            "EV target met: {delivered} kWh from {charge:?}"
        );
        assert!(
            charge[1] >= charge[0] - 1e-6 && charge[1] >= charge[2] - 1e-6,
            "charged the cheapest block: {charge:?}"
        );
        assert!(
            plan.ev_batt_kw["garage"].iter().all(|&b| b < 1e-6),
            "battery→EV is off by default"
        );
        let from_grid: f64 = plan.ev_grid_kw["garage"].iter().sum::<f64>() * inputs.dt_hours;
        assert!(
            (from_grid - 5.0).abs() < 0.05,
            "EV charged from grid: {from_grid}"
        );
    }

    /// The grid-connection import cap binds: with an 11 kW charger and one cheap block, the LP
    /// would stack the whole charge (plus any battery grid-charge) into that block — the cap forces
    /// it to spread while still meeting the target.
    #[test]
    fn grid_import_cap_spreads_the_ev_charge() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.30, n);
        inputs.import_price[1] = 0.10; // one cheap block the LP wants to stack into
        inputs.load_kw = vec![1.0; n]; // base load rides on the same connection
        let ev = vec![ev_spec(EvStrategy::CostOptimized, n)]; // 5 kWh target at up to 11 kW
        let mut flow = FlowParams::permissive(n);
        flow.max_import_kw = Some(4.0);
        let plan = optimize_unified(
            &battery(10.0, 3.0, 0.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &ev,
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        for t in 0..n {
            assert!(
                plan.grid_import_kw[t] <= 4.0 + 1e-6,
                "import cap respected at t={t}: {}",
                plan.grid_import_kw[t]
            );
        }
        let delivered: f64 = plan.ev_charge_kw["garage"].iter().sum::<f64>() * inputs.dt_hours;
        assert!(
            (delivered - 5.0).abs() < 0.05,
            "target still met under the cap (spread over blocks): {delivered}"
        );
    }

    /// The reported `discharge_kw` includes the battery→EV leg — SoC, the discharge cap and the
    /// wear objective all count it, so the report (and `classify_mode` downstream) must too.
    #[test]
    fn discharge_report_includes_battery_to_ev() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let inputs = flat_inputs(1.0, n); // punishing import ⇒ the battery is the cheap source
        let mut spec = ev_spec(EvStrategy::CostOptimized, n);
        spec.allow_battery_to_ev = true;
        spec.target_energy_kwh = 3.0;
        let plan = optimize_unified(
            &battery(10.0, 5.0, 8.0), // charged battery
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[spec],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let from_batt: f64 = plan.ev_batt_kw["garage"].iter().sum::<f64>();
        assert!(
            from_batt > 2.9,
            "the car should charge from the battery here: {from_batt}"
        );
        // Reported discharge covers every discharge path, so it matches the SoC drawdown exactly
        // (η = 1, nothing else charges/discharges).
        let discharged: f64 = plan.discharge_kw.iter().sum::<f64>() * inputs.dt_hours;
        let soc_drop = 8.0 - plan.soc_kwh.last().unwrap();
        assert!(
            (discharged - soc_drop).abs() < 1e-3,
            "discharge_kw ({discharged}) must equal the SoC drawdown ({soc_drop})"
        );
        for t in 0..n {
            assert!(
                plan.discharge_kw[t] >= plan.ev_batt_kw["garage"][t] - 1e-6,
                "block {t}: discharge must include the EV leg"
            );
        }
    }

    /// An on/off charger whose deadline lands mid-block (`deadline_frac < 1`) is rate-capped in that
    /// block too — the relay can run only the usable fraction, so it can't deliver a full block of
    /// charge by a deadline only partway into it (the binary branch honours `deadline_frac`).
    #[test]
    fn ev_on_off_respects_mid_block_deadline_fraction() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let inputs = flat_inputs(0.20, n);
        let mut spec = ev_spec(EvStrategy::CostOptimized, n);
        spec.on_off = true;
        spec.max_kw = 11.0;
        spec.target_energy_kwh = 10.0; // wants to charge as much as it can
        spec.plugged = (0..n).map(|i| i == 0).collect(); // only block 0 is before the deadline
        spec.deadline_block = 0;
        spec.deadline_frac = 0.5; // …and only half of block 0 is usable
        let plan = optimize_unified(
            &battery(10.0, 3.0, 8.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[spec],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let charge = &plan.ev_charge_kw["garage"];
        // Block 0 is capped at max_kw × frac = 5.5 kW (not the full 11), later blocks are unplugged.
        assert!(
            charge[0] <= 5.5 + 1e-6,
            "on/off deadline block rate-capped to the usable fraction: {charge:?}"
        );
        assert!(
            charge[1..].iter().all(|&c| c < 1e-6),
            "no charge after the deadline: {charge:?}"
        );
    }

    /// `solar_only` never imports grid power (or the battery) to the car: with no PV the charger
    /// stays idle and the target simply goes unmet, rather than buying grid energy.
    #[test]
    fn ev_solar_only_never_grid_charges() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n);
        let inputs = flat_inputs(0.20, n); // pv_kw = 0
        let mut spec = ev_spec(EvStrategy::SolarOnly, n);
        spec.allow_battery_to_ev = true; // even allowed, solar_only forbids the battery leg too
        let plan = optimize_unified(
            &battery(10.0, 3.0, 8.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[spec],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.ev_charge_kw["garage"].iter().all(|&c| c < 1e-6),
            "solar_only with no PV must not charge: {:?}",
            plan.ev_charge_kw["garage"]
        );
    }

    /// Battery wear suppresses uneconomic cycling: a wear cost above the price spread stops the
    /// arbitrage the same spread would otherwise drive.
    #[test]
    fn wear_suppresses_marginal_cycling() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.20, n);
        // Cheap then expensive: a 0.20 EUR/kWh spread.
        inputs.import_price = vec![0.10, 0.10, 0.30, 0.30];
        inputs.load_kw = vec![1.0; n];
        let spec = battery(10.0, 4.0, 0.0);

        let no_wear = optimize_unified(
            &spec,
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &{
                let mut f = FlowParams::permissive(n);
                f.amortisation = 0.0;
                f
            },
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let high_wear = optimize_unified(
            &spec,
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &{
                let mut f = FlowParams::permissive(n);
                f.amortisation = 0.50; // wear > spread → cycling is uneconomic
                f
            },
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();

        let cycled = |p: &UnifiedPlan| p.discharge_kw.iter().sum::<f64>();
        assert!(
            cycled(&no_wear) > cycled(&high_wear) + 0.5,
            "high wear should suppress cycling: {} vs {}",
            cycled(&no_wear),
            cycled(&high_wear)
        );
    }

    /// The export gate zeroes grid export even when exporting would otherwise pay.
    #[test]
    fn export_gate_blocks_export() {
        let n = 2;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.20, n);
        inputs.export_price = vec![0.15; n]; // exporting pays
        inputs.pv_kw = vec![5.0; n];
        inputs.load_kw = vec![0.0; n];
        let mut flow = FlowParams::permissive(n);
        flow.export_allowed = vec![false; n]; // ...but export is gated off

        let plan = optimize_unified(
            &battery(2.0, 1.0, 2.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.grid_export_kw.iter().all(|&e| e < 1e-6),
            "export gate must zero grid export: {:?}",
            plan.grid_export_kw
        );
        // The full battery can't store it either, so the surplus is curtailed.
        assert!(plan.curtail_kw.iter().sum::<f64>() > 0.0);
    }

    /// Inverter-off curtails all PV and serves the load from the grid.
    #[test]
    fn inverter_off_curtails_all_pv() {
        let n = 2;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.20, n);
        inputs.pv_kw = vec![4.0; n];
        inputs.load_kw = vec![1.0; n];
        let mut flow = FlowParams::permissive(n);
        flow.inverter_on = vec![false; n];

        let plan = optimize_unified(
            &battery(10.0, 3.0, 5.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        for t in 0..n {
            assert!(
                (plan.curtail_kw[t] - 4.0).abs() < 1e-4,
                "all PV curtailed at t={t}"
            );
            assert!(
                (plan.grid_import_kw[t] - 1.0).abs() < 1e-4,
                "load from grid at t={t}"
            );
            assert!(plan.discharge_kw[t] < 1e-6 && plan.charge_kw[t] < 1e-6);
        }
    }

    /// Inverter-off gates the EV's solar leg too: PV flows through the inverter, so with it off the
    /// car can't draw solar — it stays idle and all PV curtails.
    #[test]
    fn inverter_off_gates_ev_solar() {
        let n = 2;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut inputs = flat_inputs(0.20, n);
        inputs.pv_kw = vec![4.0; n];
        inputs.load_kw = vec![1.0; n];
        let mut flow = FlowParams::permissive(n);
        flow.inverter_on = vec![false; n];
        // A `solar_only` charger that wants energy — with the inverter off it can draw neither solar
        // (gated) nor grid/battery (its strategy forbids them), so it stays idle.
        let spec = ev_spec(EvStrategy::SolarOnly, n);
        let plan = optimize_unified(
            &battery(10.0, 3.0, 5.0),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &[spec],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        for t in 0..n {
            assert!(
                plan.ev_solar_kw["garage"][t] < 1e-6,
                "no EV solar when the inverter is off at t={t}: {:?}",
                plan.ev_solar_kw["garage"]
            );
            assert!(
                (plan.curtail_kw[t] - 4.0).abs() < 1e-4,
                "all PV curtailed at t={t}: {:?}",
                plan.curtail_kw
            );
        }
    }

    /// Two HVAC-served zones (`a`, `b`), each an insulated room, no underfloor heating — for the
    /// shared-capacity and single-mode tests.
    fn thermal_two_zone(outside_c: f64, ground_c: f64, x0_c: f64, n: usize) -> ThermalContext {
        let model = Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [ { material: "concrete", thickness: 0.1 } ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { a: { volume: 40 }, b: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["a", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["a", "outside"], area: 25 },
                    { boundary_type: "floor", zones: ["b", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["b", "outside"], area: 25 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let dt = 3600.0;
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(outside_c),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(ground_c),
        );
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(x0_c).get::<kelvin>(),
        );
        build_context(
            &ss,
            &net,
            &x0,
            &vec![u0; n],
            dt,
            &["a".to_string(), "b".to_string()],
            &[],
            None,
        )
        .unwrap()
    }

    #[test]
    fn comfort_ceiling_is_held_with_ac() {
        let n = 12;
        // Hot summer: the free response drifts above the 26 °C cooling setpoint.
        let thermal = thermal_for_hvac(40.0, 30.0, 32.0, n);
        let free_max = thermal.free_response["a"]
            .iter()
            .cloned()
            .fold(f64::MIN, f64::max)
            - KELVIN_OFFSET;
        assert!(
            free_max > 26.0,
            "scenario must overheat without AC: {free_max}"
        );
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &hvac_cfg(10.0, 0.0, 18.0, 26.0),
            &thermal,
            &flat_inputs(0.2, n),
            &FlowParams::permissive(n),
            &vec![35.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.cool_kw["a"].iter().sum::<f64>() > 0.0,
            "expected cooling"
        );
        let with_ac_max = plan.zone_temp_c["a"]
            .iter()
            .cloned()
            .fold(f64::MIN, f64::max);
        assert!(
            with_ac_max < free_max - 1.0,
            "AC reduces the peak temperature: {with_ac_max} vs {free_max}"
        );
        assert!(
            *plan.zone_temp_c["a"].last().unwrap() < 26.0 + 0.5,
            "ceiling held at steady state: {}",
            plan.zone_temp_c["a"].last().unwrap()
        );
    }

    #[test]
    fn hvac_electricity_uses_block_cop() {
        let n = 4;
        let thermal = thermal_for_hvac(40.0, 30.0, 33.0, n);
        let hvac = HvacConfig {
            comfort_penalty: 100.0,
            comfort: HashMap::from([(
                "a".to_string(),
                HvacComfort {
                    t_heat: 18.0,
                    t_cool: 24.0,
                },
            )]),
            units: HashMap::from([(
                "ac".to_string(),
                HvacUnit {
                    zones: vec!["a".to_string()],
                    max_cool_kw: 10.0,
                    max_heat_kw: 0.0,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Curve(vec![
                        CopPoint { t: 25.0, cop: 4.0 },
                        CopPoint { t: 35.0, cop: 2.0 },
                    ]),
                    heating_cop: CopSpec::Constant(3.0),
                },
            )]),
        };
        // No PV / load / battery → grid import exactly covers the cooling electricity (cool / COP),
        // with the COP read from each block's outdoor temperature.
        let outdoor = vec![25.0, 35.0, 25.0, 35.0];
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &hvac,
            &thermal,
            &flat_inputs(0.2, n),
            &FlowParams::permissive(n),
            &outdoor,
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.cool_kw["a"].iter().sum::<f64>() > 0.0,
            "expected cooling"
        );
        for (i, &t_out) in outdoor.iter().enumerate() {
            let cop = if t_out <= 25.0 { 4.0 } else { 2.0 };
            let expect = plan.cool_kw["a"][i] / cop;
            assert!(
                (plan.grid_import_kw[i] - expect).abs() < 1e-6,
                "block {i}: grid {} should equal cool/COP {expect}",
                plan.grid_import_kw[i]
            );
        }
    }

    #[test]
    fn ducted_unit_shares_capacity() {
        let n = 6;
        // Both rooms hot → both want cooling, but the ducted unit's 3 kW is shared between them.
        let thermal = thermal_two_zone(40.0, 30.0, 33.0, n);
        let hvac = HvacConfig {
            comfort_penalty: 100.0,
            comfort: HashMap::from([
                (
                    "a".to_string(),
                    HvacComfort {
                        t_heat: 18.0,
                        t_cool: 24.0,
                    },
                ),
                (
                    "b".to_string(),
                    HvacComfort {
                        t_heat: 18.0,
                        t_cool: 24.0,
                    },
                ),
            ]),
            units: HashMap::from([(
                "ducted".to_string(),
                HvacUnit {
                    zones: vec!["a".to_string(), "b".to_string()],
                    max_cool_kw: 3.0,
                    max_heat_kw: 3.0,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.0),
                },
            )]),
        };
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &hvac,
            &thermal,
            &flat_inputs(0.2, n),
            &FlowParams::permissive(n),
            &vec![35.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let mut peak = 0.0_f64;
        for i in 0..n {
            let total = plan.cool_kw["a"][i] + plan.cool_kw["b"][i];
            assert!(total <= 3.0 + 1e-6, "shared cap exceeded at {i}: {total}");
            peak = peak.max(total);
        }
        assert!(
            peak > 3.0 - 0.1,
            "the shared cap should bind when both rooms are hot: {peak}"
        );
    }

    #[test]
    fn per_zone_damper_caps_a_room() {
        let n = 6;
        // Both rooms hot; the unit has ample capacity, but room a's damper limits it to 1 kW.
        let thermal = thermal_two_zone(40.0, 30.0, 33.0, n);
        let hvac = HvacConfig {
            comfort_penalty: 100.0,
            comfort: HashMap::from([
                (
                    "a".to_string(),
                    HvacComfort {
                        t_heat: 18.0,
                        t_cool: 24.0,
                    },
                ),
                (
                    "b".to_string(),
                    HvacComfort {
                        t_heat: 18.0,
                        t_cool: 24.0,
                    },
                ),
            ]),
            units: HashMap::from([(
                "ducted".to_string(),
                HvacUnit {
                    zones: vec!["a".to_string(), "b".to_string()],
                    max_cool_kw: 10.0,
                    max_heat_kw: 10.0,
                    per_zone_max_kw: HashMap::from([("a".to_string(), 1.0)]),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.0),
                },
            )]),
        };
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &hvac,
            &thermal,
            &flat_inputs(0.2, n),
            &FlowParams::permissive(n),
            &vec![35.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.cool_kw["a"].iter().all(|&c| c <= 1.0 + 1e-6),
            "room a is damper-limited to 1 kW: {:?}",
            plan.cool_kw["a"]
        );
        assert!(
            plan.cool_kw["b"].iter().cloned().fold(0.0, f64::max) > 1.0 + 1e-6,
            "room b (no damper) cools harder than 1 kW"
        );
        // The damper-starved room ends warmer than the freely-cooled one.
        assert!(
            *plan.zone_temp_c["a"].last().unwrap() > *plan.zone_temp_c["b"].last().unwrap(),
            "damped room a stays warmer than room b"
        );
    }

    #[test]
    fn no_unit_heats_and_cools_the_same_block() {
        let n = 4;
        // A mild house at ~25 °C, but zone a's ceiling is 22 (wants cooling) while zone b's floor is
        // 28 (wants heating). One single-compressor unit can't serve both at once near-term.
        let thermal = thermal_two_zone(25.0, 25.0, 25.0, n);
        let mk = || HvacConfig {
            comfort_penalty: 100.0,
            comfort: HashMap::from([
                (
                    "a".to_string(),
                    HvacComfort {
                        t_heat: 5.0,
                        t_cool: 22.0,
                    },
                ),
                (
                    "b".to_string(),
                    HvacComfort {
                        t_heat: 28.0,
                        t_cool: 45.0,
                    },
                ),
            ]),
            units: HashMap::from([(
                "ducted".to_string(),
                HvacUnit {
                    zones: vec!["a".to_string(), "b".to_string()],
                    max_cool_kw: 5.0,
                    max_heat_kw: 5.0,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.0),
                },
            )]),
        };
        let solve_sm = || {
            optimize_unified(
                &no_battery(),
                &no_heating(),
                &mk(),
                &thermal,
                &flat_inputs(0.2, n),
                &FlowParams::permissive(n),
                &vec![25.0; n],
                &[],
                &[],
                None,
                false,
                &[],
            )
            .unwrap()
        };
        let near = BINARY_HEAT_BLOCKS.min(n);

        // The near-term heat-XOR-cool gate applies to EVERY unit: without it a negative-price
        // block lets the LP burn paid-for electricity by heating and cooling simultaneously.
        let plan = solve_sm();
        for i in 0..near {
            let c = plan.cool_kw["a"][i] + plan.cool_kw["b"][i];
            let h = plan.hvac_heat_kw["a"][i] + plan.hvac_heat_kw["b"][i];
            assert!(c < 1e-6 || h < 1e-6, "block {i}: cool={c} heat={h}");
        }
    }

    /// A thermal context for zone `"a"` with a controllable load `"boiler"` registered at its air node
    /// (the air-kernel source the LP scales by the load's on/off decision).
    fn thermal_for_load(outside_c: f64, ground_c: f64, x0_c: f64, n: usize) -> ThermalContext {
        let model = Model::from_json(
            r#"{
                materials: {
                    air: { thermal_conductivity: 0.026, specific_heat_capacity: 1000, density: 1.2 },
                    concrete: { thermal_conductivity: 1.5, specific_heat_capacity: 1000, density: 2000 },
                    insulation: { thermal_conductivity: 0.04, specific_heat_capacity: 1000, density: 30 },
                },
                boundary_types: {
                    floor: { layers: [
                        { material: "concrete", thickness: 0.05 },
                        { marker: "heating" },
                        { material: "concrete", thickness: 0.05 },
                    ] },
                    wall: { layers: [
                        { material: "concrete", thickness: 0.1 },
                        { material: "insulation", thickness: 0.12 },
                    ] },
                },
                zones: { a: { volume: 40 } },
                boundaries: [
                    { boundary_type: "floor", zones: ["a", "ground"], area: 16 },
                    { boundary_type: "wall",  zones: ["a", "outside"], area: 25 },
                ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let dt = 3600.0;
        let mut u0 = ss.zero_input();
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["outside"],
            ThermodynamicTemperature::new::<degree_celsius>(outside_c),
        );
        ss.set_boundary_temp(
            &mut u0,
            net.zone_indices["ground"],
            ThermodynamicTemperature::new::<degree_celsius>(ground_c),
        );
        let x0 = DVector::from_element(
            ss.n_states(),
            ThermodynamicTemperature::new::<degree_celsius>(x0_c).get::<kelvin>(),
        );
        build_context(
            &ss,
            &net,
            &x0,
            &vec![u0; n],
            dt,
            &[],
            &[("boiler".to_string(), "a".to_string())],
            None,
        )
        .unwrap()
    }

    /// A controllable load spec for `"boiler"` in zone `"a"`: rated draw, signed heat, a window mask,
    /// and a run-hours target.
    fn load_spec(
        rated_kw: f64,
        heat_kw: f64,
        window: Vec<bool>,
        run_hours: f64,
    ) -> ControllableLoadSpec {
        ControllableLoadSpec {
            name: "boiler".to_string(),
            zone: "a".to_string(),
            rated_kw,
            heat_kw,
            window,
            run_hours,
        }
    }

    fn solve_with_loads(
        thermal: &ThermalContext,
        inputs: &DispatchInputs,
        loads: &[ControllableLoadSpec],
    ) -> UnifiedPlan {
        let n = inputs.import_price.len();
        optimize_unified(
            &no_battery(),
            &no_heating(),
            &HvacConfig::default(),
            thermal,
            inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[],
            loads,
            None,
            false,
            &[],
        )
        .unwrap()
    }

    /// KEYSTONE: a controllable load with `run_hours = N` against a cheap-vs-expensive price curve is
    /// scheduled into the **cheapest N hours within its window**, and never outside the window.
    #[test]
    fn controllable_load_runs_cheapest_hours_in_window() {
        let n = 8;
        // Inert thermal (warm zone, wide band) so only price drives the schedule.
        let thermal = thermal_for_load(20.0, 18.0, 20.0, n);
        let mut inputs = flat_inputs(0.20, n);
        // A distinct price per block; the three cheapest in-window blocks are 2, 5, 6 below.
        inputs.import_price = vec![0.90, 0.90, 0.10, 0.90, 0.90, 0.12, 0.15, 0.90];
        // Window covers blocks 1..=6 (so block 0 and 7 are out-of-window, even if cheap-ish).
        let window: Vec<bool> = (0..n).map(|i| (1..=6).contains(&i)).collect();
        // Source load (sign +) but tiny heat so it doesn't perturb the (wide-band) comfort.
        let load = load_spec(2.0, 0.0, window.clone(), 3.0); // 3 run-hours
        let plan = solve_with_loads(&thermal, &inputs, &[load]);
        let draw = &plan.controllable_load_kw["boiler"];

        // Exactly 3 hours on (dt = 1 h), each at the rated 2 kW; 0 elsewhere.
        let on: Vec<usize> = (0..n).filter(|&i| draw[i] > 1e-6).collect();
        assert_eq!(
            on,
            vec![2, 5, 6],
            "should run the cheapest in-window blocks: {draw:?}"
        );
        for &i in &on {
            assert!((draw[i] - 2.0).abs() < 1e-6, "rated draw when on: {draw:?}");
        }
        // Never outside the window (block 0 and 7 must stay off even though 0 is mid-priced).
        assert!(
            draw[0] < 1e-6 && draw[7] < 1e-6,
            "never runs outside the window: {draw:?}"
        );
        // The run-hours target is met, so the import cost is the 3 cheapest in-window prices × 2 kWh.
        let expected = (0.10 + 0.12 + 0.15) * 2.0;
        assert!(
            (plan.total_cost - expected).abs() < 1e-6,
            "cost {} vs {expected}",
            plan.total_cost
        );
    }

    /// `controllable: false` ⇒ no `ControllableLoadSpec` reaches the optimizer, so the plan is exactly
    /// the passive path (empty `loads`). The two solves are byte-identical here.
    #[test]
    fn no_controllable_load_is_identical_to_passive() {
        let n = 6;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert, no load source
        let mut inputs = flat_inputs(0.20, n);
        inputs.import_price = vec![0.30, 0.10, 0.30, 0.10, 0.30, 0.10];
        inputs.load_kw = vec![1.0; n];

        let passive = solve_with_loads(&thermal, &inputs, &[]);
        let also_passive = solve_with_loads(&thermal, &inputs, &[]);
        assert_eq!(passive.total_cost, also_passive.total_cost);
        assert_eq!(passive.grid_import_kw, also_passive.grid_import_kw);
        assert!(
            passive.controllable_load_kw.is_empty(),
            "no controllable load ⇒ empty schedule map"
        );
    }

    /// Heat-when-on couples: a controllable **sink** (negative heat) lowers the predicted zone
    /// temperature only in the blocks it runs, leaving the off-blocks at the free response.
    #[test]
    fn controllable_sink_cools_only_on_blocks() {
        let n = 6;
        // Warm zone with a wide comfort band so the sink never trips the (penalized) comfort floor —
        // its only effect on the prediction is the air-node cooling in the on-blocks.
        let thermal = thermal_for_load(24.0, 22.0, 24.0, n);
        let inputs = flat_inputs(0.20, n);
        // Force a deterministic single on-block: run_hours = 1 h with a window of only block 3.
        let window: Vec<bool> = (0..n).map(|i| i == 3).collect();
        // A sink: 2 kW rated draw, −1.5 kW air heat (cools the room while on).
        let load = load_spec(2.0, -1.5, window, 1.0);
        let plan = solve_with_loads(&thermal, &inputs, &[load]);
        let draw = &plan.controllable_load_kw["boiler"];
        assert!(draw[3] > 1e-6, "the single in-window block runs: {draw:?}");
        assert!(
            (0..n).filter(|&i| i != 3).all(|i| draw[i] < 1e-6),
            "off everywhere but block 3: {draw:?}"
        );

        // The free response (no load) is the baseline (°C): with no load the prediction equals it.
        // The on-block prediction must dip below it, and the blocks before the load runs are unchanged.
        let with = &plan.zone_temp_c["a"];
        let free_c: Vec<f64> = thermal.free_response["a"]
            .iter()
            .map(|k| k - KELVIN_OFFSET)
            .collect();
        // Step index k-1 = 2 is the end of block 2 (before the load runs in block 3); unchanged.
        assert!(
            (with[2] - free_c[2]).abs() < 1e-9,
            "no effect before it runs"
        );
        // From the on-block onward the sink has cooled the zone.
        assert!(
            with[3] < free_c[3] - 1e-6,
            "sink cools the on-block: {with:?} vs {free_c:?}"
        );
        assert!(
            with[5] < free_c[5] - 1e-6,
            "the cooling persists after: {with:?} vs {free_c:?}"
        );
    }
    /// The block-0 commitment pins the relay INSIDE the LP: forced-on holds full power in a block
    /// the free optimum would leave off (and vice versa), and the whole plan is consistent with it.
    #[test]
    fn committed_block0_relay_is_pinned_in_the_lp() {
        let n = 8;
        let thermal = thermal_for(10.0, 12.0, 22.0, n); // warm house — free optimum heats nothing
        let mut inputs = flat_inputs(0.20, n);
        inputs.import_price[0] = 50.0; // astronomically expensive block 0
        let heating = heating_cfg(2.0, 18.0, 23.0);

        // Free solve: block 0 stays off (warm house, absurd price).
        let free = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![10.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            free.heat_kw["a"][0] < 1e-6,
            "free optimum keeps block 0 off"
        );

        // Committed ON: the latch says the relay is already running this block — the plan must
        // hold it at full power regardless of price, and the block-0 flows must carry it.
        let committed = HashMap::from([("a".to_string(), 2.0)]);
        let held = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![10.0; n],
            &[],
            &[],
            Some(&committed),
            false,
            &[],
        )
        .unwrap();
        assert!(
            (held.heat_kw["a"][0] - 2.0).abs() < 1e-6,
            "committed-on block 0 holds full power: {}",
            held.heat_kw["a"][0]
        );
        // grid_import covers the held heating electricity (2 kW / COP 3) in block 0.
        assert!(
            held.grid_import_kw[0] > 0.6,
            "block-0 flows carry the committed heat: {}",
            held.grid_import_kw[0]
        );

        // Committed OFF during a cold block the free optimum would heat.
        let mut cold_inputs = flat_inputs(0.20, n);
        cold_inputs.import_price[0] = 0.001; // nearly free block 0
        let cold = thermal_for(-15.0, 5.0, 17.5, n); // cold house below the band
        let committed_off = HashMap::from([("a".to_string(), 0.0)]);
        let held_off = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &cold,
            &cold_inputs,
            &FlowParams::permissive(n),
            &vec![-15.0; n],
            &[],
            &[],
            Some(&committed_off),
            false,
            &[],
        )
        .unwrap();
        assert!(
            held_off.heat_kw["a"][0] < 1e-6,
            "committed-off block 0 stays off: {}",
            held_off.heat_kw["a"][0]
        );
    }

    /// `relax_binaries` yields a valid plan whose relays may be fractional — the timeout fallback.
    #[test]
    fn relaxed_binaries_solve_produces_a_valid_plan() {
        let n = 8;
        let thermal = thermal_for(-10.0, 8.0, 19.0, n);
        let inputs = flat_inputs(0.20, n);
        let heating = heating_cfg(2.0, 19.0, 22.0);
        let plan = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![-10.0; n],
            &[],
            &[],
            None,
            true,
            &[],
        )
        .unwrap();
        assert!(plan.total_cost.is_finite());
        // Every heat value stays inside the physical envelope even without integrality.
        assert!(plan.heat_kw["a"]
            .iter()
            .all(|&h| (-1e-9..=2.0 + 1e-9).contains(&h)));
    }

    /// The comfort-loop term skipping (|kernel|·max < 1e-4 K dropped) changes the plan only
    /// negligibly: temperatures within 0.05 K of the exact affine prediction's implied comfort.
    #[test]
    fn term_skipping_keeps_temperatures_accurate() {
        let n = 12;
        let thermal = thermal_for(-5.0, 8.0, 19.0, n);
        let mut inputs = flat_inputs(0.25, n);
        inputs.import_price[2] = 0.05; // a cheap block to pre-heat in
        let heating = heating_cfg(2.0, 19.0, 22.5);
        let plan = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![-5.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        // Re-evaluate the LP's chosen schedule through the EXACT affine prediction and compare to
        // the plan's reported per-block temperatures (which the LP computed with skipped terms).
        let schedule = HashMap::from([("a".to_string(), plan.heat_kw["a"].clone())]);
        for k in 1..=n {
            let exact = thermal.predict("a", k, &schedule, &HashMap::new(), &HashMap::new());
            let reported = plan.zone_temp_c["a"][k - 1] + 273.15;
            assert!(
                (exact - reported).abs() < 0.05,
                "block {k}: exact {exact:.4} K vs reported {reported:.4} K"
            );
        }
    }
    /// A night-setback window lowers the band floor during its hours: the LP heats less inside the
    /// window and pre-heats before it ends (the slab-vs-tariff arbitrage the schedule exists for).
    #[test]
    fn comfort_schedule_window_lowers_the_night_floor() {
        let n = 8;
        // Mildly cold: holding the 19 °C floor needs some heat, but a 16 °C night floor lets the
        // house coast (it can't drop 3 K in 4 h) — the setback's saving is visible.
        let cold = thermal_for(5.0, 10.0, 19.2, n);
        let inputs = flat_inputs(0.20, n);
        let mut heating = heating_cfg(2.0, 19.0, 22.0);
        // Blocks 0..4 are "night" (minute 0..240 of the local day in the fixture below).
        heating.zones.get_mut("a").unwrap().windows =
            vec![crate::optimize::config::ComfortWindow {
                start: "00:00".to_string(),
                end: "04:00".to_string(),
                t_min: Some(16.0),
                t_max: None,
            }];
        // Hourly blocks starting at local midnight: minutes 0, 60, …, 420.
        let minutes: Vec<u32> = (0..n as u32).map(|i| i * 60).collect();
        let with_setback = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &cold,
            &inputs,
            &FlowParams::permissive(n),
            &vec![-10.0; n],
            &[],
            &[],
            None,
            false,
            &minutes,
        )
        .unwrap();
        let flat = optimize_unified(
            &no_battery(),
            &heating_cfg(2.0, 19.0, 22.0),
            &HvacConfig::default(),
            &cold,
            &inputs,
            &FlowParams::permissive(n),
            &vec![-10.0; n],
            &[],
            &[],
            None,
            false,
            &minutes,
        )
        .unwrap();
        let night_heat = |p: &UnifiedPlan| p.heat_kw["a"][0..4].iter().sum::<f64>();
        assert!(
            night_heat(&with_setback) < night_heat(&flat) - 1e-6,
            "setback nights heat less: {} vs {}",
            night_heat(&with_setback),
            night_heat(&flat)
        );
    }
    /// The terminal slab-heat credit banks cheap end-of-horizon heat the finite horizon would
    /// otherwise never buy (its comfort benefit falls past the edge), bounded by the band ceiling.
    #[test]
    fn terminal_heat_value_banks_late_cheap_heat() {
        let n = 8;
        let thermal = thermal_for(15.0, 12.0, 22.0, n);
        let inputs = flat_inputs(0.05, n); // cheap throughout
                                           // Floor far below any drift — the base plan buys NO comfort heat, isolating the credit.
        let heating = heating_cfg(2.0, 15.0, 23.0);
        let mut flow = FlowParams::permissive(n);
        let base = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![0.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        flow.terminal_heat_value = 0.10; // banked heat worth 2x the import price
        let banked = optimize_unified(
            &no_battery(),
            &heating,
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![0.0; n],
            &[],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let tail = |p: &UnifiedPlan| p.heat_kw["a"][n - 6..].iter().sum::<f64>();
        // The credit buys tail heat the base plan wouldn't — but only up to the per-zone BANK CAP
        // (one full-power hour = 2 kWh), never unlimited saturation.
        assert!(
            tail(&banked) > tail(&base) + 1.5,
            "credited tail buys heat: {} vs {}",
            tail(&banked),
            tail(&base)
        );
        assert!(
            tail(&banked) <= tail(&base) + 2.0 + 1e-6,
            "…bounded by the bank cap: {} vs {}",
            tail(&banked),
            tail(&base)
        );
        // …but never past the comfort ceiling: the final predicted temperature stays ≤ t_max
        // (+ small tolerance) because the slack penalty dwarfs the credit.
        let last_c = banked.zone_temp_c["a"][n - 1];
        assert!(last_c <= 23.0 + 0.1, "ceiling respected: {last_c}");
    }
    /// Above-target bonus charging absorbs otherwise-WASTED energy only: curtailment-regime PV
    /// (export disabled, sun up) and negative-price grid blocks — never plain-priced energy.
    #[test]
    fn ev_bonus_charging_absorbs_curtailment_and_negative_prices_only() {
        let n = 4;
        let thermal = thermal_for(20.0, 18.0, 20.0, n); // inert
        let mut spec = ev_spec(EvStrategy::CostOptimized, n);
        spec.target_energy_kwh = 0.0; // target reached
        spec.bonus_energy_kwh = 6.0; // car limit leaves 6 kWh of headroom

        // (a) Export-disabled sunny blocks: surplus PV goes to the car instead of curtailing.
        let mut inputs = flat_inputs(0.20, n);
        inputs.pv_kw = vec![8.0; n];
        inputs.load_kw = vec![1.0; n];
        let mut flow = FlowParams::permissive(n);
        flow.export_allowed = vec![false; n];
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &flow,
            &vec![20.0; n],
            &[spec.clone()],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        let charged: f64 = plan.ev_charge_kw["garage"].iter().sum::<f64>() * 1.0; // dt=1h
        assert!(
            (charged - 6.0).abs() < 1e-6,
            "curtailed PV fills the bonus headroom: {charged}"
        );
        assert!(
            plan.curtail_kw.iter().sum::<f64>() < 7.0 * 4.0 - 5.9,
            "curtailment drops by the absorbed energy"
        );

        // (b) Plain positive prices, no PV: NO bonus charging (target is met; energy costs money).
        let inputs = flat_inputs(0.20, n);
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[spec.clone()],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.ev_charge_kw["garage"].iter().sum::<f64>() < 1e-6,
            "no bonus from plain-priced energy"
        );

        // (c) A negative-price block: the grid PAYS us to charge the car past target.
        let mut inputs = flat_inputs(0.20, n);
        inputs.import_price[2] = -0.05;
        inputs.export_price[2] = -0.05; // the tariff caps export at import (validate precondition)
        let plan = optimize_unified(
            &no_battery(),
            &no_heating(),
            &HvacConfig::default(),
            &thermal,
            &inputs,
            &FlowParams::permissive(n),
            &vec![20.0; n],
            &[spec],
            &[],
            None,
            false,
            &[],
        )
        .unwrap();
        assert!(
            plan.ev_charge_kw["garage"][2] > 5.9,
            "negative-price block bonus-charges: {}",
            plan.ev_charge_kw["garage"][2]
        );
        assert!(plan.ev_charge_kw["garage"][0] < 1e-6);
    }
}
