//! Forward-prediction validation — "predict now, score against reality later".
//!
//! The plan carries a forward temperature prediction per zone ([`crate::app::TimestampedPlan`]).
//! The shadow loop periodically **snapshots** that prediction to a small JSON file; this module
//! later scores the elapsed part of a snapshot against the measured zone temperatures, so the
//! `/api/forecast/validation` endpoint can show how well the heat model actually predicted the day.
//!
//! Predictions are on a 15-minute block grid; measurements are hourly means stamped at the hour
//! boundary, so scoring compares only the **hour-aligned** blocks (minute 0) against the measured
//! hourly value for that hour — the same endpoint-vs-hourly-mean alignment the backtest uses.
//! Read-only: it reads measured temperatures and reads/writes only its own snapshot file.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::app::PlanReport;
use crate::estimate::hour_key;
use crate::source::SourceClients;
use crate::tools::{mean, rmse, sort_desc_by_key};

/// Keep at most this many snapshots in the file (a few days at hourly cadence).
const MAX_SNAPSHOTS: usize = 96;
/// Only score a snapshot once this much of it has elapsed, so there's something to compare.
const MIN_ELAPSED_HOURS: i64 = 3;

/// One captured forward prediction: per-zone predicted air temperature (°C) per block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub anchored_at: DateTime<Utc>,
    pub block_minutes: i64,
    pub zones: HashMap<String, Vec<f64>>,
}

impl Snapshot {
    /// Reshape a plan's per-block timeline into a per-zone prediction snapshot (one entry per block,
    /// in block order). `None` for an empty timeline.
    pub fn from_plan(plan: &PlanReport) -> Option<Snapshot> {
        let anchored_at = plan.timeline.first()?.t;
        let mut zones: HashMap<String, Vec<f64>> = HashMap::new();
        for block in &plan.timeline {
            for (zone, &temp) in &block.temp_c {
                zones.entry(zone.clone()).or_default().push(temp);
            }
        }
        Some(Snapshot {
            anchored_at,
            block_minutes: 15,
            zones,
        })
    }
}

/// Where the snapshots are persisted (a bind-mountable JSON file so they survive container restarts).
fn store_path() -> String {
    std::env::var("MPC_FORECAST_STORE").unwrap_or_else(|_| "forecast_snapshots.json".to_string())
}

/// Load the persisted snapshots (an absent or unreadable file is an empty history, not an error).
pub fn load_snapshots() -> Vec<Snapshot> {
    match std::fs::read_to_string(store_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Append a snapshot, capping the history to [`MAX_SNAPSHOTS`] (oldest dropped first).
pub fn append_snapshot(snapshot: Snapshot) -> Result<()> {
    let mut snapshots = load_snapshots();
    snapshots.push(snapshot);
    let len = snapshots.len();
    if len > MAX_SNAPSHOTS {
        snapshots.drain(0..len - MAX_SNAPSHOTS);
    }
    let json = serde_json::to_string(&snapshots).context("serializing forecast snapshots")?;
    // Write to a temp file then rename, so a crash mid-write can't corrupt the history (rename is
    // atomic on the same filesystem; a leftover `.tmp` is harmless).
    let path = store_path();
    if let Some(parent) = std::path::Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("creating forecast snapshot directory")?;
    }
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, json).context("writing forecast snapshot store")?;
    std::fs::rename(&tmp, &path).context("replacing forecast snapshot store")?;
    Ok(())
}

/// One scored (predicted, measured) point.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationPoint {
    pub t: DateTime<Utc>,
    pub predicted_c: f64,
    pub measured_c: f64,
}

/// One zone's forward-prediction accuracy over the scored window.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneValidation {
    pub zone: String,
    pub n: usize,
    pub rmse_k: f64,
    pub mean_bias_k: f64,
    pub points: Vec<ValidationPoint>,
}

/// The scorecard for the most recent sufficiently-elapsed snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub anchored_at: DateTime<Utc>,
    pub scored_until: DateTime<Utc>,
    pub zones: Vec<ZoneValidation>,
    /// Mean RMSE across the scored zones (None if nothing could be scored).
    pub mean_rmse_k: Option<f64>,
}

/// Score one zone's predicted blocks against the measured hourly values keyed by [`hour_key`]. Only
/// the **hour-aligned** blocks (minute 0) that have elapsed (`t <= scored_until`) and have a measured
/// value are compared. Returns `None` if no block could be scored. Pure — no IO.
fn score_zone(
    zone: &str,
    predicted: &[f64],
    anchored_at: DateTime<Utc>,
    block_minutes: i64,
    scored_until: DateTime<Utc>,
    by_hour: &HashMap<i64, f64>,
) -> Option<ZoneValidation> {
    let mut points = Vec::new();
    for (i, &pred) in predicted.iter().enumerate() {
        let t = anchored_at + Duration::minutes(block_minutes * i as i64);
        if t > scored_until || t.minute() != 0 {
            continue;
        }
        if let Some(&measured_c) = by_hour.get(&hour_key(t)) {
            points.push(ValidationPoint {
                t,
                predicted_c: pred,
                measured_c,
            });
        }
    }
    if points.is_empty() {
        return None;
    }
    let n = points.len();
    let sum_sq: f64 = points
        .iter()
        .map(|p| (p.predicted_c - p.measured_c).powi(2))
        .sum();
    let sum_err: f64 = points.iter().map(|p| p.predicted_c - p.measured_c).sum();
    Some(ZoneValidation {
        zone: zone.to_string(),
        n,
        rmse_k: rmse(sum_sq, n),
        mean_bias_k: mean(sum_err, n),
        points,
    })
}

/// Score the most recent snapshot that has at least [`MIN_ELAPSED_HOURS`] elapsed against the
/// measured zone temperatures, at hourly resolution.
pub async fn validate(db: &SourceClients) -> Result<ValidationReport> {
    let now = Utc::now();
    let snapshots = load_snapshots();
    // Still warming up (no sufficiently-elapsed snapshot yet): return an empty scorecard — a clean
    // 200, so the dashboard shows "warming up" instead of erroring.
    let Some(snapshot) = snapshots
        .iter()
        .rev()
        .find(|s| now - s.anchored_at >= Duration::hours(MIN_ELAPSED_HOURS))
    else {
        return Ok(ValidationReport {
            anchored_at: now,
            scored_until: now,
            zones: Vec::new(),
            mean_rmse_k: None,
        });
    };

    let blocks = snapshot.zones.values().map(Vec::len).max().unwrap_or(0) as i64;
    let horizon_end = snapshot.anchored_at + Duration::minutes(snapshot.block_minutes * blocks);
    let scored_until = now.min(horizon_end);

    let start = snapshot.anchored_at.to_rfc3339();
    let stop = scored_until.to_rfc3339();

    let mut zones = Vec::new();
    for (zone, predicted) in &snapshot.zones {
        let measured = db
            .read_zone_temperature_series(zone, &start, &stop, "1h")
            .await
            .unwrap_or_default();
        if measured.is_empty() {
            continue;
        }
        let by_hour: HashMap<i64, f64> = measured
            .iter()
            .map(|s| (hour_key(s.time), s.value))
            .collect();
        if let Some(scored) = score_zone(
            zone,
            predicted,
            snapshot.anchored_at,
            snapshot.block_minutes,
            scored_until,
            &by_hour,
        ) {
            zones.push(scored);
        }
    }
    sort_desc_by_key(&mut zones, |z| z.rmse_k);

    let mean_rmse_k = (!zones.is_empty())
        .then(|| zones.iter().map(|z| z.rmse_k).sum::<f64>() / zones.len() as f64);

    Ok(ValidationReport {
        anchored_at: snapshot.anchored_at,
        scored_until,
        zones,
        mean_rmse_k,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn score_zone_aligns_hourly_blocks_only() {
        // Anchor at :15 so only blocks 3, 7, 11 (the :00 boundaries) are hour-aligned.
        let anchored = utc("2026-01-15T08:15:00Z");
        // 12 blocks of 15 min; set the hour-aligned blocks (3 → 09:00, 7 → 10:00, 11 → 11:00).
        let mut predicted = vec![0.0; 12];
        predicted[3] = 21.0;
        predicted[7] = 22.0;
        predicted[11] = 23.0;
        let by_hour: HashMap<i64, f64> = [
            (hour_key(utc("2026-01-15T09:00:00Z")), 21.0), // block 3 → exact match
            (hour_key(utc("2026-01-15T10:00:00Z")), 21.5), // block 7 → predicted 22.0, err +0.5
            (hour_key(utc("2026-01-15T11:00:00Z")), 23.5), // block 11 → predicted 23.0, err -0.5
        ]
        .into_iter()
        .collect();
        let scored_until = utc("2026-01-15T11:15:00Z");
        let z = score_zone("a", &predicted, anchored, 15, scored_until, &by_hour).unwrap();
        assert_eq!(z.n, 3, "only the three hour-aligned blocks score");
        assert!(
            (z.mean_bias_k - 0.0).abs() < 1e-9,
            "errors +0.5 and -0.5 cancel"
        );
        assert!((z.rmse_k - (0.5f64.powi(2) * 2.0 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn score_zone_skips_blocks_past_scored_until() {
        let anchored = utc("2026-01-15T00:00:00Z");
        let predicted = vec![20.0; 12]; // hourly-aligned at blocks 0,4,8
        let by_hour: HashMap<i64, f64> = (0..3)
            .map(|h| (hour_key(anchored + Duration::hours(h)), 20.0))
            .collect();
        // Only ~1h elapsed: blocks at 00:00 and 01:00 are in range; 02:00 is not.
        let scored_until = anchored + Duration::minutes(90);
        let z = score_zone("a", &predicted, anchored, 15, scored_until, &by_hour).unwrap();
        assert_eq!(z.n, 2);
    }

    #[test]
    fn snapshot_store_round_trips_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snaps.json");
        std::env::set_var("MPC_FORECAST_STORE", &path);

        for h in 0..(MAX_SNAPSHOTS + 5) {
            let snap = Snapshot {
                anchored_at: Utc.timestamp_opt(h as i64 * 3600, 0).single().unwrap(),
                block_minutes: 15,
                zones: HashMap::from([("a".to_string(), vec![20.0, 21.0])]),
            };
            append_snapshot(snap).unwrap();
        }
        let loaded = load_snapshots();
        assert_eq!(loaded.len(), MAX_SNAPSHOTS, "history is capped");
        // Capped to the newest MAX_SNAPSHOTS, so the first kept anchor is #5 (0–4 evicted).
        assert_eq!(loaded.first().unwrap().anchored_at.timestamp(), 5 * 3600);
        std::env::remove_var("MPC_FORECAST_STORE");
    }
}
