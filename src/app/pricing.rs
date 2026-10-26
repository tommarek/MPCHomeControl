//! Electricity pricing for the plan: OTE spot → tariffed import/export, the placeholder curve, and
//! the per-block fill of unpublished prices (estimate → persistence → placeholder).

use chrono::{DateTime, Duration, FixedOffset, Timelike, Utc};

use super::BLOCK_SECONDS;
use crate::optimize::config::{SiteConfig, TariffConfig};

/// Apply the real tariff to a spot-price series, returning `(import_price, export_price)` in
/// EUR/kWh per 15-min block. Import adds the VT/NT distribution surcharge for each block's local hour
/// — the offset is derived **per block** ([`SiteConfig::offset_at`]), so the VT/NT classification
/// stays correct across a DST changeover inside the horizon (exactly the hours that shift);
/// export is the spot minus the sell fee, floored at 0 (no benefit to exporting below the fee) and
/// capped at the import price so the dispatch LP's `export ≤ import` precondition always holds.
pub(crate) fn tariff_prices(
    tariff: &TariffConfig,
    site: &SiteConfig,
    spot_price: &[f64],
    start: DateTime<Utc>,
) -> (Vec<f64>, Vec<f64>) {
    let mask = tariff.low_tariff_mask();
    let sell_fee = tariff.sell_fee_eur();
    spot_price
        .iter()
        .enumerate()
        .map(|(b, &spot)| {
            let at = start + Duration::seconds((BLOCK_SECONDS * b as f64) as i64);
            let local_hour = at.with_timezone(&site.offset_at(at)).hour();
            let import = spot + tariff.distribution_eur(local_hour, &mask);
            // `.min(import)` is load-bearing: it guarantees export ≤ import for ALL spot prices
            // (including deeply-negative hours where `import` itself goes negative), which the
            // dispatch LP requires to stay bounded. Do not "simplify" it to a bare `.max(0.0)`.
            let export = (spot - sell_fee).max(0.0).min(import);
            (import, export)
        })
        .unzip()
}

pub(crate) fn placeholder_price_curve(
    start: DateTime<Utc>,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset,
    n: usize,
) -> Vec<f64> {
    // The peak (17–20) / off-peak (1–5) windows are local-time tariff hours, so classify each
    // block by ITS OWN local hour, at ITS OWN offset (cf. `tariff_prices`) — deriving hours from
    // the block index assumed an on-the-hour start, shifting the windows by up to 45 min on the
    // 3-of-4 ticks that start mid-hour. Levels approximate the recent CZ spot shape (≈0.10
    // EUR/kWh base) — the old 0.25/0.45 placeholder priced the pre-auction tail ~2× reality and
    // skewed the afternoon look-ahead's arbitrage.
    (0..n)
        .map(|b| {
            let at = start + Duration::seconds(BLOCK_SECONDS as i64 * b as i64);
            match at.with_timezone(&offset(at)).hour() {
                17..=20 => 0.18,
                1..=5 => 0.04,
                _ => 0.10,
            }
        })
        .collect()
}

/// Fill each block's price: the block's own published value if any, else the DAY-TYPE MEDIAN
/// estimate (`optimize::price_forecast` — the backtested-better predictor: median
/// price at the same local clock slot over the most recent same-day-type days), else the real
/// price published for the same clock block one day earlier (persistence), else the fixed
/// placeholder curve. Returns `(spot_price, price_is_placeholder, missing, persisted, estimated)`
/// — the mask is `true` for every block that wasn't itself published, regardless of which fallback
/// filled it (an estimated or day-old price is still not today's real spread, so battery arbitrage
/// against it stays banned); `missing` counts how many blocks fell back at all, `estimated` how
/// many of those used the day-type median, `persisted` how many of the REMAINDER used the day-ago
/// real price rather than the fixed curve.
pub(crate) fn fill_block_prices(
    current: &[Option<f64>],
    estimated: &[Option<f64>],
    day_ago: &[Option<f64>],
    placeholder: &[f64],
) -> (Vec<f64>, Vec<bool>, usize, usize, usize) {
    let mut missing = 0usize;
    let mut persisted = 0usize;
    let mut used_estimate = 0usize;
    let mut price = Vec::with_capacity(current.len());
    let mut is_placeholder = Vec::with_capacity(current.len());
    for (b, &p) in current.iter().enumerate() {
        match p {
            Some(v) => {
                price.push(v);
                is_placeholder.push(false);
            }
            None => {
                missing += 1;
                is_placeholder.push(true);
                match estimated.get(b).copied().flatten() {
                    Some(v) => {
                        used_estimate += 1;
                        price.push(v);
                    }
                    None => match day_ago.get(b).copied().flatten() {
                        Some(v) => {
                            persisted += 1;
                            price.push(v);
                        }
                        None => price.push(placeholder[b]),
                    },
                }
            }
        }
    }
    (price, is_placeholder, missing, persisted, used_estimate)
}

/// Describe WHICH fallback route(s) filled the `missing` blocks, for the plan's `placeholders`
/// text — a single source keeps the plain word ("persistence", "day-type median", "placeholder");
/// more than one in play reports the mix (a run blending, say, 60 day-type-median blocks with 30
/// persistence ones must not read as plain "day-type median", hiding that nearly a third of the
/// horizon fell all the way to persistence).
pub(super) fn price_fallback_label(missing: usize, estimated: usize, persisted: usize) -> String {
    let fixed_curve = missing.saturating_sub(estimated + persisted);
    let mut parts = Vec::new();
    if estimated > 0 {
        parts.push(("day-type median", estimated));
    }
    if persisted > 0 {
        parts.push(("persistence", persisted));
    }
    if fixed_curve > 0 {
        parts.push(("fixed curve", fixed_curve));
    }
    match parts.as_slice() {
        [] => "placeholder".to_string(), // unreachable when `missing > 0`; kept as a safe default
        [("fixed curve", _)] => "placeholder".to_string(), // preserve the existing single-source word
        [(label, _)] => (*label).to_string(),
        _ => parts
            .iter()
            .map(|(label, n)| format!("{n} {label}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::*;
    use crate::app::HORIZON_BLOCKS;

    #[test]
    fn placeholder_curve_classifies_by_local_hour() {
        // 23:00 UTC. In UTC+2 that is 01:00 local → off-peak (0.04); the curve must use the local hour.
        let start = utc("2024-01-01T23:00:00Z");
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert!((placeholder_price_curve(start, |_| plus2, 1)[0] - 0.04).abs() < 1e-9);
        // The same instant is 23:00 in UTC → the regular band (0.10), not off-peak.
        let utc0 = FixedOffset::east_opt(0).unwrap();
        assert!((placeholder_price_curve(start, |_| utc0, 1)[0] - 0.10).abs() < 1e-9);

        // Mid-hour start: each block keys to ITS OWN local hour, not block-index arithmetic.
        // 16:45 local start (UTC+0): block 0 is hour 16 (base), block 1 (17:00) is peak.
        let start = utc("2024-01-01T16:45:00Z");
        let curve = placeholder_price_curve(start, |_| utc0, 2);
        assert!((curve[0] - 0.10).abs() < 1e-9, "16:45 is still base");
        assert!((curve[1] - 0.18).abs() < 1e-9, "17:00 is peak");
    }

    #[test]
    fn placeholder_curve_follows_a_dst_changeover() {
        // Europe/Prague falls back at 2026-10-25 01:00 UTC (+2 → +1). Every assertion after the
        // changeover would flip under the plan start's fixed +2: 15:00 UTC (16:00 local) would
        // read as a 17:00 peak, 19:45 UTC (20:45 local, peak) as 21:45 base.
        let site = SiteConfig {
            latitude: 49.5,
            longitude: 17.4,
            utc_offset_hours: 2,
            timezone: Some("Europe/Prague".to_string()),
            ground_temperature_c: 16.0,
            public_holidays: Vec::new(),
            easter_holidays: false,
        };
        let start = utc("2026-10-24T22:00:00Z");
        let curve = placeholder_price_curve(start, |t| site.offset_at(t), HORIZON_BLOCKS);
        let block = |t: &str| ((utc(t) - start).num_seconds() / BLOCK_SECONDS as i64) as usize;
        assert!((curve[block("2026-10-25T15:00:00Z")] - 0.10).abs() < 1e-9);
        assert!((curve[block("2026-10-25T19:45:00Z")] - 0.18).abs() < 1e-9);
        // 04:00 UTC is 05:00 local (off-peak), not 06:00.
        assert!((curve[block("2026-10-25T04:00:00Z")] - 0.04).abs() < 1e-9);
        // Before the changeover +2 still applies: 23:00 UTC is 01:00 local.
        assert!((curve[block("2026-10-24T23:00:00Z")] - 0.04).abs() < 1e-9);
    }

    #[test]
    fn fill_block_prices_persists_before_falling_to_placeholder() {
        // 144-block horizon: blocks 100-143 unpublished today; the SAME clock blocks a day
        // earlier are real for 100-119 only, so 120-143 must fall all the way to the fixed curve.
        let mut current = vec![Some(0.10); HORIZON_BLOCKS];
        for p in current.iter_mut().skip(100) {
            *p = None;
        }
        let mut day_ago = vec![None; HORIZON_BLOCKS];
        for (b, p) in day_ago.iter_mut().enumerate().take(120).skip(100) {
            *p = Some(0.08 + b as f64 * 1e-4); // distinct per-block values
        }
        let placeholder: Vec<f64> = (0..HORIZON_BLOCKS).map(|_| 0.5).collect();
        let estimated = vec![None; HORIZON_BLOCKS]; // no day-type estimate in this scenario
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 44);
        assert_eq!(persisted, 20);
        assert_eq!(estimated_count, 0);
        for b in 0..100 {
            assert!(
                (price[b] - 0.10).abs() < 1e-9,
                "block {b} published unchanged"
            );
            assert!(!is_placeholder[b]);
        }
        for b in 100..120 {
            assert!(
                (price[b] - day_ago[b].unwrap()).abs() < 1e-9,
                "block {b} must equal its day-ago real price"
            );
            assert!(
                is_placeholder[b],
                "persisted block still counts as placeholder"
            );
        }
        for b in 120..HORIZON_BLOCKS {
            assert!(
                (price[b] - 0.5).abs() < 1e-9,
                "block {b} with neither source falls to the fixed curve"
            );
            assert!(is_placeholder[b]);
        }
    }

    #[test]
    fn fill_block_prices_reports_placeholder_when_nothing_persisted() {
        let current = vec![None; 4];
        let estimated = vec![None; 4];
        let day_ago = vec![None; 4];
        let placeholder = vec![0.5; 4];
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 4);
        assert_eq!(persisted, 0);
        assert_eq!(estimated_count, 0);
        assert!(price.iter().all(|&p| (p - 0.5).abs() < 1e-9));
        assert!(is_placeholder.iter().all(|&f| f));
    }

    /// The day-type median estimate is tried BEFORE day-ago persistence
    /// and the fixed placeholder — a block with both an estimate and a day-ago real price must use
    /// the estimate.
    #[test]
    fn fill_block_prices_prefers_estimate_over_persistence_and_placeholder() {
        let current = vec![None; 3]; // all three blocks unpublished
        let estimated = vec![Some(0.07), None, None]; // only block 0 has an estimate
        let day_ago = vec![Some(0.09), Some(0.11), None]; // blocks 0 and 1 have a day-ago price
        let placeholder = vec![0.5; 3];
        let (price, is_placeholder, missing, persisted, estimated_count) =
            fill_block_prices(&current, &estimated, &day_ago, &placeholder);
        assert_eq!(missing, 3);
        assert_eq!(estimated_count, 1);
        assert_eq!(persisted, 1); // only block 1 falls through to day-ago
        assert!(
            (price[0] - 0.07).abs() < 1e-9,
            "block 0 must use the estimate (0.07), not day-ago (0.09): got {}",
            price[0]
        );
        assert!((price[1] - 0.11).abs() < 1e-9, "block 1 falls to day-ago");
        assert!(
            (price[2] - 0.5).abs() < 1e-9,
            "block 2 falls to the placeholder"
        );
        assert!(is_placeholder.iter().all(|&f| f));
    }

    /// The placeholder label reports the MIX when more than one fallback
    /// route filled blocks, and keeps the plain single-word label when only one did.
    #[test]
    fn price_fallback_label_reports_the_mix_or_a_single_source() {
        assert_eq!(price_fallback_label(60, 60, 0), "day-type median");
        assert_eq!(price_fallback_label(30, 0, 30), "persistence");
        assert_eq!(price_fallback_label(4, 0, 0), "placeholder");
        assert_eq!(
            price_fallback_label(94, 60, 30),
            "60 day-type median, 30 persistence, 4 fixed curve"
        );
        assert_eq!(
            price_fallback_label(90, 60, 30),
            "60 day-type median, 30 persistence" // no fixed-curve blocks this time
        );
    }

    /// End-to-end unit scale: realistic OTE EUR/MWh samples (60-180), converted to
    /// EUR/kWh the SAME way `build_cache` converts them at the cache boundary
    /// (`price_eur_mwh / 1000.0`), must produce a `fill_block_prices` estimate on the SAME scale
    /// as real horizon blocks (0.06-0.18 EUR/kWh) — not 1000x too high.
    #[test]
    fn fill_block_prices_estimate_from_realistic_eur_mwh_history_lands_in_eur_kwh_range() {
        let target = utc("2024-01-15T05:00:00Z"); // Monday, hour 5
                                                  // Mirrors `build_cache`'s own `s.price_eur_mwh / 1000.0` conversion at the cache boundary.
        let raw_eur_mwh = [80.0_f64, 100.0];
        let history: Vec<(DateTime<Utc>, f64)> =
            [utc("2024-01-01T05:00:00Z"), utc("2024-01-08T05:00:00Z")]
                .into_iter()
                .zip(raw_eur_mwh)
                .map(|(t, mwh)| (t, mwh / 1000.0))
                .collect();
        let offset = FixedOffset::east_opt(0).unwrap();
        let estimate = crate::optimize::price_forecast::day_type_median_price(
            &history,
            target,
            |_| offset,
            &[],
            false,
        )
        .expect("2 same-type days must be enough");
        assert!(
            (0.06..=0.18).contains(&estimate),
            "estimate from realistic OTE EUR/MWh history (80, 100) must land in the real EUR/kWh \
             horizon-price range (0.06-0.18), got {estimate} — a 1000x unit bug would show ~90.0"
        );
        assert!(
            (estimate - 0.09).abs() < 1e-9,
            "median(0.08, 0.10) = 0.09, got {estimate}"
        );
    }

    #[test]
    fn tariff_prices_apply_distribution_and_sell_fee() {
        let t = tariff();
        let start = utc("2024-01-15T00:00:00Z");
        // 48 15-min blocks = 12 h. Block 0 = 00:00 (NT/low), block 40 = 10:00 (VT/high).
        let spot = vec![0.10; 48]; // EUR/kWh
        let (import, export) = tariff_prices(&t, &utc_site(), &spot, start);
        assert!((import[0] - (0.10 + 0.281 / 25.0)).abs() < 1e-12); // NT
        assert!((import[40] - (0.10 + 0.919 / 25.0)).abs() < 1e-12); // VT at 10:00
                                                                     // Export = spot − sell fee, and never exceeds import (the LP precondition).
        assert!((export[0] - (0.10 - 0.5 / 25.0)).abs() < 1e-12);
        for h in 0..spot.len() {
            assert!(
                export[h] <= import[h] + 1e-12,
                "export must not exceed import at {h}"
            );
        }
    }

    #[test]
    fn tariff_prices_floor_export_below_the_sell_fee() {
        let t = tariff();
        let start = utc("2024-01-15T00:00:00Z");
        // Spot below the export floor (0.5/25 = 0.02 EUR): export floored to 0, still ≤ import.
        let (import, export) = tariff_prices(&t, &utc_site(), &[0.005; 24], start);
        for h in 0..24 {
            assert_eq!(export[h], 0.0);
            assert!(export[h] <= import[h] + 1e-12);
        }
    }
}
