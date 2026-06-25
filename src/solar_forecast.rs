//! Read the house's PV forecast curve from InfluxDB.
//!
//! loxone stores its solar forecast in the `solar` bucket as an hourly kW curve (`hourly_json`,
//! keyed by local hour-of-day) per `forecast_date`, re-snapshotted through the day. Solcast
//! (≤9 free fetches/day) appears only in some snapshots — the day's final refresh is often a
//! `model+api` fallback — so to use the best forecast we pick the latest snapshot whose `source`
//! includes `solcast`, falling back to the latest snapshot overall. The hourly curve lives only in
//! `solar_forecast_history` (the current `solar_forecast` measurement keeps just daily summaries),
//! whose snapshots cover today and the next days — so both the backtest and the live MPC's `pv_kw`
//! read it, the latter just taking each forecast_date's latest snapshot.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Timelike, Utc};

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

/// The best forecast curve per day from `measurement`, as a local-hour → kW map plus its `source`.
/// Prefers the latest Solcast-tagged snapshot per `forecast_date`, falling back to the latest
/// snapshot overall. `measurement` is `solar_forecast_history` (past) or `solar_forecast` (current).
pub(crate) async fn forecast_curves(
    db: &SourceClients,
    measurement: &str,
    start: &str,
) -> Result<HashMap<NaiveDate, (HashMap<u32, f64>, String)>> {
    let sources: HashMap<(String, String), String> =
        raw_field_rows(db, measurement, "source", start)
            .await
            .into_iter()
            .map(|(d, t, v)| ((d, t), v))
            .collect();

    // Per date, prefer the latest Solcast snapshot (more accurate), else the latest of any source.
    let mut best: HashMap<String, (DateTime<Utc>, bool, String, String)> = HashMap::new();
    for (date, time, json) in raw_field_rows(db, measurement, "hourly_json", start).await {
        let Ok(when) = DateTime::parse_from_rfc3339(&time) else {
            continue;
        };
        let when = when.with_timezone(&Utc);
        let source = sources
            .get(&(date.clone(), time))
            .cloned()
            .unwrap_or_default();
        let has_solcast = source.contains("solcast");
        let better = match best.get(&date) {
            Some((t, sol, _, _)) => (has_solcast && !sol) || (has_solcast == *sol && when > *t),
            None => true,
        };
        if better {
            best.insert(date, (when, has_solcast, json, source));
        }
    }

    let mut out = HashMap::new();
    for (date, (_t, _sol, json, source)) in best {
        if let (Ok(d), Ok(curve)) = (
            NaiveDate::parse_from_str(&date, "%Y-%m-%d"),
            parse_hourly_json(&json),
        ) {
            out.insert(d, (curve, source));
        }
    }
    Ok(out)
}

/// The house PV forecast as hourly kW for the `horizon` hours from `start`. Hours with no forecast
/// entry are `0`. `utc_offset_hours` maps UTC to the local civil time the curve is keyed in.
///
/// The hourly curve lives only in `solar_forecast_history` (the current `solar_forecast`
/// measurement keeps just daily summaries), which holds snapshots for today and the next days; a
/// short look-back picks each forecast_date's latest (Solcast-preferred) snapshot. The fixed UTC
/// offset assumes the horizon does not cross a DST boundary.
pub async fn pv_forecast_kw(
    db: &SourceClients,
    start: DateTime<Utc>,
    horizon: usize,
    utc_offset_hours: i32,
) -> Result<Vec<f64>> {
    let offset = FixedOffset::east_opt(utc_offset_hours * 3600).context("invalid UTC offset")?;
    // The `-2d` look-back still finds the latest snapshot for every horizon date if re-snapshotting
    // paused (Solcast budget spent, an outage).
    let curves = forecast_curves(db, "solar_forecast_history", "-2d").await?;
    let mut pv_kw = Vec::with_capacity(horizon);
    let mut missing = HashSet::new();
    for h in 0..horizon {
        let local = (start + Duration::hours(h as i64)).with_timezone(&offset);
        let date = local.date_naive();
        let kw = match curves.get(&date) {
            Some((curve, _)) => curve.get(&local.hour()).copied().unwrap_or(0.0),
            None => {
                if missing.insert(date) {
                    eprintln!(
                        "  pv_forecast: no forecast curve for {date}; PV treated as 0 that day"
                    );
                }
                0.0
            }
        };
        pv_kw.push(kw);
    }
    Ok(pv_kw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hourly_json_handles_ints_and_floats() {
        let m = parse_hourly_json(r#"{"0": 0, "13": 6.2, "bad": 1.0}"#).unwrap();
        assert_eq!(m.get(&0), Some(&0.0));
        assert_eq!(m.get(&13), Some(&6.2));
        assert_eq!(m.len(), 2); // non-numeric "bad" key dropped
    }
}
