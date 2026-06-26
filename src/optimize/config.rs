//! Control configuration: site location and per-zone heating / comfort settings.
//!
//! Parsed from the same `config.json5` the InfluxDB layer reads — neither side sets
//! `deny_unknown_fields`, so the `site` and `heating` blocks coexist with `db`/`zone_mappings`
//! and each deserializer ignores the other's keys.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::source::{DataSources, SourceLocator};

/// The control-relevant slice of the configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlConfig {
    pub site: SiteConfig,
    pub heating: HeatingConfig,
    /// Reversible HVAC / air-conditioning: per-zone cooling/air-heating comfort and the equipment
    /// units that serve them. Optional — absent ⇒ no HVAC (the house has none today). See
    /// [`HvacConfig`].
    #[serde(default)]
    pub hvac: Option<HvacConfig>,
    /// Real electricity tariff (distribution fees, sell fee, VT/NT hours, mode thresholds);
    /// optional, real Czech D57d defaults applied if absent. This is the live pricing model.
    #[serde(default)]
    pub tariff: TariffConfig,
    /// Home-battery specification; optional, defaults applied if absent.
    #[serde(default)]
    pub battery: BatteryConfig,
    /// PV array(s) for the clear-sky fallback model; optional.
    #[serde(default)]
    pub pv: PvConfig,
    /// EV chargers (optional; absent ⇒ no EV). Each is a controllable flexible load the optimizer
    /// schedules toward a target SoC by a deadline, fusing the loxone wallbox ("on our charger" +
    /// power) with TeslaMate (the car's SoC / target / location). See [`EvChargerConfig`].
    #[serde(default)]
    pub chargers: Vec<EvChargerConfig>,
    /// Optional per-house overrides for *where* signals are read from (the pluggable data-source
    /// layer). Absent ⇒ the built-in InfluxDB defaults, so the current house needs no config change.
    /// See [`DataSources`] and `docs/data-sources.md`.
    #[serde(default)]
    pub data_sources: DataSources,
    /// Scheduled heat fluxes at a zone's air node — known appliances on a daily/seasonal schedule the
    /// physics model has no source for (e.g. a water heat-pump that cools its room while it runs, or a
    /// fireplace). Only the **direction** (sink/source) and the **schedule** are configured; the live
    /// calibration *learns the magnitude* from data. Empty ⇒ none. See `docs/configuration.md`.
    #[serde(default)]
    pub scheduled_loads: Vec<ScheduledLoad>,
    /// How far back to train the consumption model from measured history (days).
    #[serde(default = "default_consumption_history_days")]
    pub consumption_history_days: i64,
    /// How often the shadow MPC loop re-plans (minutes).
    #[serde(default = "default_mpc_tick_minutes")]
    pub mpc_tick_minutes: u64,
    /// Trailing window (days) the shadow loop re-fits the per-zone internal gains over, so the live
    /// forecast tracks changes in occupant behaviour. A few days smooths sensor noise yet adapts
    /// within a week; the fit falls back to the `heating` config gains when there's not enough data.
    /// Clamped to a minimum of 3 days at use.
    #[serde(default = "default_internal_gain_window_days")]
    pub internal_gain_window_days: i64,
    /// How often (hours) the shadow loop re-fits the internal gains. They drift slowly (behavioural),
    /// so this is far longer than the plan tick — keeping the self-correction's overhead negligible.
    /// 0 disables the live re-fit, pinning the gains to the `heating` config values.
    #[serde(default = "default_internal_gain_recalibrate_hours")]
    pub internal_gain_recalibrate_hours: u64,
    /// How often (minutes) the shadow loop snapshots its forward temperature prediction for the
    /// `/api/forecast/validation` scorecard (predict now, score against measured later). 0 disables
    /// snapshotting. Stored to a JSON file (`MPC_FORECAST_STORE`, default `forecast_snapshots.json`).
    #[serde(default = "default_forecast_snapshot_minutes")]
    pub forecast_snapshot_minutes: u64,
}

/// A scheduled heat flux applied at a zone's air node — an appliance the physics model has no source
/// for, active on configured local-time windows. The magnitude is either **configured** (`power_w`
/// set — a known draw) or **fitted** (`power_w` omitted — the calibration learns it from data); the
/// sign is fixed by [`LoadKind`] either way, so the house always declares *when* and *which way*. The
/// model then applies `magnitude_w × unit_profile` as an extra flux, in both the optimizer prediction
/// and the calibration drive.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledLoad {
    /// Zone whose air node the flux is applied at.
    pub zone: String,
    /// Display label (e.g. "water heat-pump"). Optional.
    #[serde(default)]
    pub label: String,
    /// Direction: a `sink` removes heat (cools the room — e.g. a heat-pump heating a tank from room
    /// air); a `source` adds heat. Fixes the sign of the magnitude.
    pub kind: LoadKind,
    /// Magnitude (W, > 0) when the draw is **known**: the model uses it as-is and the calibration does
    /// not fit it. Omit to have the calibration **learn** it from data (the default). The sign always
    /// comes from `kind` — this is the unsigned watts. When a `sensor` is set this stays the
    /// **forecast** magnitude (the future draw isn't knowable); the historical drive uses the measured
    /// signal instead.
    #[serde(default)]
    pub power_w: Option<f64>,
    /// Optional live signal reading the appliance's **electrical power** (W). When set, the
    /// calibration/backtest drive derives the zone heat flux from this *measured* draw (gated by the
    /// `windows`/`months` schedule) rather than from `power_w` or a fitted magnitude — so it stays
    /// grounded in the real run (robust to away weeks and variable run length). A sensor-driven load is
    /// a **known** input, never a fit candidate. Reuses the read-only [`SourceLocator`] layer (e.g. a
    /// Loxone smart socket's power landing in InfluxDB); the forecast path is unchanged (the future draw
    /// isn't knowable). Empty ⇒ no sensor (the magnitude is `power_w`/fitted as before).
    #[serde(default)]
    pub sensor: Option<SourceLocator>,
    /// The fraction of the measured electrical power that becomes **zone heat** (effective default
    /// `1.0`). A resistive source dissipates all of it (`1.0`); a heat-pump **sink** removes
    /// `P·(COP−1)` from the room (so ≈ `COP − 1`). The flux magnitude per step is
    /// `P_elec × power_factor`, with the sign from `kind`. Used for the air-node heat both with a
    /// `sensor` (scaling the measured draw) and for a `controllable` load (scaling its rated `power_w`).
    #[serde(default)]
    pub power_factor: Option<f64>,
    /// When `true`, the load is **switched by the optimizer** instead of following a fixed schedule:
    /// it is a deferrable electrical load (e.g. a resistive boiler / domestic-hot-water tank) the LP
    /// turns on/off *within its `windows`* to run for `run_hours`, picking the cheapest blocks
    /// (load-shifting). Its `power_w` is then the **rated electrical draw** (required), priced at the
    /// import tariff when on, and its air-node heat (`kind × power_w × power_factor`) couples into the
    /// thermal prediction only in the blocks it runs. Default `false` ⇒ the load is a **passive**
    /// scheduled flux as before (the plan is byte-identical). See `docs/configuration.md`.
    #[serde(default)]
    pub controllable: bool,
    /// For a `controllable` load: the total run time required within the windows (hours, > 0). The
    /// optimizer schedules `run_hours` of on-time at the cheapest blocks in the window (soft — an
    /// infeasible window just runs as much as it can). Required when `controllable`; ignored otherwise.
    #[serde(default)]
    pub run_hours: Option<f64>,
    /// Local-time windows the load is active in. Empty ⇒ never active.
    pub windows: Vec<LoadWindow>,
}

impl ScheduledLoad {
    /// The unsigned air-node heat (kW) when a `controllable` load is **on**, scaled by `power_factor`
    /// (effective `1.0`). The optimizer applies it with the load's sign via the kernel. `power_w` is
    /// the rated electrical draw; a resistive boiler dissipates all of it as heat (`power_factor` 1).
    pub fn controllable_heat_kw(&self) -> f64 {
        self.power_w.unwrap_or(0.0) * self.power_factor.unwrap_or(1.0) / 1000.0
    }
}

/// The direction of a [`ScheduledLoad`] — the sign of its (fitted, non-negative) magnitude.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadKind {
    /// Removes heat from the zone (negative flux).
    Sink,
    /// Adds heat to the zone (positive flux).
    Source,
}

/// One active window of a [`ScheduledLoad`], in site-local civil time.
#[derive(Debug, Clone, Deserialize)]
pub struct LoadWindow {
    /// Months (1-12) the window applies to. Empty ⇒ every month.
    #[serde(default)]
    pub months: Vec<u32>,
    /// Local start time, `"HH:MM"` (inclusive).
    pub start: String,
    /// Local end time, `"HH:MM"` (exclusive). An end ≤ start wraps past midnight (e.g. `22:00`→`06:00`).
    pub end: String,
}

impl ScheduledLoad {
    /// The **unit** signed profile at a site-local instant: `-1.0` for an active sink, `+1.0` for an
    /// active source, `0.0` when no window is active. The calibration scales this by the fitted
    /// magnitude (W, ≥ 0); the model applies `magnitude × unit_profile` as the flux.
    pub fn unit_profile(&self, month: u32, minute_of_day: u32) -> f64 {
        if self
            .windows
            .iter()
            .any(|w| w.contains(month, minute_of_day))
        {
            match self.kind {
                LoadKind::Sink => -1.0,
                LoadKind::Source => 1.0,
            }
        } else {
            0.0
        }
    }
}

impl LoadWindow {
    /// Whether this window is active at the given month (1-12) and minute-of-day (0-1439), local.
    pub fn contains(&self, month: u32, minute_of_day: u32) -> bool {
        if !self.months.is_empty() && !self.months.contains(&month) {
            return false;
        }
        match (parse_hm(&self.start), parse_hm(&self.end)) {
            (Some(s), Some(e)) if s < e => (s..e).contains(&minute_of_day), // same-day window
            (Some(s), Some(e)) => minute_of_day >= s || minute_of_day < e,  // wraps past midnight
            _ => false, // unparseable ⇒ inactive (rejected at load by validate_scheduled_loads)
        }
    }
}

/// Parse `"HH:MM"` to a minute-of-day (0-1439); `None` if malformed or out of range.
fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Home-battery specification (mirrors the loxone Growatt settings).
#[derive(Debug, Clone, Deserialize)]
pub struct BatteryConfig {
    pub capacity_kwh: f64,
    /// Minimum usable state of charge (%); the optimizer won't discharge below this.
    pub min_soc_pct: f64,
    /// Maximum charge / discharge power (kW).
    pub charge_kw: f64,
    pub discharge_kw: f64,
    /// Round-trip efficiency (0..1); split evenly across charge and discharge.
    pub round_trip_efficiency: f64,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            capacity_kwh: 10.0,
            min_soc_pct: 20.0,
            charge_kw: 5.3,
            discharge_kw: 5.3,
            round_trip_efficiency: 0.85,
        }
    }
}

impl BatteryConfig {
    /// Reject non-physical battery values at config load. The capacity, powers and efficiency feed the
    /// LP bounds and the √η SoC split directly, so a NaN/negative would silently corrupt the dispatch
    /// (or surface as a cryptic late error from `BatterySpec::validate`). Called from
    /// [`ControlConfig::load`]. A range `contains` check also rejects NaN.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.capacity_kwh.is_finite() && self.capacity_kwh > 0.0,
            "battery.capacity_kwh must be finite and > 0 (got {})",
            self.capacity_kwh
        );
        anyhow::ensure!(
            (0.0..=100.0).contains(&self.min_soc_pct),
            "battery.min_soc_pct must be between 0 and 100 (got {})",
            self.min_soc_pct
        );
        anyhow::ensure!(
            self.charge_kw.is_finite() && self.charge_kw >= 0.0,
            "battery.charge_kw must be finite and ≥ 0 (got {})",
            self.charge_kw
        );
        anyhow::ensure!(
            self.discharge_kw.is_finite() && self.discharge_kw >= 0.0,
            "battery.discharge_kw must be finite and ≥ 0 (got {})",
            self.discharge_kw
        );
        anyhow::ensure!(
            self.round_trip_efficiency.is_finite()
                && self.round_trip_efficiency > 0.0
                && self.round_trip_efficiency <= 1.0,
            "battery.round_trip_efficiency must be in (0, 1] (got {})",
            self.round_trip_efficiency
        );
        Ok(())
    }
}

/// PV-array configuration for the clear-sky fallback model (the live plan prefers the Solcast
/// forecast, which already covers all arrays; these are used only when it is unavailable).
#[derive(Debug, Clone, Deserialize)]
pub struct PvConfig {
    /// System efficiency (inverter + wiring losses), 0..1.
    #[serde(default = "default_pv_system_efficiency")]
    pub system_efficiency: f64,
    /// One entry per roof array; empty means "use a single default array".
    #[serde(default)]
    pub arrays: Vec<PvArrayConfig>,
}

// A custom impl (not derived) so a wholly-absent `pv` block keeps the real 0.85 efficiency — a
// derived `Default` would zero it.
impl Default for PvConfig {
    fn default() -> Self {
        Self {
            system_efficiency: default_pv_system_efficiency(),
            arrays: Vec::new(),
        }
    }
}

impl PvConfig {
    /// Reject non-physical array geometry at config load. `kwp`/`tilt`/`azimuth` feed the solar-position
    /// and plane-of-array irradiance math (which the forecast path `unwrap()`s on the site coordinates),
    /// so a NaN or an out-of-range tilt would corrupt the PV forecast fed to the optimizer. Called from
    /// [`ControlConfig::load`]. (`system_efficiency` is clamped at use, so it needs no gate here.)
    pub fn validate(&self) -> Result<()> {
        for a in &self.arrays {
            anyhow::ensure!(
                a.kwp.is_finite() && a.kwp > 0.0,
                "pv.arrays[{}].kwp must be finite and > 0 (got {})",
                a.name,
                a.kwp
            );
            anyhow::ensure!(
                (0.0..=90.0).contains(&a.tilt),
                "pv.arrays[{}].tilt must be between 0 and 90 degrees (got {})",
                a.name,
                a.tilt
            );
            anyhow::ensure!(
                (0.0..=360.0).contains(&a.azimuth),
                "pv.arrays[{}].azimuth must be between 0 and 360 degrees (got {})",
                a.name,
                a.azimuth
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PvArrayConfig {
    pub name: String,
    /// Peak DC power (kWp).
    pub kwp: f64,
    /// Tilt from horizontal (degrees).
    pub tilt: f64,
    /// Azimuth (degrees, 0 = north, clockwise; 180 = south).
    pub azimuth: f64,
}

fn default_pv_system_efficiency() -> f64 {
    0.85
}

fn default_consumption_history_days() -> i64 {
    30
}
fn default_mpc_tick_minutes() -> u64 {
    60
}
fn default_internal_gain_window_days() -> i64 {
    7
}
fn default_internal_gain_recalibrate_hours() -> u64 {
    24
}
fn default_forecast_snapshot_minutes() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct SiteConfig {
    pub latitude: f64,
    pub longitude: f64,
    /// Fixed offset from UTC to local civil time, in hours (e.g. +2 for central-Europe summer).
    pub utc_offset_hours: i32,
    /// Ground temperature (°C) under the slab — the `ground` boundary condition for the thermal
    /// model. A site/season constant (it varies far slower than air); used by the state estimator,
    /// the plan, and the passive backtest. Optional; defaults to a typical central-European slab.
    #[serde(default = "default_ground_temperature_c")]
    pub ground_temperature_c: f64,
}

fn default_ground_temperature_c() -> f64 {
    16.0
}

/// Real electricity tariff: how the OTE spot price becomes the per-kWh import and export price.
///
/// The OTE day-ahead price is in EUR/MWh; every fee here is in **CZK/kWh** (the form the Czech
/// distribution tariff is quoted in), converted with [`Self::eur_czk_rate`]. Mirrors the
/// `loxone_smart_home` Growatt settings so the shadow plan sees the same economics as the live
/// controller. Import pays distribution + system services (two-tariff VT/NT by local hour); export
/// (FVE buyback) is the spot minus a fixed sell fee, with no distribution.
#[derive(Debug, Clone, Deserialize)]
pub struct TariffConfig {
    /// EUR→CZK exchange rate used to convert the CZK fees against the EUR spot price.
    pub eur_czk_rate: f64,
    /// High-tariff (VT) import distribution + system-services surcharge (CZK/kWh).
    pub distribution_high_czk: f64,
    /// Low-tariff (NT) import distribution + system-services surcharge (CZK/kWh).
    pub distribution_low_czk: f64,
    /// Low-tariff (NT) local-hour ranges, comma-separated `start-end` (end exclusive), e.g.
    /// `"0-10,11-12,13-14,15-17,18-24"` for the Czech D57d tariff. Hours outside any range are VT.
    pub low_tariff_hours: String,
    /// Fixed fee deducted per kWh exported to the grid (CZK/kWh). The FVE buyback is `spot − fee`.
    pub sell_fee_czk: f64,
    /// Strict export floor (CZK/kWh): never export when the spot price is below this, so export
    /// revenue stays ≥ 0. Used to gate the per-hour export recommendation.
    pub export_price_min_czk: f64,
    /// Battery wear cost charged once per kWh discharged (CZK/kWh).
    pub battery_amortisation_czk: f64,
    /// Power the inverter off when the spot price is below this (CZK/kWh): the deeply-negative
    /// hours where exporting would cost money. Drives the shadow inverter-off recommendation.
    pub inverter_off_price_czk: f64,
}

impl Default for TariffConfig {
    fn default() -> Self {
        // The real Czech D57d / Growatt values from loxone_smart_home.
        Self {
            eur_czk_rate: 25.0,
            distribution_high_czk: 0.919,
            distribution_low_czk: 0.281,
            low_tariff_hours: "0-10,11-12,13-14,15-17,18-24".to_string(),
            sell_fee_czk: 0.5,         // loxone live: ≈0.5 CZK/kWh FVE buyback fee
            export_price_min_czk: 0.5, // loxone live: GROWATT_EXPORT_PRICE_MIN=0.5 (= the sell fee)
            battery_amortisation_czk: 1.0, // loxone live: GROWATT_BATTERY_AMORTISATION_CZK=1.0
            inverter_off_price_czk: -2.0,
        }
    }
}

impl TariffConfig {
    /// Reject economically-inverted misconfigurations: a negative battery wear cost would make the LP
    /// objective *reward* discharging (over-cycling the real battery), and a non-positive exchange rate
    /// would invert every CZK conversion. Called from [`ControlConfig::load`].
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.eur_czk_rate.is_finite() && self.eur_czk_rate > 0.0,
            "tariff.eur_czk_rate must be a finite value > 0 (got {})",
            self.eur_czk_rate
        );
        anyhow::ensure!(
            self.battery_amortisation_czk.is_finite() && self.battery_amortisation_czk >= 0.0,
            "tariff.battery_amortisation_czk must be finite and ≥ 0 (a negative wear cost would reward discharging)"
        );
        // Every fee/threshold feeds the per-kWh price arithmetic and the export / inverter-off gates; a
        // NaN or infinity from a config typo would silently corrupt the economics (a NaN comparison is
        // always false, an infinity always true). Require them finite. The distribution surcharges and
        // sell fee are physical costs (≥ 0); the export floor and inverter-off threshold are price
        // points that may legitimately be negative (the inverter-off default is −2.0), so those are
        // only required to be finite.
        for (name, v) in [
            ("distribution_high_czk", self.distribution_high_czk),
            ("distribution_low_czk", self.distribution_low_czk),
            ("sell_fee_czk", self.sell_fee_czk),
        ] {
            anyhow::ensure!(
                v.is_finite() && v >= 0.0,
                "tariff.{name} must be finite and ≥ 0 (got {v})"
            );
        }
        for (name, v) in [
            ("export_price_min_czk", self.export_price_min_czk),
            ("inverter_off_price_czk", self.inverter_off_price_czk),
        ] {
            anyhow::ensure!(v.is_finite(), "tariff.{name} must be finite (got {v})");
        }
        Ok(())
    }

    /// The exchange rate clamped strictly positive, so it is always safe as a divisor.
    fn rate(&self) -> f64 {
        self.eur_czk_rate.max(1e-9)
    }

    /// Per-local-hour low-tariff (NT) mask, parsed from [`Self::low_tariff_hours`]. Unparseable or
    /// out-of-range segments are skipped — those hours stay high (VT), the costlier, safe default.
    pub fn low_tariff_mask(&self) -> [bool; 24] {
        let mut mask = [false; 24];
        for segment in self.low_tariff_hours.split(',') {
            let Some((a, b)) = segment.trim().split_once('-') else {
                continue;
            };
            if let (Ok(start), Ok(end)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                let (start, end) = (start.min(24), end.min(24));
                if start <= end {
                    for slot in mask.iter_mut().take(end).skip(start) {
                        *slot = true;
                    }
                } else {
                    // Midnight-wrapping range (e.g. `22-4`): mark [start, 24) and [0, end).
                    for slot in mask.iter_mut().skip(start) {
                        *slot = true;
                    }
                    for slot in mask.iter_mut().take(end) {
                        *slot = true;
                    }
                }
            }
        }
        mask
    }

    /// Import distribution + system-services surcharge (EUR/kWh) for one local hour, given the
    /// precomputed [`Self::low_tariff_mask`].
    pub fn distribution_eur(&self, local_hour: u32, low_tariff_mask: &[bool; 24]) -> f64 {
        let czk = if low_tariff_mask[(local_hour % 24) as usize] {
            self.distribution_low_czk
        } else {
            self.distribution_high_czk
        };
        czk / self.rate()
    }

    /// Export sell fee (EUR/kWh).
    pub fn sell_fee_eur(&self) -> f64 {
        self.sell_fee_czk / self.rate()
    }

    /// Convert an internal EUR amount to CZK for reporting.
    pub fn eur_to_czk(&self, eur: f64) -> f64 {
        eur * self.eur_czk_rate
    }

    /// Convert a CZK/kWh fee or threshold to the optimizer's EUR/kWh working currency.
    pub fn czk_to_eur(&self, czk: f64) -> f64 {
        czk / self.rate()
    }
}

/// Heat-pump and comfort settings.
#[derive(Debug, Clone, Deserialize)]
pub struct HeatingConfig {
    /// Coefficient of performance: heat delivered per unit electricity. **1.0** for direct electric
    /// (resistive) underfloor heating — this house — and `> 1` only for a heat pump.
    pub cop: f64,
    /// Penalty for a comfort-band violation, in price-units per Kelvin per step.
    pub comfort_penalty: f64,
    /// Per-zone comfort + heater limits. Zones absent here are not controlled.
    pub zones: HashMap<String, ZoneComfort>,
}

impl HeatingConfig {
    /// Reject non-physical heat-pump / comfort settings at config load. `cop` is a divisor in the
    /// electricity accounting (`heat / cop`, used unguarded in the CLI demo), and `comfort_penalty` is
    /// an LP objective coefficient — so a zero/NaN COP would divide-by-zero and a negative penalty would
    /// invert the objective (rewarding comfort violations). Called from [`ControlConfig::load`].
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.cop.is_finite() && self.cop > 0.0,
            "heating.cop must be finite and > 0 (got {})",
            self.cop
        );
        anyhow::ensure!(
            self.comfort_penalty.is_finite() && self.comfort_penalty >= 0.0,
            "heating.comfort_penalty must be finite and ≥ 0 (got {})",
            self.comfort_penalty
        );
        // Per-zone: the band edges feed the LP comfort constraints, max_heat_kw the heater bound, and
        // the gain the thermal input — each must be finite (the band ordered, the power non-negative).
        for (zone, z) in &self.zones {
            anyhow::ensure!(
                z.max_heat_kw.is_finite() && z.max_heat_kw >= 0.0,
                "heating.zones[{zone}].max_heat_kw must be finite and ≥ 0 (got {})",
                z.max_heat_kw
            );
            anyhow::ensure!(
                z.t_min.is_finite() && z.t_max.is_finite() && z.t_min <= z.t_max,
                "heating.zones[{zone}]: t_min ({}) and t_max ({}) must be finite with t_min ≤ t_max",
                z.t_min,
                z.t_max
            );
            anyhow::ensure!(
                z.internal_gain_w.is_finite(),
                "heating.zones[{zone}].internal_gain_w must be finite (got {})",
                z.internal_gain_w
            );
        }
        Ok(())
    }

    /// Per-zone constant internal heat gain (W), keeping only the positive ones. The calibrated
    /// occupants/appliances/fireplace term ([`crate::validate::calibrate_internal_gains`]); fed into
    /// the live forecast's known thermal inputs so it matches the validated backtest. A gain is a
    /// heat *source*, so a zero or (mis-entered) negative value is dropped rather than cooling a zone.
    pub fn internal_gains(&self) -> HashMap<String, f64> {
        self.zones
            .iter()
            .filter(|(_, z)| z.internal_gain_w > 0.0)
            .map(|(zone, z)| (zone.clone(), z.internal_gain_w))
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneComfort {
    /// Maximum heating power the zone's underfloor circuit can deliver (kW).
    pub max_heat_kw: f64,
    /// Lower comfort-band edge (degrees Celsius).
    pub t_min: f64,
    /// Upper comfort-band edge (degrees Celsius).
    pub t_max: f64,
    /// Constant internal heat gain (W) — occupants, appliances, cooking, fireplace — that the
    /// physics model has no other source for. Calibrated against the winter active backtest by
    /// [`crate::validate::calibrate_internal_gains`] and injected at the zone's air node in the live
    /// forecast so it doesn't run cold. Optional; default 0 (no extra gain).
    #[serde(default)]
    pub internal_gain_w: f64,
}

/// A coefficient-of-performance specification: a constant, or a curve of `(outdoor °C, COP)`
/// breakpoints. A heat pump's efficiency falls as the temperature lift grows, so a curve lets the
/// optimizer see a realistic per-block COP from the outdoor-temperature forecast (the per-block COP
/// is a *known* scalar, so the dispatch stays a linear program).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CopSpec {
    /// One COP, used at every outdoor temperature.
    Constant(f64),
    /// `(outdoor_temp_c, cop)` breakpoints in **strictly increasing** temperature; clamped piecewise-linear
    /// interpolation between them, held flat beyond the ends.
    Curve(Vec<CopPoint>),
}

/// One breakpoint of a [`CopSpec::Curve`].
#[derive(Debug, Clone, Deserialize)]
pub struct CopPoint {
    /// Outdoor air temperature (°C) at this breakpoint.
    pub t: f64,
    /// Coefficient of performance at this outdoor temperature.
    pub cop: f64,
}

impl CopSpec {
    /// The COP at outdoor temperature `t_out_c`. A `Constant` ignores the temperature; a `Curve`
    /// interpolates linearly between its breakpoints and is held flat beyond the ends. Assumes a
    /// validated curve (non-empty, ascending `t`, positive `cop`); a stray empty curve falls back
    /// to `1.0` so the electricity term never divides by zero.
    pub fn cop_at(&self, t_out_c: f64) -> f64 {
        match self {
            CopSpec::Constant(c) => *c,
            CopSpec::Curve(points) => match points.as_slice() {
                [] => 1.0,
                [only] => only.cop,
                _ => {
                    if t_out_c <= points[0].t {
                        return points[0].cop;
                    }
                    if t_out_c >= points[points.len() - 1].t {
                        return points[points.len() - 1].cop;
                    }
                    let i = points
                        .windows(2)
                        .position(|w| t_out_c >= w[0].t && t_out_c <= w[1].t)
                        .unwrap_or(0);
                    let (a, b) = (&points[i], &points[i + 1]);
                    let span = b.t - a.t;
                    if span <= 0.0 {
                        return a.cop;
                    }
                    a.cop + (b.cop - a.cop) * (t_out_c - a.t) / span
                }
            },
        }
    }

    /// Validate (non-empty curve, ascending finite temperatures, finite positive COPs / constant).
    /// `is_finite` matters because an infinity passes a bare `> 0.0` and would then poison the COP
    /// interpolation and the per-block electricity term (`heat / cop`) in the LP.
    fn validate(&self, ctx: &str) -> Result<()> {
        match self {
            CopSpec::Constant(c) => {
                anyhow::ensure!(
                    c.is_finite() && *c > 0.0,
                    "{ctx}: COP must be a finite value > 0 (got {c})"
                );
            }
            CopSpec::Curve(points) => {
                anyhow::ensure!(!points.is_empty(), "{ctx}: COP curve has no points");
                for p in points {
                    anyhow::ensure!(
                        p.cop.is_finite() && p.cop > 0.0,
                        "{ctx}: COP must be a finite value > 0 (got {})",
                        p.cop
                    );
                    anyhow::ensure!(
                        p.t.is_finite(),
                        "{ctx}: COP curve temperature must be finite (got {})",
                        p.t
                    );
                }
                anyhow::ensure!(
                    points.windows(2).all(|w| w[1].t > w[0].t),
                    "{ctx}: COP curve points must be in strictly increasing outdoor temperature"
                );
            }
        }
        Ok(())
    }
}

/// Reversible HVAC / air-conditioning. Each [`HvacUnit`] injects/removes heat at the **air node** of
/// the zones it serves (cooling = heat removed, air-heating = heat added), co-existing with the
/// underfloor heating. A unit serving one zone is a room split; a unit serving several is a
/// central/ducted system sharing one compressor's capacity. Comfort (the per-zone deadband) is a
/// property of the *room*; capacity / COP / grouping is a property of the *equipment*.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HvacConfig {
    /// Penalty for a comfort-band violation in an HVAC zone, price-units per Kelvin per step.
    #[serde(default = "default_hvac_comfort_penalty")]
    pub comfort_penalty: f64,
    /// Per-zone comfort deadband: air-heating holds the zone ≥ `t_heat`, cooling holds it ≤ `t_cool`.
    #[serde(default)]
    pub comfort: HashMap<String, HvacComfort>,
    /// Equipment. Each unit serves ≥1 zone, sharing its capacity across them.
    #[serde(default)]
    pub units: HashMap<String, HvacUnit>,
}

fn default_hvac_comfort_penalty() -> f64 {
    50.0
}

/// A room's HVAC comfort deadband (°C). The free-float band is `[t_heat, t_cool]`.
#[derive(Debug, Clone, Deserialize)]
pub struct HvacComfort {
    /// Lower edge: air-heating runs to keep the zone at or above this.
    pub t_heat: f64,
    /// Upper edge: cooling runs to keep the zone at or below this.
    pub t_cool: f64,
}

/// One HVAC unit: the equipment serving a set of zones from a shared capacity.
#[derive(Debug, Clone, Deserialize)]
pub struct HvacUnit {
    /// Zones this unit serves (≥1). One zone = a room split; several = a central/ducted unit.
    pub zones: Vec<String>,
    /// Total cooling power the unit can deliver (kW), **shared** across its `zones`.
    pub max_cool_kw: f64,
    /// Total air-heating power the unit can deliver (kW), **shared** across its `zones`.
    pub max_heat_kw: f64,
    /// Optional per-zone delivery cap (kW) — a damper/register limit on how much of the shared
    /// capacity one room can take. Zones absent here are bounded only by the unit total.
    #[serde(default)]
    pub per_zone_max_kw: HashMap<String, f64>,
    /// Cooling COP (EER) — constant or a curve vs outdoor °C.
    pub cooling_cop: CopSpec,
    /// Air-heating COP — constant or a curve vs outdoor °C.
    pub heating_cop: CopSpec,
    /// A single-compressor ducted unit can't heat and cool in the same block; `true` forbids it. A
    /// multi-split / VRF unit (or any single-zone unit) leaves this `false`.
    #[serde(default)]
    pub single_mode: bool,
}

impl HvacConfig {
    /// The zones served by at least one unit (sorted, de-duplicated) — those that get an air-node
    /// actuator and a cooling/air-heating kernel.
    pub fn served_zones(&self) -> Vec<String> {
        let mut zones: Vec<String> = self
            .units
            .values()
            .flat_map(|u| u.zones.iter().cloned())
            .collect();
        zones.sort();
        zones.dedup();
        zones
    }

    /// Validate the block: every served (or damper-capped) zone has a comfort entry, deadbands are
    /// ordered, capacities are non-negative, and the COP specs are well-formed.
    pub fn validate(&self) -> Result<()> {
        // Every numeric here feeds the LP (penalty + comfort bounds as objective/constraint terms,
        // capacities as variable bounds). A bare `>= 0.0` passes infinity (NaN is caught, since any
        // NaN comparison is false), so each is checked `is_finite()` too — matching HeatingConfig.
        anyhow::ensure!(
            self.comfort_penalty.is_finite() && self.comfort_penalty >= 0.0,
            "hvac.comfort_penalty must be finite and ≥ 0 (got {})",
            self.comfort_penalty
        );
        for (zone, c) in &self.comfort {
            anyhow::ensure!(
                c.t_heat.is_finite() && c.t_cool.is_finite() && c.t_cool >= c.t_heat,
                "hvac.comfort[{zone}]: t_heat ({}) and t_cool ({}) must be finite with t_cool ≥ t_heat",
                c.t_heat,
                c.t_cool
            );
        }
        for (name, unit) in &self.units {
            anyhow::ensure!(!unit.zones.is_empty(), "hvac unit {name:?} serves no zones");
            anyhow::ensure!(
                unit.max_cool_kw.is_finite()
                    && unit.max_cool_kw >= 0.0
                    && unit.max_heat_kw.is_finite()
                    && unit.max_heat_kw >= 0.0,
                "hvac unit {name:?}: capacities must be finite and non-negative"
            );
            for (zone, &cap) in &unit.per_zone_max_kw {
                anyhow::ensure!(
                    cap.is_finite() && cap >= 0.0,
                    "hvac unit {name:?}: per_zone_max_kw[{zone}] must be finite and ≥ 0 (got {cap})"
                );
            }
            unit.cooling_cop
                .validate(&format!("hvac unit {name:?} cooling_cop"))?;
            unit.heating_cop
                .validate(&format!("hvac unit {name:?} heating_cop"))?;
            for zone in unit.zones.iter().chain(unit.per_zone_max_kw.keys()) {
                anyhow::ensure!(
                    self.comfort.contains_key(zone),
                    "hvac unit {name:?} references zone {zone:?} with no hvac.comfort entry"
                );
            }
        }
        // Each zone carries a single cooling / air-heating decision, so it must belong to exactly
        // one unit (a deterministic order makes the error message stable).
        let mut unit_names: Vec<&String> = self.units.keys().collect();
        unit_names.sort();
        let mut owner: HashMap<&String, &String> = HashMap::new();
        for name in unit_names {
            for zone in &self.units[name].zones {
                if let Some(prev) = owner.insert(zone, name) {
                    anyhow::bail!(
                        "hvac zone {zone:?} is served by more than one unit ({prev:?} and {name:?}); a zone may belong to only one unit"
                    );
                }
            }
        }
        Ok(())
    }
}

/// How much the MPC can control a charger.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvControl {
    /// Continuously set charge power 0…max (e.g. the loxone wallbox). Full scheduling.
    #[default]
    Modulating,
    /// Only switch on/off at the rated power; scheduled as a near-term binary.
    OnOff,
    /// No control — observe only. Its expected load is forecast into `load_kw`, never scheduled.
    Monitored,
}

/// The charging strategy: how the optimizer trades cost, solar, and the deadline. A config default,
/// overridable live from the dashboard.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvStrategy {
    /// Meet the target by the deadline at the lowest cost (charges the cheapest/solar blocks).
    #[default]
    CostOptimized,
    /// Charge only from surplus PV; never grid or battery. May miss the target.
    SolarOnly,
    /// Solar first, top up from cheap grid to meet the deadline.
    SolarPreferred,
    /// Charge at full rate immediately until the target (price-blind). Also the load model for a
    /// `monitored` charger.
    ChargeNow,
}

/// An EV charger: a controllable flexible load with a car battery, a target, and a deadline.
#[derive(Debug, Clone, Deserialize)]
pub struct EvChargerConfig {
    /// Stable identifier — the dashboard key and the controller's MQTT channel (e.g. `"garage"`).
    pub name: String,
    /// How controllable the charger is.
    #[serde(default)]
    pub control: EvControl,
    /// Maximum charge power the charger can deliver (kW).
    pub max_kw: f64,
    /// Minimum charge power when on (kW) — most chargers can't modulate below ~1.4 kW (6 A). 0 = none.
    #[serde(default)]
    pub min_kw: f64,
    /// AC→DC charging efficiency (0..1): energy reaching the car battery per kWh drawn from the house.
    #[serde(default = "default_ev_efficiency")]
    pub efficiency: f64,
    /// The car battery's usable capacity (kWh) — for SoC%↔energy when a `%` target is used.
    pub battery_kwh: f64,
    /// Allow the home battery to charge the car. Default `false`: double-conversion-lossy, so off
    /// unless explicitly enabled (when on it's wear-gated by the battery amortisation term).
    #[serde(default)]
    pub allow_battery_to_ev: bool,
    /// Default charging strategy (overridable live from the dashboard).
    #[serde(default)]
    pub strategy: EvStrategy,
    /// Default target state of charge (%, 0..100). Live preference and the car's own charge limit
    /// take precedence (see the fusion layer).
    #[serde(default = "default_ev_target_pct")]
    pub target_pct: f64,
    /// Default "charged-by" local time-of-day, `"HH:MM"` (the deadline). Overridable live.
    #[serde(default = "default_ev_deadline")]
    pub deadline: String,
    /// Optional default cap on the charge rate (kW), clamped to `max_kw`; overridable live.
    #[serde(default)]
    pub max_rate_kw: Option<f64>,
    /// Data sources fused into the charger's live state (role → source). Recognised roles:
    /// `on_charger` (the wallbox, authoritative for "controllable now"), `power`, `soc`, `target`,
    /// `capacity`, and `tesla_power` (to detect charging away). Each is a [`SourceLocator`]. The
    /// wallbox roles (`on_charger`/`power`) are car-agnostic; the per-car `soc`/`target` may instead
    /// live under `cars` (below) when more than one car shares this wallbox.
    #[serde(default)]
    pub sources: HashMap<String, SourceLocator>,
    /// Optional: more than one car shares this wallbox. Each entry's `present` signal (1/0) says which
    /// car is on our wallbox now; the fusion uses that car's `soc`/`target`. Empty ⇒ a single car,
    /// whose `soc`/`target` come from `sources` above. (`present` is derived per house — e.g. a
    /// TeslaMate query for "plugged in AND at home" — since the wallbox itself can't identify the car.)
    #[serde(default)]
    pub cars: Vec<EvCar>,
}

/// One car that shares a wallbox (see [`EvChargerConfig::cars`]).
#[derive(Debug, Clone, Deserialize)]
pub struct EvCar {
    pub name: String,
    /// 1/0 — this car is the one currently on our wallbox.
    pub present: SourceLocator,
    /// This car's state of charge (%).
    pub soc: SourceLocator,
    /// This car's own charge limit (%); optional.
    #[serde(default)]
    pub target: Option<SourceLocator>,
    /// This car's usable battery capacity (kWh) from a data source (e.g. derived from TeslaMate),
    /// preferred over the static `capacity_kwh` when it reads.
    #[serde(default)]
    pub capacity: Option<SourceLocator>,
    /// Static usable battery capacity (kWh); used when no `capacity` source reads. Falls back to the
    /// charger's `battery_kwh`.
    #[serde(default)]
    pub capacity_kwh: Option<f64>,
}

fn default_ev_efficiency() -> f64 {
    0.9
}
fn default_ev_target_pct() -> f64 {
    80.0
}
fn default_ev_deadline() -> String {
    "07:00".to_string()
}

impl EvChargerConfig {
    /// The deadline as `(hour, minute)` local time, if it parses as `HH:MM`.
    pub fn deadline_hm(&self) -> Option<(u32, u32)> {
        let (h, m) = self.deadline.split_once(':')?;
        let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some((h, m))
    }

    /// The effective max charge rate (kW): the configured `max_rate_kw` cap, clamped to `max_kw`.
    pub fn effective_max_kw(&self) -> f64 {
        self.max_rate_kw
            .unwrap_or(self.max_kw)
            .clamp(0.0, self.max_kw)
    }

    /// Validate one charger: positive capacities/efficiency, ordered power, a parseable deadline, and
    /// the data sources a controllable charger needs.
    pub fn validate(&self) -> Result<()> {
        let n = &self.name;
        anyhow::ensure!(!n.trim().is_empty(), "charger has an empty name");
        // The name becomes a Loxone virtual-input key stem; the controller drops a stem containing
        // these chars to protect the `key=value;…` datagram, which would silently un-actuate the
        // charger. Reject them at config load so the charger isn't shown controllable but never driven.
        anyhow::ensure!(
            !n.contains([';', '=', '\n', '\r']),
            "charger {n:?}: name must not contain ';', '=', newline, or carriage return"
        );
        // `is_finite()` on the bare lower-bound checks (an infinity passes `> 0.0` and would poison the
        // LP charge-power bounds). The bounded-range checks below (efficiency, target_pct, max_rate_kw)
        // already reject non-finite values via their upper bound.
        anyhow::ensure!(
            self.max_kw.is_finite() && self.max_kw > 0.0,
            "charger {n:?}: max_kw must be a finite value > 0"
        );
        anyhow::ensure!(
            self.min_kw.is_finite() && self.min_kw >= 0.0 && self.min_kw <= self.max_kw,
            "charger {n:?}: min_kw must be finite and in [0, max_kw]"
        );
        anyhow::ensure!(
            self.efficiency > 0.0 && self.efficiency <= 1.0,
            "charger {n:?}: efficiency must be in (0, 1]"
        );
        anyhow::ensure!(
            self.battery_kwh.is_finite() && self.battery_kwh > 0.0,
            "charger {n:?}: battery_kwh must be a finite value > 0"
        );
        anyhow::ensure!(
            (0.0..=100.0).contains(&self.target_pct),
            "charger {n:?}: target_pct must be in [0, 100]"
        );
        anyhow::ensure!(
            self.deadline_hm().is_some(),
            "charger {n:?}: deadline {:?} must be local HH:MM",
            self.deadline
        );
        if let Some(r) = self.max_rate_kw {
            anyhow::ensure!(
                (0.0..=self.max_kw).contains(&r),
                "charger {n:?}: max_rate_kw must be in [0, max_kw]"
            );
        }
        // A controllable charger needs to know when the car is on *our* wallbox; a monitored one needs
        // its power to forecast the load it adds. The wallbox `power` source covers both.
        if self.control == EvControl::Monitored {
            anyhow::ensure!(
                self.sources.contains_key("power"),
                "charger {n:?}: a monitored charger needs a `power` source to forecast its load"
            );
        } else {
            anyhow::ensure!(
                self.sources.contains_key("on_charger") || self.sources.contains_key("power"),
                "charger {n:?}: a controllable charger needs an `on_charger` or `power` source (the wallbox)"
            );
        }
        Ok(())
    }
}

impl ControlConfig {
    /// Validate the EV-charger block: unique names and each charger well-formed.
    pub fn validate_chargers(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for c in &self.chargers {
            anyhow::ensure!(seen.insert(&c.name), "duplicate charger name {:?}", c.name);
            c.validate()?;
        }
        Ok(())
    }

    pub fn from_json5(text: &str) -> Result<Self> {
        Ok(json5::from_str(text)?)
    }

    /// Bound the site's coordinates and UTC offset to physically-valid ranges. Called from [`load`].
    /// The UTC offset feeds `FixedOffset::east_opt` in several readers, and the coordinates feed
    /// `spa::calc_solar_position` (which the solar path `unwrap()`s and which rejects out-of-range
    /// latitude/longitude) — so a typo'd value would silently degrade a local-time conversion or panic
    /// mid-forecast. Fail fast at load instead. (A range `contains` check is also false for NaN.)
    fn validate_site(&self) -> Result<()> {
        // Real civil offsets span UTC−12..+14; `FixedOffset` itself only accepts ±24h.
        anyhow::ensure!(
            (-12..=14).contains(&self.site.utc_offset_hours),
            "site.utc_offset_hours ({}) is out of range (must be between -12 and +14)",
            self.site.utc_offset_hours
        );
        anyhow::ensure!(
            (-90.0..=90.0).contains(&self.site.latitude),
            "site.latitude ({}) is out of range (must be between -90 and 90)",
            self.site.latitude
        );
        anyhow::ensure!(
            (-180.0..=180.0).contains(&self.site.longitude),
            "site.longitude ({}) is out of range (must be between -180 and 180)",
            self.site.longitude
        );
        // The slab ground temperature is a thermal boundary condition fed into the model; a NaN/inf
        // would propagate into every simulation. Bound it to physically-plausible soil temperatures.
        anyhow::ensure!(
            (-30.0..=40.0).contains(&self.site.ground_temperature_c),
            "site.ground_temperature_c ({}) is out of range (must be between -30 and 40)",
            self.site.ground_temperature_c
        );
        Ok(())
    }

    /// Reject malformed scheduled loads at load: every window time must parse as `HH:MM` and every
    /// month must be 1-12, so a typo fails fast instead of silently never firing (see
    /// [`LoadWindow::contains`]'s defensive `false`).
    fn validate_scheduled_loads(&self) -> Result<()> {
        // Controllable loads are keyed by name (label, else zone) in the plan output + the thermal
        // kernel map, so two with the same name would silently collide — reject duplicates.
        let mut controllable_names = std::collections::HashSet::new();
        for load in &self.scheduled_loads {
            anyhow::ensure!(
                !load.windows.is_empty(),
                "scheduled_load for zone {:?} has no windows (it would never fire)",
                load.zone
            );
            if let Some(w) = load.power_w {
                anyhow::ensure!(
                    w.is_finite() && w > 0.0,
                    "scheduled_load zone {:?}: power_w must be finite and > 0 (got {w}); omit it to auto-fit",
                    load.zone
                );
            }
            // `power_factor` scales the measured electrical power into zone heat (P·factor); it feeds the
            // drive flux directly, so a NaN/≤0 would silently corrupt the calibration. Reject at load.
            if let Some(f) = load.power_factor {
                anyhow::ensure!(
                    f.is_finite() && f > 0.0,
                    "scheduled_load zone {:?}: power_factor must be finite and > 0 (got {f})",
                    load.zone
                );
            }
            // A controllable load is a deferrable electrical load the optimizer switches: it needs a
            // rated draw (`power_w`, the LP's electrical and heat magnitude) and a positive `run_hours`
            // target, plus at least one window (already enforced above) to shift within. A `sensor` is
            // a *monitoring* feature (read a past draw), orthogonal to *control*; it is allowed but the
            // forecast magnitude still comes from `power_w`.
            if load.controllable {
                anyhow::ensure!(
                    load.power_w.is_some_and(|w| w.is_finite() && w > 0.0),
                    "scheduled_load zone {:?}: a controllable load needs power_w set and > 0 (the rated draw)",
                    load.zone
                );
                anyhow::ensure!(
                    load.run_hours.is_some_and(|h| h.is_finite() && h > 0.0),
                    "scheduled_load zone {:?}: a controllable load needs run_hours set and > 0",
                    load.zone
                );
                let name = if load.label.is_empty() {
                    &load.zone
                } else {
                    &load.label
                };
                anyhow::ensure!(
                    controllable_names.insert(name.clone()),
                    "duplicate controllable load name {name:?} — controllable loads need distinct labels (the plan/kernel key)"
                );
            }
            for w in &load.windows {
                anyhow::ensure!(
                    parse_hm(&w.start).is_some() && parse_hm(&w.end).is_some(),
                    "scheduled_load zone {:?}: window time must be \"HH:MM\" (got {:?}–{:?})",
                    load.zone,
                    w.start,
                    w.end
                );
                anyhow::ensure!(
                    w.months.iter().all(|m| (1..=12).contains(m)),
                    "scheduled_load zone {:?}: months must be 1-12 (got {:?})",
                    load.zone,
                    w.months
                );
            }
        }
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg = Self::from_json5(&std::fs::read_to_string(path)?)?;
        cfg.validate_chargers()?;
        cfg.data_sources.validate()?;
        cfg.tariff.validate()?;
        cfg.battery.validate()?;
        cfg.heating.validate()?;
        cfg.pv.validate()?;
        if let Some(hvac) = &cfg.hvac {
            hvac.validate()?;
        }
        cfg.validate_site()?;
        cfg.validate_scheduled_loads()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(months: &[u32], start: &str, end: &str) -> LoadWindow {
        LoadWindow {
            months: months.to_vec(),
            start: start.into(),
            end: end.into(),
        }
    }

    #[test]
    fn load_window_same_day_is_half_open() {
        let w = win(&[], "10:00", "14:00");
        assert!(!w.contains(7, 9 * 60 + 59)); // before start
        assert!(w.contains(7, 10 * 60)); // start inclusive
        assert!(w.contains(7, 13 * 60 + 59));
        assert!(!w.contains(7, 14 * 60)); // end exclusive
    }

    #[test]
    fn load_window_wraps_past_midnight() {
        let w = win(&[], "22:00", "06:00");
        assert!(w.contains(1, 23 * 60)); // late evening
        assert!(w.contains(1, 60)); // small hours (01:00)
        assert!(w.contains(1, 0)); // midnight
        assert!(!w.contains(1, 6 * 60)); // end exclusive
        assert!(!w.contains(1, 12 * 60)); // midday is outside
    }

    #[test]
    fn load_window_gates_on_month() {
        let w = win(&[5, 6, 7, 8, 9], "10:00", "20:00");
        assert!(w.contains(7, 12 * 60)); // July, in window
        assert!(!w.contains(1, 12 * 60)); // January, wrong month
    }

    #[test]
    fn unit_profile_signs_and_season() {
        // The real heat-pump: a summer-daytime / winter-overnight sink in technical_room.
        let hp = ScheduledLoad {
            zone: "technical_room".into(),
            label: "water heat-pump".into(),
            kind: LoadKind::Sink,
            power_w: None,
            sensor: None,
            power_factor: None,
            controllable: false,
            run_hours: None,
            windows: vec![
                win(&[5, 6, 7, 8, 9], "10:00", "20:00"),
                win(&[10, 11, 12, 1, 2, 3, 4], "01:00", "05:00"),
            ],
        };
        assert_eq!(hp.unit_profile(7, 12 * 60), -1.0); // summer midday: cooling
        assert_eq!(hp.unit_profile(7, 6 * 60), 0.0); // summer early morning: off
        assert_eq!(hp.unit_profile(1, 2 * 60), -1.0); // winter night: cooling
        assert_eq!(hp.unit_profile(1, 12 * 60), 0.0); // winter midday: off
                                                      // A source flips the sign.
        let src = ScheduledLoad {
            kind: LoadKind::Source,
            ..hp
        };
        assert_eq!(src.unit_profile(7, 12 * 60), 1.0);
    }

    #[test]
    fn validate_scheduled_loads_rejects_malformed() {
        let ok = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"sink",windows:[{start:"01:00",end:"05:00"}]}] }"#,
        )
        .unwrap();
        assert!(ok.validate_scheduled_loads().is_ok());

        let bad_time = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"sink",windows:[{start:"25:00",end:"05:00"}]}] }"#,
        )
        .unwrap();
        assert!(bad_time.validate_scheduled_loads().is_err());

        let no_windows = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"source",windows:[]}] }"#,
        )
        .unwrap();
        assert!(no_windows.validate_scheduled_loads().is_err());

        // A configured `power_w` must be finite and > 0; a non-positive value is rejected.
        let fixed_ok = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"sink",power_w:800,windows:[{start:"01:00",end:"05:00"}]}] }"#,
        )
        .unwrap();
        assert_eq!(fixed_ok.scheduled_loads[0].power_w, Some(800.0));
        assert!(fixed_ok.validate_scheduled_loads().is_ok());

        let bad_power = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"sink",power_w:0,windows:[{start:"01:00",end:"05:00"}]}] }"#,
        )
        .unwrap();
        assert!(bad_power.validate_scheduled_loads().is_err());

        // A `sensor`-driven load parses (a SourceLocator) and a valid `power_factor` passes; the
        // forecast magnitude (`power_w`) and the schedule remain.
        let sensor_ok = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"sink",power_w:800,power_factor:2.5,
                   sensor:{type:"influx",bucket:"loxone",measurement:"power",field:"hp_w"},
                   windows:[{start:"01:00",end:"05:00"}]}] }"#,
        )
        .unwrap();
        assert!(sensor_ok.scheduled_loads[0].sensor.is_some());
        assert_eq!(sensor_ok.scheduled_loads[0].power_factor, Some(2.5));
        assert!(sensor_ok.validate_scheduled_loads().is_ok());

        // A non-positive / non-finite `power_factor` is rejected (it scales the measured flux directly).
        for bad in ["0", "-1", "NaN"] {
            let cfg = ControlConfig::from_json5(&format!(
                r#"{{ site:{{latitude:50,longitude:14,utc_offset_hours:1}},
                     heating:{{cop:1,comfort_penalty:0,zones:{{}}}},
                     scheduled_loads:[{{zone:"x",kind:"sink",power_factor:{bad},
                       windows:[{{start:"01:00",end:"05:00"}}]}}] }}"#,
            ))
            .unwrap();
            assert!(
                cfg.validate_scheduled_loads().is_err(),
                "power_factor {bad} should be rejected"
            );
        }
    }

    #[test]
    fn validate_scheduled_loads_gates_controllable() {
        // A well-formed controllable load (rated draw + run_hours + a window) passes and parses.
        let ok = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"technical_room",kind:"source",controllable:true,
                   power_w:2000,run_hours:3,windows:[{start:"00:00",end:"06:00"}]}] }"#,
        )
        .unwrap();
        let load = &ok.scheduled_loads[0];
        assert!(load.controllable);
        assert_eq!(load.run_hours, Some(3.0));
        assert_eq!(load.power_w, Some(2000.0));
        assert!(ok.validate_scheduled_loads().is_ok());
        // The on-heat is rated × power_factor (default 1.0): 2 kW.
        assert!((load.controllable_heat_kw() - 2.0).abs() < 1e-9);

        // Controllable without `power_w` is rejected (the LP has no rated draw / heat magnitude).
        let no_power = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[{zone:"x",kind:"source",controllable:true,run_hours:2,
                   windows:[{start:"00:00",end:"06:00"}]}] }"#,
        )
        .unwrap();
        assert!(no_power.validate_scheduled_loads().is_err());

        // Two controllable loads sharing a name (label, else zone) collide on the plan/kernel key.
        let dup = ControlConfig::from_json5(
            r#"{ site:{latitude:50,longitude:14,utc_offset_hours:1},
                 heating:{cop:1,comfort_penalty:0,zones:{}},
                 scheduled_loads:[
                   {zone:"a",kind:"source",controllable:true,power_w:1000,run_hours:1,windows:[{start:"00:00",end:"06:00"}]},
                   {zone:"a",kind:"source",controllable:true,power_w:1000,run_hours:1,windows:[{start:"00:00",end:"06:00"}]}] }"#,
        )
        .unwrap();
        assert!(dup.validate_scheduled_loads().is_err());

        // Controllable without `run_hours` (or non-positive) is rejected.
        for run in ["", ",run_hours:0", ",run_hours:-1"] {
            let cfg = ControlConfig::from_json5(&format!(
                r#"{{ site:{{latitude:50,longitude:14,utc_offset_hours:1}},
                     heating:{{cop:1,comfort_penalty:0,zones:{{}}}},
                     scheduled_loads:[{{zone:"x",kind:"source",controllable:true,power_w:2000{run},
                       windows:[{{start:"00:00",end:"06:00"}}]}}] }}"#,
            ))
            .unwrap();
            assert!(
                cfg.validate_scheduled_loads().is_err(),
                "run_hours {run:?} should be rejected for a controllable load"
            );
        }
    }

    #[test]
    fn tariff_validate_rejects_inverted_economics() {
        assert!(TariffConfig::default().validate().is_ok());
        // A negative wear cost would reward discharging; a non-positive rate inverts CZK conversion.
        let negative_wear = TariffConfig {
            battery_amortisation_czk: -1.0,
            ..Default::default()
        };
        assert!(negative_wear.validate().is_err());
        let bad_rate = TariffConfig {
            eur_czk_rate: 0.0,
            ..Default::default()
        };
        assert!(bad_rate.validate().is_err());
    }

    #[test]
    fn tariff_validate_rejects_non_finite_fees() {
        // A NaN/inf in any fee corrupts the per-kWh price arithmetic and the export gate.
        let nan_dist = TariffConfig {
            distribution_high_czk: f64::NAN,
            ..Default::default()
        };
        assert!(nan_dist.validate().is_err());
        let inf_fee = TariffConfig {
            sell_fee_czk: f64::INFINITY,
            ..Default::default()
        };
        assert!(inf_fee.validate().is_err());
        // The inverter-off threshold is legitimately negative (default −2.0) — a finite negative is OK,
        // but a NaN there is not.
        let neg_ok = TariffConfig {
            inverter_off_price_czk: -5.0,
            ..Default::default()
        };
        assert!(neg_ok.validate().is_ok());
        let nan_threshold = TariffConfig {
            inverter_off_price_czk: f64::NAN,
            ..Default::default()
        };
        assert!(nan_threshold.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_physical_battery_heating_pv() {
        // Battery: capacity > 0, min_soc in [0,100], powers ≥ 0, efficiency in (0,1].
        assert!(BatteryConfig::default().validate().is_ok());
        for mutate in [
            |b: &mut BatteryConfig| b.capacity_kwh = 0.0,
            |b: &mut BatteryConfig| b.min_soc_pct = 150.0,
            |b: &mut BatteryConfig| b.charge_kw = f64::NAN,
            |b: &mut BatteryConfig| b.round_trip_efficiency = 0.0,
            |b: &mut BatteryConfig| b.round_trip_efficiency = 1.5,
        ] {
            let mut b = BatteryConfig::default();
            mutate(&mut b);
            assert!(b.validate().is_err());
        }

        // Heating: cop is a divisor (> 0, finite); comfort_penalty an objective coefficient (≥ 0).
        let heating = |cop: f64, pen: f64| HeatingConfig {
            cop,
            comfort_penalty: pen,
            zones: HashMap::new(),
        };
        assert!(heating(1.0, 5.0).validate().is_ok());
        assert!(heating(0.0, 5.0).validate().is_err());
        assert!(heating(f64::NAN, 5.0).validate().is_err());
        assert!(heating(3.5, -1.0).validate().is_err());
        // Per-zone comfort band: ordered finite edges, finite non-negative power and gain.
        let zoned = |z: ZoneComfort| HeatingConfig {
            cop: 1.0,
            comfort_penalty: 5.0,
            zones: HashMap::from([("lr".to_string(), z)]),
        };
        let zone = |t_min: f64, t_max: f64, max_heat_kw: f64, internal_gain_w: f64| ZoneComfort {
            max_heat_kw,
            t_min,
            t_max,
            internal_gain_w,
        };
        assert!(zoned(zone(20.0, 24.0, 4.0, 0.0)).validate().is_ok());
        assert!(zoned(zone(24.0, 20.0, 4.0, 0.0)).validate().is_err()); // t_min > t_max
        assert!(zoned(zone(f64::NAN, 24.0, 4.0, 0.0)).validate().is_err()); // NaN edge
        assert!(zoned(zone(20.0, 24.0, -1.0, 0.0)).validate().is_err()); // negative power
        assert!(zoned(zone(20.0, 24.0, 4.0, f64::INFINITY))
            .validate()
            .is_err()); // inf gain

        // PV array geometry: kwp > 0, tilt in [0,90], azimuth in [0,360].
        let pv = |arr: PvArrayConfig| PvConfig {
            system_efficiency: 0.85,
            arrays: vec![arr],
        };
        let arr = |tilt: f64, azimuth: f64, kwp: f64| PvArrayConfig {
            name: "roof".to_string(),
            kwp,
            tilt,
            azimuth,
        };
        assert!(pv(arr(30.0, 180.0, 5.0)).validate().is_ok());
        assert!(pv(arr(120.0, 180.0, 5.0)).validate().is_err()); // tilt > 90
        assert!(pv(arr(30.0, 400.0, 5.0)).validate().is_err()); // azimuth > 360
        assert!(pv(arr(30.0, 180.0, 0.0)).validate().is_err()); // kwp 0
        assert!(pv(arr(f64::NAN, 180.0, 5.0)).validate().is_err()); // NaN tilt
    }

    #[test]
    fn parses_site_and_heating_ignoring_other_keys() {
        // The `db` block (used by influxdb.rs) is present and must be ignored here.
        let cfg = ControlConfig::from_json5(
            r#"{
                db: { host: "http://localhost:8086", org: "x" },
                site: { latitude: 49.5, longitude: 17.4, utc_offset_hours: 2 },
                heating: {
                    cop: 3.5,
                    comfort_penalty: 5.0,
                    zones: {
                        livingroom: { max_heat_kw: 4.0, t_min: 20.0, t_max: 24.0 },
                        kitchen: { max_heat_kw: 3.0, t_min: 19.0, t_max: 23.0 },
                    },
                },
            }"#,
        )
        .unwrap();

        assert_eq!(cfg.site.utc_offset_hours, 2);
        assert_eq!(cfg.heating.cop, 3.5);
        assert_eq!(cfg.heating.zones.len(), 2);
        assert_eq!(cfg.heating.zones["livingroom"].t_max, 24.0);
        // The optional blocks fall back to defaults when absent.
        assert_eq!(cfg.consumption_history_days, 30);
        assert_eq!(cfg.tariff.eur_czk_rate, 25.0);
    }

    #[test]
    fn validate_site_bounds_coordinates_and_offset() {
        let base = ControlConfig::from_json5(
            r#"{
                site: { latitude: 49.5, longitude: 17.4, utc_offset_hours: 2 },
                heating: { cop: 3.5, comfort_penalty: 5.0,
                    zones: { livingroom: { max_heat_kw: 4.0, t_min: 20.0, t_max: 24.0 } } },
            }"#,
        )
        .unwrap();
        let with = |lat: f64, lon: f64, off: i32| {
            let mut c = base.clone();
            c.site.latitude = lat;
            c.site.longitude = lon;
            c.site.utc_offset_hours = off;
            c
        };
        // A real site passes.
        assert!(with(49.5, 17.4, 2).validate_site().is_ok());
        // Out-of-range latitude/longitude would make `spa::calc_solar_position` error and the solar
        // path `unwrap()` panic — reject at load.
        assert!(with(91.0, 17.4, 2).validate_site().is_err());
        assert!(with(49.5, 181.0, 2).validate_site().is_err());
        // NaN is rejected too (range `contains` is false for NaN).
        assert!(with(f64::NAN, 17.4, 2).validate_site().is_err());
        // An impossible civil offset is rejected.
        assert!(with(49.5, 17.4, 20).validate_site().is_err());
        // A non-finite / absurd slab ground temperature (a thermal boundary condition) is rejected.
        let mut c = base.clone();
        c.site.ground_temperature_c = f64::NAN;
        assert!(c.validate_site().is_err());
        let mut c = base.clone();
        c.site.ground_temperature_c = 200.0;
        assert!(c.validate_site().is_err());
    }

    #[test]
    fn consumption_history_days_overrides_default() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 3.0, comfort_penalty: 1.0, zones: {} },
                consumption_history_days: 14,
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.consumption_history_days, 14);
    }

    #[test]
    fn absent_pv_and_battery_blocks_keep_real_defaults() {
        // A config with no `pv`/`battery` blocks must still yield the real hardware defaults.
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 3.0, comfort_penalty: 1.0, zones: {} },
            }"#,
        )
        .unwrap();
        // The explicit PvConfig Default keeps 0.85 (a derived Default would zero it).
        assert_eq!(cfg.pv.system_efficiency, 0.85);
        assert!(cfg.pv.arrays.is_empty());
        assert_eq!(cfg.battery.capacity_kwh, 10.0);
        assert_eq!(cfg.battery.min_soc_pct, 20.0);
        assert_eq!(cfg.battery.charge_kw, 5.3);
        assert_eq!(cfg.battery.round_trip_efficiency, 0.85);
    }

    #[test]
    fn low_tariff_mask_parses_d57d_ranges() {
        let t = TariffConfig::default(); // "0-10,11-12,13-14,15-17,18-24"
        let mask = t.low_tariff_mask();
        // End-exclusive: 0..10 low, 10 high, 11 low, 12 high, 13 low, 14 high, 15..17 low, 17 high.
        assert!(mask[0] && mask[9], "00:00–09:59 is NT");
        assert!(!mask[10], "10:00 is VT");
        assert!(mask[11] && !mask[12] && mask[13] && !mask[14]);
        assert!(mask[15] && mask[16] && !mask[17]);
        assert!(mask[18] && mask[23], "evening is NT");
    }

    #[test]
    fn distribution_picks_vt_or_nt_and_converts_to_eur() {
        let t = TariffConfig::default();
        let mask = t.low_tariff_mask();
        // VT hour 10: 0.919 CZK / 25 = 0.03676 EUR/kWh; NT hour 3: 0.281 / 25 = 0.01124.
        assert!((t.distribution_eur(10, &mask) - 0.919 / 25.0).abs() < 1e-12);
        assert!((t.distribution_eur(3, &mask) - 0.281 / 25.0).abs() < 1e-12);
        assert!((t.sell_fee_eur() - 0.5 / 25.0).abs() < 1e-12);
    }

    #[test]
    fn malformed_low_tariff_segments_are_skipped_as_vt() {
        let t = TariffConfig {
            low_tariff_hours: "0-6,garbage,9-,-3,22-24".to_string(),
            ..TariffConfig::default()
        };
        let mask = t.low_tariff_mask();
        assert!(mask[0] && mask[5] && !mask[6]); // "0-6" honored
        assert!(mask[22] && mask[23]); // "22-24" honored
        assert!(!mask[9] && !mask[15]); // malformed segments ignored → stay VT
    }

    #[test]
    fn low_tariff_wrapping_and_oob_ranges() {
        // A midnight-wrapping range (start > end) marks [start, 24) ∪ [0, end); a fully out-of-range
        // segment clamps to 24-24 and marks nothing (no panic, stays VT).
        let t = TariffConfig {
            low_tariff_hours: "20-10,30-32".to_string(),
            ..TariffConfig::default()
        };
        let mask = t.low_tariff_mask();
        assert!(mask[20] && mask[23] && mask[0] && mask[9]); // "20-10" wraps over midnight
        assert!(!mask[10] && !mask[19]); // the daytime middle stays high (VT)
        assert_eq!(mask.iter().filter(|&&m| m).count(), 14); // 20..24 (4) + 0..10 (10)
    }

    #[test]
    fn absent_tariff_block_uses_real_czech_defaults() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 3.0, comfort_penalty: 1.0, zones: {} },
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.tariff.eur_czk_rate, 25.0);
        assert_eq!(cfg.tariff.distribution_high_czk, 0.919);
        assert_eq!(cfg.tariff.sell_fee_czk, 0.5);
        assert_eq!(cfg.site.ground_temperature_c, 16.0); // site default too
    }

    #[test]
    fn missing_heating_block_errors() {
        let err = ControlConfig::from_json5(
            r#"{ site: { latitude: 0, longitude: 0, utc_offset_hours: 0 } }"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn internal_gains_default_to_zero_and_parse_when_present() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 1.0, comfort_penalty: 1.0, zones: {
                    with_gain: { max_heat_kw: 3.0, t_min: 20.0, t_max: 23.0, internal_gain_w: 150 },
                    no_gain:   { max_heat_kw: 2.0, t_min: 19.0, t_max: 23.0 },
                } },
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.heating.zones["with_gain"].internal_gain_w, 150.0);
        assert_eq!(cfg.heating.zones["no_gain"].internal_gain_w, 0.0); // serde default
                                                                       // The recalibration knobs default to 7 days / 24 hours when absent.
        assert_eq!(cfg.internal_gain_window_days, 7);
        assert_eq!(cfg.internal_gain_recalibrate_hours, 24);
    }

    #[test]
    fn internal_gains_keeps_only_positive() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 1.0, comfort_penalty: 1.0, zones: {
                    warm:    { max_heat_kw: 3.0, t_min: 20.0, t_max: 23.0, internal_gain_w: 150 },
                    zero:    { max_heat_kw: 2.0, t_min: 19.0, t_max: 23.0, internal_gain_w: 0 },
                    bogus:   { max_heat_kw: 2.0, t_min: 19.0, t_max: 23.0, internal_gain_w: -50 },
                } },
            }"#,
        )
        .unwrap();
        let gains = cfg.heating.internal_gains();
        assert_eq!(gains.len(), 1, "only the positive gain is kept");
        assert_eq!(gains["warm"], 150.0);
        assert!(!gains.contains_key("zero") && !gains.contains_key("bogus"));
    }

    #[test]
    fn absent_hvac_block_is_none() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 1.0, comfort_penalty: 1.0, zones: {} },
            }"#,
        )
        .unwrap();
        assert!(cfg.hvac.is_none());
    }

    #[test]
    fn parses_hvac_units_and_validates() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 1.0, comfort_penalty: 1.0, zones: {} },
                hvac: {
                    comfort_penalty: 40.0,
                    comfort: {
                        bedroom:    { t_heat: 20.0, t_cool: 26.0 },
                        room_1:     { t_heat: 20.0, t_cool: 26.0 },
                        livingroom: { t_heat: 20.0, t_cool: 26.0 },
                    },
                    units: {
                        bedroom_ac: {
                            zones: ["bedroom"],
                            max_cool_kw: 3.5, max_heat_kw: 3.5,
                            cooling_cop: 3.0, heating_cop: 3.5,
                        },
                        upstairs_ducted: {
                            zones: ["room_1", "livingroom"],
                            max_cool_kw: 8.0, max_heat_kw: 9.0,
                            per_zone_max_kw: { room_1: 4.0, livingroom: 5.0 },
                            cooling_cop: [ { t: 25, cop: 3.6 }, { t: 35, cop: 2.3 } ],
                            heating_cop: [ { t: -10, cop: 2.0 }, { t: 7, cop: 3.5 }, { t: 15, cop: 4.6 } ],
                            single_mode: true,
                        },
                    },
                },
            }"#,
        )
        .unwrap();
        let hvac = cfg.hvac.unwrap();
        hvac.validate().unwrap();
        assert_eq!(hvac.comfort_penalty, 40.0);
        assert_eq!(hvac.served_zones(), vec!["bedroom", "livingroom", "room_1"]);
        assert!(hvac.units["upstairs_ducted"].single_mode);
        assert!(!hvac.units["bedroom_ac"].single_mode); // serde default
    }

    #[test]
    fn hvac_comfort_penalty_defaults_when_absent() {
        let cfg = ControlConfig::from_json5(
            r#"{
                site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                heating: { cop: 1.0, comfort_penalty: 1.0, zones: {} },
                hvac: { comfort: {}, units: {} },
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.hvac.unwrap().comfort_penalty, 50.0); // serde default
    }

    #[test]
    fn cop_spec_constant_and_curve_interpolate() {
        let constant = CopSpec::Constant(3.2);
        assert_eq!(constant.cop_at(-10.0), 3.2);
        assert_eq!(constant.cop_at(35.0), 3.2);

        // Curve: clamped flat beyond the ends, linear between.
        let curve = CopSpec::Curve(vec![
            CopPoint { t: 0.0, cop: 4.0 },
            CopPoint { t: 10.0, cop: 3.0 },
            CopPoint { t: 30.0, cop: 2.0 },
        ]);
        assert_eq!(
            curve.cop_at(-5.0),
            4.0,
            "clamped to first point below the range"
        );
        assert_eq!(
            curve.cop_at(40.0),
            2.0,
            "clamped to last point above the range"
        );
        assert!(
            (curve.cop_at(5.0) - 3.5).abs() < 1e-12,
            "midpoint of 0–10 segment"
        );
        assert!(
            (curve.cop_at(20.0) - 2.5).abs() < 1e-12,
            "midpoint of 10–30 segment"
        );
        assert_eq!(curve.cop_at(10.0), 3.0, "exact breakpoint");
        curve.validate("test").unwrap();
    }

    #[test]
    fn hvac_validate_rejects_unserved_comfort_and_bad_curves() {
        // A unit referencing a zone with no comfort entry is rejected.
        let mut hvac = HvacConfig {
            comfort_penalty: 50.0,
            comfort: HashMap::from([(
                "bedroom".to_string(),
                HvacComfort {
                    t_heat: 20.0,
                    t_cool: 26.0,
                },
            )]),
            units: HashMap::from([(
                "ac".to_string(),
                HvacUnit {
                    zones: vec!["office".to_string()], // not in comfort
                    max_cool_kw: 3.0,
                    max_heat_kw: 3.0,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.5),
                    single_mode: false,
                },
            )]),
        };
        assert!(hvac.validate().is_err(), "unserved zone must fail");

        // A non-ascending COP curve is rejected.
        hvac.comfort.insert(
            "office".to_string(),
            HvacComfort {
                t_heat: 20.0,
                t_cool: 26.0,
            },
        );
        hvac.units.get_mut("ac").unwrap().cooling_cop = CopSpec::Curve(vec![
            CopPoint { t: 30.0, cop: 3.0 },
            CopPoint { t: 10.0, cop: 4.0 },
        ]);
        assert!(hvac.validate().is_err(), "descending curve must fail");
    }

    #[test]
    fn hvac_validate_rejects_non_finite_values() {
        // A well-formed single-zone unit; each clone injects one infinity (NaN is already caught by the
        // ordering/`>= 0` comparisons, but `inf >= 0.0` / `inf > 0.0` slip through without is_finite).
        let base = || HvacConfig {
            comfort_penalty: 50.0,
            comfort: HashMap::from([(
                "office".to_string(),
                HvacComfort {
                    t_heat: 20.0,
                    t_cool: 26.0,
                },
            )]),
            units: HashMap::from([(
                "ac".to_string(),
                HvacUnit {
                    zones: vec!["office".to_string()],
                    max_cool_kw: 3.0,
                    max_heat_kw: 3.0,
                    per_zone_max_kw: HashMap::new(),
                    cooling_cop: CopSpec::Constant(3.0),
                    heating_cop: CopSpec::Constant(3.5),
                    single_mode: false,
                },
            )]),
        };
        assert!(base().validate().is_ok());

        let inf = f64::INFINITY;
        let mut h = base();
        h.comfort_penalty = inf;
        assert!(h.validate().is_err(), "infinite comfort_penalty");
        let mut h = base();
        h.comfort.get_mut("office").unwrap().t_cool = inf;
        assert!(h.validate().is_err(), "infinite comfort edge");
        let mut h = base();
        h.units.get_mut("ac").unwrap().max_cool_kw = inf;
        assert!(h.validate().is_err(), "infinite capacity");
        let mut h = base();
        h.units.get_mut("ac").unwrap().per_zone_max_kw =
            HashMap::from([("office".to_string(), inf)]);
        assert!(h.validate().is_err(), "infinite damper cap");
        let mut h = base();
        h.units.get_mut("ac").unwrap().cooling_cop = CopSpec::Constant(inf);
        assert!(h.validate().is_err(), "infinite constant COP");
        // An infinite curve temperature passes the ascending check (`inf > 0`) but not is_finite.
        let mut h = base();
        h.units.get_mut("ac").unwrap().heating_cop = CopSpec::Curve(vec![
            CopPoint { t: 0.0, cop: 3.0 },
            CopPoint { t: inf, cop: 4.0 },
        ]);
        assert!(h.validate().is_err(), "infinite curve temperature");
    }

    #[test]
    fn hvac_validate_rejects_a_zone_in_two_units() {
        let unit = |zone: &str| HvacUnit {
            zones: vec![zone.to_string()],
            max_cool_kw: 3.0,
            max_heat_kw: 3.0,
            per_zone_max_kw: HashMap::new(),
            cooling_cop: CopSpec::Constant(3.0),
            heating_cop: CopSpec::Constant(3.0),
            single_mode: false,
        };
        let hvac = HvacConfig {
            comfort_penalty: 50.0,
            comfort: HashMap::from([(
                "bedroom".to_string(),
                HvacComfort {
                    t_heat: 20.0,
                    t_cool: 26.0,
                },
            )]),
            units: HashMap::from([
                ("ac1".to_string(), unit("bedroom")),
                ("ac2".to_string(), unit("bedroom")),
            ]),
        };
        assert!(
            hvac.validate().is_err(),
            "a zone served by two units must fail"
        );
    }

    #[test]
    fn charger_validation_rules() {
        // Parse a top-level `chargers` list and run validate_chargers().
        let check = |chargers: &str| -> Result<()> {
            ControlConfig::from_json5(&format!(
                r#"{{ site: {{ latitude: 49.5, longitude: 17.4, utc_offset_hours: 2 }},
                       heating: {{ cop: 3.0, comfort_penalty: 1.0, zones: {{}} }},
                       chargers: {chargers} }}"#
            ))?
            .validate_chargers()
        };
        let wallbox =
            r#"sources: { power: { type: "influx", bucket: "b", measurement: "m", field: "f" } }"#;

        // A controllable charger with a wallbox source is valid.
        check(&format!(
            r#"[{{ name: "g", max_kw: 11, battery_kwh: 75, {wallbox} }}]"#
        ))
        .unwrap();
        // …without any wallbox source it's rejected.
        assert!(check(r#"[{ name: "g", max_kw: 11, battery_kwh: 75 }]"#).is_err());
        // A monitored charger needs a `power` source.
        assert!(
            check(r#"[{ name: "m", control: "monitored", max_kw: 7, battery_kwh: 75 }]"#).is_err()
        );
        check(&format!(
            r#"[{{ name: "m", control: "monitored", max_kw: 7, battery_kwh: 75, {wallbox} }}]"#
        ))
        .unwrap();
        // max_rate_kw above max_kw is rejected.
        assert!(check(&format!(
            r#"[{{ name: "g", max_kw: 11, max_rate_kw: 20, battery_kwh: 75, {wallbox} }}]"#
        ))
        .is_err());
        // Duplicate names are rejected.
        assert!(check(&format!(
            r#"[{{ name: "g", max_kw: 11, battery_kwh: 75, {wallbox} }},
                 {{ name: "g", max_kw: 11, battery_kwh: 75, {wallbox} }}]"#
        ))
        .is_err());
        // A name with a datagram-breaking char (`;`, `=`, newline, CR) is rejected — it would corrupt
        // the Loxone `key=value;…` virtual-input datagram and silently un-actuate the charger.
        for bad in ["a;b", "a=b", "a\nb"] {
            assert!(
                check(&format!(
                    r#"[{{ name: "{bad}", max_kw: 11, battery_kwh: 75, {wallbox} }}]"#
                ))
                .is_err(),
                "charger name {bad:?} must be rejected"
            );
        }
        // Non-finite numeric fields are rejected (an infinity passes a bare `> 0` but poisons the LP).
        for bad in [
            r#"name: "g", max_kw: 1e999, battery_kwh: 75"#,
            r#"name: "g", max_kw: 11, battery_kwh: 1e999"#,
        ] {
            assert!(
                check(&format!(r#"[{{ {bad}, {wallbox} }}]"#)).is_err(),
                "non-finite charger field must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn growatt_locator_defaults_and_overrides() {
        let base = r#"site: { latitude: 0, longitude: 0, utc_offset_hours: 0 },
                      heating: { cop: 3.0, comfort_penalty: 1.0, zones: {} }"#;
        // No data_sources block ⇒ the built-in default: solar/solar/<metric>, native unit.
        let cfg = ControlConfig::from_json5(&format!("{{ {base} }}")).unwrap();
        match cfg.data_sources.growatt_locator("InputPower") {
            SourceLocator::Influx {
                bucket,
                measurement,
                field,
                scale,
                ..
            } => {
                assert_eq!(
                    (bucket.as_str(), measurement.as_str(), field.as_str()),
                    ("solar", "solar", "InputPower")
                );
                assert_eq!(scale, 1.0);
            }
            _ => panic!("default Growatt locator must be Influx"),
        }
        // A mapped metric resolves to the override (any backend); unmapped metrics still default.
        let cfg = ControlConfig::from_json5(&format!(
            r#"{{ {base}, data_sources: {{ growatt: {{
                 SOC: {{ type: "influx", bucket: "b2", measurement: "m2", field: "soc", scale: 0.5 }} }} }} }}"#
        ))
        .unwrap();
        assert!(matches!(cfg.data_sources.growatt_locator("SOC"),
            SourceLocator::Influx { field, scale, .. } if field == "soc" && (scale - 0.5).abs() < 1e-9));
        assert!(matches!(cfg.data_sources.growatt_locator("InputPower"),
            SourceLocator::Influx { field, .. } if field == "InputPower"));
    }
}
