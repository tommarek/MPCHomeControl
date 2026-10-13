pub mod sun;

use chrono::{DateTime, SecondsFormat, Utc};
use uom::si::f64::ThermodynamicTemperature;
use uom::si::thermodynamic_temperature::{degree_celsius, kelvin};

/// Kelvin → Celsius (the thermal model carries state in kelvin; reports are in Celsius).
pub fn k_to_c(kelvin_value: f64) -> f64 {
    ThermodynamicTemperature::new::<kelvin>(kelvin_value).get::<degree_celsius>()
}

/// Celsius → Kelvin.
pub fn c_to_k(celsius: f64) -> f64 {
    ThermodynamicTemperature::new::<degree_celsius>(celsius).get::<kelvin>()
}

/// Mean of `sum` over `n` samples (0 when `n == 0`).
pub fn mean(sum: f64, n: usize) -> f64 {
    if n > 0 {
        sum / n as f64
    } else {
        0.0
    }
}

/// Root-mean-square from a sum-of-squared-errors over `n` samples (0 when `n == 0`).
pub fn rmse(sse: f64, n: usize) -> f64 {
    mean(sse, n).sqrt()
}

/// Round `x` to `places` decimal places (half away from zero) — for report/JSON output.
pub fn round_to(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

/// `t` as RFC 3339 at whole seconds with a `Z` suffix — the API's timestamp format.
pub fn rfc3339_secs(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Sort `items` in place by a descending `f64` key. `f64::total_cmp` gives a genuine total order —
/// a NaN-as-`Equal` comparator would be intransitive (NaN == 1.0, NaN == 2.0, yet 1.0 < 2.0), which
/// `slice::sort_by` detects since Rust 1.81 and PANICS on ("user-provided comparison is not a total
/// order"). NaN keys sort last (total_cmp places -NaN below every number; the descending flip puts
/// +NaN first — so map NaN to -inf explicitly to keep the worst-first lists honest).
pub fn sort_desc_by_key<T>(items: &mut [T], key: impl Fn(&T) -> f64) {
    let k = |x: &T| {
        let v = key(x);
        if v.is_nan() {
            f64::NEG_INFINITY
        } else {
            v
        }
    };
    items.sort_by(|a, b| k(b).total_cmp(&k(a)));
}

/// Serializes the tests that mutate the process environment (`MPC_*_STORE` path overrides).
///
/// `cargo test` runs them on parallel threads in ONE process, and glibc's `setenv` can realloc
/// `environ` out from under a concurrent `getenv` — a genuine data race (which is why `set_var` is
/// `unsafe` from the 2024 edition), showing up as rare crashes or one test reading another's path.
/// Acquire it for the whole body of any test that touches the environment.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    #[test]
    fn conversions_round_trip() {
        assert!((super::c_to_k(0.0) - 273.15).abs() < 1e-9);
        assert!((super::k_to_c(super::c_to_k(21.5)) - 21.5).abs() < 1e-9);
    }

    #[test]
    fn sort_desc_orders_finite_high_to_low() {
        let mut v = vec![1.0, 3.0, 2.0, 0.5];
        super::sort_desc_by_key(&mut v, |x| *x);
        assert_eq!(v, vec![3.0, 2.0, 1.0, 0.5]);
    }

    #[test]
    fn sort_desc_is_nan_safe() {
        // A non-comparable key must not panic (the call sites only ever pass finite stats, but the
        // comparator stays total either way).
        let mut v = vec![1.0, f64::NAN, 2.0];
        super::sort_desc_by_key(&mut v, |x| *x);
        assert_eq!(v.len(), 3);
    }
}
