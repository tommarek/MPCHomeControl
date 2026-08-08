//! Forward-prediction validation — "predict now, score against reality later".
//!
//! The plan carries a forward temperature prediction per zone ([`crate::app::TimestampedPlan`]).
//! The MPC loop periodically **snapshots** that prediction to a small JSON file; this module
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
/// A PARSE failure is logged — silently reading a corrupt store as empty is indistinguishable from
/// a fresh install, and [`append_snapshot`] would then overwrite the whole history.
pub fn load_snapshots() -> Vec<Snapshot> {
    match std::fs::read_to_string(store_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("[mpc] forecast snapshot store is unparseable ({e}); reading as empty");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Append a snapshot, capping the history to [`MAX_SNAPSHOTS`] (oldest dropped first).
///
/// If the existing store is present but unparseable it is moved aside to `<path>.corrupt` rather
/// than silently overwritten — otherwise one bad file cost the entire ~4-day lead-time history with
/// no trace, and the endpoint would just quietly report thin bins.
pub fn append_snapshot(snapshot: Snapshot) -> Result<()> {
    let path_str = store_path();
    if let Ok(raw) = std::fs::read_to_string(&path_str) {
        if serde_json::from_str::<Vec<Snapshot>>(&raw).is_err() {
            let aside = format!("{path_str}.corrupt");
            match std::fs::rename(&path_str, &aside) {
                Ok(()) => eprintln!(
                    "[mpc] forecast snapshot store was unparseable — moved to {aside}; starting a \
                     fresh history"
                ),
                Err(e) => eprintln!("[mpc] could not preserve the corrupt snapshot store ({e})"),
            }
        }
    }
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
    /// Lead-time-resolved accuracy across ALL stored snapshots (see [`lead_time_scores`]) — how
    /// the prediction degrades with how far ahead it was made. Bins with `n = 0` had no scoreable
    /// points (the deep bins are thin; the store holds ~4 days).
    pub leads: Vec<LeadBin>,
    /// How many stored snapshots fed the lead bins.
    pub snapshots_scored: usize,
}

/// The instant `predicted[i]` actually refers to: `TimelineBlock::temp_c` is the temperature at the
/// **end** of block `i` while `Snapshot::anchored_at` is the *start* of block 0, so the prediction
/// lands one whole block later than the naive `anchored_at + block·i`. Both scorers below MUST use
/// this — scoring against the block start compares values 15 min apart and inflates the error.
fn block_end(anchored_at: DateTime<Utc>, block_minutes: i64, i: usize) -> DateTime<Utc> {
    anchored_at + Duration::minutes(block_minutes * (i as i64 + 1))
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
        let t = block_end(anchored_at, block_minutes, i);
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

/// Lead-time bins for the resolved scorecard (hours, half-open `[from, to)`).
pub const LEAD_BINS_H: [(f64, f64); 5] = [
    (0.0, 3.0),
    (3.0, 6.0),
    (6.0, 12.0),
    (12.0, 24.0),
    (24.0, 36.0),
];

/// One zone's accuracy within a lead bin.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneLeadScore {
    pub zone: String,
    pub n: usize,
    pub rmse_k: f64,
    pub mean_bias_k: f64,
}

/// Prediction accuracy at one lead-time range, across all stored snapshots. OBSERVABILITY ONLY —
/// nothing feeds back into calibration yet; the obvious future consumers (lead-dependent PV
/// calibration, per-lead thermal bias) are follow-ups.
#[derive(Debug, Clone, Serialize)]
pub struct LeadBin {
    pub lead_from_h: f64,
    pub lead_to_h: f64,
    pub n: usize,
    pub rmse_k: f64,
    pub mean_bias_k: f64,
    pub zones: Vec<ZoneLeadScore>,
}

/// Score ALL stored snapshots into lead-time bins: how accuracy degrades with how far ahead the
/// prediction was made. The same point filter as [`score_zone`] (hour-aligned, elapsed, measured);
/// lead = `t − anchored_at`, binned half-open per [`LEAD_BINS_H`]. Bins with no points are
/// returned with `n = 0` so the consumer can grey them out (the deep bins are thin — the store
/// holds ~4 days). Pure.
pub fn lead_time_scores(
    snapshots: &[Snapshot],
    measured: &HashMap<String, HashMap<i64, f64>>,
    now: DateTime<Utc>,
) -> Vec<LeadBin> {
    // (bin, zone) → (sum_sq, sum_err, n)
    let mut acc: HashMap<(usize, String), (f64, f64, usize)> = HashMap::new();
    for snap in snapshots {
        for (zone, predicted) in &snap.zones {
            let Some(by_hour) = measured.get(zone) else {
                continue;
            };
            for (i, &pred) in predicted.iter().enumerate() {
                let t = block_end(snap.anchored_at, snap.block_minutes, i);
                if t > now || t.minute() != 0 {
                    continue;
                }
                let lead_h = (t - snap.anchored_at).num_minutes() as f64 / 60.0;
                // Half-open bins, EXCEPT that the last bin includes its upper edge: with the
                // end-of-block convention the final block of a 36 h horizon lands at exactly
                // 36.0 h, and a strict `<` silently dropped the deepest-lead point of every
                // snapshot — the one the deep bin most needs, since it is the thinnest.
                let last = LEAD_BINS_H.len() - 1;
                let Some(bin) = LEAD_BINS_H.iter().position(|&(from, to)| {
                    lead_h >= from && (lead_h < to || (lead_h == to && to == LEAD_BINS_H[last].1))
                }) else {
                    continue;
                };
                if let Some(&m) = by_hour.get(&hour_key(t)) {
                    let e = acc.entry((bin, zone.clone())).or_insert((0.0, 0.0, 0));
                    e.0 += (pred - m) * (pred - m);
                    e.1 += pred - m;
                    e.2 += 1;
                }
            }
        }
    }
    LEAD_BINS_H
        .iter()
        .enumerate()
        .map(|(b, &(from, to))| {
            let mut zones: Vec<ZoneLeadScore> = acc
                .iter()
                .filter(|((bin, _), _)| *bin == b)
                .map(|((_, zone), &(sq, err, n))| ZoneLeadScore {
                    zone: zone.clone(),
                    n,
                    rmse_k: rmse(sq, n),
                    mean_bias_k: mean(err, n),
                })
                .collect();
            zones.sort_by(|a, z| a.zone.cmp(&z.zone));
            let (sq, err, n) = zones.iter().fold((0.0, 0.0, 0), |(sq, err, n), z| {
                (
                    sq + z.rmse_k * z.rmse_k * z.n as f64,
                    err + z.mean_bias_k * z.n as f64,
                    n + z.n,
                )
            });
            LeadBin {
                lead_from_h: from,
                lead_to_h: to,
                n,
                rmse_k: if n > 0 { rmse(sq, n) } else { 0.0 },
                mean_bias_k: if n > 0 { mean(err, n) } else { 0.0 },
                zones,
            }
        })
        .collect()
}

/// Score the most recent snapshot that has at least [`MIN_ELAPSED_HOURS`] elapsed against the
/// measured zone temperatures, at hourly resolution.
pub async fn validate(db: &SourceClients) -> Result<ValidationReport> {
    let now = Utc::now();
    // Reading + parsing the ~90 KB store is blocking file IO on a bind-mounted volume; keep it off
    // the async runtime the same way the write path and the EV-preference reads already are.
    let snapshots = tokio::task::spawn_blocking(load_snapshots).await?;
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
            leads: Vec::new(),
            snapshots_scored: 0,
        });
    };

    let blocks = snapshot.zones.values().map(Vec::len).max().unwrap_or(0) as i64;
    let horizon_end = snapshot.anchored_at + Duration::minutes(snapshot.block_minutes * blocks);
    let scored_until = now.min(horizon_end);

    // One measured read per zone over the FULL snapshot span (the store holds ~4 days of hourly
    // points — the same cost class as the old single-snapshot window, still behind the endpoint
    // cache): feeds both the single-snapshot scorecard and the lead-resolved bins.
    let span_start = snapshots
        .iter()
        .map(|s| s.anchored_at)
        .min()
        .unwrap_or(snapshot.anchored_at);
    let start = span_start.to_rfc3339();
    let stop = now.to_rfc3339();
    let zone_names: std::collections::HashSet<&String> =
        snapshots.iter().flat_map(|s| s.zones.keys()).collect();
    let mut measured: HashMap<String, HashMap<i64, f64>> = HashMap::new();
    for zone in zone_names {
        let series = db
            .read_zone_temperature_series(zone, &start, &stop, "1h")
            .await
            .unwrap_or_default();
        if !series.is_empty() {
            measured.insert(zone.clone(), crate::estimate::keep_first_by_hour(&series));
        }
    }

    let mut zones = Vec::new();
    for (zone, predicted) in &snapshot.zones {
        let Some(by_hour) = measured.get(zone) else {
            continue;
        };
        if let Some(scored) = score_zone(
            zone,
            predicted,
            snapshot.anchored_at,
            snapshot.block_minutes,
            scored_until,
            by_hour,
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
        leads: lead_time_scores(&snapshots, &measured, now),
        snapshots_scored: snapshots.len(),
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
    fn lead_time_scores_bin_edges_and_aggregation() {
        // One snapshot, hourly blocks, constant +1 K error; 40 h of predictions but only 36 h of
        // bins — the tail beyond the last bin is dropped.
        let snap = Snapshot {
            anchored_at: utc("2026-01-10T00:00:00Z"),
            block_minutes: 60,
            zones: HashMap::from([("lr".to_string(), vec![22.0; 40])]),
        };
        let by_hour: HashMap<i64, f64> = (0..40)
            .map(|h| (hour_key(utc("2026-01-10T00:00:00Z")) + h, 21.0))
            .collect();
        let measured = HashMap::from([("lr".to_string(), by_hour)]);
        let now = utc("2026-01-12T00:00:00Z"); // everything elapsed
        let bins = lead_time_scores(&[snap], &measured, now);
        assert_eq!(bins.len(), LEAD_BINS_H.len());
        // `predicted[i]` ends at anchored + (i+1)h, so the leads run 1..40 — bin 0 gets leads 1,2.
        // Half-open edges: lead 3.0 h lands in [3,6), not [0,3).
        assert_eq!(bins[0].n, 2);
        assert_eq!(bins[1].n, 3); // 3,4,5
        assert_eq!(bins[2].n, 6); // 6..12
        assert_eq!(bins[3].n, 12); // 12..24
                                   // The LAST bin includes its upper edge, so lead 36.0 counts here (a 36 h horizon's final
                                   // block lands exactly on it); leads 37..40 are still outside every bin.
        assert_eq!(bins[4].n, 13); // 24..=36
        for b in &bins {
            assert!((b.rmse_k - 1.0).abs() < 1e-9);
            assert!((b.mean_bias_k - 1.0).abs() < 1e-9);
            assert_eq!(b.zones.len(), 1);
        }
        // Future points and unmeasured zones are excluded.
        let early = lead_time_scores(
            &[Snapshot {
                anchored_at: utc("2026-01-10T00:00:00Z"),
                block_minutes: 60,
                zones: HashMap::from([("lr".to_string(), vec![22.0; 40])]),
            }],
            &measured,
            utc("2026-01-10T02:00:00Z"),
        );
        assert_eq!(early[0].n, 2); // block ends at 01:00 and 02:00 have elapsed
        assert_eq!(early[1].n, 0);
        let none = lead_time_scores(&[], &measured, now);
        assert!(none.iter().all(|b| b.n == 0));
    }

    #[test]
    fn score_zone_aligns_hourly_blocks_only() {
        // Anchor at :15. `predicted[i]` is the END of block i, i.e. anchored + 15·(i+1), so the
        // hour-aligned entries are 2 → 09:00, 6 → 10:00, 10 → 11:00.
        let anchored = utc("2026-01-15T08:15:00Z");
        let mut predicted = vec![0.0; 12];
        predicted[2] = 21.0;
        predicted[6] = 22.0;
        predicted[10] = 23.0;
        let by_hour: HashMap<i64, f64> = [
            (hour_key(utc("2026-01-15T09:00:00Z")), 21.0), // block 2 → exact match
            (hour_key(utc("2026-01-15T10:00:00Z")), 21.5), // block 6 → predicted 22.0, err +0.5
            (hour_key(utc("2026-01-15T11:00:00Z")), 23.5), // block 10 → predicted 23.0, err -0.5
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
        // `predicted[i]` ends at anchored + 15·(i+1), so the hour-aligned entries are 3 → 01:00,
        // 7 → 02:00, 11 → 03:00.
        let predicted = vec![20.0; 12];
        let by_hour: HashMap<i64, f64> = (0..4)
            .map(|h| (hour_key(anchored + Duration::hours(h)), 20.0))
            .collect();
        // Only ~90 min elapsed: the 01:00 endpoint is in range; 02:00 and 03:00 are not.
        let scored_until = anchored + Duration::minutes(90);
        let z = score_zone("a", &predicted, anchored, 15, scored_until, &by_hour).unwrap();
        assert_eq!(z.n, 1);
    }

    #[test]
    fn snapshot_store_round_trips_and_caps() {
        let _env = crate::tools::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
