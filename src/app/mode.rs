//! The per-block battery-mode read-off (`classify_mode`) in `loxone_smart_home`'s vocabulary — what
//! the armed Growatt controller applies downstream.

use serde::Serialize;

/// The recommended Growatt configuration for one 15-min block: the inverter **slot mode** plus the
/// two **independent, price-gated toggles** — export enable/disable and the inverter on/off master
/// switch — mirroring how the live controller is actually set up. It is a read-off of the recommended
/// intent, not a literal echo of the controller's register; the brain itself never actuates — the
/// **armed** controllers apply it downstream (Growatt the battery, loxone the heating/EV).
#[derive(Debug, Clone, Serialize)]
pub struct ModeStep {
    /// Battery action in `loxone_smart_home`'s vocabulary: `regular` / `charge_from_grid` /
    /// `discharge_to_grid` / `sell_production` / `battery_hold` / `inverter_off` (see [`classify_mode`]).
    pub slot: String,
    /// Whether grid export is enabled this block — an **orthogonal toggle** (the inverter can export
    /// in any battery mode), off below the export-floor spot price.
    pub export_enabled: bool,
    /// Whether the inverter is powered on (off only in deeply-negative-price blocks).
    pub inverter_on: bool,
    /// Recommended battery **charge** power this block (kW; 0 when not charging) — the explicit
    /// power the inverter's charge powerRate would be set to.
    pub charge_kw: f64,
    /// Recommended battery **discharge** power this block (kW; 0 when not discharging).
    pub discharge_kw: f64,
}

/// One block's flows for [`classify_mode`]. The BATTERY grid legs are separate from the totals:
/// `charge_from_grid`/`discharge_to_grid` must key on what the *battery* exchanges with the grid —
/// the totals also carry EV grid charging and solar export, and using them would let a
/// solar-charging-battery + EV-import block actuate forced AC charge (and battery→EV during solar
/// export actuate a battery drain to grid). The totals still drive `sell_production` (PV export)
/// and `battery_hold` (house importing while the battery sits).
pub(super) struct BlockFlows {
    pub(super) charge_kw: f64,
    pub(super) discharge_kw: f64,
    /// Battery AC-charge from the grid only (no EV leg).
    pub(super) batt_grid_charge_kw: f64,
    /// Battery→grid export only (no solar, no EV).
    pub(super) batt_to_grid_kw: f64,
    /// Total grid import (incl. EV charging).
    pub(super) grid_import_kw: f64,
    /// Total grid export (incl. solar).
    pub(super) grid_export_kw: f64,
    pub(super) soc_kwh: f64,
    pub(super) inverter_on: bool,
}

/// The battery action for one block, in `loxone_smart_home`'s published vocabulary
/// (`growatt_status.current_mode`), so the dashboard speaks the same battery-mode language the house
/// has always used: `regular` (self-consumption — including passive solar-charge / load-discharge,
/// which loxone also reports as `regular`), `charge_from_grid`, `discharge_to_grid`, `sell_production`
/// (exporting surplus solar with the battery passive), `battery_hold` (importing while the battery is
/// held for a pricier block), `inverter_off`.
///
/// **Export-enabled and inverter-on are orthogonal toggles** (settable in any mode) — they are tracked
/// separately on [`ModeStep`] and are NOT folded into this status.
pub(super) fn classify_mode(
    f: &BlockFlows,
    min_soc_kwh: f64,
    max_soc_kwh: f64,
    min_dispatch_kw: f64,
) -> &'static str {
    const EPS: f64 = 0.05; // kW — ignore solver dust

    // The ACTUATOR's floor (config `battery.min_dispatch_kw`): the Growatt controller rounds any
    // nonzero powerrate UP to its minimum (~2.45 kW). A `Rounded` plan's own LP now ENFORCES
    // "0 or >= floor" on both grid-arbitrage legs directly (`optimize::unified::round_dispatch_legs`
    // / `FlowParams::min_dispatch_kw`) — this threshold is only ever hit there for a leg sitting
    // EXACTLY at the floor (pinned `Some(true)`, no slack above `min_dispatch_kw − DISPATCH_TOL`),
    // which must NOT be demoted. It remains the real GUARD for the two cases the LP can't rule
    // out: a `Relaxed` fallback plan (the pinned re-solve itself failed, so nothing enforced the
    // floor) and a block the SoC guard left free (`round_dispatch_legs`' `None`) — either can
    // still land below the floor, and demoting it to `regular` tracks the plan far closer than
    // actuating ~8× the planned energy at a price only the smaller amount justified.
    let threshold = EPS.max(min_dispatch_kw - crate::optimize::unified::DISPATCH_TOL);
    let BlockFlows {
        charge_kw,
        discharge_kw,
        batt_grid_charge_kw,
        batt_to_grid_kw,
        grid_import_kw,
        grid_export_kw,
        soc_kwh,
        inverter_on,
    } = *f;
    if !inverter_on {
        "inverter_off"
    } else if batt_grid_charge_kw >= threshold {
        "charge_from_grid"
    } else if batt_to_grid_kw >= threshold {
        "discharge_to_grid"
    } else if charge_kw > EPS || discharge_kw > EPS {
        // Battery active without grid involvement (solar-charging / covering the load) — loxone
        // reports this as `regular`; the spill of a little surplus to grid is incidental, not a sale.
        "regular"
    } else if grid_export_kw > EPS && soc_kwh < max_soc_kwh - 0.2 {
        // Exporting surplus although the battery has room to store it — loxone forces the inverter to
        // grid-first here (otherwise load_first would quietly charge), which it reports as
        // `sell_production`. With a full battery the surplus exports passively in `regular` (below).
        "sell_production"
    } else if grid_import_kw > EPS && soc_kwh > min_soc_kwh + 0.2 {
        "battery_hold" // importing while the battery is available — held for a pricier block
    } else {
        "regular" // self-consumption / idle (incl. passive surplus export with a full battery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_mode_uses_loxone_vocabulary() {
        // Args: charge, discharge, batt_grid_charge, batt_to_grid, grid_import(total),
        // grid_export(total), soc_kwh, inverter; min_soc=2, max_soc=10.
        let m = |c, d, bgc, btg, gi, ge, soc, inv| {
            classify_mode(
                &BlockFlows {
                    charge_kw: c,
                    discharge_kw: d,
                    batt_grid_charge_kw: bgc,
                    batt_to_grid_kw: btg,
                    grid_import_kw: gi,
                    grid_export_kw: ge,
                    soc_kwh: soc,
                    inverter_on: inv,
                },
                2.0,
                10.0,
                0.0,
            )
        };
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, false), "inverter_off"); // paused
                                                                                 // A grid dispatch below the ACTUATOR's floor is demoted to regular: the controller would
                                                                                 // round it up to ~2.45 kW, actuating up to ~8x the planned energy.
        let floor = |bgc, btg| {
            classify_mode(
                &BlockFlows {
                    charge_kw: bgc,
                    discharge_kw: btg,
                    batt_grid_charge_kw: bgc,
                    batt_to_grid_kw: btg,
                    grid_import_kw: bgc,
                    grid_export_kw: btg,
                    soc_kwh: 5.0,
                    inverter_on: true,
                },
                2.0,
                10.0,
                2.45,
            )
        };
        assert_eq!(floor(0.3, 0.0), "regular", "sub-floor grid charge demoted");
        assert_eq!(
            floor(0.0, 0.3),
            "regular",
            "sub-floor grid discharge demoted"
        );
        assert_eq!(
            floor(3.0, 0.0),
            "charge_from_grid",
            "above the floor is untouched"
        );
        // A leg sitting EXACTLY at the floor (how a pinned `Rounded` block actually lands — see
        // `round_dispatch_legs`) must NOT be demoted: `>=`, not `>`.
        assert_eq!(
            floor(2.45, 0.0),
            "charge_from_grid",
            "exactly at the floor is not demoted"
        );
        assert_eq!(
            floor(0.0, 2.45),
            "discharge_to_grid",
            "exactly at the floor is not demoted"
        );
        assert_eq!(
            m(2.0, 0.0, 2.0, 0.0, 2.0, 0.0, 5.0, true),
            "charge_from_grid"
        ); // AC-charging
        assert_eq!(
            m(0.0, 2.0, 0.0, 2.0, 0.0, 2.0, 5.0, true),
            "discharge_to_grid"
        ); // battery → grid
        assert_eq!(
            m(0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 5.0, true),
            "sell_production"
        ); // export though battery has room
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 10.0, true), "regular"); // full battery — passive export
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 5.0, true), "battery_hold"); // importing, battery saved
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 2.0, true), "regular"); // importing at the SoC floor
        assert_eq!(m(2.0, 0.0, 0.0, 0.0, 0.0, 1.0, 5.0, true), "regular"); // solar charge + tiny spill
        assert_eq!(m(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, true), "regular"); // self-consume / idle

        // The regression this split exists for: EV grid draw during a SOLAR battery charge is not
        // charge_from_grid (the inverter must not force AC charge)...
        assert_eq!(m(2.0, 0.0, 0.0, 0.0, 7.0, 0.0, 5.0, true), "regular");
        // ...and battery→EV during solar export is not discharge_to_grid (no battery drain to grid).
        assert_eq!(m(0.0, 2.0, 0.0, 0.0, 0.0, 3.0, 5.0, true), "regular");
    }
}
