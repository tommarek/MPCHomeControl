//! The terminal SoC valuation: what a kWh left in the battery at the horizon end is worth to the
//! LP — the post-horizon outlook value, the in-horizon-median fallback, and the choice between them.

/// Value (EUR/kWh) of the energy left in the battery at the horizon end — the avoided future import
/// that stored charge represents. Mirrors the loxone MILP terminal value, which is what actually
/// keeps the battery from draining at the horizon edge (loxone's reserve-SoC floor is computed for
/// transparency but penalised at **zero** — the overnight hold is emergent from the objective, not a
/// per-block floor, which would double-count and force "grid-charge at the evening peak").
///
/// = **median** import (spot+dist) net of wear over the horizon (the *typical* worth of stored
/// energy, not the cheapest, which under-values it), **capped** at the cheapest grid-charge
/// break-even (`min_import / round_trip_η`) so it can never alone justify buying grid power to hoard
/// SoC, floored at 0. Apply it to leftover SoC times the discharge-leg efficiency at the call site.
pub(crate) fn terminal_soc_value(
    import_price: &[f64],
    amortisation: f64,
    round_trip_eta: f64,
) -> f64 {
    if import_price.is_empty() {
        return 0.0;
    }
    let mut net: Vec<f64> = import_price.iter().map(|&p| p - amortisation).collect();
    net.sort_by(f64::total_cmp);
    // The true statistical median (averaged two middle elements on an even horizon) — the principled
    // "typical" avoided-import value, with no upward bias toward the higher middle element.
    let mid = net.len() / 2;
    let median = if net.len() % 2 == 1 {
        net[mid]
    } else {
        (net[mid - 1] + net[mid]) / 2.0
    }
    .max(0.0);
    let cheapest = import_price.iter().cloned().fold(f64::INFINITY, f64::min);
    // The break-even cap ("leftover SoC is worth at most re-acquiring it at the cheapest block")
    // only makes sense for a NON-NEGATIVE cheapest price: at a negative price the LP charges to
    // capacity there regardless (it is *paid* to), so the cap stops guarding a phantom
    // charge-and-credit loop and instead collapses the whole plan's terminal value to zero —
    // draining the battery at the horizon edge on any day with one negative-price block.
    let break_even = if cheapest.is_finite() && cheapest >= 0.0 {
        cheapest / round_trip_eta.max(1e-3)
    } else if cheapest.is_finite() {
        f64::INFINITY // negative cheapest: the median alone values the leftover SoC
    } else {
        0.0
    };
    (median.min(break_even)).max(0.0) * 0.99
}

/// Value (EUR/kWh) of the energy left in the battery at the horizon end, from the **post-horizon**
/// import price curve (`post_import`, tariffed EUR/kWh, ~24 h from the TRUE grid end — see
/// `coordinator::day_type_median_curve`) instead of the in-horizon median (see [`terminal_soc_value`]):
/// a stored kWh is worth what it earns at its best post-horizon use, capped by the cheapest
/// re-acquisition at or before that use (the "next attractive charging opportunity" — a night refill
/// before a morning peak caps the peak's value at the refill price, grossed up by the round-trip
/// loss). With `p[u]` the price at use `u` and `R(u) = min_{c ≤ u} max(p[c], 0)` the cheapest
/// non-negative refill at or before `u`:
/// `V = 0.99 · max(0, max_u min(p[u] − amortisation, R(u) / round_trip_eta))`.
/// Monotone non-decreasing in every `p[i]`, `0 ≤ V ≤ max(p)`, floored at 0 (a negative refill price
/// is clamped to 0 — a free refill, never a paid one — and a use whose net value is negative never
/// wins the max). Unlike [`terminal_soc_value`], no in-horizon price enters this formula at all, so
/// one negative in-horizon block cannot collapse the terminal value to 0.
pub(super) fn terminal_soc_value_outlook(
    post_import: &[f64],
    amortisation: f64,
    round_trip_eta: f64,
) -> f64 {
    if post_import.is_empty() {
        return 0.0;
    }
    let eta = round_trip_eta.max(1e-3);
    let mut cheapest_refill_so_far = f64::INFINITY;
    let mut best = 0.0f64;
    for &p in post_import {
        cheapest_refill_so_far = cheapest_refill_so_far.min(p.max(0.0));
        let use_value = p - amortisation;
        let refill_value = cheapest_refill_so_far / eta;
        best = best.max(use_value.min(refill_value));
    }
    (best * 0.99).max(0.0)
}

/// Pick the terminal SoC valuation `current_plan` passes to the LP: the post-horizon outlook value
/// ([`terminal_soc_value_outlook`]) when `post_curve` covers every block, else the in-horizon-median
/// value (`horizon_median`, [`terminal_soc_value`]) — the fallback for a thin/cold-start price
/// history. Returns `(value, source, note)`: `source` is `"outlook"` or `"horizon_median"`; `note`
/// is the placeholder-inputs message, `Some` exactly on the fallback path.
pub(crate) fn select_terminal_value(
    post_curve: &[Option<f64>],
    horizon_median: f64,
    amortisation: f64,
    round_trip_eta: f64,
) -> (f64, &'static str, Option<String>) {
    let uncovered = post_curve.iter().filter(|p| p.is_none()).count();
    if uncovered == 0 {
        let p: Vec<f64> = post_curve
            .iter()
            .map(|p| p.expect("checked uncovered == 0"))
            .collect();
        (
            terminal_soc_value_outlook(&p, amortisation, round_trip_eta),
            "outlook",
            None,
        )
    } else {
        let note = format!(
            "terminal SoC value (outlook uncovered {uncovered}/{} blocks; horizon median)",
            post_curve.len()
        );
        (horizon_median, "horizon_median", Some(note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::prop;
    use test_strategy::proptest;

    #[test]
    fn terminal_value_uses_median_capped_at_break_even() {
        // Break-even cap binds (median 0.30 > cheapest/η = 0.10/0.85 ≈ 0.1176): terminal ≈ 0.1176·0.99.
        let t = terminal_soc_value(&[0.10, 0.20, 0.30, 0.40], 0.0, 0.85);
        assert!((t - 0.10 / 0.85 * 0.99).abs() < 1e-9, "got {t}");
        // The break-even cap (cheapest/η) values leftover SoC above a bare cheapest×0.99 floor.
        assert!(t > 0.10 * 0.99);

        // Median binds when it's the smaller (flat prices, high wear): median (0.40−0.30)=0.10 < cap 0.40.
        let t = terminal_soc_value(&[0.40, 0.40, 0.40, 0.40], 0.30, 1.0);
        assert!((t - 0.10 * 0.99).abs() < 1e-9, "got {t}");

        // True (averaged) median on an even horizon where the median binds under the cap:
        // [.10,.11,.12,.50] → (.11+.12)/2 = .115 < cap .10/.8 = .125 (loxone's upper-middle would be .12).
        let t = terminal_soc_value(&[0.10, 0.11, 0.12, 0.50], 0.0, 0.80);
        assert!((t - 0.115 * 0.99).abs() < 1e-9, "got {t}");
    }

    #[test]
    fn terminal_value_survives_a_negative_price_block() {
        // One negative block must NOT collapse the terminal value to 0 (the LP would then drain
        // the battery at the horizon edge on every such day): the break-even cap is skipped for a
        // negative cheapest price and the median values the leftover SoC.
        let v = terminal_soc_value(&[-0.10, 0.20], 0.0, 0.85);
        assert!((v - 0.05 * 0.99).abs() < 1e-9, "median-valued: {v}");
        // All-negative horizon: median < 0 ⇒ floored at 0 (leftover energy really is worthless).
        assert_eq!(terminal_soc_value(&[-0.10, -0.20], 0.0, 0.85), 0.0);
        assert_eq!(terminal_soc_value(&[], 0.0, 0.85), 0.0);
    }

    #[test]
    fn outlook_value_caps_a_later_peak_at_the_cheapest_preceding_refill() {
        // Night refill 0.10, rising through the morning to a 0.20 peak: every later use is capped
        // at the night refill grossed up by the round-trip loss, not the peak price itself.
        let v = terminal_soc_value_outlook(&[0.10, 0.12, 0.15, 0.20], 0.0, 0.85);
        assert!((v - 0.99 * 0.10 / 0.85).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn outlook_value_opening_block_wins_after_wear_when_nothing_cheaper_follows() {
        // The night block (0.20) opens the curve, so its own price is its own refill cap; later,
        // cheaper blocks only depress the refill cap for blocks after them.
        let v = terminal_soc_value_outlook(&[0.20, 0.10, 0.15], 0.03, 1.0);
        assert!((v - 0.99 * 0.17).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn outlook_value_evening_peak_as_the_first_block_is_uncapped() {
        // Nothing precedes the opening peak, so its own price is its own refill — the use value
        // wins outright, not the (irrelevant) refill division.
        let v = terminal_soc_value_outlook(&[0.22, 0.05, 0.10], 0.0, 0.85);
        assert!((v - 0.99 * 0.22).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn outlook_value_cheapest_block_first_caps_every_later_use() {
        let v = terminal_soc_value_outlook(&[0.05, 0.20, 0.15], 0.0, 0.85);
        assert!((v - 0.99 * 0.05 / 0.85).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn outlook_value_flat_curve_is_price_minus_wear_and_zero_once_wear_catches_up() {
        let v = terminal_soc_value_outlook(&[0.10; 4], 0.03, 0.85);
        assert!((v - 0.99 * 0.07).abs() < 1e-9, "got {v}");
        // Wear at or above the flat price: every use is non-positive, floored at 0.
        assert_eq!(terminal_soc_value_outlook(&[0.10; 3], 0.15, 0.85), 0.0);
        assert_eq!(terminal_soc_value_outlook(&[0.10; 3], 0.10, 0.85), 0.0);
    }

    #[test]
    fn outlook_value_negative_prices_are_clamped_not_paid_for() {
        // A negative opening block offers no refill (clamped to 0) and is itself a losing use.
        assert_eq!(terminal_soc_value_outlook(&[-0.10, 0.30], 0.0, 0.85), 0.0);
        // A later negative block resets the refill cap to 0 for everything after it.
        let v = terminal_soc_value_outlook(&[0.30, -0.10, 0.30], 0.03, 0.85);
        assert!((v - 0.99 * 0.27).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn outlook_value_all_negative_or_empty_is_zero() {
        assert_eq!(
            terminal_soc_value_outlook(&[-0.10, -0.20, -0.30], 0.0, 0.85),
            0.0
        );
        assert_eq!(terminal_soc_value_outlook(&[], 0.0, 0.85), 0.0);
    }

    #[test]
    fn outlook_value_depends_on_order_not_just_the_multiset() {
        // Same two prices, opposite order: cheap-then-dear caps the dear use; dear-then-cheap does
        // not (nothing precedes the dear opening block).
        let cheap_then_dear = terminal_soc_value_outlook(&[0.05, 0.20], 0.0, 0.85);
        let dear_then_cheap = terminal_soc_value_outlook(&[0.20, 0.05], 0.0, 0.85);
        assert!((cheap_then_dear - 0.99 * 0.05 / 0.85).abs() < 1e-9);
        assert!((dear_then_cheap - 0.99 * 0.20).abs() < 1e-9);
        assert!(
            dear_then_cheap > cheap_then_dear,
            "the two orderings must value the SAME prices differently"
        );
    }

    #[test]
    fn select_terminal_value_fully_covered_uses_the_outlook_with_no_note() {
        let post_curve = vec![Some(0.10), Some(0.20)];
        let (v, source, note) = select_terminal_value(&post_curve, 0.08, 0.0, 0.85);
        assert!((v - terminal_soc_value_outlook(&[0.10, 0.20], 0.0, 0.85)).abs() < 1e-12);
        assert_eq!(source, "outlook");
        assert!(note.is_none());
    }

    #[test]
    fn select_terminal_value_one_uncovered_block_falls_back_with_a_note() {
        let mut post_curve = vec![Some(0.20); 96];
        post_curve[5] = None;
        let (v, source, note) = select_terminal_value(&post_curve, 0.08, 0.0, 0.85);
        assert_eq!(v, 0.08);
        assert_eq!(source, "horizon_median");
        let note = note.expect("an uncovered block must produce a placeholder note");
        assert!(
            note.contains("uncovered 1/96"),
            "note must say exactly how many blocks were uncovered: {note}"
        );
    }

    #[proptest]
    fn outlook_value_is_bounded_and_monotone(
        #[strategy(prop::collection::vec(-1.0..2.0f64, 1..12))] prices: Vec<f64>,
        #[strategy(0.0..0.5f64)] amortisation: f64,
        #[strategy(0.3..1.0f64)] eta: f64,
        #[strategy(0..12usize)] bump_index: usize,
    ) {
        let v = terminal_soc_value_outlook(&prices, amortisation, eta);
        let max_p = prices
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max)
            .max(0.0);
        assert!(v >= 0.0);
        assert!(v <= max_p + 1e-9);

        // Increasing any one price can only raise (never lower) the value.
        let idx = bump_index % prices.len();
        let mut bumped = prices.clone();
        bumped[idx] += 0.37;
        let v_bumped = terminal_soc_value_outlook(&bumped, amortisation, eta);
        assert!(v_bumped >= v - 1e-9);
    }
}
