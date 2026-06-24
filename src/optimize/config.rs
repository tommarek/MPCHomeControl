//! Control configuration: site location and per-zone heating / comfort settings.
//!
//! Parsed from the same `config.json5` the InfluxDB layer reads — neither side sets
//! `deny_unknown_fields`, so the `site` and `heating` blocks coexist with `db`/`zone_mappings`
//! and each deserializer ignores the other's keys.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

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

// Hand-written so a wholly-absent `pv` block (where `ControlConfig`'s `#[serde(default)]` calls
// `PvConfig::default()`) keeps the real 0.85 efficiency — a derived `Default` would zero it.
impl Default for PvConfig {
    fn default() -> Self {
        Self {
            system_efficiency: default_pv_system_efficiency(),
            arrays: Vec::new(),
        }
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
    /// A divisor that can never be zero (the config's `gt=0` analogue), so price math is finite.
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
                for slot in mask.iter_mut().take(end.min(24)).skip(start.min(24)) {
                    *slot = true;
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

    /// Validate (non-empty curve, ascending temperatures, positive COPs / constant).
    fn validate(&self, ctx: &str) -> Result<()> {
        match self {
            CopSpec::Constant(c) => {
                anyhow::ensure!(*c > 0.0, "{ctx}: COP must be positive (got {c})");
            }
            CopSpec::Curve(points) => {
                anyhow::ensure!(!points.is_empty(), "{ctx}: COP curve has no points");
                for p in points {
                    anyhow::ensure!(p.cop > 0.0, "{ctx}: COP must be positive (got {})", p.cop);
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
        anyhow::ensure!(
            self.comfort_penalty >= 0.0,
            "hvac.comfort_penalty must be non-negative"
        );
        for (zone, c) in &self.comfort {
            anyhow::ensure!(
                c.t_cool >= c.t_heat,
                "hvac.comfort[{zone}]: t_cool ({}) must be ≥ t_heat ({})",
                c.t_cool,
                c.t_heat
            );
        }
        for (name, unit) in &self.units {
            anyhow::ensure!(!unit.zones.is_empty(), "hvac unit {name:?} serves no zones");
            anyhow::ensure!(
                unit.max_cool_kw >= 0.0 && unit.max_heat_kw >= 0.0,
                "hvac unit {name:?}: capacities must be non-negative"
            );
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

impl ControlConfig {
    pub fn from_json5(text: &str) -> Result<Self> {
        Ok(json5::from_str(text)?)
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_json5(&std::fs::read_to_string(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Regression: the derived `Default` would zero this; the explicit impl keeps 0.85.
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
    fn low_tariff_out_of_order_or_oob_ranges_mark_nothing() {
        // `.take(end).skip(start)` yields an empty range when start ≥ end or start ≥ 24, so a
        // malformed (out-of-order / out-of-range) segment safely marks no hours (stays VT).
        let t = TariffConfig {
            low_tariff_hours: "20-10,30-32".to_string(),
            ..TariffConfig::default()
        };
        assert_eq!(t.low_tariff_mask(), [false; 24]);
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
                                                                       // The two new recalibration knobs fall back to their defaults when absent.
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
}
