pub mod sun;

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

/// Sort `items` in place by a descending `f64` key, treating a non-comparable key (NaN) as equal so
/// the sort is total. Used wherever per-zone error stats are ranked worst-first.
pub fn sort_desc_by_key<T>(items: &mut [T], key: impl Fn(&T) -> f64) {
    items.sort_by(|a, b| {
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Calculate reciprocal sum of reciprocals.
/// Accepts >=2 arguments.
/// If the values are resistances, then the output is resistance when
/// connected in parallel.
/// If the values are conductivity, then the output is conductivity when
/// connected in series. Combines series/parallel thermal resistances (e.g. when collapsing
/// convection + conduction layers).
#[allow(unused_macros)]
macro_rules! reciprocal_sum {
    ($head:expr, $( $tail:expr ),+) => {
        ($head.recip() $(+ $tail.recip())*).recip()
    }
}
#[allow(unused_imports)]
pub(crate) use reciprocal_sum; // Make the macro visible

#[cfg(test)]
mod tests {
    use approx::assert_ulps_eq;
    use proptest::prelude::prop;
    use test_strategy::proptest;

    #[test]
    fn reciprocal_sum_four_identical() {
        assert_eq!(reciprocal_sum!(2.0f64, 2.0f64, 2.0f64, 2.0f64), 0.5);
    }

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
        // `unwrap_or(Equal)` keeps the comparator total either way).
        let mut v = vec![1.0, f64::NAN, 2.0];
        super::sort_desc_by_key(&mut v, |x| *x);
        assert_eq!(v.len(), 3);
    }

    #[proptest]
    fn reciprocal_sum_vec_pairwise_vs_vec(
        #[strategy(prop::collection::vec(prop::num::f64::NORMAL, 1..100))] values: Vec<f64>,
    ) {
        let pairwise = values
            .iter()
            .copied()
            .reduce(|a, b| reciprocal_sum!(a, b))
            .unwrap();
        let expected = values.iter().map(|x| x.recip()).sum::<f64>().recip();
        assert_ulps_eq!(pairwise, expected);
    }
}
