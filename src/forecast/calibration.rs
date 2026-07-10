//! Online self-correction for the forecast models.
//!
//! Every forecast carries a systematic bias — the house's Solcast PV curve, for instance,
//! under-predicts actual generation by ~20-25%. Rather than hand-tune the models, we recompute a
//! multiplicative **calibration** from a trailing window of (forecast, actual) pairs on each cycle
//! and apply it to the next forecast. Because it is recomputed from recent realized data every
//! run, the correction automatically tracks reality as the days go by — the models stay reliable
//! without manual intervention. The same primitive applies to any forecast with a measured
//! outcome (PV, consumption, …).

/// A multiplicative forecast correction fit from realized data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    scale: f64,
}

impl Default for Calibration {
    fn default() -> Self {
        Self::neutral()
    }
}

impl Calibration {
    /// Sensible bounds on the correction, so a sparse or degenerate trailing window can't produce
    /// a wild multiplier.
    pub const DEFAULT_MIN: f64 = 0.5;
    pub const DEFAULT_MAX: f64 = 2.0;

    /// No correction (scale 1).
    pub fn neutral() -> Self {
        Self { scale: 1.0 }
    }

    /// Fit a correction from realized totals over the trailing window: `actual / forecast`,
    /// clamped to `[min, max]`. Falls back to neutral if the forecast total is non-positive or the
    /// inputs are not finite (not enough signal to trust a correction).
    pub fn from_totals(forecast_total: f64, actual_total: f64, min: f64, max: f64) -> Self {
        if forecast_total <= 0.0
            || !forecast_total.is_finite()
            || !actual_total.is_finite()
            || actual_total < 0.0
        {
            return Self::neutral();
        }
        Self {
            scale: (actual_total / forecast_total).clamp(min, max),
        }
    }

    /// [`Self::from_totals`] with the default clamp `[0.5, 2.0]`.
    pub fn from_totals_default(forecast_total: f64, actual_total: f64) -> Self {
        Self::from_totals(
            forecast_total,
            actual_total,
            Self::DEFAULT_MIN,
            Self::DEFAULT_MAX,
        )
    }

    /// The multiplicative factor (1.0 = no correction).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Apply the correction to a forecast value.
    pub fn apply(&self, value: f64) -> f64 {
        value * self.scale
    }
}

/// Hour-band-aware PV calibration: one multiplicative ratio per local-time band (morning / midday
/// / evening) plus the overall scalar as fallback. A totals-based scalar corrects ENERGY but not
/// TIMING — Solcast's bias differs most on the shoulders of the day (horizon effects, array
/// orientation), exactly where the battery's morning grid-charge vs wait-for-PV and evening
/// export-vs-hold decisions live.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PvBandCalibration {
    bands: [Calibration; 3],
    overall: Calibration,
}

impl PvBandCalibration {
    /// Band of a local hour-of-day: 0 = morning (< 11), 1 = midday (11–14), 2 = evening (≥ 15).
    pub fn band_of_hour(local_hour: u32) -> usize {
        match local_hour {
            h if h < 11 => 0,
            h if h < 15 => 1,
            _ => 2,
        }
    }

    pub fn neutral() -> Self {
        Self {
            bands: [Calibration::neutral(); 3],
            overall: Calibration::neutral(),
        }
    }

    /// Fit per-band ratios from the backtest's clean-hour sums; a band with fewer than
    /// `min_band_hours` scored hours falls back to the overall ratio (never a ratio from a couple
    /// of cloudy shoulder hours).
    pub fn from_backtest(
        band_forecast_kwh: [f64; 3],
        band_actual_kwh: [f64; 3],
        band_hours: [usize; 3],
        overall: Calibration,
        min_band_hours: usize,
    ) -> Self {
        /// A band also needs real ENERGY, not just clean hours: shoulder hours can be "clean" at
        /// near-zero kWh, and a ratio fit from noise would just hit the clamp (or degrade to
        /// neutral on a zero forecast sum, silently replacing the intended overall fallback).
        const MIN_BAND_KWH: f64 = 2.0;
        let mut bands = [overall; 3];
        for b in 0..3 {
            if band_hours[b] >= min_band_hours
                && band_forecast_kwh[b] >= MIN_BAND_KWH
                && band_actual_kwh[b] > 0.0
            {
                bands[b] =
                    Calibration::from_totals_default(band_forecast_kwh[b], band_actual_kwh[b]);
            }
        }
        Self { bands, overall }
    }

    /// Calibrate one value forecast for the given local hour.
    pub fn apply_at(&self, value: f64, local_hour: u32) -> f64 {
        self.bands[Self::band_of_hour(local_hour)].apply(value)
    }

    /// The energy-level (overall) scale, for reporting.
    pub fn overall_scale(&self) -> f64 {
        self.overall.scale()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    #[test]
    fn neutral_is_identity() {
        let c = Calibration::neutral();
        assert_eq!(c.scale(), 1.0);
        assert_eq!(c.apply(3.5), 3.5);
    }

    #[test]
    fn fits_the_actual_over_forecast_ratio() {
        // Forecast under-predicted (347 vs 456) -> scale ~1.31 corrects future forecasts up.
        let c = Calibration::from_totals_default(347.0, 456.0);
        assert_abs_diff_eq!(c.scale(), 456.0 / 347.0, epsilon = 1e-12);
        assert_abs_diff_eq!(c.apply(10.0), 10.0 * 456.0 / 347.0, epsilon = 1e-9);
    }

    #[test]
    fn clamps_extreme_ratios() {
        assert_eq!(Calibration::from_totals_default(1.0, 100.0).scale(), 2.0); // capped high
        assert_eq!(Calibration::from_totals_default(100.0, 1.0).scale(), 0.5); // capped low
    }

    #[test]
    fn degenerate_inputs_fall_back_to_neutral() {
        assert_eq!(Calibration::from_totals_default(0.0, 5.0).scale(), 1.0);
        assert_eq!(Calibration::from_totals_default(-1.0, 5.0).scale(), 1.0);
        assert_eq!(Calibration::from_totals_default(5.0, f64::NAN).scale(), 1.0);
        assert_eq!(Calibration::from_totals_default(5.0, -3.0).scale(), 1.0); // negative actual
    }

    #[test]
    fn band_calibration_uses_band_ratios_with_overall_fallback() {
        // Morning under-predicted 2x (>= 8 clean hours), evening has too few hours -> overall 1.2.
        let overall = Calibration::from_totals_default(100.0, 120.0);
        let c = PvBandCalibration::from_backtest(
            [10.0, 50.0, 2.0],
            [20.0, 55.0, 4.0],
            [10, 30, 3],
            overall,
            8,
        );
        assert!(
            (c.apply_at(1.0, 8) - 2.0).abs() < 1e-9,
            "morning band ratio"
        );
        assert!(
            (c.apply_at(1.0, 12) - 1.1).abs() < 1e-9,
            "midday band ratio"
        );
        assert!(
            (c.apply_at(1.0, 18) - 1.2).abs() < 1e-9,
            "sparse evening falls back to overall"
        );
        assert!((c.overall_scale() - 1.2).abs() < 1e-9);
    }
}
