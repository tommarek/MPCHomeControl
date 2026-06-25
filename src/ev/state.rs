//! Multi-source fusion of an EV charger's live state.
//!
//! Each charger maps **roles** to data sources (`config.json5`'s `sources` block). Recognised roles:
//! - `power` — the loxone wallbox charge power (kW after `scale`); the authoritative "on our charger".
//! - `on_charger` — an optional wallbox boolean (`ev_charging`, 1/0).
//! - `soc` — the car's state of charge (%, TeslaMate `battery_level`).
//! - `target` — the car's own charge limit (%, TeslaMate `charge_limit_soc`).
//! - `capacity` — the car's usable battery capacity (kWh).
//! - `tesla_power` — the car-side charge power (TeslaMate `charger_power`), to detect charging *away*.
//!
//! Fusion rule (the linchpin): the **wallbox** decides `on_our_charger` / `controllable_now`;
//! TeslaMate supplies SoC / target / capacity, used only while the car is on our charger. A car that
//! TeslaMate shows charging while our wallbox is idle is `charging_elsewhere` — observed, not modelled.

use serde::Serialize;

use crate::optimize::config::{EvChargerConfig, EvControl};
use crate::source::{SourceClients, SourceLocator};

/// Wallbox **power** is continuous, so a fresh reading is recent — older than this (minutes) means the
/// car isn't actively drawing (charging paused/done), not that it's gone.
const POWER_FRESH_MIN: i64 = 15;
/// The wallbox **connected** flag is written *on change* (plug / unplug), so it must persist far longer
/// than a power sample: a car can sit plugged-in (idle, or while the MPC pauses charging) for hours
/// with no new event. Reading it on the short power window would age a connected car out to "away" and
/// the MPC would stop scheduling it — a feedback bug. This window spans an overnight charge; the last
/// value is the true state (unplug writes a `0`), so a generous bound is safe.
const CONNECTED_FRESH_MIN: i64 = 720;
/// Car-side signals (SoC, target) may be older — TeslaMate sleeps. Newer than this (minutes) is used.
const CAR_FRESH_MIN: i64 = 180;
/// Wallbox power (kW) above which a car is treated as actively drawing on our charger.
pub(crate) const ON_CHARGER_KW: f64 = 0.1;

/// The fused live state of one EV charger, for the optimizer inputs and the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct EvState {
    pub name: String,
    pub control: EvControl,
    /// A car is plugged into **our** wallbox (authoritative: the loxone `ev` signal).
    pub on_our_charger: bool,
    /// The MPC may schedule charging now (`on_our_charger` and not a `monitored` charger).
    pub controllable_now: bool,
    /// TeslaMate shows the car charging, but not on our wallbox (a supercharger, work, …).
    pub charging_elsewhere: bool,
    /// Car state of charge (%), if known (TeslaMate). `None` ⇒ unknown (no SoC source / stale).
    pub soc_pct: Option<f64>,
    /// Effective target SoC (%): live preference > the car's own charge limit > config default.
    pub target_pct: f64,
    /// Car battery usable capacity (kWh).
    pub capacity_kwh: f64,
    /// Charge power our wallbox is currently delivering (kW).
    pub charger_power_kw: f64,
    /// Energy still needed to reach the target (kWh), if SoC is known.
    pub energy_needed_kwh: Option<f64>,
    /// Which car is on our wallbox, when more than one shares it; `None` for a single-car charger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_car: Option<String>,
}

impl EvState {
    /// A short status for the dashboard badge.
    pub fn status(&self) -> &'static str {
        if self.on_our_charger {
            if self.charger_power_kw > ON_CHARGER_KW {
                "charging"
            } else {
                "connected"
            }
        } else if self.charging_elsewhere {
            "charging_away"
        } else {
            "away"
        }
    }
}

/// Energy (kWh) to bring the car from `soc_pct` to `target_pct` at `capacity_kwh`, clamped at 0 (a
/// car already at/above its target needs nothing). This is what the optimizer schedules toward.
fn energy_to_target(soc_pct: f64, target_pct: f64, capacity_kwh: f64) -> f64 {
    ((target_pct - soc_pct) / 100.0 * capacity_kwh).max(0.0)
}

/// Resolve `(soc, car_target, capacity_kwh, active_car)` for the charger. With a shared wallbox
/// (`charger.cars` non-empty) it returns the values of the first car whose `present` signal is set —
/// so a two-car house never plans against the wrong car's SoC; with no `present` car, SoC/target are
/// unknown. A single-car charger (empty `cars`) reads them from the charger-level `sources` as before.
async fn select_car(
    sources: &SourceClients,
    charger: &EvChargerConfig,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<String>) {
    if charger.cars.is_empty() {
        let s = &charger.sources;
        return (
            read_role(sources, s.get("soc"), CAR_FRESH_MIN).await,
            read_role(sources, s.get("target"), CAR_FRESH_MIN).await,
            read_role(sources, s.get("capacity"), CAR_FRESH_MIN).await,
            None,
        );
    }
    for car in &charger.cars {
        let present = sources
            .read_locator(&car.present, CONNECTED_FRESH_MIN)
            .await;
        if present.is_some_and(|p| p >= 0.5) {
            let soc = sources.read_locator(&car.soc, CAR_FRESH_MIN).await;
            let target = match &car.target {
                Some(t) => sources.read_locator(t, CAR_FRESH_MIN).await,
                None => None,
            };
            // Prefer a capacity *source* (e.g. TeslaMate-derived), then the static config value.
            let capacity = match &car.capacity {
                Some(c) => sources.read_locator(c, CAR_FRESH_MIN).await,
                None => None,
            }
            .or(car.capacity_kwh);
            return (soc, target, capacity, Some(car.name.clone()));
        }
    }
    (None, None, None, None)
}

/// Latest value of an optional source, within its freshness window.
async fn read_role(
    sources: &SourceClients,
    source: Option<&SourceLocator>,
    max_age_min: i64,
) -> Option<f64> {
    match source {
        Some(s) => sources.read_locator(s, max_age_min).await,
        None => None,
    }
}

/// Fuse a charger's configured sources into its live [`EvState`]. `target_override` is the live
/// dashboard preference (highest precedence). Best-effort: missing/stale sources degrade gracefully.
/// The sources come from the pluggable [`SourceClients`] registry — Influx, Postgres, HTTP — so the
/// fusion is agnostic to where each signal lives.
pub async fn fuse_charger(
    sources: &SourceClients,
    charger: &EvChargerConfig,
    target_override: Option<f64>,
) -> EvState {
    let s = &charger.sources;
    // Wallbox roles are car-agnostic (the physical charger).
    let power = read_role(sources, s.get("power"), POWER_FRESH_MIN).await;
    let on_flag = read_role(sources, s.get("on_charger"), CONNECTED_FRESH_MIN).await;
    let tesla_power = read_role(sources, s.get("tesla_power"), POWER_FRESH_MIN).await;
    // Per-car SoC / target / capacity: whichever car is present on a shared wallbox, else the single car.
    let (soc, car_target, capacity, active_car) = select_car(sources, charger).await;

    let charger_power_kw = power.unwrap_or(0.0).max(0.0);
    let on_our_charger = charger_power_kw > ON_CHARGER_KW || on_flag.is_some_and(|f| f >= 0.5);
    let controllable_now = on_our_charger && charger.control != EvControl::Monitored;
    let charging_elsewhere = !on_our_charger && tesla_power.is_some_and(|p| p > ON_CHARGER_KW);

    let target_pct = target_override
        .or(car_target)
        .unwrap_or(charger.target_pct)
        .clamp(0.0, 100.0);
    let capacity_kwh = capacity.filter(|c| *c > 0.0).unwrap_or(charger.battery_kwh);
    let soc_pct = soc.map(|v| v.clamp(0.0, 100.0));
    let energy_needed_kwh = soc_pct.map(|s| energy_to_target(s, target_pct, capacity_kwh));

    EvState {
        name: charger.name.clone(),
        control: charger.control,
        on_our_charger,
        controllable_now,
        charging_elsewhere,
        soc_pct,
        target_pct,
        capacity_kwh,
        charger_power_kw,
        energy_needed_kwh,
        active_car,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Fusion logic that doesn't touch InfluxDB is exercised here by reconstructing EvState directly;
    // the InfluxDB reads are covered by the live dashboard/controller smoke tests.

    #[test]
    fn status_labels_cover_the_states() {
        let mut st = EvState {
            name: "garage".into(),
            control: EvControl::Modulating,
            on_our_charger: true,
            controllable_now: true,
            charging_elsewhere: false,
            soc_pct: Some(50.0),
            target_pct: 80.0,
            capacity_kwh: 60.0,
            charger_power_kw: 7.0,
            energy_needed_kwh: Some(18.0),
            active_car: None,
        };
        // Every status the backend can produce must have a matching dashboard `EV_BADGE` entry, or a
        // charger renders a blank badge. app.js is the same file the server embeds; assert coverage
        // here (driven by `status()`'s own outputs, so a newly-added status can't silently drift).
        let app_js = include_str!("../dashboard/app.js");
        let check = |st: &EvState, expect: &str| {
            assert_eq!(st.status(), expect);
            assert!(
                app_js.contains(&format!("{expect}:")),
                "src/dashboard/app.js EV_BADGE is missing a `{expect}` entry"
            );
        };
        check(&st, "charging");
        st.charger_power_kw = 0.0;
        check(&st, "connected");
        st.on_our_charger = false;
        st.controllable_now = false;
        st.charging_elsewhere = true;
        check(&st, "charging_away");
        st.charging_elsewhere = false;
        check(&st, "away");
    }

    #[test]
    fn energy_to_target_covers_the_real_scenario() {
        // SoC gap × capacity = energy to the target: (60 − 44) / 100 × 75 = 12 kWh.
        assert!((super::energy_to_target(44.0, 60.0, 75.0) - 12.0).abs() < 1e-9);
        // Already at/above target ⇒ nothing needed (no negative energy).
        assert_eq!(super::energy_to_target(80.0, 60.0, 75.0), 0.0);
        assert_eq!(super::energy_to_target(60.0, 60.0, 75.0), 0.0);
    }
}
