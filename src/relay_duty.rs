//! Event-based duty cycle for the on-change-logged heating relay.
//!
//! The relay (`loxone`/`relay`/`tag1=heating`) logs a point only on a state CHANGE — a handful per
//! room per day — so averaging those raw edge values per hour is a mean of samples, not a duty
//! cycle: a quiet hour with no edge reads as "off" even when the relay held fully on. [`relay_duty`]
//! integrates the on-change events instead, giving the true time-weighted ON fraction of a window.

use chrono::{DateTime, Duration, TimeZone, Utc};

/// How far back a live relay read looks up each relay's last state before its window. The relay
/// logs only on change, so a zone held in one state longer than this has no record at all (read as
/// OFF); a pushed-down `last()` over this span stays one cheap query.
pub(crate) const RELAY_PRIOR_STATE_LOOKBACK_DAYS: i64 = 365;

// --- Pure: duty reconstruction -------------------------------------------------------------------

/// Reconstruct the time-weighted relay duty (0..1) over `[t, t+dt)` from on-change events — the
/// relay logs a point only when it flips, so an `aggregateWindow(mean)` over a quiet window would
/// read "no data", not "off". `events` must be sorted ascending by time; each entry is the relay's
/// new value as of that instant, holding until the next event. `None` when no event at or before
/// `t` is known — there is no way to tell what the relay was doing without a starting state, and a
/// guessed 0 would silently fabricate "measured no heat" (see [`crate::ledger::Measured::heat_kwh`]).
pub fn relay_duty(events: &[(DateTime<Utc>, f64)], t: DateTime<Utc>, dt: Duration) -> Option<f64> {
    let total = dt.num_seconds() as f64;
    if total <= 0.0 {
        return Some(0.0);
    }
    let end = t + dt;
    let start_pos = events.iter().rposition(|(et, _)| *et <= t)?;
    let mut weighted = 0.0;
    let mut cursor = t;
    let mut value = events[start_pos].1;
    for (et, v) in &events[start_pos + 1..] {
        if *et >= end {
            break;
        }
        if *et > cursor {
            weighted += value * (*et - cursor).num_seconds() as f64;
            cursor = *et;
        }
        value = *v;
    }
    weighted += value * (end - cursor).num_seconds() as f64;
    Some((weighted / total).clamp(0.0, 1.0))
}

/// Time-weighted ON fraction of the hour ENDING at each of `hours` (unix-hour keys, the same
/// stop-stamped convention `read_heating_kw`/`build_input` use), from raw on-change relay events —
/// the true duty cycle, built on [`relay_duty`]. `state_before` (the last known event before the
/// read window, if any) seeds the state for the first hour(s); with no prior state (`None`) the
/// relay is assumed OFF from the start of time (the caller must flag this).
pub(crate) fn relay_duty_hourly(
    events: &[(DateTime<Utc>, f64)],
    state_before: Option<(DateTime<Utc>, f64)>,
    hours: &[i64],
) -> Vec<f64> {
    let mut combined: Vec<(DateTime<Utc>, f64)> = Vec::with_capacity(events.len() + 1);
    if let Some(sb) = state_before {
        combined.push(sb);
    } else if let Some(&first_hour) = hours.first() {
        // No known prior state: assume OFF from well before the window (a day is ample margin —
        // `relay_duty` only looks for the LATEST event at-or-before each hour's start).
        let origin = Utc.timestamp_opt((first_hour - 24) * 3600, 0).single();
        if let Some(t) = origin {
            combined.push((t, 0.0));
        }
    }
    combined.extend_from_slice(events);
    combined.sort_by_key(|(t, _)| *t);
    hours
        .iter()
        .map(|&h| {
            let end = Utc.timestamp_opt(h * 3600, 0).single().unwrap_or_default();
            let start = end - Duration::hours(1);
            relay_duty(&combined, start, Duration::hours(1)).unwrap_or(0.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn hk(s: &str) -> i64 {
        t(s).timestamp().div_euclid(3600)
    }

    // ---------- relay_duty ----------

    #[test]
    fn relay_duty_on_throughout() {
        let events = vec![(t("2026-09-30T00:00:00Z"), 1.0)];
        let duty = relay_duty(&events, t("2026-09-30T01:00:00Z"), Duration::minutes(15)).unwrap();
        assert!((duty - 1.0).abs() < 1e-9);
    }

    #[test]
    fn relay_duty_off_throughout() {
        let events = vec![(t("2026-09-30T00:00:00Z"), 0.0)];
        let duty = relay_duty(&events, t("2026-09-30T01:00:00Z"), Duration::minutes(15)).unwrap();
        assert!(duty.abs() < 1e-9);
    }

    #[test]
    fn relay_duty_switches_inside_block() {
        // Off at 01:00, flips on at 01:05 — on for 10 of the 15 minutes.
        let events = vec![
            (t("2026-09-30T00:00:00Z"), 0.0),
            (t("2026-09-30T01:05:00Z"), 1.0),
        ];
        let duty = relay_duty(&events, t("2026-09-30T01:00:00Z"), Duration::minutes(15)).unwrap();
        assert!((duty - (10.0 / 15.0)).abs() < 1e-9, "duty was {duty}");
    }

    #[test]
    fn relay_duty_unknown_with_no_prior_event() {
        let events = vec![(t("2026-09-30T02:00:00Z"), 1.0)]; // only an event AFTER the block
        assert!(relay_duty(&events, t("2026-09-30T01:00:00Z"), Duration::minutes(15)).is_none());
        assert!(relay_duty(&[], t("2026-09-30T01:00:00Z"), Duration::minutes(15)).is_none());
    }

    // ---------- relay_duty_hourly ----------

    #[test]
    fn relay_duty_hourly_matches_the_lead_example() {
        // 2026-01-10: ON 04:15 -> 06:30. True duty: 04-05h=0.75, 05-06h=1.0, 06-07h=0.5.
        let events = vec![
            (t("2026-01-10T04:15:00Z"), 1.0),
            (t("2026-01-10T06:30:00Z"), 0.0),
        ];
        let state_before = Some((t("2026-01-10T00:00:00Z"), 0.0));
        let hours = vec![
            hk("2026-01-10T05:00:00Z"),
            hk("2026-01-10T06:00:00Z"),
            hk("2026-01-10T07:00:00Z"),
        ];
        let duty = relay_duty_hourly(&events, state_before, &hours);
        assert!((duty[0] - 0.75).abs() < 1e-9, "{duty:?}");
        assert!((duty[1] - 1.0).abs() < 1e-9, "{duty:?}");
        assert!((duty[2] - 0.5).abs() < 1e-9, "{duty:?}");
    }

    #[test]
    fn relay_duty_hourly_scores_a_blip_near_zero_and_holds_through_a_quiet_hour() {
        // An isolated 1-s blip inside 02-03h scores near-zero; 05-06h has no event at all but the
        // relay held on from 04:15.
        let events = vec![
            (t("2026-01-10T02:30:00Z"), 1.0),
            (t("2026-01-10T02:30:01Z"), 0.0),
            (t("2026-01-10T04:15:00Z"), 1.0),
            (t("2026-01-10T06:30:00Z"), 0.0),
        ];
        let hours = vec![hk("2026-01-10T03:00:00Z"), hk("2026-01-10T06:00:00Z")];
        let state_before = Some((t("2026-01-10T00:00:00Z"), 0.0));
        let true_duty = relay_duty_hourly(&events, state_before, &hours);
        // True duty for 02-03h: on for 1 second only.
        assert!(true_duty[0] < 0.01, "{true_duty:?}");
        // True duty for 05-06h: fully on (04:15 on, still on through 06:30).
        assert!((true_duty[1] - 1.0).abs() < 1e-9, "{true_duty:?}");
    }

    #[test]
    fn relay_duty_hourly_event_exactly_on_the_hour_boundary() {
        // An event AT the hour boundary belongs to the hour that STARTS there, not the one ending.
        let events = vec![(t("2026-01-10T05:00:00Z"), 1.0)];
        let state_before = Some((t("2026-01-10T00:00:00Z"), 0.0));
        let hours = vec![hk("2026-01-10T05:00:00Z"), hk("2026-01-10T06:00:00Z")];
        let duty = relay_duty_hourly(&events, state_before, &hours);
        assert!(duty[0] < 1e-9, "04-05h should still read OFF: {duty:?}");
        assert!(
            (duty[1] - 1.0).abs() < 1e-9,
            "05-06h should read ON: {duty:?}"
        );
    }

    #[test]
    fn relay_duty_hourly_with_no_state_before_assumes_off() {
        let events = vec![(t("2026-01-10T05:30:00Z"), 1.0)];
        let hours = vec![hk("2026-01-10T05:00:00Z"), hk("2026-01-10T06:00:00Z")];
        let duty = relay_duty_hourly(&events, None, &hours);
        assert!((duty[0] - 0.0).abs() < 1e-9);
        assert!((duty[1] - 0.5).abs() < 1e-9);
    }

    // ---------- relay_duty_hourly edge cases ----------

    #[test]
    fn relay_duty_hourly_no_events_state_before_on_holds_every_hour() {
        let state_before = Some((t("2026-01-10T00:00:00Z"), 1.0));
        let hours = vec![
            hk("2026-01-10T05:00:00Z"),
            hk("2026-01-10T06:00:00Z"),
            hk("2026-01-10T07:00:00Z"),
        ];
        let duty = relay_duty_hourly(&[], state_before, &hours);
        assert!(duty.iter().all(|&d| (d - 1.0).abs() < 1e-9), "{duty:?}");
    }

    #[test]
    fn relay_duty_hourly_no_events_state_before_off_holds_every_hour() {
        let state_before = Some((t("2026-01-10T00:00:00Z"), 0.0));
        let hours = vec![hk("2026-01-10T05:00:00Z"), hk("2026-01-10T06:00:00Z")];
        let duty = relay_duty_hourly(&[], state_before, &hours);
        assert!(duty.iter().all(|&d| d.abs() < 1e-9), "{duty:?}");
    }

    #[test]
    fn relay_duty_hourly_multiple_toggles_within_one_hour() {
        // ON 10 min, OFF 20 min, ON 30 min within a single hour -> 40/60 ON exactly.
        let state_before = Some((t("2026-01-10T05:00:00Z"), 1.0));
        let events = vec![
            (t("2026-01-10T05:10:00Z"), 0.0),
            (t("2026-01-10T05:30:00Z"), 1.0),
        ];
        let hours = vec![hk("2026-01-10T06:00:00Z")];
        let duty = relay_duty_hourly(&events, state_before, &hours);
        assert!((duty[0] - (40.0 / 60.0)).abs() < 1e-9, "{duty:?}");
    }

    #[test]
    fn relay_duty_hourly_spans_dst_spring_forward_cet_to_cest() {
        // 2026-03-29 01:00Z is the Europe/Prague CET -> CEST changeover (A1: the hour grid is UTC
        // stop-stamped, so the transition itself is invisible — this proves that rather than
        // asserting it, by checking total duty-hours == total measured ON time and no hour > 1).
        let state_before = Some((t("2026-03-28T23:00:00Z"), 0.0));
        let events = vec![
            (t("2026-03-29T00:30:00Z"), 1.0),
            (t("2026-03-29T02:15:00Z"), 0.0),
        ];
        let hours = vec![
            hk("2026-03-29T01:00:00Z"),
            hk("2026-03-29T02:00:00Z"),
            hk("2026-03-29T03:00:00Z"),
        ];
        let duty = relay_duty_hourly(&events, state_before, &hours);
        for d in &duty {
            assert!(*d <= 1.0 + 1e-9, "duty exceeded 1.0: {duty:?}");
        }
        let total_duty_hours: f64 = duty.iter().sum();
        let total_on_time_hours = 1.75; // 30 + 60 + 15 minutes ON, spread across the three hours
        assert!(
            (total_duty_hours - total_on_time_hours).abs() < 1e-9,
            "{duty:?}"
        );
    }
}
