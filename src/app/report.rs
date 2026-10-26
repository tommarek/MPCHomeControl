//! The serializable output types of the state and plan reports (`/api/state`, `/api/plan`, the
//! timeline the publisher reads) and the plan's per-room pull break-even indicator.

use std::collections::HashMap;
use std::time::Instant;

use super::mode::ModeStep;
use super::pv::PvNowcast;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Per pulled zone `z` (`comfort.pull_eur_per_kh × priority > 0`), an APPROXIMATE indicator (not an
/// exact indifference price) of the import price (EUR/kWh electricity) below which the LP spends
/// extra kWh heating `z` for the pull: `Σ_z' w_z' · G_z'z · cop`, where `w_z' = pull_eur_per_kh ×
/// priority_z'` and `G_z'z` is `z`'s heater's horizon-summed Kelvin·hour effect on zone `z'` (`Σ`
/// over `kernels[(z', z)]`'s fine lags — see [`crate::optimize::thermal::ThermalContext::kernels`]'s
/// doc; the fine-step Δ cancels against the kWh normalization, so no `dt` factor is needed here).
/// Summing over EVERY pulled zone `z'`, not just `z` itself, is deliberate: heating `z` warms its
/// thermally-coupled neighbours too, and a neighbour's own pull adds to what the LP is willing to
/// pay for `z`'s heat — e.g. heating `ground_hall` partly warms `livingroom`/`kitchen`, so its
/// break-even is higher than its own target price alone would suggest. (A readout for the zones
/// BELOW their target: the pull is one-sided, so a zone already at target has no marginal pull.)
///
/// Uses whichever `kernels` the caller passes — the PRUNED, `coupling_min_k`-filtered map the LP
/// itself solved against where that's what's at hand (a cross-zone pair below the threshold is
/// reported as contributing nothing, matching what the LP actually priced), or the full map when
/// available. Empty when no zone carries a pull.
///
/// Caveat (kernel over-response, see `docs/configuration.md`): `G` is the MODEL's Kelvin·hour
/// response — if the model over-responds relative to the real house, this break-even is
/// proportionally too low for a given real Kelvin·hour, i.e. the owner pays more per REAL K·h
/// than `w` nominally sets. Re-read this once the kernel gain is corrected.
pub(crate) fn pull_break_even_eur_per_kwh(
    heating: &crate::optimize::config::HeatingConfig,
    kernels: &HashMap<(String, String), Vec<f64>>,
) -> HashMap<String, f64> {
    let comfort = &heating.comfort;
    let priority: Vec<(&String, f64)> = heating
        .zones
        .keys()
        .map(|z| (z, comfort.pull_eur_per_kh * comfort.priority(z)))
        .filter(|(_, w)| *w > 0.0)
        .collect();
    priority
        .iter()
        .map(|(z, _)| {
            let be = priority
                .iter()
                .map(|(zt, w)| {
                    let g: f64 = kernels
                        .get(&((*zt).clone(), (*z).clone()))
                        .map(|k| k.iter().sum())
                        .unwrap_or(0.0);
                    w * g
                })
                .sum::<f64>()
                * heating.cop;
            ((*z).clone(), be)
        })
        .collect()
}

/// The current air temperature of one zone — the model estimate re-anchored to the latest measured
/// reading (see [`super::current_state`]), so it tracks reality including unmodelled disturbances.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneTemp {
    pub zone: String,
    pub temp_c: f64,
}

/// The current per-zone thermal state (measured-anchored).
#[derive(Debug, Clone, Serialize)]
pub struct StateReport {
    pub zones: Vec<ZoneTemp>,
    /// The disturbance observer's per-zone constant flux (W, + heats); present only when
    /// `estimator.disturbance` is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disturbance_w: Option<HashMap<String, f64>>,
}

/// One zone's recent **measured** temperature history, for the dashboard's Indoor comfort sparklines.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneSeries {
    pub zone: String,
    /// `(iso8601, °C)` samples, oldest first.
    pub series: Vec<(String, f64)>,
}

/// The whole-house dispatch plan over the horizon, plus the PV self-correction that fed it.
#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub horizon_hours: usize,
    /// Grid cash only — the comfort pull cost (`pull_cost_eur`) is NOT part of this figure.
    pub total_cost_eur: f64,
    /// The same horizon cost converted to CZK (via the tariff's exchange rate) for local reporting.
    pub total_cost_czk: f64,
    /// The CZK/EUR rate this plan was priced with (`tariff.eur_czk_rate`). Reported so a client
    /// converts with the SAME rate the optimizer used, instead of dividing the two cost totals —
    /// which is undefined on a near-zero-cost horizon.
    pub eur_czk_rate: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    /// PV energy curtailed over the horizon (kWh) — solar neither used, stored, nor exported.
    pub pv_curtailed_kwh: f64,
    pub heating_kwh: f64,
    /// HVAC cooling energy delivered over the horizon (kWh) — 0 when no HVAC is configured.
    pub cooling_kwh: f64,
    /// HVAC air-side heating energy delivered over the horizon (kWh) — 0 when no HVAC is configured.
    pub hvac_heating_kwh: f64,
    pub battery_charge_kwh: f64,
    pub battery_discharge_kwh: f64,
    /// Implied battery wear cost over the horizon (CZK) = discharged kWh × amortisation.
    pub battery_wear_czk: f64,
    pub final_soc_kwh: f64,
    pub pv_raw_kwh: f64,
    pub pv_calibrated_kwh: f64,
    pub pv_calibration_scale: f64,
    /// The PV intraday nowcast's outcome this cycle — always present (`applied: false` + `reason`
    /// when skipped). See [`PvNowcast`].
    pub pv_nowcast: PvNowcast,
    /// Which **data feeds** fell back to placeholders this cycle (empty = all data feeds live).
    /// PV-array and battery hardware specs come from `config.json5`; a "PV (Solcast unavailable…)"
    /// entry here means the clear-sky model over those arrays stood in for the Solcast forecast.
    pub placeholder_inputs: Vec<String>,
    /// `true` when a **safety-critical** input fell back (fictional thermal seed, or no outside
    /// temperature at all): the plan is still served for inspection, but the publisher refuses to
    /// actuate it — letting the controllers' deadman revert to their failsafe is safer than
    /// heating decisions computed from a made-up house state.
    pub degraded: bool,
    /// `true` when this plan came from the binary-RELAXED fallback LP (the fix-and-round
    /// re-solve itself failed). Its relays/on-off decisions may be fractional; the publisher's
    /// threshold would round them up to full power and the loop's latch would then pin that
    /// rounding into the next strict solves — so the publisher skips actuation for relaxed plans,
    /// and the loop neither latches nor snapshots from them.
    pub relaxed: bool,
    /// `true` when the strict MILP stalled and this plan is the FIX-AND-ROUND result: relaxed LP
    /// → deterministic rounding → fully-pinned re-solve. Integral and self-consistent, so it is
    /// actuated/latched/snapshotted like a strict plan — the flag is transparency only.
    pub rounded: bool,
    /// The controls the optimizer chose for the coming block — the battery plan drives the **armed**
    /// Growatt controller and the heating decisions the **armed** loxone controller (downstream).
    pub first_step: FirstStep,
    /// The full per-15-min-block plan as **timestamped rows** — prices, PV, SoC, battery, grid,
    /// heating and predicted temperature per controlled zone, plus the recommended Growatt mode.
    /// Chart-ready (one object per block) and the source for `/api/plan/timeline`.
    pub timeline: Vec<TimelineBlock>,
    /// **Block 1** (the NEXT block) with its start instant `t` — on-the-mark switching ("switch
    /// exactly on the quarter-hour marks"): the publisher applies this exact block as the next
    /// command at `apply_at = t`, and the dashboard can show "next block: …" without indexing
    /// `timeline` itself. `None` when the plan has fewer than 2 blocks. From `mark − 120 s` the
    /// loop overrides its `heat_kw`/`cool_kw`/`hvac_heat_kw` to the value FROZEN at the first tick
    /// inside that window and sets `frozen: true` (the pre-mark freeze) — the publisher emits a
    /// next command ONLY then, so what it promotes always matches what the loop itself latches at
    /// rollover.
    pub next_step: Option<TimelineBlock>,
    /// Per-EV-charger live state + the optimizer's charge schedule. Empty when no charger is
    /// configured; the source for `/api/ev` and the dashboard EV screen.
    #[serde(default)]
    pub ev: Vec<EvChargerPlan>,
    /// The NEXT SOLAR DAY's PV surplus (today's remaining daylight before local noon, tomorrow's
    /// from noon on — see `next_solar_day_mask`) over the house load under the **p10** (conservatively low) Solcast
    /// percentile (kWh); `None` until the forecast writer stores the p10 curve.
    #[serde(default)]
    pub p10_surplus_kwh: Option<f64>,
    /// The part of `p10_surplus_kwh` the battery cannot absorb (kWh) — energy at risk of
    /// curtailment even under the conservative forecast. `None` when p10 is unavailable.
    #[serde(default)]
    pub curtailment_risk_kwh: Option<f64>,
    /// The Kalman disturbance observer's per-zone constant flux (W, + heats) as folded into this
    /// plan's forecast (see `ForecastContext.internal_gain_w`); empty when the observer didn't run
    /// (`estimator.mode` is `anchor`, `estimator.disturbance` is off, or the filter degenerated to
    /// open-loop with no updates applied).
    #[serde(default)]
    pub disturbance_w: HashMap<String, f64>,
    /// The terminal slab-heat credit ACTUALLY applied per zone this solve (EUR per kWh thermal) —
    /// the displaced future-heating price from the outlook when one was available, else the flat
    /// median-based value (see `optimize::coordinator::displaced_price_by_zone`,
    /// `optimize::unified::FlowParams::terminal_heat_value_by_zone`). Empty when no zone got a
    /// positive credit (no heating demand this cycle).
    #[serde(default)]
    pub terminal_heat_credit_eur_per_kwh: HashMap<String, f64>,
    /// Count of blocks over the horizon that are PV-dark (forecast `pv_kw <=` the PV-present
    /// threshold) AND not already bound to 0 by the export-off or placeholder-price gate — i.e.
    /// where the always-on PV-dark export gate is the sole reason `batt_to_grid` is forced to 0.
    /// This does NOT mean the ungated LP would have exported in every one of these blocks (it may
    /// have found export unprofitable there anyway) — only that the gate applies.
    #[serde(default)]
    pub export_pv_gated_blocks: usize,
    /// Value (EUR/kWh) of the energy left in the battery at the horizon end, AS ACTUALLY PASSED to
    /// the LP (after the p10 precharge-guard halving, if applied) — see
    /// [`super::terminal::terminal_soc_value_outlook`] and [`super::terminal_soc_value`].
    #[serde(default)]
    pub terminal_soc_value_eur_per_kwh: f64,
    /// Which valuation produced [`Self::terminal_soc_value_eur_per_kwh`]: `"outlook"` (the
    /// post-horizon day-type-median curve) or `"horizon_median"` (the in-horizon-median fallback
    /// for a thin/cold-start price history).
    #[serde(default)]
    pub terminal_soc_value_source: String,
    /// Total within-horizon comfort pull cost (EUR) — see
    /// [`crate::optimize::unified::UnifiedPlan::pull_cost`]. `0` when no zone carries a pull
    /// (`comfort.pull_eur_per_kh × priority`). NOT part of `total_cost_eur`/`total_cost_czk` (grid
    /// cash alone).
    #[serde(default)]
    pub pull_cost_eur: f64,
    /// Kelvin·hours each pulled zone's air sat on the wrong side of its target within the pull band
    /// this horizon (below the heat target; above the cool target of an AC room) — see
    /// [`crate::optimize::unified::UnifiedPlan::pull_kh`]. Empty when no zone carries a pull.
    #[serde(default)]
    pub pull_kh: HashMap<String, f64>,
    /// Planned relay on↔off changes over the horizon, summed over the heated zones — see
    /// [`crate::optimize::unified::UnifiedPlan::relay_switches`]. Additive field.
    #[serde(default)]
    pub relay_switches: f64,
    /// The same, per heated zone.
    #[serde(default)]
    pub relay_switches_by_zone: HashMap<String, f64>,
    /// Each controlled room's heating / cooling mode this plan ran in, with its hysteresis state
    /// ([`crate::optimize::room_mode`]); the loop persists it and `/api/rooms` + `/api/zones`
    /// report it. Empty when no room is controlled.
    #[serde(default)]
    pub room_modes: HashMap<String, crate::optimize::room_mode::RoomModeState>,
    /// Per pulled zone, an approximate indicator (not an exact indifference price) of the import
    /// price (EUR/kWh electricity) below which the LP buys extra heat for the pull —
    /// `Σ_z' w_z' · G_z'z · heating.cop`,
    /// where `G_z'z` is zone z's heater's horizon-summed Kelvin·hour effect on zone z' (see
    /// `pull_break_even_eur_per_kwh`'s doc for the cross-zone-gain derivation and the kernel-gain
    /// caveat). Empty when no zone carries a pull.
    #[serde(default)]
    pub pull_break_even_eur_per_kwh: HashMap<String, f64>,
}

/// One EV charger's live fused state and the plan's charge schedule (per block) with its source
/// breakdown — the brain only reports it; the **armed** loxone controller drives the wallbox downstream.
#[derive(Debug, Clone, Serialize)]
pub struct EvChargerPlan {
    pub name: String,
    /// `charging` | `connected` | `charging_away` | `away` (see [`crate::ev::EvState::status`]).
    pub status: String,
    pub on_our_charger: bool,
    pub controllable_now: bool,
    pub charging_elsewhere: bool,
    /// Car state of charge (%), if a source provides it.
    pub soc_pct: Option<f64>,
    pub target_pct: f64,
    /// The stored preference exceeded the car's own charge limit and was capped to it — the
    /// dashboard's pending-save overlay reconciles on this (a capped answer is final, not
    /// cache lag). See `EvState::target_capped`.
    pub target_capped: bool,
    /// Usable battery capacity (kWh) used for %↔kWh (a `capacity` source, or the `battery_kwh` fallback).
    pub capacity_kwh: f64,
    /// Which car is on the wallbox (multi-car chargers); `None` for a single-car charger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_car: Option<String>,
    pub strategy: crate::optimize::config::EvStrategy,
    /// Live charge power our wallbox is delivering (kW).
    pub charger_power_kw: f64,
    /// Planned charge power (kW) per block, and its solar / grid / battery split.
    pub charge_kw: Vec<f64>,
    pub solar_kw: Vec<f64>,
    pub grid_kw: Vec<f64>,
    pub batt_kw: Vec<f64>,
    /// Energy the plan delivers to the car over the horizon (kWh).
    pub charged_kwh: f64,
    /// Where the charge-by deadline came from: "pref" / "learned" / "config".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_source: Option<String>,
    /// The effective deadline, local `"HH:MM"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_hm: Option<String>,
    /// The same deadline as an absolute instant (see [`crate::ev::state::EvState::deadline_at`]).
    /// Consumers must prefer this over re-resolving `deadline_hm`, which would use THEIR timezone
    /// rather than the site's — the dashboard's "ready by" marker reads exactly this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
}

/// One block of the plan (15 min near-term, 1 h beyond `horizon.fine_hours` — see `dt_minutes`), as
/// a flat timestamped row for charting and to verify the heat model's forward prediction against
/// measured data later. All powers are kW, prices price-units/kWh.
///
/// `Deserialize` (added for the decision ledger's `ledger import --plan`) carries `#[serde(default)]`
/// on the fields a saved plan JSON might predate (`frozen`, `cool_kw`, `hvac_heat_kw`, `curtail_kw`)
/// — the live `/api/plan/latest` envelope always has them, but an older saved snapshot shouldn't fail
/// the whole import over one missing key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineBlock {
    /// Block start instant (UTC).
    pub t: DateTime<Utc>,
    /// This block's duration in minutes — 15 for a fine (near-term) block, 60 for an hourly one
    /// (the multi-rate grid). The publisher derives each command's `valid_until` from it; the
    /// dashboard plots an hourly block four times as wide as a fine one.
    pub dt_minutes: u32,
    pub import_price: f64,
    pub export_price: f64,
    /// This block's price is the PLACEHOLDER curve (unpublished day-ahead tail) — battery
    /// arbitrage is forbidden here and the dashboard hatches the tail.
    pub price_is_placeholder: bool,
    /// Forecast PV generation (kW) — the calibrated Solcast curve, or the clear-sky fallback.
    pub pv_kw: f64,
    /// Forecast base house load (kW) — the consumption model's prediction the optimizer planned
    /// around — the BASE load, excluding heating/EV electricity (LP decision variables). The
    /// dashboard charts it against the measured total `house_kw` with the distinction labeled.
    pub load_kw: f64,
    /// Battery state of charge (kWh) at the end of the block.
    pub soc_kwh: f64,
    pub charge_kw: f64,
    pub discharge_kw: f64,
    pub grid_import_kw: f64,
    pub grid_export_kw: f64,
    /// PV curtailed this block (kW) — solar neither used, stored, nor exported.
    #[serde(default)]
    pub curtail_kw: f64,
    /// Underfloor-heating power (kW) per heated zone.
    pub heat_kw: HashMap<String, f64>,
    /// HVAC cooling power (kW) per HVAC zone.
    #[serde(default)]
    pub cool_kw: HashMap<String, f64>,
    /// HVAC air-side heating power (kW) per HVAC zone.
    #[serde(default)]
    pub hvac_heat_kw: HashMap<String, f64>,
    /// Controllable scheduled-load draw (kW) per load this block (`on · rated_kw`) — the load-shift
    /// schedule. Empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    /// Planned EV charge power (kW) per charger this block — the same schedule reported per-charger
    /// in [`PlanReport::ev`]'s `charge_kw`, folded into the timeline row too so [`Self::frozen`]
    /// can pin it exactly like every other actuation field: `PlanReport::ev` is never frozen, so a
    /// publisher reading the EV setpoint from it could see a later tick's `/next` change the EV
    /// rate inside the freeze window even though the loop's own decision was pinned. Empty when no
    /// EV charger is configured.
    #[serde(default)]
    pub ev_charge_kw: HashMap<String, f64>,
    /// **Predicted** air temperature (°C) per controlled zone at the end of the block.
    pub temp_c: HashMap<String, f64>,
    /// The outside-temperature forecast (°C) the planner used for this block (the block mean of the
    /// optimizer context's fine-lattice `temperature_c`). Additive: absent in plans saved before it
    /// existed (`None`, omitted from the JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_c: Option<f64>,
    /// Recommended Growatt slot mode and the price-gated export / inverter levers — applied by the
    /// Growatt controller for the live block.
    pub slot: String,
    pub export_enabled: bool,
    pub inverter_on: bool,
    /// `true` only on [`PlanReport::next_step`] once the loop's
    /// pre-mark freeze window has pinned its `heat_kw`/`cool_kw`/`hvac_heat_kw` to the value decided
    /// by the FIRST tick inside that window (`mpc_loop`'s `committed_next`) — never on an ordinary
    /// `timeline` row, which always reports the tick's own fresh LP output. The publisher emits a NEXT
    /// command ONLY when this is `true`, so what a controller applies at the mark always equals what
    /// the brain itself latches at rollover (see `mpc_loop::freeze_committed_next`).
    #[serde(default)]
    pub frozen: bool,
}

/// The live internal-gain self-correction, published by the MPC loop after each re-fit so the
/// `/api/calibration/gains` endpoint can report what the model is currently assuming and when it
/// was learnt (see [`crate::validate::fit_internal_gains`]).
#[derive(Debug, Clone, Serialize)]
pub struct GainsSnapshot {
    /// When this fit landed (UTC).
    pub fitted_at: DateTime<Utc>,
    /// Trailing window (days) the fit was run over.
    pub window_days: i64,
    /// The fitted per-zone internal-gain profiles (W per daypart) now in use by the plan.
    pub gains_w: HashMap<String, crate::optimize::config::GainProfile>,
    /// Per scheduled-load magnitude (W) now in use, aligned to `config.scheduled_loads` — each tagged
    /// `configured` (`power_w` set), `fitted` (learnt from data), or `measured` (driven by a `sensor`).
    pub scheduled: Vec<ScheduledFit>,
}

/// One scheduled load's magnitude as the plan currently sees it, for `/api/calibration/gains`.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledFit {
    /// The load's label, or its zone when the label is empty.
    pub label: String,
    /// The zone whose air node the load acts on.
    pub zone: String,
    /// The magnitude in use (W, ≥ 0); the sign comes from the load's `kind`. For a `"measured"` load
    /// this is the configured **forecast** magnitude (`power_w`, or 0) — the live flux tracks the sensor.
    pub magnitude_w: f64,
    /// `"measured"` if a `sensor` drives the flux from the real draw, else `"configured"` if `power_w`
    /// was set, else `"fitted"` (learnt from data).
    pub source: String,
}

/// A plan with the instant it was computed — what the MPC loop publishes for the API.
#[derive(Debug, Clone, Serialize)]
pub struct TimestampedPlan {
    pub computed_at: DateTime<Utc>,
    /// Monotonic instant the plan was published, for clock-jump-proof freshness checks (`/readyz`):
    /// a wall-clock step (NTP) mustn't make a fresh plan look stale. Skipped in serialization —
    /// `Instant` isn't serializable, and the wall-clock `computed_at` is what the API exposes.
    #[serde(skip)]
    pub published: Instant,
    /// [`crate::web::SharedConfig::fingerprint`] of the config snapshot this plan was solved with.
    #[serde(skip)]
    pub config_fingerprint: String,
    pub plan: PlanReport,
}

/// The first-step (next 15-min block) decisions extracted from the plan.
#[derive(Debug, Clone, Serialize)]
pub struct FirstStep {
    /// Start instant of the first block (UTC).
    pub hour_start: DateTime<Utc>,
    /// Underfloor-heating power (kW) per heated zone for the coming block.
    pub heat_kw: HashMap<String, f64>,
    /// HVAC cooling power (kW) per HVAC zone for the coming block.
    pub cool_kw: HashMap<String, f64>,
    /// HVAC air-side heating power (kW) per HVAC zone for the coming block.
    pub hvac_heat_kw: HashMap<String, f64>,
    /// Controllable scheduled-load draw (kW) per load for the coming block (`on · rated_kw`, 0 when
    /// off) — the boiler controller's setpoint. Empty when no controllable load is configured.
    #[serde(default)]
    pub controllable_load_kw: HashMap<String, f64>,
    pub battery_charge_kw: f64,
    pub battery_discharge_kw: f64,
    pub grid_import_kw: f64,
    pub grid_export_kw: f64,
    /// Recommended Growatt setup for the coming block — slot mode + the two toggles, applied by the
    /// Growatt controller.
    pub mode: ModeStep,
}

#[cfg(test)]
mod tests {

    #[test]
    fn pull_break_even_sums_cross_zone_gains() {
        use super::pull_break_even_eur_per_kwh;
        use crate::optimize::config::{
            BandSpec, ComfortConfig, HeatingConfig, HeatingZone, RoomComfort,
        };
        use std::collections::HashMap;

        let zone = || HeatingZone {
            max_heat_kw: 1.0,
            internal_gain_w: 0.0,
        };
        let room = |priority: f64| RoomComfort {
            heat: BandSpec {
                target: 21.0,
                delta: Some(1.0),
                below: None,
                above: None,
            },
            cool: None,
            priority,
            windows: Vec::new(),
        };
        // pull 0.1 EUR/K·h: p carries priority 1.0 (w = 0.1), q priority 2.0 (w = 0.2), r priority 0.
        let heating = HeatingConfig {
            cop: 2.0,
            zones: HashMap::from([
                ("p".to_string(), zone()),
                ("q".to_string(), zone()),
                ("r".to_string(), zone()), // priority 0: excluded from the map
            ]),
            comfort: ComfortConfig {
                pull_eur_per_kh: 0.1,
                mode_switch_hours: 24.0,
                rooms: HashMap::from([
                    ("p".to_string(), room(1.0)),
                    ("q".to_string(), room(2.0)),
                    ("r".to_string(), room(0.0)),
                ]),
                ..ComfortConfig::default()
            },
            gain_groups: Vec::new(),
            extra_gain_zones: Vec::new(),
            coupling_min_k: 0.0,
            relay_switch_cost_eur: 0.0,
        };
        // A hand-built 2-zone kernel map: self + cross terms for "p"/"q", plus a "r" entry that
        // must never be read (r carries no pull).
        let kernels: HashMap<(String, String), Vec<f64>> = HashMap::from([
            (("p".to_string(), "p".to_string()), vec![1.0, 0.5]), // G_pp = 1.5
            (("q".to_string(), "p".to_string()), vec![0.2, 0.1]), // G_qp = 0.3
            (("p".to_string(), "q".to_string()), vec![0.1, 0.05]), // G_pq = 0.15
            (("q".to_string(), "q".to_string()), vec![2.0, 1.0]), // G_qq = 3.0
            (("r".to_string(), "r".to_string()), vec![99.0]),
        ]);

        let be = pull_break_even_eur_per_kwh(&heating, &kernels);
        assert_eq!(be.len(), 2);
        // break_even[p] = cop * (w_p * G_pp + w_q * G_qp) = 2*(0.1*1.5 + 0.2*0.3) = 0.42
        assert!((be["p"] - 0.42).abs() < 1e-9, "{:?}", be);
        // break_even[q] = cop * (w_p * G_pq + w_q * G_qq) = 2*(0.1*0.15 + 0.2*3.0) = 1.23
        assert!((be["q"] - 1.23).abs() < 1e-9, "{:?}", be);
        assert!(!be.contains_key("r"));

        // No pull ⇒ empty map.
        let mut no_pull = heating;
        no_pull.comfort.pull_eur_per_kh = 0.0;
        assert!(pull_break_even_eur_per_kwh(&no_pull, &kernels).is_empty());
    }
}
