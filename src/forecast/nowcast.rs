//! PV intraday nowcast: blend the last hour's measured-vs-forecast ratio into the coming few
//! hours of the PV curve. Solcast gets the day's *shape* right but misses today's actual cloud —
//! the trailing ratio is a cheap, zero-latency correction for exactly the blocks the battery's
//! next dispatch decision rides on. Pure math only; `app.rs` supplies the measured/forecast
//! samples and applies [`blend`] to the live plan's PV curve, `pv_backtest.rs` replays the same
//! math against history as the accuracy proof.

/// Why [`nowcast_ratio`] could not produce a ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NowcastSkip {
    /// The forecast over the sample window averaged below the configured floor (dawn/dusk/night —
    /// too small a denominator to trust a measured/forecast ratio).
    LowForecast { mean_kw: f64 },
    /// No (measured, forecast) pairs were supplied.
    NoSamples,
}

/// `r = Σ measured / Σ forecast` over `samples`, clamped to `clamp`. Skipped (no ratio) when the
/// forecast's MEAN over the samples is below `min_forecast_kw`, the forecast sum is non-positive,
/// or there are no samples — in all three cases the ratio would be either undefined (a zero or
/// negative denominator) or dominated by sensor/rounding noise. The `sum_forecast <= 0.0` check is
/// load-bearing independent of `min_forecast_kw`: a misconfigured `min_forecast_kw: 0` must not let
/// a zero forecast sum through to `sum_measured / sum_forecast` (NaN or ±inf, which would then
/// reach `blend` and corrupt `pv_kw`). The clamp's positive lower bound keeps a RETURNED result
/// always finite and `> 0`.
pub fn nowcast_ratio(
    samples: &[(f64, f64)],
    min_forecast_kw: f64,
    clamp: (f64, f64),
) -> Result<f64, NowcastSkip> {
    if samples.is_empty() {
        return Err(NowcastSkip::NoSamples);
    }
    let sum_forecast: f64 = samples.iter().map(|&(_, f)| f).sum();
    let mean_forecast = sum_forecast / samples.len() as f64;
    if sum_forecast <= 0.0 || !mean_forecast.is_finite() || mean_forecast < min_forecast_kw {
        return Err(NowcastSkip::LowForecast {
            mean_kw: mean_forecast,
        });
    }
    let sum_measured: f64 = samples.iter().map(|&(m, _)| m).sum();
    let r = sum_measured / sum_forecast;
    Ok(r.clamp(clamp.0, clamp.1))
}

/// Exponential-decay blend weight at lead time `tau_hours` (hours from now to the block start,
/// clamped by the caller to `>= 0`): `exp(-τ/efold_hours)` for `0 <= τ < max_hours`, else `0` — no
/// nowcast influence beyond the configured horizon, where the trailing ratio has nothing to say
/// about weather that far out.
pub fn weight(tau_hours: f64, efold_hours: f64, max_hours: f64) -> f64 {
    if tau_hours < 0.0 || tau_hours >= max_hours {
        0.0
    } else {
        (-tau_hours / efold_hours).exp()
    }
}

/// Blend `r` into `pv_kw` in place: `pv[b] *= 1 + w(τ_b)·(r − 1)`, `τ_b = tau_hours(b)`. Returns
/// the count of blocks with nonzero weight — since `tau_hours` is non-decreasing in `b` (the plan
/// grid's block starts only move forward), these are exactly blocks `0..touched`.
pub fn blend(
    pv_kw: &mut [f64],
    tau_hours: impl Fn(usize) -> f64,
    r: f64,
    efold_hours: f64,
    max_hours: f64,
) -> usize {
    let mut touched = 0;
    for (b, kw) in pv_kw.iter_mut().enumerate() {
        let w = weight(tau_hours(b), efold_hours, max_hours);
        if w > 0.0 {
            *kw *= 1.0 + w * (r - 1.0);
            touched += 1;
        }
    }
    touched
}

/// The hourly-replay parity weight for target hour `h+k` (`k` = 1, 2 or 3 hours ahead of the
/// reference hour `h`): the MEAN of [`weight`] over the four 15-min block starts inside hour `k`
/// (`τ ∈ {k−1, k−0.75, k−0.5, k−0.25}`) — exact parity with the live blend, which multiplies each
/// 15-min block individually rather than treating the whole hour as one block (the forecast is
/// flat within an hour, so this is the only place the two paths could otherwise diverge).
pub fn hour_weight(k: u32, efold_hours: f64, max_hours: f64) -> f64 {
    let k = f64::from(k);
    [k - 1.0, k - 0.75, k - 0.5, k - 0.25]
        .iter()
        .map(|&tau| weight(tau, efold_hours, max_hours))
        .sum::<f64>()
        / 4.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    const EFOLD_H: f64 = 1.5;
    const MAX_H: f64 = 3.0;
    const CLAMP: (f64, f64) = (0.3, 1.5);

    #[test]
    fn worked_example_from_spec() {
        // 14:30 UTC "now", r = 0.8 (measured 4.0 kW vs calibrated forecast 5.0 kW over the last
        // hour). The 15:00 block (tau = 0.5 h) forecast 2.4 kW -> ~2.06 kW; the 18:00 block
        // (tau = 3.5 h, beyond max_hours) is untouched.
        let r = nowcast_ratio(&[(4.0, 5.0)], 0.5, CLAMP).unwrap();
        assert_abs_diff_eq!(r, 0.8, epsilon = 1e-12);

        let mut pv_kw = [2.4, 1.0];
        let tau = |b: usize| if b == 0 { 0.5 } else { 3.5 };
        let touched = blend(&mut pv_kw, tau, r, EFOLD_H, MAX_H);
        assert_eq!(touched, 1);
        assert_abs_diff_eq!(pv_kw[0], 2.0561, epsilon = 1e-3);
        assert_abs_diff_eq!(pv_kw[1], 1.0, epsilon = 1e-12); // untouched
    }

    #[test]
    fn clamp_binds_both_ends() {
        // Measured far exceeds forecast -> clamped to the hi bound.
        let r_hi = nowcast_ratio(&[(10.0, 1.0)], 0.5, CLAMP).unwrap();
        assert_abs_diff_eq!(r_hi, 1.5, epsilon = 1e-12);
        // Measured far below forecast -> clamped to the lo bound.
        let r_lo = nowcast_ratio(&[(0.05, 1.0)], 0.5, CLAMP).unwrap();
        assert_abs_diff_eq!(r_lo, 0.3, epsilon = 1e-12);
    }

    #[test]
    fn low_forecast_gates_out() {
        let err = nowcast_ratio(&[(0.1, 0.2)], 0.5, CLAMP).unwrap_err();
        assert_eq!(err, NowcastSkip::LowForecast { mean_kw: 0.2 });
    }

    #[test]
    fn zero_forecast_sum_gates_out_even_with_zero_min_forecast_kw() {
        // A misconfigured `min_forecast_kw: 0` must not let sum_forecast == 0 reach the division
        // (NaN/inf, which would then corrupt `blend`'s output).
        let err = nowcast_ratio(&[(1.0, 0.0), (2.0, 0.0)], 0.0, CLAMP).unwrap_err();
        assert_eq!(err, NowcastSkip::LowForecast { mean_kw: 0.0 });
    }

    #[test]
    fn no_samples_is_skipped() {
        assert_eq!(
            nowcast_ratio(&[], 0.5, CLAMP).unwrap_err(),
            NowcastSkip::NoSamples
        );
    }

    #[test]
    fn ratio_one_is_a_blend_no_op() {
        let mut pv_kw = [3.0, 4.0, 5.0];
        let before = pv_kw;
        blend(&mut pv_kw, |b| b as f64 * 0.5, 1.0, EFOLD_H, MAX_H);
        assert_eq!(pv_kw, before);
    }

    #[test]
    fn decay_reaches_zero_at_and_beyond_max_hours() {
        assert_eq!(weight(MAX_H, EFOLD_H, MAX_H), 0.0);
        assert_eq!(weight(MAX_H + 1.0, EFOLD_H, MAX_H), 0.0);
        assert_eq!(weight(-0.1, EFOLD_H, MAX_H), 0.0); // negative tau (block before "now") excluded
    }

    #[test]
    fn weight_at_zero_lead_is_one() {
        assert_eq!(weight(0.0, EFOLD_H, MAX_H), 1.0);
    }

    #[test]
    fn hour_weight_matches_mean_of_quarter_hour_weights() {
        let w = hour_weight(1, EFOLD_H, MAX_H);
        let expect = (weight(0.0, EFOLD_H, MAX_H)
            + weight(0.25, EFOLD_H, MAX_H)
            + weight(0.5, EFOLD_H, MAX_H)
            + weight(0.75, EFOLD_H, MAX_H))
            / 4.0;
        assert_abs_diff_eq!(w, expect, epsilon = 1e-12);
    }
}
