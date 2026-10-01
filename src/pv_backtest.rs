//! Backtest PV-generation forecasts against the inverter's actual output.
//!
//! The house records its solar forecast (`solar_forecast_history`, an hourly kW curve per
//! `forecast_date`) alongside live Growatt telemetry. This compares that forecast against the
//! **actual** PV power (`InputPower`, the total PV-string DC input) over recent days. The stored
//! forecast blends sources — pure `solcast`, `model+solcast+api`, or `model+api` (no Solcast) —
//! so each day's `source` is reported rather than assumed to be Solcast.
//!
//! **Curtailment.** Solcast forecasts the array's *potential*; the inverter only harvests what it
//! can use. When grid export is disabled and the battery is full, the panels are curtailed and
//! actual drops below potential — not a forecast error. Those hours (`export_enabled == 0` and
//! `SOC` ≈ full) are detected and **excluded** from scoring.
//!
//! Caveat: `InputPower` is DC; Solcast is an AC estimate, so actual runs a few percent high from
//! inverter losses. Our own clear-sky model can be added to the comparison once the real array
//! specs (peak power, tilt, azimuth) are configured — a documented follow-up.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{ensure, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Timelike, Utc};

use serde::Serialize;

use crate::app::{CALIBRATION_MIN_BAND_HOURS, CALIBRATION_MIN_SCORED_HOURS};
use crate::forecast::calibration::{Calibration, PvBandCalibration};
use crate::influxdb::TimeSample;
use crate::solar_forecast::{fold_snapshots, forecast_snapshots, SnapshotCurve, SnapshotPick};
use crate::source::SourceClients;
use crate::tools::{mean, rmse};

const DAYLIGHT_KW: f64 = 0.05;
/// Battery state-of-charge (%) at/above which the battery is treated as full for curtailment.
const SOC_FULL: f64 = 99.0;

/// Per-day comparison of the Solcast forecast against actual generation (over clean hours).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvDayCompare {
    pub date: NaiveDate,
    /// Which forecast source produced this day's curve (`solcast`, `model+solcast+api`, …).
    pub source: String,
    /// Forecast energy (kWh) summed over the scored (clean daylight) hours.
    pub solcast_kwh: f64,
    /// Actual generation (kWh) over the same hours.
    pub actual_kwh: f64,
    pub clean_hours: usize,
    pub curtailed_hours: usize,
    /// RMS error (kW) over the scored hours.
    pub rmse_kw: f64,
    /// Mean signed error, actual − solcast (kW): positive = the house out-generated the forecast.
    pub bias_kw: f64,
}

/// Whole-backtest summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvBacktest {
    pub days: Vec<PvDayCompare>,
    pub overall_rmse_kw: f64,
    pub total_solcast_kwh: f64,
    pub total_actual_kwh: f64,
    /// Clean-hour forecast/actual sums split by local-hour band (morning/midday/evening) —
    /// feeds the shape-aware calibration ([`crate::forecast::calibration::PvBandCalibration`]).
    pub band_solcast_kwh: [f64; 3],
    pub band_actual_kwh: [f64; 3],
    pub band_clean_hours: [usize; 3],
    pub scored_hours: usize,
    pub curtailed_hours: usize,
    /// Days excluded from scoring because the stored forecast was too incomplete to compare fairly
    /// (a forecast-snapshotting gap left only an end-of-day remnant, or nothing). Surfaced so a data
    /// gap reads as a gap rather than silently dragging the calibration — never as a forecast error.
    pub incomplete_forecast_days: Vec<NaiveDate>,
    /// Lead-time-resolved accuracy over every retained snapshot of the last [`LEAD_WINDOW_DAYS`]
    /// days (see [`PvLeadBin`]) — how forecast skill degrades with how far ahead it was made.
    pub leads: Vec<PvLeadBin>,
    /// The nowcast replay (the accuracy proof) over the same lead window — see [`PvNowcastReplay`].
    pub nowcast: PvNowcastReplay,
}

/// Accumulator for one day's scored hours.
#[derive(Default)]
struct DayScore {
    solcast_kwh: f64,
    actual_kwh: f64,
    /// Clean-hour sums split by the local-hour band (morning/midday/evening) — the raw material
    /// for the shape-aware PV calibration.
    band_solcast_kwh: [f64; 3],
    band_actual_kwh: [f64; 3],
    band_clean_hours: [usize; 3],
    clean_hours: usize,
    /// Scored hours where the forecast itself predicted daylight (≥ [`DAYLIGHT_KW`]). Far below
    /// `clean_hours` means the stored curve was a truncated remnant (a snapshotting gap), not a
    /// genuine zero forecast — used to drop the day from the calibration.
    forecast_hours: usize,
    curtailed_hours: usize,
    sse: f64,
    bias_sum: f64,
}

/// Score one day: compare `solcast` vs `actual` over daylight hours, skipping `curtailed` ones.
fn score_day(
    solcast: &HashMap<u32, f64>,
    actual: &HashMap<u32, f64>,
    curtailed: &HashSet<u32>,
) -> DayScore {
    let mut s = DayScore::default();
    for hour in 0..24u32 {
        // A missing forecast hour is a genuine 0 kW prediction; a missing *actual* hour is
        // unscoreable (no ground truth) and is skipped.
        let forecast = solcast.get(&hour).copied().unwrap_or(0.0);
        let Some(&measured) = actual.get(&hour) else {
            continue;
        };
        // Daylight only: skip night hours where both are ~zero.
        if forecast < DAYLIGHT_KW && measured < DAYLIGHT_KW {
            continue;
        }
        if curtailed.contains(&hour) {
            s.curtailed_hours += 1;
            continue;
        }
        s.clean_hours += 1;
        if forecast >= DAYLIGHT_KW {
            s.forecast_hours += 1;
        }
        s.solcast_kwh += forecast;
        s.actual_kwh += measured;
        let band = crate::forecast::calibration::PvBandCalibration::band_of_hour(hour);
        s.band_solcast_kwh[band] += forecast;
        s.band_actual_kwh[band] += measured;
        s.band_clean_hours[band] += 1;
        s.sse += (measured - forecast).powi(2);
        s.bias_sum += measured - forecast;
    }
    s
}

/// PV lead-time bins (hours ahead the snapshot was recorded, half-open `[from, to)`). The near-term
/// bins are already daylight-only (both this binning and [`score_day`] skip hours where forecast
/// AND measured are both below [`DAYLIGHT_KW`]) — split finer here (0-1/1-2/2-3/3-6 h) so the
/// nowcast's target horizon has its own comparable buckets, instead of hiding inside one 0-6 h bin
/// whose hour-of-day composition differs from the 6-12/12-24/24-48 h bins (see `docs/api.md`).
pub const PV_LEAD_BINS_H: [(f64, f64); 7] = [
    (0.0, 1.0),
    (1.0, 2.0),
    (2.0, 3.0),
    (3.0, 6.0),
    (6.0, 12.0),
    (12.0, 24.0),
    (24.0, 48.0),
];

/// Only snapshots for dates within this many days feed the lead bins — bounds the cost of
/// retaining every snapshot when the backtest window is long.
const LEAD_WINDOW_DAYS: i64 = 14;

/// Accuracy accumulated over one lead bin for one source class.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PvLeadScore {
    pub n: usize,
    pub rmse_kw: f64,
    pub bias_kw: f64,
    pub forecast_kwh: f64,
    pub actual_kwh: f64,
}

/// One PV lead-time bin: overall plus the solcast / non-solcast source split (directly useful
/// while two forecast writers coexist during the scraper cut-over). OBSERVABILITY ONLY — nothing
/// feeds back into the calibration yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvLeadBin {
    pub lead_from_h: f64,
    pub lead_to_h: f64,
    pub all: PvLeadScore,
    pub solcast: PvLeadScore,
    pub other: PvLeadScore,
}

/// Raw accumulator behind [`PvLeadScore`].
#[derive(Default, Clone, Copy)]
struct LeadAcc {
    sse: f64,
    bias_sum: f64,
    n: usize,
    forecast_kwh: f64,
    actual_kwh: f64,
}

impl LeadAcc {
    fn add(&mut self, forecast: f64, measured: f64) {
        self.sse += (measured - forecast).powi(2);
        self.bias_sum += measured - forecast;
        self.n += 1;
        self.forecast_kwh += forecast;
        self.actual_kwh += measured;
    }
    fn score(&self) -> PvLeadScore {
        PvLeadScore {
            n: self.n,
            rmse_kw: rmse(self.sse, self.n),
            bias_kw: mean(self.bias_sum, self.n),
            forecast_kwh: self.forecast_kwh,
            actual_kwh: self.actual_kwh,
        }
    }
}

/// Target lead bins for the nowcast replay (`h+k`, `k` = 1, 2, 3 h ahead of the reference hour) —
/// matching [`PV_LEAD_BINS_H`]'s own 0-1/1-2/2-3 h split, so the replay's accuracy is directly
/// comparable to the plain-forecast numbers in those same buckets.
const REPLAY_LEAD_BINS_H: [(f64, f64); 3] = [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)];

/// Grace period (seconds) added to `hour_end(h)` before picking the "as of" snapshot — real
/// snapshots are stamped a few seconds AFTER the hour they report on finished (e.g. 07:00:02 UTC),
/// so evaluating `snapshot_as_of` at EXACTLY `hour_end` would exclude that fresh snapshot and have
/// the reference hour (and its targets) read a 3-4 h-older one instead — live instead holds the
/// fresh snapshot and skips with "forecast refreshed" when it lacks the just-ended hour's key. 120 s
/// comfortably covers the observed 2-6 s stamping delay with margin for clock skew.
const SNAPSHOT_LANDING_GRACE_S: i64 = 120;

/// The nowcast replay's configuration, echoed back in the output so a client can see exactly what
/// was swept (or the live config, for the live endpoint).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NowcastReplayParams {
    pub efold_hours: f64,
    pub max_hours: f64,
    pub clamp: [f64; 2],
    pub min_forecast_kw: f64,
    pub window_minutes: u32,
}

/// RMSE/bias of one arm (plain or nowcast) over one lead bin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct ReplayScore {
    pub rmse_kw: f64,
    pub bias_kw: f64,
}

/// One nowcast-replay lead bin: plain (band-calibrated forecast only) vs nowcast, scored on the
/// IDENTICAL sample set (`n` is shared between both arms by construction).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplayLeadBin {
    pub lead_from_h: f64,
    pub lead_to_h: f64,
    pub n: usize,
    pub plain: ReplayScore,
    pub nowcast: ReplayScore,
}

/// The nowcast replay's whole-window result (the proof): plain vs nowcast accuracy per target lead
/// bin, over every (reference hour, target hour) pair the 14-day lead window supplies. `all` scores
/// EVERY target (a reference hour that was gated/curtailed/refreshed/missing contributes
/// `nowcast == plain`); `applied` is the subset whose reference hour actually produced a ratio.
///
/// Not a like-for-like of live: each reference hour is evaluated once, at `hour_end(h) +
/// SNAPSHOT_LANDING_GRACE_S`, so the refresh hours never get a nowcast here (conservative), while
/// live re-applies every tick and resumes ~30 min after a refresh on a shorter, noisier window —
/// a case this hourly replay does not exercise.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PvNowcastReplay {
    pub params: NowcastReplayParams,
    pub all: Vec<ReplayLeadBin>,
    pub applied: Vec<ReplayLeadBin>,
    /// Reference hours attempted (one per (date, local hour) with a measured actual, within the
    /// lead window): `n_ref == n_ref_applied + n_ref_gated + n_ref_curtailed + n_ref_refreshed +
    /// n_ref_missing`.
    pub n_ref: usize,
    pub n_ref_applied: usize,
    pub n_ref_gated: usize,
    pub n_ref_curtailed: usize,
    /// Reference hours whose as-of snapshot exists but doesn't yet have hour `h`'s key — the
    /// "forecast refreshed" case, live's own skip reason (spec Decision 1). Distinct from
    /// `n_ref_missing` (no snapshot recorded at all as of `hour_end(h) + SNAPSHOT_LANDING_GRACE_S`).
    pub n_ref_refreshed: usize,
    pub n_ref_missing: usize,
    /// How often the applied ratio sat exactly at the configured clamp bound.
    pub n_clamp_lo: usize,
    pub n_clamp_hi: usize,
    /// Dates whose trailing BAND calibration (the preceding ≤ 7 SCORED days) had fewer than
    /// `CALIBRATION_MIN_SCORED_HOURS` scored hours and fell back to neutral (no fit).
    pub n_neutral_calibration_days: usize,
    /// Mean of the trailing calibration's `overall_scale()` across every date the replay ran on
    /// (diagnostic only).
    pub mean_scale: f64,
}

/// Accumulator for one lead bin's plain vs nowcast scores (reuses [`LeadAcc`]'s sse/bias/kWh
/// machinery for both arms).
#[derive(Default, Clone, Copy)]
struct ReplayBinAcc {
    plain: LeadAcc,
    nowcast: LeadAcc,
}

/// Running accumulator across every date of the replay.
#[derive(Default)]
struct ReplayAcc {
    all: [ReplayBinAcc; 3],
    applied: [ReplayBinAcc; 3],
    n_ref: usize,
    n_ref_applied: usize,
    n_ref_gated: usize,
    n_ref_curtailed: usize,
    n_ref_refreshed: usize,
    n_ref_missing: usize,
    n_clamp_lo: usize,
    n_clamp_hi: usize,
}

/// One scored day's band + total sums, kept in a trailing deque of the preceding ≤ 7 scored days
/// so the replay can fit the SAME [`PvBandCalibration`] shape `app::build_cache` fits for the live
/// plan, instead of crediting the nowcast for fixing a bare trailing scalar's own shoulder-of-day
/// error (rework F2).
#[derive(Clone, Copy)]
struct TrailingDayScore {
    band_solcast_kwh: [f64; 3],
    band_actual_kwh: [f64; 3],
    band_clean_hours: [usize; 3],
    solcast_kwh: f64,
    actual_kwh: f64,
    clean_hours: usize,
}

/// Fit a [`PvBandCalibration`] from the trailing deque exactly as `app::build_cache` fits one from
/// a 7-day `backtest_pv` call: gated on `CALIBRATION_MIN_SCORED_HOURS` total scored hours across
/// the window, neutral (and counted) below that. `trailing` already holds at most the preceding 7
/// scored days (the caller evicts past that).
fn trailing_calibration(
    trailing: &VecDeque<TrailingDayScore>,
    n_neutral_calibration_days: &mut usize,
) -> PvBandCalibration {
    let scored_hours: usize = trailing.iter().map(|d| d.clean_hours).sum();
    if scored_hours < CALIBRATION_MIN_SCORED_HOURS {
        *n_neutral_calibration_days += 1;
        return PvBandCalibration::neutral();
    }
    let mut band_sol = [0.0f64; 3];
    let mut band_act = [0.0f64; 3];
    let mut band_hours = [0usize; 3];
    let (mut tot_sol, mut tot_act) = (0.0, 0.0);
    for d in trailing {
        for b in 0..3 {
            band_sol[b] += d.band_solcast_kwh[b];
            band_act[b] += d.band_actual_kwh[b];
            band_hours[b] += d.band_clean_hours[b];
        }
        tot_sol += d.solcast_kwh;
        tot_act += d.actual_kwh;
    }
    PvBandCalibration::from_backtest(
        band_sol,
        band_act,
        band_hours,
        Calibration::from_totals_default(tot_sol, tot_act),
        CALIBRATION_MIN_BAND_HOURS,
    )
}

/// The snapshot that was LATEST as of `as_of` (Solcast-preferred among ties) — mirrors
/// `solar_forecast`'s `supersedes(Latest, ..)` rule, restricted to `when <= as_of`: the curve the
/// live planner would have actually held at that instant. `None` when nothing was recorded yet.
fn snapshot_as_of(snaps: &[SnapshotCurve], as_of: DateTime<Utc>) -> Option<&SnapshotCurve> {
    let mut best: Option<&SnapshotCurve> = None;
    for s in snaps {
        if s.when > as_of {
            continue;
        }
        best = Some(match best {
            None => s,
            Some(b) => {
                let s_solcast = s.source.contains("solcast");
                let b_solcast = b.source.contains("solcast");
                if (s_solcast && !b_solcast) || (s_solcast == b_solcast && s.when > b.when) {
                    s
                } else {
                    b
                }
            }
        });
    }
    best
}

/// Replay the nowcast against one date's hours: for each local hour `h` with a measured actual,
/// pick the snapshot the live planner would have held ([`snapshot_as_of`]) at `hour_end(h) +
/// SNAPSHOT_LANDING_GRACE_S` — parity with the live rule, which lets a snapshot that lands a few
/// seconds into the next hour count as held for the hour it reports on — derive a ratio from the
/// single `(measured_h, calibration.apply_at(forecast_h, h))` pair when that snapshot actually has
/// hour `h`'s key (else `n_ref_refreshed`, no earlier-snapshot fallback: live doesn't have one
/// either), and blend it into targets `h+1..=h+3` (same date only) — scored against `act_h` on the
/// identical sample set as the plain (`calibration.apply_at(forecast, hour)`) baseline. Pure (no
/// IO): `calibration` (the per-date trailing BAND calibration — the SAME shape `app.rs::build_cache`
/// fits, over the preceding ≤ 7 scored days, not a bare scalar) and `params` are supplied by the
/// caller. A reference hour that is curtailed/gated/refreshed/missing still contributes its targets
/// to `acc.all` with `nowcast == plain`.
fn replay_date(
    snaps: &[SnapshotCurve],
    act_h: &HashMap<u32, f64>,
    curtailed: &HashSet<u32>,
    hour_end_utc: impl Fn(u32) -> DateTime<Utc>,
    calibration: &PvBandCalibration,
    params: &NowcastReplayParams,
    acc: &mut ReplayAcc,
) {
    let clamp = (params.clamp[0], params.clamp[1]);
    for h in 0..24u32 {
        let Some(&measured_h) = act_h.get(&h) else {
            continue;
        };
        acc.n_ref += 1;
        let as_of = hour_end_utc(h) + chrono::Duration::seconds(SNAPSHOT_LANDING_GRACE_S);
        let Some(chosen) = snapshot_as_of(snaps, as_of) else {
            acc.n_ref_missing += 1;
            continue;
        };
        let ratio: Option<f64> = if curtailed.contains(&h) {
            acc.n_ref_curtailed += 1;
            None
        } else {
            match chosen.curve.get(&h).copied() {
                None => {
                    // The as-of snapshot exists but doesn't have hour h's key yet — live's own
                    // "forecast refreshed" case. No fallback to an earlier snapshot: live doesn't
                    // have one either (it just skips the sample).
                    acc.n_ref_refreshed += 1;
                    None
                }
                Some(f_h) => {
                    match crate::forecast::nowcast::nowcast_ratio(
                        &[(measured_h, calibration.apply_at(f_h, h))],
                        params.min_forecast_kw,
                        clamp,
                    ) {
                        Ok(r) => {
                            if (r - clamp.0).abs() < 1e-9 {
                                acc.n_clamp_lo += 1;
                            }
                            if (r - clamp.1).abs() < 1e-9 {
                                acc.n_clamp_hi += 1;
                            }
                            acc.n_ref_applied += 1;
                            Some(r)
                        }
                        Err(crate::forecast::nowcast::NowcastSkip::LowForecast { .. }) => {
                            acc.n_ref_gated += 1;
                            None
                        }
                        Err(crate::forecast::nowcast::NowcastSkip::NoSamples) => {
                            acc.n_ref_missing += 1;
                            None
                        }
                    }
                }
            }
        };
        for k in 1..=3u32 {
            let th = h + k;
            if th >= 24 {
                continue; // same date only
            }
            let Some(&measured_t) = act_h.get(&th) else {
                continue;
            };
            if curtailed.contains(&th) {
                continue;
            }
            let forecast_t = chosen.curve.get(&th).copied().unwrap_or(0.0);
            if forecast_t < DAYLIGHT_KW && measured_t < DAYLIGHT_KW {
                continue;
            }
            let plain = calibration.apply_at(forecast_t, th);
            let nowcast_val = match ratio {
                Some(r) => {
                    let w = crate::forecast::nowcast::hour_weight(
                        k,
                        params.efold_hours,
                        params.max_hours,
                    );
                    plain * (1.0 + w * (r - 1.0))
                }
                None => plain,
            };
            let bin = (k - 1) as usize;
            acc.all[bin].plain.add(plain, measured_t);
            acc.all[bin].nowcast.add(nowcast_val, measured_t);
            if ratio.is_some() {
                acc.applied[bin].plain.add(plain, measured_t);
                acc.applied[bin].nowcast.add(nowcast_val, measured_t);
            }
        }
    }
}

/// Reduce a [`ReplayAcc`] into the served [`PvNowcastReplay`] shape.
fn finish_replay(
    acc: ReplayAcc,
    params: NowcastReplayParams,
    n_neutral_calibration_days: usize,
    mean_scale: f64,
) -> PvNowcastReplay {
    let to_bins = |accs: &[ReplayBinAcc; 3]| -> Vec<ReplayLeadBin> {
        REPLAY_LEAD_BINS_H
            .iter()
            .zip(accs.iter())
            .map(|(&(from, to), a)| ReplayLeadBin {
                lead_from_h: from,
                lead_to_h: to,
                n: a.plain.n,
                plain: ReplayScore {
                    rmse_kw: rmse(a.plain.sse, a.plain.n),
                    bias_kw: mean(a.plain.bias_sum, a.plain.n),
                },
                nowcast: ReplayScore {
                    rmse_kw: rmse(a.nowcast.sse, a.nowcast.n),
                    bias_kw: mean(a.nowcast.bias_sum, a.nowcast.n),
                },
            })
            .collect()
    };
    PvNowcastReplay {
        all: to_bins(&acc.all),
        applied: to_bins(&acc.applied),
        n_ref: acc.n_ref,
        n_ref_applied: acc.n_ref_applied,
        n_ref_gated: acc.n_ref_gated,
        n_ref_curtailed: acc.n_ref_curtailed,
        n_ref_refreshed: acc.n_ref_refreshed,
        n_ref_missing: acc.n_ref_missing,
        n_clamp_lo: acc.n_clamp_lo,
        n_clamp_hi: acc.n_clamp_hi,
        n_neutral_calibration_days,
        mean_scale,
        params,
    }
}

/// Fold one date's snapshots into the lead accumulators: every snapshot's curve is scored against
/// the same actuals/curtailment sets the day scoring used, per hour, into the bin of its lead
/// (`hour-ending instant − snapshot time`; negative leads — remnant snapshots recorded after the
/// hour — contribute nothing). The same hour filter as [`score_day`]. Pure.
#[allow(clippy::too_many_arguments)] // the date context is a flat set of parallel lookups
fn score_leads(
    snaps: &[SnapshotCurve],
    act_h: &HashMap<u32, f64>,
    curtailed: &HashSet<u32>,
    hour_end_utc: impl Fn(u32) -> DateTime<Utc>,
    acc: &mut [(LeadAcc, LeadAcc, LeadAcc)],
) {
    for snap in snaps {
        let is_solcast = snap.source.contains("solcast");
        for hour in 0..24u32 {
            let forecast = snap.curve.get(&hour).copied().unwrap_or(0.0);
            let Some(&measured) = act_h.get(&hour) else {
                continue;
            };
            if forecast < DAYLIGHT_KW && measured < DAYLIGHT_KW {
                continue;
            }
            if curtailed.contains(&hour) {
                continue;
            }
            // Seconds, not `num_minutes()`: a snapshot recorded a few seconds AFTER an hour ended
            // (every intraday snapshot does, e.g. 07:00:02 UTC) gives a small negative lead that
            // `num_minutes()` truncated to 0 — landing a remnant hour (forecast 0, its key is
            // absent from that snapshot) in the 0-1 h bin as a bogus near-zero-forecast sample.
            let lead_h = (hour_end_utc(hour) - snap.when).num_seconds() as f64 / 3600.0;
            if lead_h < 0.0 {
                continue; // the hour ended before the snapshot was recorded — a remnant, not real lead
            }
            let Some(bin) = PV_LEAD_BINS_H
                .iter()
                .position(|&(from, to)| lead_h >= from && lead_h < to)
            else {
                continue; // beyond the last bin
            };
            let (all, solcast, other) = &mut acc[bin];
            all.add(forecast, measured);
            if is_solcast {
                solcast.add(forecast, measured);
            } else {
                other.add(forecast, measured);
            }
        }
    }
}

/// Actual PV power (kW) per hour, from the `InputPower` (W) hourly mean.
async fn read_pv_kw(db: &SourceClients, start: &str) -> Result<Vec<TimeSample>> {
    // Through the SIGNAL MAP, not a hardcoded bucket/measurement/field. `growatt_series` is what
    // `/api/live`, the dashboard overlay and `what_if` all use, and its whole purpose is that
    // "history and live views can never disagree on where a metric lives". Reading raw here meant a
    // house that remapped `data_sources.growatt.InputPower` got an empty series → this returns an
    // error → `/api/pv/backtest` 500s and PV calibration falls back to neutral FOREVER, planning on
    // an uncalibrated forecast. It also ignored the locator's `scale`. The default locator is
    // `solar`/`solar`/`InputPower` with scale 1.0, so this is behaviour-preserving for this house.
    let mut series = db
        .growatt_series("InputPower", start, "now()", "1h")
        .await?;
    for s in &mut series {
        s.value /= 1000.0;
    }
    // Drop non-finite or negative readings (PV input power is physically >= 0; a sensor glitch
    // must not corrupt the stats or the calibration ratio).
    series.retain(|s| s.value.is_finite() && s.value >= 0.0);
    Ok(series)
}

/// Backtest the stored PV forecast against actual generation over the last `days` days, excluding
/// curtailed hours. The forecast curve is keyed in the site's local civil time; the offset derives
/// per sample ([`SiteConfig::offset_at`]), so a window crossing a DST changeover keys both sides
/// correctly. The forecast's local hour-of-day keys align with the stop-stamped hourly-mean actuals
/// at zero shift — verified empirically (a ±1 h shift raises RMSE).
/// Everything [`backtest_pv`] reads from InfluxDB, pre-indexed — so a parameter sweep (the
/// `backtest-pv-nowcast` CLI) can re-run [`score_pv_backtest_data`] many times over ONE read.
pub(crate) struct BacktestData {
    dates: Vec<NaiveDate>,
    forecasts: HashMap<NaiveDate, crate::solar_forecast::DayCurve>,
    lead_snapshots: HashMap<NaiveDate, Vec<SnapshotCurve>>,
    actual: HashMap<(NaiveDate, u32), f64>,
    export_on: HashMap<(NaiveDate, u32), f64>,
    soc_pct: HashMap<(NaiveDate, u32), f64>,
}

/// The read step: actual PV, curtailment flags and forecast snapshots over the last `days` days —
/// one single-field hourly query per series over the whole window (the same cost shape as the
/// `/api/pv/backtest` endpoint, whose `days` is capped at 60; the CLI caps at 21). No scoring here
/// — see [`score_pv_backtest_data`].
pub(crate) async fn fetch_pv_backtest_data(
    db: &SourceClients,
    site: &crate::optimize::config::SiteConfig,
    days: i64,
) -> Result<BacktestData> {
    ensure!(days > 0, "backtest window must be positive");
    let start = format!("-{days}d");

    let pv = read_pv_kw(db, &start).await?;
    ensure!(
        !pv.is_empty(),
        "no actual PV (InputPower) data in the window"
    );
    // The curtailment flags resolve through the pluggable signal map (default: the `solar`-bucket
    // `export_enabled` / `SOC`); a different inverter remaps them without code.
    let export = db.curtailment_export_series(&start, "now()", "1h").await?;
    let soc = db.curtailment_soc_series(&start, "now()", "1h").await?;
    // Score against the FULLEST snapshot per day, not the latest (which for a finished day is only the
    // end-of-day remnant — see `SnapshotPick`). Look back a couple of days further than the actuals
    // window: a day's full-day forecast is often snapshotted the evening before it begins.
    let fc_start = format!("-{}d", days + 2);
    let snapshots = forecast_snapshots(db, "solar_forecast_history", &fc_start).await?;
    // Lead scoring keeps EVERY snapshot, but only for recent dates (a long backtest window would
    // otherwise multiply hours × snapshots); the per-day pick below still uses the full window.
    let lead_cutoff = Utc::now().date_naive() - chrono::Days::new(LEAD_WINDOW_DAYS as u64);
    let lead_snapshots: HashMap<NaiveDate, Vec<SnapshotCurve>> = snapshots
        .iter()
        .filter(|(d, _)| **d >= lead_cutoff)
        .map(|(d, v)| {
            (
                *d,
                v.iter()
                    .map(|s| SnapshotCurve {
                        when: s.when,
                        source: s.source.clone(),
                        curve: s.curve.clone(),
                        p10: s.p10.clone(),
                    })
                    .collect(),
            )
        })
        .collect();
    let forecasts = fold_snapshots(snapshots, SnapshotPick::Fullest);
    ensure!(
        !forecasts.is_empty(),
        "no solar forecast history in the window"
    );

    // Index everything by (local date, local hour) — the offset derives per sample, so a window
    // crossing a DST changeover keys each side correctly.
    let key = |t: DateTime<Utc>| {
        let local = t.with_timezone(&site.offset_at(t));
        (local.date_naive(), local.hour())
    };
    // Keep-FIRST on an hour collision, the convention every reader here follows (see
    // `estimate::keep_first_by_hour`): these series are stop-stamped over `stop: now()`, so Flux
    // clamps the final window and emits a trailing PARTIAL sample sharing the completed hour's key.
    // Keeping the LAST let a fraction-of-an-hour mean stand in for the full hour — scored against
    // the previous full hour's forecast, and able to mis-mark that hour (and, via the `h-1` rule,
    // the one before) as curtailed. This feeds `PvBandCalibration`, i.e. the multiplier applied to
    // the planning PV forecast every cycle.
    let keep_first = |samples: &[TimeSample]| -> HashMap<(NaiveDate, u32), f64> {
        let mut m: HashMap<(NaiveDate, u32), f64> = HashMap::new();
        for s in samples {
            m.entry(key(s.time)).or_insert(s.value);
        }
        m
    };
    let actual = keep_first(&pv);
    let export_on = keep_first(&export);
    let soc_pct = keep_first(&soc);

    let mut dates: Vec<NaiveDate> = forecasts.keys().copied().collect();
    dates.sort();

    Ok(BacktestData {
        dates,
        forecasts,
        lead_snapshots,
        actual,
        export_on,
        soc_pct,
    })
}

/// The pure score step: everything [`backtest_pv`] used to do after its reads, now parameterized
/// by `nowcast_cfg` so the same fetched [`BacktestData`] can be re-scored under many parameter
/// combinations (the `backtest-pv-nowcast` sweep) without re-querying InfluxDB.
pub(crate) fn score_pv_backtest_data(
    data: &BacktestData,
    site: &crate::optimize::config::SiteConfig,
    nowcast_cfg: &crate::optimize::config::NowcastConfig,
) -> PvBacktest {
    let (forecasts, lead_snapshots, actual, export_on, soc_pct) = (
        &data.forecasts,
        &data.lead_snapshots,
        &data.actual,
        &data.export_on,
        &data.soc_pct,
    );
    let mut days_out = Vec::new();
    let mut incomplete: Vec<NaiveDate> = Vec::new();
    let (mut tot_sse, mut tot_n, mut tot_sol, mut tot_act, mut tot_curt) =
        (0.0, 0usize, 0.0, 0.0, 0);
    let (mut band_sol, mut band_act, mut band_hours) = ([0.0_f64; 3], [0.0_f64; 3], [0_usize; 3]);
    let mut lead_acc =
        vec![(LeadAcc::default(), LeadAcc::default(), LeadAcc::default()); PV_LEAD_BINS_H.len()];
    let replay_params = NowcastReplayParams {
        efold_hours: nowcast_cfg.efold_hours,
        max_hours: nowcast_cfg.max_hours,
        clamp: nowcast_cfg.clamp,
        min_forecast_kw: nowcast_cfg.min_forecast_kw,
        window_minutes: nowcast_cfg.window_minutes,
    };
    let mut replay_acc = ReplayAcc::default();
    // The per-date trailing calibration (spec Decision 6, rework F2): the SAME band-calibration
    // shape `app::build_cache` fits for the live plan — not a bare scalar, which credits the
    // nowcast for fixing the scalar's own shoulder-of-day error instead of today's weather — over
    // the preceding ≤ 7 SCORED days (the `PvDayCompare` rows) STRICTLY BEFORE the date under
    // replay, within this same read. Pushed only where `days_out` itself is pushed below, so a
    // date's own hours never see their own totals.
    let mut trailing: VecDeque<TrailingDayScore> = VecDeque::with_capacity(7);
    let mut n_neutral_calibration_days = 0usize;
    let (mut sum_scale, mut n_scale_dates) = (0.0, 0usize);
    for &date in &data.dates {
        let day = &forecasts[&date];
        let (forecast, source) = (&day.curve, &day.source);
        let mut act_h: HashMap<u32, f64> = HashMap::new();
        let mut curtailed: HashSet<u32> = HashSet::new();
        for hour in 0..24u32 {
            if let Some(&a) = actual.get(&(date, hour)) {
                act_h.insert(hour, a);
            }
            // Curtailed when export is disabled and the battery is full (nowhere for PV to go).
            // The export flag is min-aggregated and SoC max-aggregated (see the series wrappers),
            // so an hour where the inverter throttled for only part of the hour still flags.
            // Missing export/soc data defaults to "not curtailed" (score the hour) rather than
            // dropping it — conservative and transparent.
            let exporting = export_on.get(&(date, hour)).copied().unwrap_or(1.0) >= 0.5;
            let battery_full = soc_pct.get(&(date, hour)).copied().unwrap_or(0.0) >= SOC_FULL;
            if !exporting && battery_full {
                curtailed.insert(hour);
            }
        }
        // Also exclude the hour immediately preceding each curtailed hour: the battery typically
        // fills partway through it, so its actuals are already throttled while the hourly flags
        // still read clean — scoring it would bias the calibration low. score_day's
        // ratio-of-totals design tolerates dropping the extra hours cheaply.
        let with_preceding: HashSet<u32> = curtailed
            .iter()
            .flat_map(|&h| if h > 0 { vec![h, h - 1] } else { vec![h] })
            .collect();
        let curtailed = with_preceding;
        // Lead-time scoring: every retained snapshot for this date, against the same actuals and
        // curtailment exclusions. The hour-ending UTC instant reconstructs from the local key
        // (hour 0 = the hour ending at this date's local midnight).
        if let Some(snaps) = lead_snapshots.get(&date) {
            let hour_end = |hour: u32| {
                let naive = date.and_hms_opt(hour, 0, 0).unwrap();
                let approx = chrono::Utc.from_utc_datetime(&naive);
                let off = site.offset_at(approx);
                chrono::Utc.from_utc_datetime(
                    &(naive - chrono::Duration::seconds(off.local_minus_utc() as i64)),
                )
            };
            score_leads(snaps, &act_h, &curtailed, hour_end, &mut lead_acc);

            let calibration = trailing_calibration(&trailing, &mut n_neutral_calibration_days);
            sum_scale += calibration.overall_scale();
            n_scale_dates += 1;
            replay_date(
                snaps,
                &act_h,
                &curtailed,
                hour_end,
                &calibration,
                &replay_params,
                &mut replay_acc,
            );
        }
        let score = score_day(forecast, &act_h, &curtailed);
        tot_curt += score.curtailed_hours;
        // Skip days with no scoreable hours (e.g. fully curtailed) rather than emit a 0-error row.
        if score.clean_hours == 0 {
            continue;
        }
        // Skip days whose stored forecast covers under half the generating daylight hours: a
        // snapshotting gap left only an end-of-day remnant (or nothing), so the forecast reads
        // near-zero. Scoring it would crater the Solcast total and spuriously inflate the PV
        // calibration — exclude it and surface it as a data gap instead of a forecast error.
        if score.forecast_hours * 2 < score.clean_hours {
            incomplete.push(date);
            continue;
        }
        tot_sse += score.sse;
        tot_n += score.clean_hours;
        tot_sol += score.solcast_kwh;
        tot_act += score.actual_kwh;
        for b in 0..3 {
            band_sol[b] += score.band_solcast_kwh[b];
            band_act[b] += score.band_actual_kwh[b];
            band_hours[b] += score.band_clean_hours[b];
        }
        days_out.push(PvDayCompare {
            date,
            source: source.clone(),
            solcast_kwh: score.solcast_kwh,
            actual_kwh: score.actual_kwh,
            clean_hours: score.clean_hours,
            curtailed_hours: score.curtailed_hours,
            rmse_kw: rmse(score.sse, score.clean_hours),
            bias_kw: mean(score.bias_sum, score.clean_hours),
        });
        trailing.push_back(TrailingDayScore {
            band_solcast_kwh: score.band_solcast_kwh,
            band_actual_kwh: score.band_actual_kwh,
            band_clean_hours: score.band_clean_hours,
            solcast_kwh: score.solcast_kwh,
            actual_kwh: score.actual_kwh,
            clean_hours: score.clean_hours,
        });
        if trailing.len() > 7 {
            trailing.pop_front();
        }
    }

    PvBacktest {
        days: days_out,
        overall_rmse_kw: rmse(tot_sse, tot_n),
        total_solcast_kwh: tot_sol,
        total_actual_kwh: tot_act,
        band_solcast_kwh: band_sol,
        band_actual_kwh: band_act,
        band_clean_hours: band_hours,
        scored_hours: tot_n,
        curtailed_hours: tot_curt,
        incomplete_forecast_days: incomplete,
        leads: PV_LEAD_BINS_H
            .iter()
            .zip(&lead_acc)
            .map(|(&(from, to), (all, solcast, other))| PvLeadBin {
                lead_from_h: from,
                lead_to_h: to,
                all: all.score(),
                solcast: solcast.score(),
                other: other.score(),
            })
            .collect(),
        nowcast: finish_replay(
            replay_acc,
            replay_params,
            n_neutral_calibration_days,
            if n_scale_dates > 0 {
                sum_scale / n_scale_dates as f64
            } else {
                1.0
            },
        ),
    }
}

/// Backtest the stored PV forecast against actual generation over the last `days` days (read +
/// score in one call — see [`fetch_pv_backtest_data`] / [`score_pv_backtest_data`] to re-score a
/// single read under several `nowcast_cfg`s, as the `backtest-pv-nowcast` sweep does).
pub async fn backtest_pv(
    db: &SourceClients,
    site: &crate::optimize::config::SiteConfig,
    days: i64,
    nowcast_cfg: &crate::optimize::config::NowcastConfig,
) -> Result<PvBacktest> {
    let data = fetch_pv_backtest_data(db, site, days).await?;
    Ok(score_pv_backtest_data(&data, site, nowcast_cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_leads_bins_by_snapshot_age_and_skips_remnants() {
        let noon_utc = |h: u32| {
            DateTime::parse_from_rfc3339(&format!("2026-07-01T{h:02}:00:00Z"))
                .unwrap()
                .with_timezone(&Utc)
        };
        // Two snapshots for the day: one recorded 20 h before local noon (lead ~12-24 bin for
        // midday hours), one recorded AFTER noon (a remnant — negative lead for midday).
        let curve: HashMap<u32, f64> = [(11, 3.0), (12, 4.0)].into_iter().collect();
        let snaps = vec![
            SnapshotCurve {
                when: noon_utc(12) - chrono::Duration::hours(20),
                source: "solcast".to_string(),
                curve: curve.clone(),
                p10: None,
            },
            SnapshotCurve {
                when: noon_utc(12) + chrono::Duration::hours(2),
                source: "model+api".to_string(),
                curve,
                p10: None,
            },
        ];
        let act_h: HashMap<u32, f64> = [(11, 3.5), (12, 4.5)].into_iter().collect();
        let mut acc = vec![
            (LeadAcc::default(), LeadAcc::default(), LeadAcc::default());
            PV_LEAD_BINS_H.len()
        ];
        score_leads(&snaps, &act_h, &HashSet::new(), noon_utc, &mut acc);
        // The early snapshot's two hours land in the 12-24 h bin (index 5; leads 19 h and 20 h)...
        assert_eq!(acc[5].0.n, 2);
        // ...credited to the solcast split; the remnant contributes nothing anywhere.
        assert_eq!(acc[5].1.n, 2);
        assert_eq!(acc[5].2.n, 0);
        let other: usize = acc
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != 5)
            .map(|(_, (a, _, _))| a.n)
            .sum();
        assert_eq!(other, 0);
        let s = acc[5].0.score();
        assert!((s.bias_kw - 0.5).abs() < 1e-9);
        assert!((s.forecast_kwh - 7.0).abs() < 1e-9);
    }

    #[test]
    fn score_leads_respects_curtailment_and_daylight() {
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64);
        let curve: HashMap<u32, f64> = [(10, 2.0), (11, 3.0), (2, 0.01)].into_iter().collect();
        let snaps = vec![SnapshotCurve {
            when: t0 - chrono::Duration::hours(1),
            source: "solcast".to_string(),
            curve,
            p10: None,
        }];
        let act_h: HashMap<u32, f64> = [(10, 2.0), (11, 3.0), (2, 0.02)].into_iter().collect();
        let curtailed: HashSet<u32> = [11].into_iter().collect();
        let mut acc = vec![
            (LeadAcc::default(), LeadAcc::default(), LeadAcc::default());
            PV_LEAD_BINS_H.len()
        ];
        score_leads(&snaps, &act_h, &curtailed, hour_end, &mut acc);
        // Hour 11 curtailed, hour 2 below daylight — only hour 10 scores (lead 11 h -> the
        // 6-12 h bin, index 4).
        let total: usize = acc.iter().map(|(a, _, _)| a.n).sum();
        assert_eq!(total, 1);
        assert_eq!(acc[4].0.n, 1);
    }

    #[test]
    fn score_leads_excludes_a_remnant_recorded_seconds_after_the_hour_ended() {
        // Every intraday snapshot is stamped a few seconds AFTER the hour it just finished (e.g.
        // 07:00:02 UTC) — `(hour_end - snap.when).num_minutes()` used to truncate that tiny
        // negative duration to 0, landing the remnant hour (key absent -> forecast 0) in the 0-1 h
        // bin as a bogus near-zero-forecast sample (rework F1). It must be excluded everywhere.
        let hour_end_instant = DateTime::parse_from_rfc3339("2026-07-01T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| {
            if h == 7 {
                hour_end_instant
            } else {
                hour_end_instant + chrono::Duration::hours(h as i64 - 7)
            }
        };
        let snaps = vec![SnapshotCurve {
            when: hour_end_instant + chrono::Duration::seconds(4), // recorded 4s AFTER hour 7 ended
            source: "solcast".to_string(),
            curve: HashMap::new(), // hour 7's key is genuinely absent (the snapshot starts later)
            p10: None,
        }];
        let act_h: HashMap<u32, f64> = [(7, 7.26)].into_iter().collect(); // real generation
        let mut acc = vec![
            (LeadAcc::default(), LeadAcc::default(), LeadAcc::default());
            PV_LEAD_BINS_H.len()
        ];
        score_leads(&snaps, &act_h, &HashSet::new(), hour_end, &mut acc);
        let total: usize = acc.iter().map(|(a, _, _)| a.n).sum();
        assert_eq!(total, 0, "the remnant hour must not land in any lead bin");
    }

    #[test]
    fn score_day_excludes_curtailed_and_night() {
        let solcast = HashMap::from([(2, 0.0), (10, 1.0), (11, 2.0), (12, 3.0)]);
        let actual = HashMap::from([(2, 0.0), (10, 1.5), (11, 2.0), (12, 2.5)]);
        let curtailed = HashSet::from([12]); // hour 12 curtailed -> excluded
        let s = score_day(&solcast, &actual, &curtailed);
        assert_eq!(s.clean_hours, 2); // hours 10, 11 (hour 2 is night, hour 12 curtailed)
        assert_eq!(s.curtailed_hours, 1);
        assert!((s.solcast_kwh - 3.0).abs() < 1e-12); // 1 + 2
        assert!((s.actual_kwh - 3.5).abs() < 1e-12); // 1.5 + 2
        assert!((s.sse - 0.25).abs() < 1e-12); // 0.5^2 + 0^2
        assert!((s.bias_sum - 0.5).abs() < 1e-12); // +0.5 + 0
    }

    #[test]
    fn score_day_counts_forecast_coverage() {
        // The data-gap pattern: an end-of-day remnant forecast (only hour 19) vs a full afternoon of
        // actual generation. `forecast_hours` must register the gap so the backtest can drop the day.
        let solcast = HashMap::from([(19, 1.5)]);
        let actual = HashMap::from([(10, 4.0), (11, 5.0), (12, 6.0), (19, 1.0)]);
        let s = score_day(&solcast, &actual, &HashSet::new());
        assert_eq!(s.clean_hours, 4); // four daylight hours have actual generation
        assert_eq!(s.forecast_hours, 1); // the forecast predicted daylight in only one of them
        assert!(s.forecast_hours * 2 < s.clean_hours); // the completeness guard would skip this day
    }

    #[test]
    fn score_day_skips_hours_without_actual() {
        let solcast = HashMap::from([(10, 1.0), (11, 2.0)]);
        let actual = HashMap::from([(10, 1.0)]); // no actual for hour 11
        let s = score_day(&solcast, &actual, &HashSet::new());
        assert_eq!(s.clean_hours, 1);
    }

    fn replay_params() -> NowcastReplayParams {
        NowcastReplayParams {
            efold_hours: 1.5,
            max_hours: 3.0,
            clamp: [0.3, 1.5],
            min_forecast_kw: 0.5,
            window_minutes: 60,
        }
    }

    #[test]
    fn replay_date_blends_targets_on_a_hand_computed_fixture() {
        // `act_h` is scanned for BOTH reference hours and target measurements, so h=11 (the
        // target of h=10) is itself also a reference hour here — its own target (h+1=12) has no
        // measured actual, so it contributes nothing and doesn't contaminate bin 0.
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64);
        let curve: HashMap<u32, f64> = [(10, 2.0), (11, 3.0)].into_iter().collect();
        let snaps = vec![SnapshotCurve {
            when: hour_end(10) - chrono::Duration::hours(1),
            source: "solcast".to_string(),
            curve,
            p10: None,
        }];
        let act_h: HashMap<u32, f64> = [(10, 2.4), (11, 3.3)].into_iter().collect();
        let mut acc = ReplayAcc::default();
        replay_date(
            &snaps,
            &act_h,
            &HashSet::new(),
            hour_end,
            &PvBandCalibration::neutral(),
            &replay_params(),
            &mut acc,
        );

        // h=10: forecast 2.0 vs measured 2.4 -> r = 1.2 (no clamp). h=11: forecast 3.0 vs
        // measured 3.3 -> r = 1.1 (no clamp). Both applied.
        assert_eq!(acc.n_ref, 2);
        assert_eq!(acc.n_ref_applied, 2);
        assert_eq!(acc.n_ref_gated, 0);
        assert_eq!(acc.n_ref_curtailed, 0);
        assert_eq!(acc.n_ref_missing, 0);
        assert_eq!(acc.n_clamp_lo, 0);
        assert_eq!(acc.n_clamp_hi, 0);

        // Only h=10's target h+1=11 is scoreable (forecast 3.0, measured 3.3); h=11's own target
        // (12) and h=10's h+2/h+3 targets have no measured actual.
        assert_eq!(acc.all[0].plain.n, 1);
        assert_eq!(acc.all[1].plain.n, 0);
        assert_eq!(acc.all[2].plain.n, 0);
        assert!((mean(acc.all[0].plain.bias_sum, 1) - (3.3 - 3.0)).abs() < 1e-9);
        // nowcast = plain * (1 + w(0.75 h mean) * (r - 1)), r = 1.2.
        let w1 = (0..4)
            .map(|q| crate::forecast::nowcast::weight(f64::from(q) * 0.25, 1.5, 3.0))
            .sum::<f64>()
            / 4.0;
        let expect_nowcast_1 = 3.0 * (1.0 + w1 * 0.2);
        assert!((mean(acc.all[0].nowcast.bias_sum, 1) - (3.3 - expect_nowcast_1)).abs() < 1e-6);
        // `applied` mirrors `all` here since h=10's reference hour produced a ratio.
        assert_eq!(acc.applied[0].plain.n, 1);
        assert!((acc.applied[0].nowcast.bias_sum - acc.all[0].nowcast.bias_sum).abs() < 1e-12);
    }

    #[test]
    fn replay_date_applies_each_hours_own_band_not_a_scalar() {
        // Morning (band 0, hour < 11) ratio 2.0x, midday (band 1, 11 <= hour < 15) ratio 1.0x —
        // distinct enough that a scalar fallback would be visibly wrong. Reference hour h=10 is
        // morning; its target h+1=11 is midday, so this also proves the TARGET is calibrated by
        // its own hour's band, not the reference hour's band (rework F2).
        let calibration = PvBandCalibration::from_backtest(
            [10.0, 10.0, 10.0],
            [20.0, 10.0, 10.0], // morning doubles, midday/evening unchanged
            [10, 10, 10],
            Calibration::from_totals_default(30.0, 40.0),
            8,
        );
        assert!(
            (calibration.apply_at(1.0, 10) - 2.0).abs() < 1e-9,
            "morning band"
        );
        assert!(
            (calibration.apply_at(1.0, 11) - 1.0).abs() < 1e-9,
            "midday band"
        );

        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64);
        let curve: HashMap<u32, f64> = [(10, 2.0), (11, 3.0)].into_iter().collect();
        let snaps = vec![SnapshotCurve {
            when: hour_end(10) - chrono::Duration::hours(1),
            source: "solcast".to_string(),
            curve,
            p10: None,
        }];
        // h=10 forecast 2.0 * band0 2.0x = 4.0 calibrated; measured 4.4 -> r = 1.1.
        let act_h: HashMap<u32, f64> = [(10, 4.4), (11, 3.3)].into_iter().collect();
        let mut acc = ReplayAcc::default();
        replay_date(
            &snaps,
            &act_h,
            &HashSet::new(),
            hour_end,
            &calibration,
            &replay_params(),
            &mut acc,
        );

        assert_eq!(acc.n_ref, 2); // h=10 and h=11 both have a measured actual
        assert_eq!(acc.n_ref_applied, 2);

        // Target h+1=11 (midday, band1 1.0x): plain = 3.0 * 1.0 = 3.0 — NOT 3.0 * 2.0 = 6.0, which
        // would be the bug (the reference hour's band leaking onto the target).
        assert_eq!(acc.all[0].plain.n, 1);
        assert!((mean(acc.all[0].plain.bias_sum, 1) - (3.3 - 3.0)).abs() < 1e-9);
        let w1 = (0..4)
            .map(|q| crate::forecast::nowcast::weight(f64::from(q) * 0.25, 1.5, 3.0))
            .sum::<f64>()
            / 4.0;
        let expect_nowcast_1 = 3.0 * (1.0 + w1 * 0.1); // r = 1.1
        assert!((mean(acc.all[0].nowcast.bias_sum, 1) - (3.3 - expect_nowcast_1)).abs() < 1e-6);
    }

    #[test]
    fn trailing_calibration_is_neutral_below_the_scored_hours_gate_and_fits_above_it() {
        // Below CALIBRATION_MIN_SCORED_HOURS total clean hours across the trailing days -> neutral
        // (and counted); at/above it -> a real fit from the summed band sums.
        let mut trailing: VecDeque<TrailingDayScore> = VecDeque::new();
        let mut n_neutral = 0usize;
        let sparse_day = TrailingDayScore {
            band_solcast_kwh: [1.0, 1.0, 1.0],
            band_actual_kwh: [2.0, 2.0, 2.0],
            band_clean_hours: [1, 1, 1],
            solcast_kwh: 3.0,
            actual_kwh: 6.0,
            clean_hours: 3, // one day alone is well under CALIBRATION_MIN_SCORED_HOURS (24)
        };
        trailing.push_back(sparse_day);
        let c = trailing_calibration(&trailing, &mut n_neutral);
        assert_eq!(c, PvBandCalibration::neutral());
        assert_eq!(n_neutral, 1);

        // Pad with enough further days to clear the gate (8 more * 3h = 24h, total 27h >= 24).
        for _ in 0..8 {
            trailing.push_back(sparse_day);
        }
        let c = trailing_calibration(&trailing, &mut n_neutral);
        assert_ne!(c, PvBandCalibration::neutral());
        assert_eq!(
            n_neutral, 1,
            "the gate-clearing call must not increment the neutral counter"
        );
    }

    #[test]
    fn replay_date_counts_curtailed_and_gated_reference_hours_as_plain_only() {
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64);
        // h=5: curtailed. h=8: forecast mean (0.1 kW) below min_forecast_kw -> gated. Both share
        // the same target bin (k=1 -> th=6 and th=9 respectively), isolating it from h=6/h=9's
        // own (applied) reference-hour contributions, which land in bins 1/2 instead.
        let curve: HashMap<u32, f64> = [(5, 2.0), (6, 3.0), (8, 0.1), (9, 3.0)]
            .into_iter()
            .collect();
        let snaps = vec![SnapshotCurve {
            when: t0,
            source: "solcast".to_string(),
            curve,
            p10: None,
        }];
        let act_h: HashMap<u32, f64> = [(5, 2.0), (6, 3.3), (8, 0.1), (9, 3.3)]
            .into_iter()
            .collect();
        let curtailed: HashSet<u32> = [5].into_iter().collect();
        let mut acc = ReplayAcc::default();
        replay_date(
            &snaps,
            &act_h,
            &curtailed,
            hour_end,
            &PvBandCalibration::neutral(),
            &replay_params(),
            &mut acc,
        );

        assert_eq!(acc.n_ref, 4); // hours 5, 6, 8, 9 all have a measured actual
        assert_eq!(acc.n_ref_curtailed, 1);
        assert_eq!(acc.n_ref_gated, 1);
        assert_eq!(acc.n_ref_applied, 2); // h=6 and h=9 are plain reference hours, both applied
        assert_eq!(acc.n_ref_missing, 0);

        // Bin 0 (k=1) gets exactly h=5 -> th6 and h=8 -> th9, both un-applied -> nowcast == plain.
        assert_eq!(acc.all[0].plain.n, 2);
        assert_eq!(acc.applied[0].plain.n, 0);
        assert!((acc.all[0].plain.bias_sum - acc.all[0].nowcast.bias_sum).abs() < 1e-12);
    }

    #[test]
    fn snapshot_as_of_never_lets_a_later_snapshot_leak() {
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let early = SnapshotCurve {
            when: t0 - chrono::Duration::hours(2),
            source: "solcast".to_string(),
            curve: [(0, 1.0)].into_iter().collect(),
            p10: None,
        };
        let late = SnapshotCurve {
            when: t0 + chrono::Duration::hours(1), // recorded AFTER as_of — must not be picked
            source: "solcast".to_string(),
            curve: [(0, 99.0)].into_iter().collect(),
            p10: None,
        };
        let snaps = [early, late];
        let chosen = snapshot_as_of(&snaps, t0).unwrap();
        assert_eq!(chosen.curve.get(&0), Some(&1.0));
    }

    #[test]
    fn snapshot_landing_grace_lets_a_just_after_hour_snapshot_count_as_held() {
        // A real intraday snapshot stamped 4s after an hour ended (e.g. 07:00:02-07:00:06 UTC) —
        // SNAPSHOT_LANDING_GRACE_S must let `snapshot_as_of` still pick it (parity with live), but
        // its curve doesn't have the just-ended hour's key yet -> "forecast refreshed", counted in
        // `n_ref_refreshed`, NOT a fallback to an older snapshot (rework R1 — the fallback branch
        // was removed; live has no such fallback either).
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hour_end = move |h: u32| t0 + chrono::Duration::hours(h as i64 - 5);
        let snap = SnapshotCurve {
            when: t0 + chrono::Duration::seconds(4),
            source: "solcast".to_string(),
            curve: [(6, 4.0)].into_iter().collect(), // covers only hour 6 onward, not hour 5
            p10: None,
        };
        let snaps = vec![snap];
        let act_h: HashMap<u32, f64> = [(5, 3.3)].into_iter().collect();
        let mut acc = ReplayAcc::default();
        replay_date(
            &snaps,
            &act_h,
            &HashSet::new(),
            hour_end,
            &PvBandCalibration::neutral(),
            &replay_params(),
            &mut acc,
        );
        // Without the grace period the +4s snapshot would be excluded entirely (when > as_of) ->
        // n_ref_missing; with it, the snapshot IS chosen but lacks hour 5's key -> n_ref_refreshed.
        assert_eq!(acc.n_ref, 1);
        assert_eq!(acc.n_ref_missing, 0);
        assert_eq!(acc.n_ref_refreshed, 1);
        assert_eq!(acc.n_ref_applied, 0);
    }

    #[test]
    fn replay_lead_bins_map_k_to_the_near_term_bins() {
        assert_eq!(REPLAY_LEAD_BINS_H, [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)]);
        assert_eq!(&REPLAY_LEAD_BINS_H[..], &PV_LEAD_BINS_H[..3]);
    }
}
