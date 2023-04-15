pub mod sun;

/// Calculate reciprocal sum of reciprocals.
/// Accepts >=2 arguments.
/// If the values are resistances, then the output is resistance when
/// connected in parallel.
/// If the values are conductivity, then the output is conductivity when
/// connected in series.
macro_rules! reciprocal_sum {
    ($head:expr, $( $tail:expr ),+) => {
        ($head.recip() $(+ $tail.recip())*).recip()
    }
}
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
