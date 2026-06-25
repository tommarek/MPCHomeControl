//! Temperature-aware electricity-consumption forecast.
//!
//! Ported from `loxone_smart_home`'s `consumption_forecast.py`: a binned lookup model
//! `(temperature_bucket, hour, is_weekend) → median kWh`, built from historical hourly
//! samples with IQR outlier removal and a fallback chain for sparse bins. Designed for an
//! all-electric house where heating load dominates in winter.
//!
//! Pure and IO-free — callers feed it samples (e.g. read from InfluxDB) via
//! [`ConsumptionModel::add_sample`], then call [`ConsumptionModel::build`] before predicting.

use std::collections::HashMap;

const TEMP_BUCKET_SIZE: f64 = 2.0;
const TEMP_MIN: f64 = -20.0;
const TEMP_MAX: f64 = 40.0;
/// 30 buckets of 2 °C spanning −20..40 °C.
const NUM_BUCKETS: i32 = 30;
/// A 1–3 sample "median" is just noise, so require at least this many before a bin is trusted
/// as an exact-key median. Sparse bins still feed the hourly/global fallbacks.
const MIN_BIN_SAMPLES: usize = 4;
/// Hard default used only when there is no data at all.
const DEFAULT_KWH: f64 = 1.0;

/// `(temp_bucket, hour, is_weekend)`
type BinKey = (i32, u32, bool);

/// Binned consumption lookup model (see module docs).
#[derive(Debug, Clone)]
pub struct ConsumptionModel {
    bins: HashMap<BinKey, Vec<f64>>,
    medians: HashMap<BinKey, f64>,
    hourly_fallback: HashMap<u32, f64>,
    global_median: f64,
    data_points: usize,
}

impl Default for ConsumptionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsumptionModel {
    pub fn new() -> Self {
        // Seed `global_median` with `DEFAULT_KWH` so an un-built model predicts a sane fallback
        // (rather than the 0 kWh a zero-initialised field would give).
        ConsumptionModel {
            bins: HashMap::new(),
            medians: HashMap::new(),
            hourly_fallback: HashMap::new(),
            global_median: DEFAULT_KWH,
            data_points: 0,
        }
    }

    /// Temperature (°C) → bucket index `0..=29` (30 buckets of 2 °C from −20 to 40 °C).
    pub fn temp_to_bucket(temperature: f64) -> i32 {
        let clamped = temperature.clamp(TEMP_MIN, TEMP_MAX);
        (((clamped - TEMP_MIN) / TEMP_BUCKET_SIZE) as i32).min(NUM_BUCKETS - 1)
    }

    /// Record one historical hourly observation: `kwh` consumed at `temperature` (°C), during
    /// `hour` (0–23), on a weekend day or not.
    pub fn add_sample(&mut self, temperature: f64, hour: u32, is_weekend: bool, kwh: f64) {
        self.bins
            .entry((Self::temp_to_bucket(temperature), hour, is_weekend))
            .or_default()
            .push(kwh);
        self.data_points += 1;
    }

    /// Compute the per-bin medians (with IQR outlier removal) and the hourly/global fallbacks.
    /// Call once after adding samples and before [`predict`](Self::predict).
    pub fn build(&mut self) {
        self.medians.clear();
        self.hourly_fallback.clear();
        let mut hourly_all: HashMap<u32, Vec<f64>> = HashMap::new();
        let mut all_bin_medians: Vec<f64> = Vec::new();

        for (key, values) in &self.bins {
            // IQR outlier removal using interpolated quartiles (see `linear_quantile`).
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            let q1 = linear_quantile(&sorted, 0.25);
            let q3 = linear_quantile(&sorted, 0.75);
            let iqr = q3 - q1;
            let (lower, upper) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
            let filtered: Vec<f64> = values
                .iter()
                .copied()
                .filter(|v| (lower..=upper).contains(v))
                .collect();
            let bin_median = median(if filtered.is_empty() {
                values
            } else {
                &filtered
            });

            // Only trust a bin as an exact-key median once it has enough samples; sparse bins
            // are left out so the fallback chain handles them, but still feed the fallbacks.
            if values.len() >= MIN_BIN_SAMPLES {
                self.medians.insert(*key, bin_median);
            }
            hourly_all.entry(key.1).or_default().push(bin_median);
            all_bin_medians.push(bin_median);
        }

        for (hour, vals) in &hourly_all {
            self.hourly_fallback.insert(*hour, median(vals));
        }
        self.global_median = if all_bin_medians.is_empty() {
            DEFAULT_KWH
        } else {
            median(&all_bin_medians)
        };
    }

    /// Predicted consumption (kWh) for a one-hour period, using the fallback chain:
    /// exact bin → opposite day type → adjacent temperature buckets → hourly median → global.
    pub fn predict(&self, temperature: f64, hour: u32, is_weekend: bool) -> f64 {
        let bucket = Self::temp_to_bucket(temperature);
        if let Some(&m) = self.medians.get(&(bucket, hour, is_weekend)) {
            return m;
        }
        if let Some(&m) = self.medians.get(&(bucket, hour, !is_weekend)) {
            return m;
        }
        // Adjacent temperature buckets, same day type only (matches the source model: the
        // opposite-day-type relaxation above is preferred over a temperature mismatch).
        for delta in [1, -1, 2, -2] {
            if let Some(&m) = self.medians.get(&(bucket + delta, hour, is_weekend)) {
                return m;
            }
        }
        self.hourly_fallback
            .get(&hour)
            .copied()
            .unwrap_or(self.global_median)
    }

    pub fn data_points(&self) -> usize {
        self.data_points
    }
}

/// Linear-interpolated quantile `p` (0..1) of an already-sorted slice. Matches the Python
/// `_linear_quantile`: at the minimum bin size (n=4) raw order statistics put q3 on the max
/// element, so the IQR fence would never trim — interpolation fixes exactly the sparse,
/// noisy bins that need it most.
fn linear_quantile(sorted: &[f64], p: f64) -> f64 {
    match sorted {
        [] => 0.0,
        [only] => *only,
        _ => {
            let idx = p * (sorted.len() - 1) as f64;
            let lo = idx.floor() as usize;
            let hi = (lo + 1).min(sorted.len() - 1);
            let frac = idx - lo as f64;
            sorted[lo] * (1.0 - frac) + sorted[hi] * frac
        }
    }
}

/// Median matching Python's `statistics.median` (mean of the two middle values for even n).
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut s = values.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_buckets_span_minus20_to_40() {
        assert_eq!(ConsumptionModel::temp_to_bucket(-20.0), 0);
        assert_eq!(ConsumptionModel::temp_to_bucket(-100.0), 0); // clamped low
        assert_eq!(ConsumptionModel::temp_to_bucket(-19.0), 0);
        assert_eq!(ConsumptionModel::temp_to_bucket(0.0), 10);
        assert_eq!(ConsumptionModel::temp_to_bucket(40.0), 29);
        assert_eq!(ConsumptionModel::temp_to_bucket(100.0), 29); // clamped high
    }

    #[test]
    fn median_matches_python_semantics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5); // mean of the two middle
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn linear_quantile_interpolates() {
        let s = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(linear_quantile(&s, 0.0), 1.0);
        assert_eq!(linear_quantile(&s, 1.0), 4.0);
        assert_eq!(linear_quantile(&s, 0.25), 1.75);
        assert_eq!(linear_quantile(&[5.0], 0.9), 5.0);
        assert_eq!(linear_quantile(&[], 0.5), 0.0);
    }

    #[test]
    fn exact_bin_used_when_enough_samples() {
        let mut m = ConsumptionModel::new();
        for kwh in [3.0, 3.2, 3.4, 3.6] {
            m.add_sample(5.0, 8, false, kwh);
        }
        m.build();
        assert_eq!(m.data_points(), 4);
        assert_eq!(m.predict(5.0, 8, false), 3.3);
    }

    #[test]
    fn iqr_removes_outlier() {
        let mut m = ConsumptionModel::new();
        for kwh in [2.0, 2.1, 2.2, 2.3, 100.0] {
            m.add_sample(5.0, 8, false, kwh);
        }
        m.build();
        // 100.0 is outside the IQR fence; median of the kept {2.0,2.1,2.2,2.3} = 2.15.
        assert!((m.predict(5.0, 8, false) - 2.15).abs() < 1e-9);
    }

    #[test]
    fn fallback_chain() {
        let mut m = ConsumptionModel::new();
        for kwh in [4.0; 4] {
            m.add_sample(5.0, 8, false, kwh); // bucket 12
        }
        for kwh in [6.0; 4] {
            m.add_sample(7.0, 8, false, kwh); // bucket 13
        }
        m.build();
        assert_eq!(m.predict(5.0, 8, false), 4.0); // exact bin
        assert_eq!(m.predict(5.0, 8, true), 4.0); // opposite day type
        assert_eq!(m.predict(9.0, 8, false), 6.0); // adjacent bucket (14 → 13)
        assert_eq!(m.predict(35.0, 8, false), 5.0); // hourly fallback = median(4, 6)
        assert_eq!(m.predict(5.0, 14, false), 5.0); // global fallback = median(4, 6)
    }
}
