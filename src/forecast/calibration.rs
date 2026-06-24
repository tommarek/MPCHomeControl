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

    /// Apply the correction to a whole forecast series.
    pub fn apply_series(&self, values: &[f64]) -> Vec<f64> {
        values.iter().map(|&v| self.apply(v)).collect()
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
    fn apply_series_scales_each() {
        let c = Calibration::from_totals_default(10.0, 20.0); // scale 2.0
        assert_eq!(c.apply_series(&[1.0, 2.0, 3.0]), vec![2.0, 4.0, 6.0]);
    }
}
