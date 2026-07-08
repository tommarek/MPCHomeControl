//! Read the house's PV forecast curve from InfluxDB.
//!
//! loxone stores its solar forecast in the `solar` bucket as an hourly kW curve (`hourly_json`,
//! keyed by local hour-of-day) per `forecast_date`, re-snapshotted through the day. Each snapshot
//! only covers the hours **from its record time forward**, so a midday snapshot holds just the
//! afternoon and the *latest* snapshot of a finished day is a near-empty end-of-day remnant.
//! Solcast (≤9 free fetches/day) appears only in some snapshots — the day's final refresh is often
//! a `model+api` fallback. The hourly curve lives only in `solar_forecast_history` (the current
//! `solar_forecast` measurement keeps just daily summaries), whose snapshots cover today and the
//! next days. Both the live MPC's `pv_kw` and the backtest read it, but pick differently (see
//! [`SnapshotPick`]): the live path takes each date's *latest* snapshot (the remaining-day forecast
//! it should plan against), while the backtest takes the *fullest* (full-day) snapshot so a finished
//! day is scored against the forecast made while the whole day was still ahead — not the remnant.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};

use crate::influxdb::InfluxQuery;
use crate::source::SourceClients;

/// Parse an `hourly_json` blob (`{"0": 0, "13": 6.2, …}`) into a local-hour → kW map.
pub(crate) fn parse_hourly_json(text: &str) -> Result<HashMap<u32, f64>> {
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(text)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| Some((k.parse::<u32>().ok()?, v.as_f64()?)))
        .collect())
}

/// Raw `(forecast_date, _time, _value)` rows for a forecast measurement's field.
async fn raw_field_rows(
    db: &SourceClients,
    measurement: &str,
    field: &str,
    start: &str,
) -> Vec<(String, String, String)> {
    // The forecast bucket resolves through the pluggable signal map (default `solar`); a house storing
    // the curve elsewhere remaps `data_sources.pv_forecast` without code.
    let query = InfluxQuery::new(&db.pv_forecast_bucket(), start, Some("now()"))
        .filter("_measurement", measurement)
        .filter("_field", field);
    db.read_rows(&query)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some((
                r.get("forecast_date")?.clone(),
                r.get("_time")?.clone(),
                r.get("_value")?.clone(),
            ))
        })
        .collect()
}

/// Which snapshot to use when a `forecast_date` has several (see the module docs: each curve covers
/// only the hours from its record time forward).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotPick {
    /// The latest (Solcast-preferred) snapshot — the forecast for the hours still ahead. This is what
    /// the live planner wants: at noon it should plan only the remaining afternoon.
    Latest,
    /// The most complete (max-energy, Solcast-preferred) snapshot — the full-day forecast made while
    /// the whole day was still ahead. This is what *scoring a finished day* needs; using the latest
    /// remnant instead makes the forecast read near-zero and craters the PV calibration.
    Fullest,
}

/// Comparison key for [`SnapshotPick`] selection (the curve/source ride along separately). `sum` is
/// the curve's total energy — a proxy for completeness, since a truncated remnant sums far lower.
struct SnapKey {
    when: DateTime<Utc>,
    has_solcast: bool,
    sum: f64,
}

/// Whether `cand` should replace the current `best` snapshot under `pick`. The 1 kWh tolerance on
/// "equally full" keeps a marginally-larger non-Solcast curve from displacing a Solcast one.
fn supersedes(pick: SnapshotPick, cand: &SnapKey, best: &SnapKey) -> bool {
    let solcast_or_fresher = (cand.has_solcast && !best.has_solcast)
        || (cand.has_solcast == best.has_solcast && cand.when > best.when);
    match pick {
        SnapshotPick::Latest => solcast_or_fresher,
        SnapshotPick::Fullest => {
            cand.sum > best.sum + 1.0 || ((cand.sum - best.sum).abs() <= 1.0 && solcast_or_fresher)
        }
    }
}

/// The chosen forecast curve per day from `measurement`, as a local-hour → kW map plus its `source`,
/// selected per [`SnapshotPick`]. `measurement` is `solar_forecast_history` (past) or `solar_forecast`.
pub(crate) async fn forecast_curves(
    db: &SourceClients,
    measurement: &str,
    start: &str,
    pick: SnapshotPick,
) -> Result<HashMap<NaiveDate, (HashMap<u32, f64>, String)>> {
    let sources: HashMap<(String, String), String> =
        raw_field_rows(db, measurement, "source", start)
            .await
            .into_iter()
            .map(|(d, t, v)| ((d, t), v))
            .collect();

    let mut best: HashMap<NaiveDate, (SnapKey, HashMap<u32, f64>, String)> = HashMap::new();
    for (date, time, json) in raw_field_rows(db, measurement, "hourly_json", start).await {
        let (Ok(d), Ok(when)) = (
            NaiveDate::parse_from_str(&date, "%Y-%m-%d"),
            DateTime::parse_from_rfc3339(&time),
        ) else {
            continue;
        };
        let Ok(curve) = parse_hourly_json(&json) else {
            continue;
        };
        let source = sources
            .get(&(date.clone(), time))
            .cloned()
            .unwrap_or_default();
        let cand = SnapKey {
            when: when.with_timezone(&Utc),
            has_solcast: source.contains("solcast"),
            sum: curve.values().sum(),
        };
        if best
            .get(&d)
            .is_none_or(|(b, _, _)| supersedes(pick, &cand, b))
        {
            best.insert(d, (cand, curve, source));
        }
    }

    Ok(best
        .into_iter()
        .map(|(d, (_k, curve, source))| (d, (curve, source)))
        .collect())
}

/// The hourly PV forecast plus its per-date coverage: `hours_missing[h]` marks horizon hours whose
/// date has **no stored curve at all** (a snapshotter gap — NOT a genuine zero-PV hour), so the
/// caller can splice a fallback into exactly those hours instead of planning hard 0 kW.
pub struct PvForecast {
    /// Hourly kW for the horizon; hours of a missing date are `0.0` (see `hours_missing`).
    pub hourly_kw: Vec<f64>,
    /// Per horizon hour: `true` when the hour's local date had no stored forecast curve.
    pub hours_missing: Vec<bool>,
    /// The distinct local dates with no stored curve (for the placeholder message).
    pub missing_dates: Vec<chrono::NaiveDate>,
}

/// The house PV forecast as hourly kW for the `horizon` hours from `start`. Hours whose date has a
/// curve but no entry for that hour are `0` (night); hours whose **date has no curve at all** are
/// flagged in `hours_missing` — the 24 h horizon always crosses midnight, so a missing tomorrow
/// (Solcast budget spent, writer outage) would otherwise plan phantom 0 kW mornings.
/// The curve is keyed in the site's local civil time; the offset derives **per horizon hour**
/// ([`SiteConfig::offset_at`]), so a DST changeover inside the horizon keys correctly.
///
/// The hourly curve lives only in `solar_forecast_history` (the current `solar_forecast`
/// measurement keeps just daily summaries), which holds snapshots for today and the next days; a
/// short look-back picks each forecast_date's latest (Solcast-preferred) snapshot.
pub async fn pv_forecast_kw(
    db: &SourceClients,
    start: DateTime<Utc>,
    horizon: usize,
    site: &crate::optimize::config::SiteConfig,
) -> Result<PvForecast> {
    // The `-2d` look-back still finds the latest snapshot for every horizon date if re-snapshotting
    // paused (Solcast budget spent, an outage).
    let curves = forecast_curves(db, "solar_forecast_history", "-2d", SnapshotPick::Latest).await?;
    let mut pv_kw = Vec::with_capacity(horizon);
    let mut hours_missing = Vec::with_capacity(horizon);
    let mut missing = HashSet::new();
    for h in 0..horizon {
        let at = start + Duration::hours(h as i64);
        // The curve is HOUR-ENDING: `curve[k]` is the PV of the local hour ending at `k` (Solcast
        // stamps `period_end`, and the PV backtest verified this empirically — its stop-stamped
        // measured hours align with the curve at zero shift, and a ±1 h shift raises RMSE). So
        // horizon hour [at, at+1h) reads the key of its END instant; keying by the start hour
        // served every block the PREVIOUS hour's PV, shifting the whole forecast an hour early.
        let end = at + Duration::hours(1);
        let local = end.with_timezone(&site.offset_at(end));
        let date = local.date_naive();
        let (kw, is_missing) = match curves.get(&date) {
            Some((curve, _)) => (curve.get(&local.hour()).copied().unwrap_or(0.0), false),
            None => {
                if missing.insert(date) {
                    eprintln!(
                        "  pv_forecast: no forecast curve for {date}; PV treated as 0 that day"
                    );
                }
                (0.0, true)
            }
        };
        pv_kw.push(kw);
        hours_missing.push(is_missing);
    }
    let mut missing_dates: Vec<chrono::NaiveDate> = missing.into_iter().collect();
    missing_dates.sort();
    Ok(PvForecast {
        hourly_kw: pv_kw,
        hours_missing,
        missing_dates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn snapshot_pick_latest_vs_fullest() {
        // A finished day's full-day forecast (made the evening before) vs the end-of-day remnant.
        let full = SnapKey {
            when: at("2026-06-28T16:00:00Z"),
            has_solcast: true,
            sum: 76.0,
        };
        let remnant = SnapKey {
            when: at("2026-06-28T17:30:00Z"),
            has_solcast: true,
            sum: 2.0,
        };
        // Latest: the later (remnant) snapshot wins regardless of energy — correct for live planning.
        assert!(supersedes(SnapshotPick::Latest, &remnant, &full));
        // Fullest: the higher-energy full-day snapshot wins even though it is older — correct scoring.
        assert!(supersedes(SnapshotPick::Fullest, &full, &remnant));
        assert!(!supersedes(SnapshotPick::Fullest, &remnant, &full));
    }

    #[test]
    fn snapshot_pick_fullest_prefers_solcast_when_equally_full() {
        let solcast = SnapKey {
            when: at("2026-06-28T16:00:00Z"),
            has_solcast: true,
            sum: 76.0,
        };
        let model = SnapKey {
            when: at("2026-06-28T17:00:00Z"),
            has_solcast: false,
            sum: 76.3,
        }; // fresher + marginally larger, but not Solcast
        assert!(!supersedes(SnapshotPick::Fullest, &model, &solcast));
    }

    #[test]
    fn parse_hourly_json_handles_ints_and_floats() {
        let m = parse_hourly_json(r#"{"0": 0, "13": 6.2, "bad": 1.0}"#).unwrap();
        assert_eq!(m.get(&0), Some(&0.0));
        assert_eq!(m.get(&13), Some(&6.2));
        assert_eq!(m.len(), 2); // non-numeric "bad" key dropped
    }
}
