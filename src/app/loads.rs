//! Measured run hours of scheduled (controllable) loads within their current occurrence window.

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

use crate::source::SourceClients;

/// The site-local instant the controllable load's window occurrence in progress at `now` opened,
/// or `None` when `now` is outside every window. Pure — the block grid and the local-time rule are
/// the same ones `controllable_load_specs` uses to build the LP's window mask.
fn occurrence_start(
    site: &crate::optimize::config::SiteConfig,
    load: &crate::optimize::config::ScheduledLoad,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let in_window = |t: DateTime<Utc>| {
        let l = t.with_timezone(&site.offset_at(t));
        load.unit_profile(l.month(), l.hour() * 60 + l.minute()) != 0.0
    };
    if !in_window(now) {
        return None;
    }
    // Walk back to where this occurrence opened. Bounded by a day: a window cannot be longer, and an
    // all-day window would otherwise walk for ever.
    let step = Duration::minutes(15);
    let mut start = now;
    while now - start < Duration::hours(24) && in_window(start - step) {
        start -= step;
    }
    Some(start)
}

/// Hours the load actually drew inside `(start, now]`, from stop-stamped 15-minute means.
///
/// Pure, and deliberately strict at both ends:
/// * `s.time > start` — a stop-stamped sample AT `start` covers `[start − 15 min, start)`, i.e. the
///   quarter-hour BEFORE the occurrence opened. Counting it credited the appliance for running
///   under its own native control just before the MPC window began.
/// * `s.time <= now` — `stop: now()` makes Flux clamp and emit a trailing PARTIAL window; a load
///   drawing for one minute of the current block yields a near-rated mean that would otherwise
///   count as a full 15 minutes. The same keep-first/drop-the-partial convention as
///   `estimate::keep_first_by_hour` and `validate::read_heating_kw`.
fn run_hours_from_samples(
    samples: &[crate::influxdb::TimeSample],
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    rated_kw: f64,
) -> f64 {
    samples
        .iter()
        .filter(|s| s.time > start && s.time <= now && s.value / 1000.0 > 0.5 * rated_kw)
        .count() as f64
        * 0.25
}

/// Hours a **controllable** load has actually run inside the window occurrence in progress at `now`,
/// measured from its `sensor`. `None` when the load has no sensor, is outside its window, or the
/// sensor yields NO evidence either way (a read error, or a successful read with no sample in
/// range) — the caller then keeps the MPC loop's planned-actuation tally. `Some(0.0)` means the
/// sensor genuinely reported the load idle, which is evidence; an empty series is not.
///
/// Why this exists: the loop's tally counts what it PLANNED to actuate, and a strict plan is not
/// proof it reached the hardware — the publisher, the broker or the controller can be down, or the
/// controller unarmed, with nothing flowing back to this read-only brain. An over-counted tally
/// zeroes the occurrence's remaining demand, so the load silently stops being scheduled for the rest
/// of its window; an under-counted one re-demands the full target every tick and over-runs it. A
/// sensor turns both guesses into evidence.
pub async fn measured_run_hours(
    db: &SourceClients,
    site: &crate::optimize::config::SiteConfig,
    load: &crate::optimize::config::ScheduledLoad,
    now: DateTime<Utc>,
) -> Option<f64> {
    let sensor = load.sensor.as_ref()?;
    let rated_kw = load.power_w? / 1000.0;
    if rated_kw <= 0.0 {
        return None;
    }
    // Floor `now` to the 15-minute grid FIRST: the query uses an absolute stop and stop-stamped
    // windows, and Flux clamps the trailing window's bounds to the range — so an unaligned `now`
    // made both the leading and trailing windows sub-15-min partials that the `(start, now]` filter
    // still admitted as full blocks (~+0.25 h systematic overcount, which shaves the boiler's
    // remaining demand short). Aligned bounds ⇒ every returned window is complete.
    let now = {
        let secs = now.timestamp();
        DateTime::from_timestamp(secs - secs.rem_euclid(900), 0).unwrap_or(now)
    };
    let Some(start) = occurrence_start(site, load, now) else {
        return Some(0.0); // outside every window: nothing has run in an occurrence that isn't open
    };
    // An EXPLICIT absolute range, not `-Nm` … `now()`: the relative form re-anchors to the server's
    // clock at query time, so the samples could not be aligned against the `start`/`now` this
    // function reasons about.
    let series = db
        .read_locator_series(sensor, &start.to_rfc3339(), &now.to_rfc3339(), "15m")
        .await
        .ok()?;
    if !series.iter().any(|s| s.time > start && s.time <= now) {
        // No evidence at all (locator renamed, field dropped, ingest gap). Returning 0 here would
        // OVERRIDE the loop's tally with "nothing has run", re-demanding the full target every tick
        // until the load over-ran its window — the mirror image of the bug this function fixes.
        eprintln!(
            "[mpc] controllable load {:?}: sensor returned no samples for the occurrence; \
             keeping the planned-actuation tally",
            crate::optimize::coordinator::load_name(load)
        );
        return None;
    }
    Some(run_hours_from_samples(&series, start, now, rated_kw))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three failure modes the panel found in `measured_run_hours`, pinned on its pure core.
    #[test]
    fn run_hours_from_samples_is_strict_at_both_ends() {
        use crate::influxdb::TimeSample;
        let t = |h: i64, m: i64| {
            chrono::DateTime::from_timestamp(h * 3600 + m * 60, 0).expect("valid timestamp")
        };
        let (start, now) = (t(0, 0), t(1, 0));
        let s = |at: chrono::DateTime<Utc>, w: f64| TimeSample { time: at, value: w };
        // Rated 2 kW ⇒ the half-rated threshold is 1 kW.
        let rated = 2.0;

        // A stop-stamped sample AT `start` covers the quarter-hour BEFORE the occurrence opened —
        // the appliance running under its own control just before the window. Not ours.
        assert_eq!(
            run_hours_from_samples(&[s(start, 2000.0)], start, now, rated),
            0.0
        );
        // Flux clamps the trailing partial window's stop to the range, so it is stamped exactly AT
        // `now` — with block-aligned bounds (the caller floors `now` to the grid) a sample at `now`
        // is a COMPLETE block and counts; one past `now` (out of range) must not.
        assert_eq!(
            run_hours_from_samples(&[s(now + Duration::minutes(15), 2000.0)], start, now, rated),
            0.0
        );
        // Genuine in-range blocks count; a sub-threshold one (standby draw) does not.
        let samples = [
            s(t(0, 15), 2000.0),
            s(t(0, 30), 500.0),
            s(t(0, 45), 1900.0),
            s(t(1, 0), 2000.0),
        ];
        assert_eq!(run_hours_from_samples(&samples, start, now, rated), 0.75);
    }
}
