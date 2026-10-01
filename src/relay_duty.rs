//! Event-based duty cycle for the on-change-logged heating relay.
//!
//! The relay (`loxone`/`relay`/`tag1=heating`) logs a point only on a state CHANGE — a handful per
//! room per day — so averaging those raw edge values per hour is a mean of samples, not a duty
//! cycle: a quiet hour with no edge reads as "off" even when the relay held fully on. [`relay_duty`]
//! integrates the on-change events instead, giving the true time-weighted ON fraction of a window.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use uom::si::f64::Angle;

use crate::optimize::config::ControlConfig;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::validate::{calibrate_internal_gains, read_heating_kw};

/// Which duty computation `validate::read_heating_kw` uses. `Events` (default) is the
/// time-weighted duty reconstructed from the raw on-change relay log; `Legacy` is the hourly mean
/// of those same logged edges, zero-filled on an hour with no edge — kept selectable for one
/// heating season as a config revert if the event-based read regresses the live gain fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RelayDuty {
    #[default]
    Events,
    Legacy,
}

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

/// The LEGACY semantics `read_heating_kw` computes under `heating.relay_duty: "legacy"` (an
/// `aggregateWindow(1h, mean, createEmpty: false)` zero-fill): the mean of the raw event VALUES
/// whose timestamp falls in the hour ending at `hours[i]`, or `0.0` if none — a sample mean of
/// edges, not a duty cycle. Replayed here on the SAME raw events as [`relay_duty_hourly`] so the
/// two can be compared directly.
pub(crate) fn legacy_duty_hourly(events: &[(DateTime<Utc>, f64)], hours: &[i64]) -> Vec<f64> {
    hours
        .iter()
        .map(|&h| {
            let end = Utc.timestamp_opt(h * 3600, 0).single().unwrap_or_default();
            let start = end - Duration::hours(1);
            let (sum, n) = events
                .iter()
                .filter(|(t, _)| *t >= start && *t < end)
                .fold((0.0, 0u32), |(s, n), (_, v)| (s + v, n + 1));
            if n == 0 {
                0.0
            } else {
                sum / n as f64
            }
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

    // ---------- relay_duty (moved from ledger.rs) ----------

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

    // ---------- relay_duty_hourly / legacy_duty_hourly (moved from heating_backtest.rs) ----------

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
    fn legacy_duty_zero_fills_hours_with_no_event() {
        // Same scenario: legacy reads 0 for the no-event 05-06h hour, and 0.5 for a hypothetical
        // isolated 1-s blip inside 02-03h that true duty would score near-zero.
        let events = vec![
            (t("2026-01-10T02:30:00Z"), 1.0),
            (t("2026-01-10T02:30:01Z"), 0.0),
            (t("2026-01-10T04:15:00Z"), 1.0),
            (t("2026-01-10T06:30:00Z"), 0.0),
        ];
        let hours = vec![hk("2026-01-10T03:00:00Z"), hk("2026-01-10T06:00:00Z")];
        let legacy = legacy_duty_hourly(&events, &hours);
        // 02-03h has two events, values 1.0 and 0.0 -> mean 0.5.
        assert!((legacy[0] - 0.5).abs() < 1e-9, "{legacy:?}");
        // 05-06h has no event at all -> legacy zero-fills.
        assert!((legacy[1] - 0.0).abs() < 1e-9, "{legacy:?}");

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

    // ---------- new coverage (relay-duty-ingest) ----------

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

// =================================================== IO: audit-relay-duty proof tool =============

/// Per-zone legacy-vs-events comparison, the shape `--json` writes.
#[derive(Debug, Clone, Default, Serialize)]
struct ZoneAudit {
    hours: usize,
    kwh_legacy: f64,
    kwh_events: f64,
    delta_pct: Option<f64>,
    gains_config_w: Option<crate::optimize::config::GainProfile>,
    gains_legacy_w: Option<crate::optimize::config::GainProfile>,
    gains_events_w: Option<crate::optimize::config::GainProfile>,
    rmse_before_legacy_k: Option<f64>,
    rmse_after_legacy_k: Option<f64>,
    rmse_before_events_k: Option<f64>,
    rmse_after_events_k: Option<f64>,
    bias_after_legacy_k: Option<f64>,
    bias_after_events_k: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TimingsMs {
    drive_read_ms: u128,
    legacy_duty_read_ms: u128,
    events_duty_read_ms: u128,
    legacy_gain_fit_ms: u128,
    events_gain_fit_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
struct AuditReport {
    days: i64,
    generated_at: DateTime<Utc>,
    zones: HashMap<String, ZoneAudit>,
    timings_ms: TimingsMs,
}

fn parse_audit_args(args: &[String]) -> Result<(i64, Option<String>)> {
    let mut days = 7i64;
    let mut json_out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = args
                    .get(i)
                    .context("--days needs a value")?
                    .parse()
                    .context("--days must be an integer")?;
            }
            "--json" => {
                i += 1;
                json_out = Some(args.get(i).context("--json needs a path")?.clone());
            }
            other => bail!("audit-relay-duty: unknown argument {other}"),
        }
        i += 1;
    }
    ensure!(
        (1..=7).contains(&days),
        "--days must be between 1 and 7 (got {days})"
    );
    Ok((days, json_out))
}

fn find_zone_backtest<'a>(
    rows: &'a [crate::validate::ZoneBacktest],
    zone: &str,
) -> Option<&'a crate::validate::ZoneBacktest> {
    rows.iter().find(|z| z.zone == zone)
}

/// `audit-relay-duty [--days N<=7] [--json <out>]` — read-only proof tool for the relay-duty-ingest
/// item: runs `read_heating_kw` and the live gain re-fit (`calibrate_internal_gains`, the same call
/// `app::fit_live_internal_gains` makes) TWICE on the same real window, once with
/// `heating.relay_duty` forced to `legacy` and once to `events`, and prints the delivered heat
/// (kWh/zone), the fitted internal gains and the before/after backtest RMSE for both arms side by
/// side. Issues no reads beyond what the live code itself already performs for a re-fit — the
/// comparison is entirely "run the real path twice with one config field flipped".
#[allow(clippy::too_many_arguments)] // db, config, model, site and the CLI args are all distinct
pub async fn audit(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    args: &[String],
) -> Result<()> {
    let (days, json_out) = parse_audit_args(args)?;
    let start = format!("-{days}d");
    let ground_c = config.site.ground_temperature_c;

    let t0 = Instant::now();
    let data = crate::estimate::read_drive_data(db, &start, "now()", ground_c, 0.5)
        .await
        .context("reading the drive hour grid")?;
    let drive_read_ms = t0.elapsed().as_millis();

    let mut heating_legacy = config.heating.clone();
    heating_legacy.relay_duty = RelayDuty::Legacy;
    let mut heating_events = config.heating.clone();
    heating_events.relay_duty = RelayDuty::Events;

    let t1 = Instant::now();
    let kwh_legacy = read_heating_kw(db, net, &heating_legacy, &data.hours, &start, "now()").await;
    let legacy_duty_read_ms = t1.elapsed().as_millis();

    let t2 = Instant::now();
    let kwh_events = read_heating_kw(db, net, &heating_events, &data.hours, &start, "now()").await;
    let events_duty_read_ms = t2.elapsed().as_millis();

    let mut zones: HashMap<String, ZoneAudit> = HashMap::new();
    let gains_config = config.heating.internal_gains();
    println!(
        "audit-relay-duty: {days}-day window [{start}, now()), {} hours on the grid",
        data.hours.len()
    );
    println!(
        "{:<18} {:>6} {:>12} {:>12} {:>9}",
        "zone", "hours", "kWh legacy", "kWh events", "delta %"
    );
    let mut zone_names: Vec<String> = kwh_legacy
        .keys()
        .chain(kwh_events.keys())
        .cloned()
        .collect();
    zone_names.sort();
    zone_names.dedup();
    let (mut total_legacy, mut total_events) = (0.0, 0.0);
    for zone in &zone_names {
        let legacy_series = kwh_legacy.get(zone).cloned().unwrap_or_default();
        let events_series = kwh_events.get(zone).cloned().unwrap_or_default();
        // `+ 0.0` folds a negative zero (an all-zero mean series) into a plain 0.00 print.
        let kwh_l: f64 = legacy_series.iter().sum::<f64>() + 0.0;
        let kwh_e: f64 = events_series.iter().sum::<f64>() + 0.0;
        total_legacy += kwh_l;
        total_events += kwh_e;
        let delta_pct = if kwh_l.abs() > 1e-9 {
            Some((kwh_e - kwh_l) / kwh_l * 100.0)
        } else {
            None
        };
        println!(
            "{:<18} {:>6} {:>12.2} {:>12.2} {:>9}",
            zone,
            legacy_series.len().max(events_series.len()),
            kwh_l,
            kwh_e,
            delta_pct.map_or("n/a".to_string(), |d| format!("{d:.1}")),
        );
        zones.insert(
            zone.clone(),
            ZoneAudit {
                hours: legacy_series.len().max(events_series.len()),
                kwh_legacy: kwh_l,
                kwh_events: kwh_e,
                delta_pct,
                gains_config_w: gains_config.get(zone).cloned(),
                ..Default::default()
            },
        );
    }
    let total_delta_pct = if total_legacy.abs() > 1e-9 {
        Some((total_events - total_legacy) / total_legacy * 100.0)
    } else {
        None
    };
    println!(
        "{:<18} {:>6} {:>12.2} {:>12.2} {:>9}",
        "TOTAL",
        "",
        total_legacy,
        total_events,
        total_delta_pct.map_or("n/a".to_string(), |d| format!("{d:.1}")),
    );
    println!(
        "reads: drive {drive_read_ms} ms, legacy duty {legacy_duty_read_ms} ms, events duty {events_duty_read_ms} ms"
    );

    let local_offset = config.site.offset_at(Utc::now());
    let cfg = crate::validate::BacktestConfig {
        warmup_hours: 48,
        window_hours: (days * 24 - 48).max(24),
        ground_temperature_c: ground_c,
        cloud_cover: 0.5,
    };

    let t3 = Instant::now();
    let (before_legacy, after_legacy, fit_legacy) = calibrate_internal_gains(
        db,
        net,
        ss,
        &heating_legacy,
        &config.scheduled_loads,
        local_offset,
        latitude,
        longitude,
        &cfg,
        &start,
        "now()",
    )
    .await
    .context("legacy-arm gain re-fit")?;
    let legacy_gain_fit_ms = t3.elapsed().as_millis();

    println!("audit-relay-duty: pausing 3s before the events-arm re-fit (server-friendly)...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let t4 = Instant::now();
    let (before_events, after_events, fit_events) = calibrate_internal_gains(
        db,
        net,
        ss,
        &heating_events,
        &config.scheduled_loads,
        local_offset,
        latitude,
        longitude,
        &cfg,
        &start,
        "now()",
    )
    .await
    .context("events-arm gain re-fit")?;
    let events_gain_fit_ms = t4.elapsed().as_millis();

    println!(
        "\n{:<18} {:>17} {:>17} {:>18} {:>18}",
        "zone", "gain legacy", "gain events", "rmse(bias) legacy", "rmse(bias) events"
    );
    let mut gain_zone_names: Vec<String> = fit_legacy
        .gains
        .keys()
        .chain(fit_events.gains.keys())
        .chain(gains_config.keys())
        .cloned()
        .chain(after_legacy.iter().map(|z| z.zone.clone()))
        .chain(after_events.iter().map(|z| z.zone.clone()))
        .collect();
    gain_zone_names.sort();
    gain_zone_names.dedup();
    for zone in &gain_zone_names {
        let gl = fit_legacy.gains.get(zone).cloned();
        let ge = fit_events.gains.get(zone).cloned();
        let after_l = find_zone_backtest(&after_legacy, zone);
        let after_e = find_zone_backtest(&after_events, zone);
        // `none` = the fit kept no gain for the zone (every daypart below its floor).
        let profile = |g: Option<&crate::optimize::config::GainProfile>| {
            g.map_or("none".to_string(), |g| {
                format!("n{:.0}/d{:.0}/e{:.0}W", g.night, g.day, g.evening)
            })
        };
        let score = |z: Option<&crate::validate::ZoneBacktest>| {
            z.map_or("-".to_string(), |z| {
                format!("{:.3}({:+.2})", z.rmse_k, z.mean_bias_k)
            })
        };
        println!(
            "{:<18} {:>17} {:>17} {:>18} {:>18}",
            zone,
            profile(gl.as_ref()),
            profile(ge.as_ref()),
            score(after_l),
            score(after_e),
        );
        let z = zones.entry(zone.clone()).or_default();
        z.gains_config_w = gains_config.get(zone).cloned();
        z.gains_legacy_w = gl;
        z.gains_events_w = ge;
        z.rmse_before_legacy_k = find_zone_backtest(&before_legacy, zone).map(|z| z.rmse_k);
        z.rmse_after_legacy_k = after_l.map(|z| z.rmse_k);
        z.bias_after_legacy_k = after_l.map(|z| z.mean_bias_k);
        z.rmse_before_events_k = find_zone_backtest(&before_events, zone).map(|z| z.rmse_k);
        z.rmse_after_events_k = after_e.map(|z| z.rmse_k);
        z.bias_after_events_k = after_e.map(|z| z.mean_bias_k);
    }
    println!("gain fits: legacy {legacy_gain_fit_ms} ms, events {events_gain_fit_ms} ms");

    if let Some(path) = json_out {
        let report = AuditReport {
            days,
            generated_at: Utc::now(),
            zones,
            timings_ms: TimingsMs {
                drive_read_ms,
                legacy_duty_read_ms,
                events_duty_read_ms,
                legacy_gain_fit_ms,
                events_gain_fit_ms,
            },
        };
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing --json {path}"))?;
        println!("audit-relay-duty: wrote {path}");
    }

    Ok(())
}
