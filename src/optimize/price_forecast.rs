//! Day-type median price estimator.
//!
//! Backtested (`scratchpad/pricestudy/backtest.py`, OTE 2022-06..2026-09, leads 2-8 days) against
//! plain repeat-yesterday persistence and found consistently better: cheapest-4-hour regret 5.6 vs
//! 6.7 EUR/MWh over the last 12 months (6.6 vs 10.6 over the whole history), hourly MAE 25.8 vs
//! 34.0. The day-ahead spot price follows the WEEKDAY/SATURDAY/SUNDAY demand shape far more than it
//! repeats yesterday exactly, and a public holiday follows the Sunday shape (low industrial/office
//! demand) — so the best predictor of "Tuesday 18:00 two days from now" is the median of the last
//! few *actual Tuesdays* at 18:00, not last Monday's number.
//!
//! Pure, IO-free: the caller supplies the price history (already bounded/cached/read) and the
//! holiday calendar (already parsed config data) — this module only computes.

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, Timelike, Utc, Weekday};

/// The three demand shapes OTE spot prices follow, by LOCAL calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayType {
    /// Monday-Friday, not a public holiday.
    Work,
    Sat,
    /// Sunday, OR a public holiday (same low-industrial-demand shape).
    Sun,
}

/// The demand-shape day type of `date`, given the site's public-holiday calendar.
pub fn day_type(date: NaiveDate, public_holidays: &[(u32, u32)], easter_holidays: bool) -> DayType {
    if date.weekday() == Weekday::Sun || is_holiday(date, public_holidays, easter_holidays) {
        DayType::Sun
    } else if date.weekday() == Weekday::Sat {
        DayType::Sat
    } else {
        DayType::Work
    }
}

fn is_holiday(date: NaiveDate, public_holidays: &[(u32, u32)], easter_holidays: bool) -> bool {
    if public_holidays.contains(&(date.month(), date.day())) {
        return true;
    }
    if easter_holidays {
        let easter = easter_sunday(date.year());
        // Good Friday (-2) and Easter Monday (+1) — the two Czech Easter public holidays; Easter
        // Sunday itself is already a Sunday.
        if date == easter - Duration::days(2) || date == easter + Duration::days(1) {
            return true;
        }
    }
    false
}

/// The Gregorian-calendar date of Easter Sunday in `year` — the "anonymous Gregorian algorithm"
/// (Meeus/Jones/Butcher computus). Valid for any Gregorian-calendar year; the house only ever
/// evaluates years near "now", well within its domain.
pub fn easter_sunday(year: i32) -> NaiveDate {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .expect("computus always yields a valid March/April date")
}

/// Parse a `"MM-DD"` public-holiday entry; `None` if malformed or out of range (validated at
/// config load — see `ControlConfig::validate`'s `validate_site`).
pub fn parse_month_day(md: &str) -> Option<(u32, u32)> {
    let (m, d) = md.split_once('-')?;
    let month: u32 = m.parse().ok()?;
    let day: u32 = d.parse().ok()?;
    // NaiveDate::from_ymd_opt on a leap year (2024) validates day-of-month bounds for every month,
    // including Feb 29 — a cheap, correct way to reject e.g. "02-30" or "13-01" without hand-rolled
    // month-length tables.
    NaiveDate::from_ymd_opt(2024, month, day)?;
    Some((month, day))
}

/// The day-type median price (same price-units as `history`'s values, e.g. EUR/kWh) at `target`'s
/// local clock slot: the median over the most recent `K = 4` days of the SAME day type as
/// `target`'s own local date, at the SAME local minute-of-day, drawn from `history` — `(time,
/// price)` samples, assumed already REAL (published, non-placeholder) and already bounded to a
/// sane window (the caller's job; this function does not filter by recency itself beyond "strictly
/// before `target`'s own local date"). Fewer than `2` matching days returns `None` — the caller
/// then falls back to its own persistence/placeholder chain.
///
/// `local_offset` computes the UTC→local offset for a given instant — pass a closure over
/// `SiteConfig::offset_at` for genuine DST-aware per-instant conversion where the caller has a
/// `SiteConfig` in hand, or `|_| some_fixed_offset` (matching `ForecastContext::local_offset`'s
/// existing single-offset convention) where it doesn't.
pub fn day_type_median_price(
    history: &[(DateTime<Utc>, f64)],
    target: DateTime<Utc>,
    local_offset: impl Fn(DateTime<Utc>) -> FixedOffset,
    public_holidays: &[(u32, u32)],
    easter_holidays: bool,
) -> Option<f64> {
    const K: usize = 4;
    const MIN_DAYS: usize = 2;

    let target_local = target.with_timezone(&local_offset(target));
    let target_date = target_local.date_naive();
    let target_type = day_type(target_date, public_holidays, easter_holidays);
    let target_minute = target_local.hour() * 60 + target_local.minute();

    let mut by_day: Vec<(NaiveDate, f64)> = history
        .iter()
        .filter_map(|&(t, price)| {
            let local = t.with_timezone(&local_offset(t));
            if local.hour() * 60 + local.minute() != target_minute {
                return None;
            }
            let date = local.date_naive();
            if date >= target_date {
                return None; // only days strictly before the target's own date count as "history"
            }
            if day_type(date, public_holidays, easter_holidays) != target_type {
                return None;
            }
            Some((date, price))
        })
        .collect();
    by_day.sort_by_key(|&(date, _)| std::cmp::Reverse(date)); // most recent first
    by_day.dedup_by_key(|&mut (d, _)| d); // one sample per calendar day at this slot

    if by_day.len() < MIN_DAYS {
        return None;
    }
    let mut top_k: Vec<f64> = by_day.into_iter().take(K).map(|(_, p)| p).collect();
    top_k.sort_by(f64::total_cmp);
    let n = top_k.len();
    Some(if n % 2 == 1 {
        top_k[n / 2]
    } else {
        (top_k[n / 2 - 1] + top_k[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn utc_at(y: i32, m: u32, day: u32, h: u32, min: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, day, h, min, 0).unwrap()
    }

    const NO_HOLIDAYS: &[(u32, u32)] = &[];

    #[test]
    fn easter_sunday_matches_known_dates() {
        // Cross-checked against published Gregorian Easter dates.
        assert_eq!(easter_sunday(2024), d(2024, 3, 31));
        assert_eq!(easter_sunday(2025), d(2025, 4, 20));
        assert_eq!(easter_sunday(2026), d(2026, 4, 5));
    }

    /// Acceptance 15: weekday -> weekend switch.
    #[test]
    fn day_type_switches_weekday_to_weekend() {
        // 2024-01-15 is a Monday, 2024-01-20 a Saturday, 2024-01-21 a Sunday.
        assert_eq!(day_type(d(2024, 1, 15), NO_HOLIDAYS, false), DayType::Work);
        assert_eq!(day_type(d(2024, 1, 19), NO_HOLIDAYS, false), DayType::Work); // Friday
        assert_eq!(day_type(d(2024, 1, 20), NO_HOLIDAYS, false), DayType::Sat);
        assert_eq!(day_type(d(2024, 1, 21), NO_HOLIDAYS, false), DayType::Sun);
    }

    /// Acceptance 15: a fixed-date public holiday on a weekday counts as Sunday; Easter Monday
    /// (computed, not listed) does too when `easter_holidays` is set.
    #[test]
    fn holiday_counts_as_sunday() {
        // 2024-01-01 is a Monday; listed as a public holiday.
        assert_eq!(day_type(d(2024, 1, 1), &[(1, 1)], false), DayType::Sun);
        // Without the holiday list it would be an ordinary Monday.
        assert_eq!(day_type(d(2024, 1, 1), NO_HOLIDAYS, false), DayType::Work);
        // Easter Monday 2024 = 2024-04-01, a Monday; must count as Sunday when easter_holidays.
        assert_eq!(day_type(d(2024, 4, 1), NO_HOLIDAYS, true), DayType::Sun);
        assert_eq!(day_type(d(2024, 4, 1), NO_HOLIDAYS, false), DayType::Work);
        // Good Friday 2024 = 2024-03-29, a Friday; must count as Sunday too.
        assert_eq!(day_type(d(2024, 3, 29), NO_HOLIDAYS, true), DayType::Sun);
    }

    /// Acceptance 15: fewer than 2 same-type days in history -> None.
    #[test]
    fn fewer_than_two_same_type_days_yields_none() {
        // Only ONE prior Monday (2024-01-08) in the history; target is Monday 2024-01-15.
        let history = vec![
            (utc_at(2024, 1, 8, 18, 0), 100.0),  // Monday
            (utc_at(2024, 1, 13, 18, 0), 999.0), // Saturday, wrong type
        ];
        let target = utc_at(2024, 1, 15, 18, 0); // Monday
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            day_type_median_price(&history, target, offset, NO_HOLIDAYS, false),
            None
        );
    }

    /// Acceptance 15: exactly 2 same-type days is enough; the median of {a, b} is their mean.
    #[test]
    fn two_same_type_days_is_enough() {
        let history = vec![
            (utc_at(2024, 1, 1, 18, 0), 100.0),
            (utc_at(2024, 1, 8, 18, 0), 200.0),
        ];
        let target = utc_at(2024, 1, 15, 18, 0); // Monday, same slot
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            day_type_median_price(&history, target, offset, NO_HOLIDAYS, false),
            Some(150.0)
        );
    }

    /// Acceptance 15: matches by 15-minute local slot, not just the hour.
    #[test]
    fn matches_by_15_minute_slot() {
        let history = vec![
            (utc_at(2024, 1, 1, 18, 15), 50.0),  // same slot, matches
            (utc_at(2024, 1, 8, 18, 15), 60.0),  // same slot, matches
            (utc_at(2024, 1, 1, 18, 0), 9999.0), // different slot, must NOT match
        ];
        let target = utc_at(2024, 1, 15, 18, 15);
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            day_type_median_price(&history, target, offset, NO_HOLIDAYS, false),
            Some(55.0)
        );
    }

    /// Acceptance 15: the most recent K=4 same-type days are used (a 5th, older day is dropped),
    /// and the median of an odd count (4 here would be even -> mean of middle two; use 5 candidates
    /// with only the 4 most recent counted to get a clean odd-of-4 check via the TAKEN subset).
    #[test]
    fn takes_only_the_most_recent_k_days() {
        // Five prior Mondays; only the 4 MOST RECENT (10, 60, 70, 80) must be used — the oldest
        // (1000.0, further back) must be excluded, or the median would be pulled toward it.
        let history = vec![
            (utc_at(2023, 12, 4, 12, 0), 1000.0), // oldest, excluded
            (utc_at(2023, 12, 11, 12, 0), 10.0),
            (utc_at(2023, 12, 18, 12, 0), 60.0),
            (utc_at(2023, 12, 25, 12, 0), 70.0),
            (utc_at(2024, 1, 1, 12, 0), 80.0),
        ];
        let target = utc_at(2024, 1, 8, 12, 0); // Monday
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        // Median of {10, 60, 70, 80} = (60+70)/2 = 65.0.
        assert_eq!(
            day_type_median_price(&history, target, offset, NO_HOLIDAYS, false),
            Some(65.0)
        );
    }

    /// Acceptance 15: DST-safety via a PER-INSTANT offset closure — a slot that reads as 18:00
    /// local either side of a DST change must still match correctly when the closure reflects the
    /// real per-instant offset (summer +2, winter +1, matching `SiteConfig::offset_at`'s shape).
    #[test]
    fn dst_safe_via_per_instant_offset_closure() {
        use chrono::TimeZone;
        // A closure mimicking a real Europe/Prague-like DST split: +2h before Oct 27 01:00 UTC,
        // +1h after (a simplified stand-in for `SiteConfig::offset_at`).
        let split = Utc.with_ymd_and_hms(2024, 10, 27, 1, 0, 0).unwrap();
        let offset = |t: DateTime<Utc>| {
            FixedOffset::east_opt(if t < split { 2 * 3600 } else { 3600 }).unwrap()
        };
        // History sample: Monday 2024-10-21 16:00 UTC = 18:00 local (+2, before the DST change).
        let history = vec![
            (utc_at(2024, 10, 21, 16, 0), 40.0),
            (utc_at(2024, 10, 14, 16, 0), 60.0),
        ];
        // Target: Monday 2024-10-28 17:00 UTC = 18:00 local (+1, after the DST change) — the SAME
        // local clock slot as the history samples, but a different UTC hour.
        let target = utc_at(2024, 10, 28, 17, 0);
        assert_eq!(
            day_type_median_price(&history, target, offset, NO_HOLIDAYS, false),
            Some(50.0),
            "must match by LOCAL slot across the DST change, not raw UTC hour"
        );
    }

    #[test]
    fn parse_month_day_accepts_valid_rejects_garbage() {
        assert_eq!(parse_month_day("01-01"), Some((1, 1)));
        assert_eq!(parse_month_day("12-31"), Some((12, 31)));
        assert_eq!(parse_month_day("02-30"), None); // no such date, even in a leap year
        assert_eq!(parse_month_day("13-01"), None);
        assert_eq!(parse_month_day("not-a-date"), None);
        assert_eq!(parse_month_day(""), None);
    }
}
