//! Battery economic-dispatch optimizer.
//!
//! Given day-ahead import/export prices and the PV-production and consumption forecasts over a
//! horizon, decide the battery charge/discharge and grid import/export per step to minimize
//! electricity cost. Formulated as a linear program and solved with the pure-Rust `microlp`
//! backend of `good_lp`.
//!
//! It is a plain LP (no integer variables). This is valid only while `export_price <=
//! import_price` at every step (enforced by [`DispatchInputs::validate`]): under that condition
//! there is no profit in routing grid energy straight back out, so the optimum never imports
//! and re-exports, and never simultaneously charges and discharges. Inverted price spreads
//! (a feed-in tariff above the spot import price, or negative import prices) make the cost
//! non-convex and need a MILP formulation instead.
//!
//! **Terminal behavior:** energy left in the battery at the end of the horizon has no value in
//! the objective, so with any positive export price the optimum drains the battery toward
//! `min_soc` by the final step. In a rolling/MPC loop, apply only the first step(s) of the plan
//! and set [`DispatchInputs::min_final_soc_kwh`] to keep a reserve.
//!
//! All powers are kW, energies kWh, prices price-units/kWh, time steps hours.

use anyhow::{ensure, Result};
use good_lp::{constraint, microlp, variable, variables, Expression, Solution, SolverModel};

/// Physical limits and state of a battery.
#[derive(Debug, Clone)]
pub struct BatterySpec {
    pub max_charge_kw: f64,
    pub max_discharge_kw: f64,
    /// Fraction of charge power that reaches storage (in `(0, 1]`).
    pub charge_efficiency: f64,
    /// Fraction of stored energy delivered on discharge (in `(0, 1]`).
    pub discharge_efficiency: f64,
    pub min_soc_kwh: f64,
    pub max_soc_kwh: f64,
    pub initial_soc_kwh: f64,
}

impl BatterySpec {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.max_charge_kw >= 0.0 && self.max_discharge_kw >= 0.0,
            "battery power limits must be non-negative"
        );
        ensure!(
            self.charge_efficiency > 0.0 && self.charge_efficiency <= 1.0,
            "charge_efficiency must be in (0, 1]"
        );
        ensure!(
            self.discharge_efficiency > 0.0 && self.discharge_efficiency <= 1.0,
            "discharge_efficiency must be in (0, 1]"
        );
        ensure!(
            self.min_soc_kwh <= self.max_soc_kwh,
            "min_soc_kwh must not exceed max_soc_kwh"
        );
        ensure!(
            self.min_soc_kwh <= self.initial_soc_kwh && self.initial_soc_kwh <= self.max_soc_kwh,
            "initial_soc_kwh must lie within [min_soc_kwh, max_soc_kwh]"
        );
        Ok(())
    }
}

/// Forecasts over the optimization horizon. All forecast vectors must be the same length.
#[derive(Debug, Clone)]
pub struct DispatchInputs {
    pub dt_hours: f64,
    /// Price paid per kWh imported from the grid, per step.
    pub import_price: Vec<f64>,
    /// Price received per kWh exported to the grid, per step (feed-in). Must be `<= import_price`.
    pub export_price: Vec<f64>,
    /// PV production forecast (kW), per step.
    pub pv_kw: Vec<f64>,
    /// Consumption forecast (kW), per step.
    pub load_kw: Vec<f64>,
    /// Optional lower bound on the state of charge at the end of the horizon. Use it in a
    /// rolling/MPC loop to stop the optimizer draining the battery at the horizon edge.
    pub min_final_soc_kwh: Option<f64>,
}

/// The optimized dispatch plan over the horizon.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchPlan {
    pub charge_kw: Vec<f64>,
    pub discharge_kw: Vec<f64>,
    pub grid_import_kw: Vec<f64>,
    pub grid_export_kw: Vec<f64>,
    /// State of charge (kWh) at the end of each step.
    pub soc_kwh: Vec<f64>,
    /// Total electricity cost over the horizon (import cost minus export revenue).
    pub total_cost: f64,
}

impl DispatchInputs {
    fn len(&self) -> usize {
        self.import_price.len()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let n = self.len();
        ensure!(n > 0, "dispatch horizon is empty");
        ensure!(
            self.export_price.len() == n && self.pv_kw.len() == n && self.load_kw.len() == n,
            "all forecast vectors must have the same length"
        );
        ensure!(self.dt_hours > 0.0, "dt_hours must be positive");
        for t in 0..n {
            ensure!(
                self.export_price[t] <= self.import_price[t] + 1e-9,
                "export_price must not exceed import_price (step {t}); this LP requires export <= import"
            );
        }
        Ok(())
    }
}

/// Solve the battery economic-dispatch LP.
pub fn optimize_dispatch(spec: &BatterySpec, inputs: &DispatchInputs) -> Result<DispatchPlan> {
    spec.validate()?;
    inputs.validate()?;
    let n = inputs.len();
    let dt = inputs.dt_hours;

    let mut vars = variables!();
    let charge: Vec<_> = (0..n)
        .map(|_| vars.add(variable().min(0.0).max(spec.max_charge_kw)))
        .collect();
    let discharge: Vec<_> = (0..n)
        .map(|_| vars.add(variable().min(0.0).max(spec.max_discharge_kw)))
        .collect();
    let grid_import: Vec<_> = (0..n).map(|_| vars.add(variable().min(0.0))).collect();
    let grid_export: Vec<_> = (0..n).map(|_| vars.add(variable().min(0.0))).collect();

    // Total cost = sum_t (import_price * import - export_price * export) * dt. Built once and
    // reused: minimized by the solver and evaluated afterwards for the reported total_cost.
    let cost: Expression = (0..n)
        .map(|t| {
            (inputs.import_price[t] * grid_import[t] - inputs.export_price[t] * grid_export[t]) * dt
        })
        .sum();

    let mut problem = vars.minimise(cost.clone()).using(microlp);

    // The state of charge after each step, as a running affine expression. Built once and
    // reused for the SoC bound constraints and for the reported soc_kwh, so the charge/discharge
    // efficiency recurrence lives in exactly one place.
    let mut soc = Expression::from(spec.initial_soc_kwh);
    let mut soc_after = Vec::with_capacity(n);
    for t in 0..n {
        // Power balance at the bus: supply == demand.
        problem = problem.with(constraint!(
            inputs.pv_kw[t] + discharge[t] + grid_import[t]
                == inputs.load_kw[t] + charge[t] + grid_export[t]
        ));
        // Charging adds energy net of losses; discharging draws extra to cover its losses.
        soc += (spec.charge_efficiency * charge[t] - discharge[t] / spec.discharge_efficiency) * dt;
        problem = problem.with(constraint!(soc.clone() >= spec.min_soc_kwh));
        problem = problem.with(constraint!(soc.clone() <= spec.max_soc_kwh));
        soc_after.push(soc.clone());
    }
    if let Some(target) = inputs.min_final_soc_kwh {
        problem = problem.with(constraint!(soc.clone() >= target));
    }

    let solution = problem.solve()?;

    // `.max(0.0)` scrubs sub-epsilon negative numerical noise on these non-negative variables.
    let values = |vs: &[good_lp::Variable]| -> Vec<f64> {
        vs.iter().map(|v| solution.value(*v).max(0.0)).collect()
    };

    Ok(DispatchPlan {
        charge_kw: values(&charge),
        discharge_kw: values(&discharge),
        grid_import_kw: values(&grid_import),
        grid_export_kw: values(&grid_export),
        soc_kwh: soc_after.iter().map(|e| e.eval_with(&solution)).collect(),
        total_cost: cost.eval_with(&solution),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    fn lossless_battery(capacity: f64, power: f64, initial: f64) -> BatterySpec {
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

    #[test]
    fn balanced_supply_needs_no_grid() {
        // PV exactly meets load every step, the battery is empty and there is no feed-in
        // revenue → the optimum is to do nothing: no import, no export, zero cost.
        let spec = lossless_battery(10.0, 5.0, 0.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.3, 0.3],
            export_price: vec![0.0, 0.0],
            pv_kw: vec![2.0, 2.0],
            load_kw: vec![2.0, 2.0],
            min_final_soc_kwh: None,
        };
        let plan = optimize_dispatch(&spec, &inputs).unwrap();
        assert_abs_diff_eq!(plan.total_cost, 0.0, epsilon = 1e-6);
        for t in 0..2 {
            assert_abs_diff_eq!(plan.grid_import_kw[t], 0.0, epsilon = 1e-6);
            assert_abs_diff_eq!(plan.grid_export_kw[t], 0.0, epsilon = 1e-6);
        }
    }

    #[test]
    fn arbitrage_charges_cheap_discharges_expensive() {
        // No PV, constant 1 kW load. Cheap hour then expensive hour: fill the battery during the
        // cheap hour (importing extra) and cover the load from storage during the expensive one.
        let spec = lossless_battery(10.0, 4.0, 0.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.10, 1.00],
            export_price: vec![0.0, 0.0],
            pv_kw: vec![0.0, 0.0],
            load_kw: vec![1.0, 1.0],
            min_final_soc_kwh: None,
        };
        let plan = optimize_dispatch(&spec, &inputs).unwrap();

        assert!(
            plan.charge_kw[0] > 0.5,
            "expected charging in the cheap hour"
        );
        assert!(
            plan.soc_kwh[0] > 0.5,
            "battery should hold energy after the cheap hour"
        );
        assert!(
            plan.discharge_kw[1] > 0.5,
            "expected discharging in the expensive hour"
        );
        assert_abs_diff_eq!(plan.grid_import_kw[1], 0.0, epsilon = 1e-6);
        // Importing 2 kWh at 0.10 (1 for the cheap-hour load, 1 stored for the expensive hour)
        // is the cheapest way to serve the 2 kWh of total load.
        assert_abs_diff_eq!(plan.total_cost, 0.10 * 2.0, epsilon = 1e-6);
    }

    #[test]
    fn losses_apply_to_the_right_side_of_the_recurrence() {
        // Charge in a cheap hour, then serve a 1 kW load from storage in a very expensive hour.
        // This pins the loss convention: charging stores `charge * charge_efficiency`, and
        // delivering 1 kWh draws `1 / discharge_efficiency` from storage.
        let spec = BatterySpec {
            max_charge_kw: 5.0,
            max_discharge_kw: 5.0,
            charge_efficiency: 0.9,
            discharge_efficiency: 0.8,
            min_soc_kwh: 0.0,
            max_soc_kwh: 10.0,
            initial_soc_kwh: 0.0,
        };
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.10, 10.0],
            export_price: vec![0.0, 0.0],
            pv_kw: vec![0.0, 0.0],
            load_kw: vec![0.0, 1.0],
            min_final_soc_kwh: None,
        };
        let plan = optimize_dispatch(&spec, &inputs).unwrap();

        assert_abs_diff_eq!(plan.discharge_kw[1], 1.0, epsilon = 1e-6);
        assert_abs_diff_eq!(
            plan.soc_kwh[0],
            plan.charge_kw[0] * spec.charge_efficiency,
            epsilon = 1e-6
        );
        assert_abs_diff_eq!(
            plan.soc_kwh[1],
            plan.soc_kwh[0] - 1.0 / spec.discharge_efficiency,
            epsilon = 1e-6
        );
    }

    #[test]
    fn min_final_soc_keeps_a_reserve() {
        // The same arbitrage setup, but require 2 kWh left at the end: the battery must not be
        // drained below that even though leftover energy has no objective value.
        let spec = lossless_battery(10.0, 4.0, 3.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.10, 1.00],
            export_price: vec![0.05, 0.05],
            pv_kw: vec![0.0, 0.0],
            load_kw: vec![1.0, 1.0],
            min_final_soc_kwh: Some(2.0),
        };
        let plan = optimize_dispatch(&spec, &inputs).unwrap();
        assert!(*plan.soc_kwh.last().unwrap() >= 2.0 - 1e-6);
    }

    #[test]
    fn respects_soc_and_power_limits() {
        let spec = lossless_battery(2.0, 1.5, 1.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.1, 0.9, 0.9],
            export_price: vec![0.0, 0.0, 0.0],
            pv_kw: vec![0.0, 0.0, 0.0],
            load_kw: vec![1.0, 1.0, 1.0],
            min_final_soc_kwh: None,
        };
        let plan = optimize_dispatch(&spec, &inputs).unwrap();
        // It charges in the cheap hour, and never violates the power or capacity limits.
        assert!(
            plan.charge_kw[0] > 0.0,
            "expected charging in the cheap hour"
        );
        for t in 0..3 {
            assert!(plan.charge_kw[t] <= spec.max_charge_kw + 1e-6);
            assert!(plan.discharge_kw[t] <= spec.max_discharge_kw + 1e-6);
            assert!(plan.soc_kwh[t] >= spec.min_soc_kwh - 1e-6);
            assert!(plan.soc_kwh[t] <= spec.max_soc_kwh + 1e-6);
        }
    }

    #[test]
    fn rejects_export_above_import() {
        let spec = lossless_battery(10.0, 5.0, 5.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.10],
            export_price: vec![0.50], // would make the LP unbounded
            pv_kw: vec![0.0],
            load_kw: vec![1.0],
            min_final_soc_kwh: None,
        };
        assert!(optimize_dispatch(&spec, &inputs).is_err());
    }

    #[test]
    fn rejects_invalid_spec() {
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![0.3],
            export_price: vec![0.1],
            pv_kw: vec![0.0],
            load_kw: vec![1.0],
            min_final_soc_kwh: None,
        };
        // Zero discharge efficiency (would divide by zero in the recurrence).
        let mut bad = lossless_battery(10.0, 5.0, 5.0);
        bad.discharge_efficiency = 0.0;
        assert!(optimize_dispatch(&bad, &inputs).is_err());
        // Initial charge outside the SoC band.
        let mut bad = lossless_battery(10.0, 5.0, 5.0);
        bad.initial_soc_kwh = 99.0;
        assert!(optimize_dispatch(&bad, &inputs).is_err());
    }

    #[test]
    fn empty_horizon_errors() {
        let spec = lossless_battery(10.0, 5.0, 5.0);
        let inputs = DispatchInputs {
            dt_hours: 1.0,
            import_price: vec![],
            export_price: vec![],
            pv_kw: vec![],
            load_kw: vec![],
            min_final_soc_kwh: None,
        };
        assert!(optimize_dispatch(&spec, &inputs).is_err());
    }
}
