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
    /// AC→DC charging efficiency (0..1): energy into the car battery per kWh drawn from the house.
    pub efficiency: f64,
    /// May the home battery charge the car? (Off ⇒ `battery→EV` is bounded to 0.)
    pub allow_battery_to_ev: bool,
    /// Per-block: is the car controllable on our wallbox this block (the plug-in window)?
    pub plugged: Vec<bool>,
    /// Energy to deliver to reach the target (kWh) — the soft goal at `deadline_block`.
    pub target_energy_kwh: f64,
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
    pub grid_export_kw: Vec<f64>,
    /// PV curtailed (kW) per block — solar neither used, stored, nor exported.
    pub curtail_kw: Vec<f64>,
    pub soc_kwh: Vec<f64>,
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
}

impl FlowParams {
    /// Permissive defaults for `n` blocks: no gates, no wear, no terminal value (the plain
    /// economic-dispatch behaviour). Used by the tests.
    #[cfg(test)]
    pub fn permissive(n: usize) -> Self {
        Self {
            export_allowed: vec![true; n],
            inverter_on: vec![true; n],
            amortisation: 0.0,
            terminal_value: 0.0,
        }
    }
}

/// Solve the unified battery + heating + HVAC dispatch as an energy-flow model.
///
/// `outdoor_temp_c` is the per-block outdoor-air forecast (°C), used to evaluate each HVAC unit's
/// COP curve per block; because it is a *known* input the per-block COP is a constant, so the
/// problem stays a (mixed-integer) linear program.
#[allow(clippy::too_many_arguments)] // battery / heating / hvac / thermal / inputs / flow / temps are distinct
pub fn optimize_unified(
    battery: &BatterySpec,
    heating: &HeatingConfig,
    hvac: &HvacConfig,
    thermal: &ThermalContext,
    inputs: &DispatchInputs,
    flow: &FlowParams,
    outdoor_temp_c: &[f64],
    ev: &[EvSpec],
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
    // Controlled zones = the union (each gets a soft comfort band), in deterministic order.
    let mut controlled: Vec<String> = heat_zones
        .iter()
        .chain(hvac_zones.iter())
        .cloned()
        .collect();
    controlled.sort();
    controlled.dedup();
    let is_heat = |z: &str| heat_zones.iter().any(|h| h == z);
    let is_hvac = |z: &str| hvac_zones.iter().any(|h| h == z);
    // Per-zone comfort band: heat below the lower edge, cool above the upper. A heated zone uses
    // its heating `t_min`; an HVAC zone uses its `t_cool` ceiling (and `t_heat` floor if not heated).
    let lower = |z: &str| {
        if is_heat(z) {
            heating.zones[z].t_min
        } else {
            hvac.comfort[z].t_heat
        }
    };
    let upper = |z: &str| {
        if is_hvac(z) {
            hvac.comfort[z].t_cool
        } else {
            heating.zones[z].t_max
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
                .map(|_| vars.add(variable().binary()))
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

    // Near-term cooling-mode binary for single-compressor (ducted) units: forces heat XOR cool.
    let mut cool_mode: HashMap<String, Vec<Variable>> = HashMap::new();
    for (uname, _served) in &unit_served {
        if hvac.units[uname].single_mode {
            cool_mode.insert(
                uname.clone(),
                (0..binary_blocks)
                    .map(|_| vars.add(variable().binary()))
                    .collect(),
            );
        }
    }

    // Soft comfort slack for every controlled zone (below the lower edge / above the upper).
    let mut slack_lo: HashMap<String, Vec<Variable>> = HashMap::new();
    let mut slack_hi: HashMap<String, Vec<Variable>> = HashMap::new();
    for z in &controlled {
        slack_lo.insert(
            z.clone(),
            (0..n).map(|_| vars.add(variable().min(0.0))).collect(),
        );
        slack_hi.insert(
            z.clone(),
            (0..n).map(|_| vars.add(variable().min(0.0))).collect(),
        );
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
    let ev_solar: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            (0..n)
                // PV reaches the car through the inverter, so — like `ev_batt` and the other solar
                // legs — it is gated on `inverter_on`: when the inverter is off (deeply-negative
                // prices) all PV curtails rather than flowing to the EV.
                .map(|i| vars.add(ev_leg(e.plugged[i] && flow.inverter_on[i], e.max_kw)))
                .collect()
        })
        .collect();
    let ev_grid: Vec<Vec<Variable>> = ev
        .iter()
        .map(|e| {
            let allow_grid = e.strategy != EvStrategy::SolarOnly;
            (0..n)
                .map(|i| vars.add(ev_leg(e.plugged[i] && allow_grid, e.max_kw)))
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
            if e.on_off {
                (0..binary_blocks)
                    .map(|_| vars.add(variable().binary()))
                    .collect()
            } else {
                Vec::new()
            }
        })
        .collect();
    let ev_shortfall: Vec<Variable> = ev.iter().map(|_| vars.add(variable().min(0.0))).collect();
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
        objective += flow.amortisation * (batt_to_load[i] + batt_to_grid[i] + ev_batt_sum(i)) * dt;
        objective += CURTAIL_PENALTY * curtail[i] * dt;
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
    for z in &controlled {
        let pen = penalty(z);
        for k in 0..n {
            objective += pen * (slack_lo[z][k] + slack_hi[z][k]);
        }
    }
    if let Some(final_soc) = soc_after.last() {
        objective -= flow.terminal_value * battery.discharge_efficiency * final_soc.clone();
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
        problem = problem.with(constraint!(soc_after[i].clone() >= battery.min_soc_kwh));
        problem = problem.with(constraint!(soc_after[i].clone() <= battery.max_soc_kwh));
    }
    if let Some(target) = inputs.min_final_soc_kwh {
        if let Some(final_soc) = soc_after.last() {
            problem = problem.with(constraint!(final_soc.clone() >= target));
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
    for z in &controlled {
        let (lo_k, hi_k) = (lower(z) + KELVIN_OFFSET, upper(z) + KELVIN_OFFSET);
        let free = &thermal.free_response[z];
        for k in 1..=n {
            let mut t_pred = Expression::from(free[k - 1]);
            for source in &heat_zones {
                if let Some(kernel) = thermal.kernels.get(&(z.clone(), source.clone())) {
                    for j in 0..k {
                        t_pred += kernel[k - j - 1] * heat[source][j];
                    }
                }
            }
            for source in &hvac_zones {
                if let Some(kernel) = thermal.air_kernels.get(&(z.clone(), source.clone())) {
                    for j in 0..k {
                        t_pred += kernel[k - j - 1] * (air_heat[source][j] - cool[source][j]);
                    }
                }
            }
            problem = problem.with(constraint!(t_pred.clone() + slack_lo[z][k - 1] >= lo_k));
            problem = problem.with(constraint!(t_pred - slack_hi[z][k - 1] <= hi_k));
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
            delivered + ev_shortfall[c] >= e.target_energy_kwh
        ));
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
    let zone_temp_c: HashMap<String, Vec<f64>> = controlled
        .iter()
        .map(|z| {
            let temps = (1..=n)
                .map(|k| thermal.predict(z, k, &heat_kw, &air_net) - KELVIN_OFFSET)
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
        discharge_kw: agg(&batt_to_load, &batt_to_grid),
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
        curtail_kw: values(&curtail),
        soc_kwh: soc_after.iter().map(|e| e.eval_with(&solution)).collect(),
        heat_kw,
        cool_kw,
        hvac_heat_kw,
        zone_temp_c,
        ev_charge_kw,
        ev_solar_kw: ev_legs(&ev_solar),
        ev_grid_kw: ev_legs(&ev_grid),
        ev_batt_kw: ev_legs(&ev_batt),
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
        build_context(&ss, &net, &x0, &vec![u0; n], dt, hvac_zones).unwrap()
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
                    single_mode: false,
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
            efficiency: 1.0,
            allow_battery_to_ev: false,
            plugged: vec![true; n],
            target_energy_kwh: 5.0,
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
                    single_mode: false,
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
                    single_mode: false,
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
                    single_mode: false,
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
    fn single_mode_unit_does_not_heat_and_cool_same_block() {
        let n = 4;
        // A mild house at ~25 °C, but zone a's ceiling is 22 (wants cooling) while zone b's floor is
        // 28 (wants heating). One single-compressor unit can't serve both at once near-term.
        let thermal = thermal_two_zone(25.0, 25.0, 25.0, n);
        let mk = |single_mode: bool| HvacConfig {
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
                    single_mode,
                },
            )]),
        };
        let solve_sm = |single| {
            optimize_unified(
                &no_battery(),
                &no_heating(),
                &mk(single),
                &thermal,
                &flat_inputs(0.2, n),
                &FlowParams::permissive(n),
                &vec![25.0; n],
                &[],
            )
            .unwrap()
        };
        let near = BINARY_HEAT_BLOCKS.min(n);

        // Without the gate, the unit cools a AND heats b in the same block.
        let free = solve_sm(false);
        let both = (0..near).any(|i| {
            let c = free.cool_kw["a"][i] + free.cool_kw["b"][i];
            let h = free.hvac_heat_kw["a"][i] + free.hvac_heat_kw["b"][i];
            c > 1e-6 && h > 1e-6
        });
        assert!(
            both,
            "without single_mode the unit should heat and cool at once"
        );

        // With the gate, every near-term block is heat-only or cool-only.
        let gated = solve_sm(true);
        for i in 0..near {
            let c = gated.cool_kw["a"][i] + gated.cool_kw["b"][i];
            let h = gated.hvac_heat_kw["a"][i] + gated.hvac_heat_kw["b"][i];
            assert!(
                c < 1e-6 || h < 1e-6,
                "single_mode block {i}: cool={c} heat={h}"
            );
        }
    }
}
