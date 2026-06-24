//! PV (photovoltaic) production forecast.
//!
//! Converts plane-of-array solar irradiance (from [`crate::tools::sun`]) into AC power for a
//! PV array. This is a clear-sky / physical baseline: output scales linearly with
//! plane-of-array irradiance relative to the 1000 W/m² STC reference, derated by a system
//! efficiency. The `loxone_smart_home` system refines this with Solcast; that cloud-aware
//! forecast can later be layered in as an InfluxDB-backed data source.
//!
//! Pure and IO-free — given a site, time and cloud cover it returns power.

use chrono::{DateTime, Duration, Utc};
use uom::si::{
    f64::{Angle, Energy, HeatFluxDensity, Power, Ratio, Time},
    heat_flux_density::watt_per_square_meter,
    time::hour,
};

use crate::tools::sun::calculate_tilted_irradiance;

/// Peak instantaneous power across a forecast series.
pub fn peak_power(series: &[(DateTime<Utc>, Power)]) -> Power {
    series
        .iter()
        .map(|(_, p)| *p)
        .fold(Power::default(), |a, b| a.max(b))
}

/// Total energy of an **hourly** forecast series (each sample held for one hour). Typed as a
/// `uom` `Energy` so callers can't accidentally read kW samples as kWh.
pub fn hourly_energy(series: &[(DateTime<Utc>, Power)]) -> Energy {
    let step = Time::new::<hour>(1.0);
    series.iter().map(|(_, p)| *p * step).sum()
}

/// A PV array (a set of panels sharing one orientation and inverter).
#[derive(Debug, Clone, Copy)]
pub struct PvArray {
    /// Nameplate DC power at STC (1000 W/m², i.e. `kWp`).
    pub peak_power: Power,
    /// Tilt from horizontal (0° = flat, 90° = vertical).
    pub tilt: Angle,
    /// Surface azimuth (compass bearing the panels face).
    pub azimuth: Angle,
    /// Combined derate for inverter + wiring + temperature losses (≈ 0.8–0.9).
    pub system_efficiency: Ratio,
}

impl PvArray {
    /// Predicted AC output at a single instant for the given site and cloud cover.
    pub fn predict(
        &self,
        latitude: Angle,
        longitude: Angle,
        datetime: &DateTime<Utc>,
        cloud_cover: Ratio,
    ) -> Power {
        let irradiance = calculate_tilted_irradiance(
            latitude,
            longitude,
            datetime,
            cloud_cover,
            self.tilt,
            self.azimuth,
        );
        // Output scales with plane-of-array irradiance relative to the STC reference, clipped
        // at the array rating: irradiance can exceed 1000 W/m² (low air mass / reflections),
        // but a real inverter cannot deliver more than its nameplate power.
        let reference = HeatFluxDensity::new::<watt_per_square_meter>(1000.0);
        let fraction: Ratio = irradiance / reference;
        (self.peak_power * fraction * self.system_efficiency).min(self.peak_power)
    }

    /// Hourly production forecast over a horizon: `hours` samples starting at `start`.
    pub fn predict_series(
        &self,
        latitude: Angle,
        longitude: Angle,
        start: &DateTime<Utc>,
        hours: u32,
        cloud_cover: Ratio,
    ) -> Vec<(DateTime<Utc>, Power)> {
        (0..hours)
            .map(|h| {
                let t = *start + Duration::hours(h as i64);
                (t, self.predict(latitude, longitude, &t, cloud_cover))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uom::si::{angle::degree, power::kilowatt, ratio::ratio};

    fn array() -> PvArray {
        PvArray {
            peak_power: Power::new::<kilowatt>(5.0),
            tilt: Angle::new::<degree>(30.0),
            azimuth: Angle::new::<degree>(180.0), // due south
            system_efficiency: Ratio::new::<ratio>(0.85),
        }
    }

    fn site() -> (Angle, Angle) {
        (Angle::new::<degree>(49.49), Angle::new::<degree>(17.43))
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn no_output_at_night() {
        let (lat, lon) = site();
        let p = array().predict(
            lat,
            lon,
            &utc("2023-06-21T23:00:00Z"),
            Ratio::new::<ratio>(0.0),
        );
        assert_eq!(p.get::<kilowatt>(), 0.0);
    }

    #[test]
    fn clear_noon_positive_and_within_rating() {
        let (lat, lon) = site();
        let kw = array()
            .predict(
                lat,
                lon,
                &utc("2023-06-21T11:00:00Z"),
                Ratio::new::<ratio>(0.0),
            )
            .get::<kilowatt>();
        assert!(kw > 0.0, "expected positive output, got {kw}");
        assert!(kw <= 5.0, "output {kw} kW exceeds the 5 kWp nameplate");
    }

    #[test]
    fn clouds_reduce_output() {
        let (lat, lon) = site();
        let noon = utc("2023-06-21T11:00:00Z");
        let clear = array().predict(lat, lon, &noon, Ratio::new::<ratio>(0.0));
        let cloudy = array().predict(lat, lon, &noon, Ratio::new::<ratio>(1.0));
        assert!(cloudy.get::<kilowatt>() < clear.get::<kilowatt>());
    }

    #[test]
    fn series_has_requested_length() {
        let (lat, lon) = site();
        let series = array().predict_series(
            lat,
            lon,
            &utc("2023-06-21T00:00:00Z"),
            24,
            Ratio::new::<ratio>(0.2),
        );
        assert_eq!(series.len(), 24);
        // Midnight entries are dark, midday entries produce.
        assert_eq!(series[0].1.get::<kilowatt>(), 0.0);
        assert!(series.iter().any(|(_, p)| p.get::<kilowatt>() > 0.0));
    }

    #[test]
    fn series_summaries() {
        use uom::si::energy::kilowatt_hour;
        let (lat, lon) = site();
        let series = array().predict_series(
            lat,
            lon,
            &utc("2023-06-21T00:00:00Z"),
            24,
            Ratio::new::<ratio>(0.0),
        );
        let peak = peak_power(&series).get::<kilowatt>();
        let energy = hourly_energy(&series).get::<kilowatt_hour>();
        assert!(peak > 0.0 && peak <= 5.0);
        // Energy is positive and can't exceed peak held for the whole 24 h window.
        assert!(energy > 0.0 && energy <= peak * 24.0 + 1e-9);
    }
}
