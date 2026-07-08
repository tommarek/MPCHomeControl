//! Read the live MPC inputs from InfluxDB, aligned to the planning horizon.
//!
//! Each reader returns `Option`: `Some` when real data covers the horizon, `None` when the caller
//! should fall back (and flag it). The pure alignment/binning is factored out and unit-tested; the
//! IO wrappers stay thin and reuse [`crate::influxdb`] + [`crate::estimate`] helpers. Read-only.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, FixedOffset, SecondsFormat, Timelike, Utc, Weekday};

use crate::estimate::{hour_key, resample_ffill};
use crate::forecast::consumption::ConsumptionModel;
use crate::influxdb::PriceSample;
use crate::source::SourceClients;

const SOLAR_BUCKET: &str = "solar";
/// Max age (minutes) for the live battery SoC read; older ⇒ treated as missing (flagged placeholder).
const SOC_MAX_AGE_MIN: i64 = 60;

/// An RFC3339 instant Flux accepts unambiguously (`…Z`, not a `+00:00` offset).
fn flux_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Block duration for the day-ahead price grid: OTE day-ahead is quoted in 15-minute (PT15M) blocks.
const BLOCK_SECONDS: i64 = 900;

/// Align native 15-minute EUR/MWh samples to the `blocks` 15-minute slots from `start` (UTC),
/// converting to EUR/kWh. Each slot is `Some(price)` when published, `None` when not — so the caller
/// can use the real prices for the published part of the horizon and fill only the unpublished gap
/// (the day-ahead set covers today fully but reaches into tomorrow only after the ~14:00 auction).
/// Returns `None` only when there are no samples at all.
fn align_blocks_15min(
    samples: &[PriceSample],
    start: DateTime<Utc>,
    blocks: usize,
) -> Option<Vec<Option<f64>>> {
    if samples.is_empty() {
        return None;
    }
    // Each OTE sample is stamped at its block start; map it to a 0-based block index from `start`.
    // The caller queries `[start, stop)`, so every sample has `time >= start` and maps to `0..blocks`;
    // an index outside that window (only reachable if the DB returned out-of-range data) is harmless —
    // it lands in `by_block` but the `0..blocks` output loop never reads it, so it is correctly ignored.
    let block_of = |t: DateTime<Utc>| (t.timestamp() - start.timestamp()).div_euclid(BLOCK_SECONDS);
    // Samples arrive sorted by time, so `insert` keeps the **latest** value for a block — a corrected
    // / re-published price overwrites an earlier one in the same block.
    let mut by_block: HashMap<i64, f64> = HashMap::new();
    for s in samples {
        by_block.insert(block_of(s.time), s.price_eur_mwh);
    }
    Some(
        (0..blocks as i64)
            .map(|b| by_block.get(&b).map(|p| p / 1000.0)) // EUR/MWh -> EUR/kWh
            .collect(),
    )
}

/// The day-ahead import price (EUR/kWh) per 15-minute block over the horizon: each slot `Some` when
/// published, `None` otherwise. `None` overall when nothing is published (the caller then uses the
/// placeholder curve for the whole horizon).
pub async fn block_prices(
    db: &SourceClients,
    start: DateTime<Utc>,
    blocks: usize,
) -> Result<Option<Vec<Option<f64>>>> {
    // Read the future day-ahead curve with an explicit stop (an open-ended range stops at now()).
    let stop = flux_time(start + Duration::seconds(BLOCK_SECONDS * blocks as i64));
    let samples = db.read_prices_range(&flux_time(start), &stop).await?;
    Ok(align_blocks_15min(&samples, start, blocks))
}

/// The hourly weather forecast over the horizon, plus how much of the grid the source actually
/// covered (a short series is forward-filled flat — real, but degraded, and the caller flags it).
pub struct WeatherForecast {
    /// Outside temperature (°C) per horizon hour.
    pub temperature_c: Vec<f64>,
    /// Cloud cover (fraction 0..1) per horizon hour.
    pub cloud_cover: Vec<f64>,
    /// Grid hours with an actual temperature sample (the rest are forward-filled).
    pub covered_hours: usize,
}

/// The open-meteo outside-temperature (°C) and cloud-cover (fraction 0..1) forecasts per hour over
/// the horizon, forward-filled onto the grid. `None` if no temperature forecast is available.
/// `covered_hours` counts the grid hours backed by a real sample — a 5 h series stretched flat over
/// a 24 h horizon is still usable, but the caller must surface it rather than present the flat tail
/// as a real forecast.
pub async fn weather_forecast(
    db: &SourceClients,
    start: DateTime<Utc>,
    horizon: usize,
) -> Result<Option<WeatherForecast>> {
    let start_str = flux_time(start);
    let stop_str = flux_time(start + Duration::hours(horizon as i64));
    // The forecast's location resolves through the pluggable signal map (default: open-meteo
    // `weather_forecast`, `room=outside`/`type=hour`); a house on a different weather source remaps it.
    let temp = db
        .weather_temperature_series(&start_str, &stop_str, "1h")
        .await
        .unwrap_or_default();
    if temp.is_empty() {
        return Ok(None);
    }
    let cloud = db
        .weather_cloud_series(&start_str, &stop_str, "1h")
        .await
        .unwrap_or_default();

    let hours: Vec<i64> = (0..horizon)
        .map(|k| hour_key(start + Duration::hours(k as i64)))
        .collect();
    let temp_keys: std::collections::HashSet<i64> = temp.iter().map(|s| hour_key(s.time)).collect();
    let covered_hours = hours.iter().filter(|h| temp_keys.contains(h)).count();
    let temperature_c = resample_ffill(&hours, &temp);
    let cloud_cover = if cloud.is_empty() {
        vec![0.3; horizon]
    } else {
        resample_ffill(&hours, &cloud)
            .iter()
            .map(|pct| (pct / 100.0).clamp(0.0, 1.0))
            .collect()
    };
    Ok(Some(WeatherForecast {
        temperature_c,
        cloud_cover,
        covered_hours,
    }))
}

/// Train the consumption model from the last `consumption_history_days` of measured **base load**:
/// the total house load (`INVPowerToLocalLoad`, W→kWh) minus every draw the optimizer re-adds as a
/// decision variable — the recorded underfloor-heating electricity (relay duty × `max_heat_kw` /
/// COP) and the wallbox charging power. Without the deduction the temperature-binned model learns
/// base-load-plus-historical-heating in its cold bins, and the LP then double-counts the whole
/// winter heating load (its `heat/cop` decision variables land on top of a forecast that already
/// contains last week's heating). Non-controllable scheduled sinks (e.g. the water heat-pump) stay
/// in the base load — the LP injects only their *thermal* flux, never their electricity.
/// TODO: when a **controllable** load (boiler) is armed, subtract its recorded draw here too.
///
/// Joined by hour with the measured outside temperature; retraining from this trailing window each
/// cycle is the consumption self-correction. `None` if no usable samples (the caller keeps a
/// fallback model).
pub async fn train_consumption(
    db: &SourceClients,
    net: &crate::rc_network::RcNetwork,
    config: &crate::optimize::config::ControlConfig,
) -> Result<Option<ConsumptionModel>> {
    let start = format!("-{}d", config.consumption_history_days);
    let load = db
        .read_series(
            SOLAR_BUCKET,
            SOLAR_BUCKET,
            "INVPowerToLocalLoad",
            &[],
            &start,
            "now()",
            "1h",
        )
        .await
        .unwrap_or_default();
    if load.is_empty() {
        return Ok(None);
    }
    let temp_by_hour: HashMap<i64, f64> = db
        .read_zone_temperature_series("outside", &start, "now()", "1h")
        .await
        .unwrap_or_default()
        .iter()
        .map(|s| (hour_key(s.time), s.value))
        .collect();
    // Per-hour deductions (kWh over the hour = mean kW): what the LP re-adds as decisions.
    let hours: Vec<i64> = load.iter().map(|s| hour_key(s.time)).collect();
    let mut deduction_kwh: HashMap<i64, f64> = HashMap::new();
    // Heating ELECTRICITY: thermal circuit kW / COP (identical at the resistive COP 1.0; correct
    // the day a heat pump lands).
    let cop = config.heating.cop.max(1e-6);
    let heating =
        crate::validate::read_heating_kw(db, net, &config.heating, &hours, &start, "now()").await;
    for powers in heating.values() {
        for (h, kw) in hours.iter().zip(powers) {
            *deduction_kwh.entry(*h).or_insert(0.0) += kw / cop;
        }
    }
    // Wallbox draw per charger. Zero-fill (NOT forward-fill): a missing hour means no recorded
    // charging, and back-filling the first sample across earlier history would over-subtract.
    for charger in &config.chargers {
        let Some(loc) = charger.sources.get("power") else {
            continue;
        };
        match db.read_locator_series(loc, &start, "now()", "1h").await {
            Ok(series) => {
                for s in &series {
                    // The power locator is scaled to kW by its `scale` (see config).
                    *deduction_kwh.entry(hour_key(s.time)).or_insert(0.0) += s.value.max(0.0);
                }
            }
            // Best-effort, mirroring read_sensor_power_w: a failed read skips the deduction
            // rather than failing the whole training (the clamp bounds the residual error).
            Err(e) => eprintln!(
                "  consumption: charger {:?} power series unavailable ({e}); not deducted",
                charger.name
            ),
        }
    }

    // Per-sample offsets: a multi-week training window can cross a DST changeover, and binning a
    // sample by the wrong local hour shifts its whole day-profile by one.
    Ok(build_consumption_model(
        &load,
        &temp_by_hour,
        &deduction_kwh,
        |t| config.site.offset_at(t),
    ))
}

/// The pure training core: bin `(load - deductions).max(0)` kWh by (outside temp, local hour,
/// weekend). `None` when under half the load hours have an outside-temp match (misaligned series —
/// training on that biased subset would be worse than the fallback).
fn build_consumption_model(
    load: &[crate::influxdb::TimeSample],
    temp_by_hour: &HashMap<i64, f64>,
    deduction_kwh: &HashMap<i64, f64>,
    offset_at: impl Fn(DateTime<Utc>) -> FixedOffset,
) -> Option<ConsumptionModel> {
    let total = load.len();
    let mut model = ConsumptionModel::new();
    let mut matched = 0usize;
    for s in load {
        let key = hour_key(s.time);
        let Some(&temperature) = temp_by_hour.get(&key) else {
            continue;
        };
        // The load series is a stop-stamped hourly mean: a sample stamped 13:00 is the energy of
        // 12:00–13:00. Bin it by the hour it COVERS (the window midpoint's hour/weekday), not the
        // stamp — otherwise every bin holds the previous hour's behaviour and the whole planned
        // load profile runs one hour late (and the weekday flag flips an hour early at Sun→Mon).
        // The temp/deduction joins stay on the raw stop-stamp key: those series share it.
        let covered = s.time - chrono::Duration::minutes(30);
        let local = covered.with_timezone(&offset_at(covered));
        let is_weekend = matches!(local.weekday(), Weekday::Sat | Weekday::Sun);
        let deducted = deduction_kwh.get(&key).copied().unwrap_or(0.0);
        let base_kwh = (s.value / 1000.0 - deducted).max(0.0);
        model.add_sample(temperature, local.hour(), is_weekend, base_kwh);
        matched += 1;
    }
    if matched * 2 < total {
        eprintln!(
            "  consumption: only {matched}/{total} load hours had an outside-temp match; using fallback"
        );
        return None;
    }
    model.build();
    Some(model)
}

/// The battery's current energy (kWh) from the live SoC (%) × capacity, or `None` if no telemetry
/// (the caller keeps the default spec's initial SoC, flagged). Reads `SOC` through the **same**
/// growatt locator as the dashboard (`solar`/`solar`/`SOC` by default) so the plan and `/api/live`
/// can never disagree — not a hardcoded field/measurement.
pub async fn battery_soc_kwh(db: &SourceClients, max_soc_kwh: f64) -> Result<Option<f64>> {
    Ok(db
        .growatt_latest("SOC", SOC_MAX_AGE_MIN)
        .await
        // Require a finite, in-range percentage. An out-of-range value (e.g. 150 %) is corrupt
        // telemetry → report `None`, a flagged placeholder, so the bad data surfaces instead of
        // seeding a quietly-wrong SoC (matches `live.rs`'s SoC guard).
        .filter(|pct| pct.is_finite() && (0.0..=100.0).contains(pct))
        .map(|pct| pct / 100.0 * max_soc_kwh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn price(hour: i64, quarter: i64, eur_mwh: f64) -> PriceSample {
        PriceSample {
            time: Utc
                .timestamp_opt(hour * 3600 + quarter * 900, 0)
                .single()
                .unwrap(),
            price_eur_mwh: eur_mwh,
        }
    }

    #[test]
    fn aligns_15min_prices_to_blocks_eur_per_kwh() {
        // Four native 15-min blocks: 80, 120, 90, 200 EUR/MWh -> 0.080..0.200 EUR/kWh.
        let samples = vec![
            price(0, 0, 80.0),
            price(0, 1, 120.0),
            price(0, 2, 90.0),
            price(0, 3, 200.0),
        ];
        let start = Utc.timestamp_opt(0, 0).single().unwrap();
        let out = align_blocks_15min(&samples, start, 4).unwrap();
        assert!((out[0].unwrap() - 0.080).abs() < 1e-12);
        assert!((out[1].unwrap() - 0.120).abs() < 1e-12);
        assert!((out[2].unwrap() - 0.090).abs() < 1e-12);
        assert!((out[3].unwrap() - 0.200).abs() < 1e-12);
    }

    #[test]
    fn marks_unpublished_blocks_as_none() {
        let samples = vec![price(0, 0, 100.0), price(0, 1, 100.0)]; // only blocks 0,1 published
        let start = Utc.timestamp_opt(0, 0).single().unwrap();
        let out = align_blocks_15min(&samples, start, 4).unwrap();
        assert!(out[0].is_some() && out[1].is_some()); // published
        assert!(out[2].is_none() && out[3].is_none()); // not yet published
        assert!(align_blocks_15min(&[], start, 1).is_none()); // no samples at all -> None
    }

    // --- build_consumption_model: the base-load deduction core ---

    fn load_sample(hour: i64, watts: f64) -> crate::influxdb::TimeSample {
        crate::influxdb::TimeSample {
            time: Utc.timestamp_opt(hour * 3600, 0).single().unwrap(),
            value: watts,
        }
    }

    /// A fixed temp for every hour so all samples land in one temperature bin.
    fn flat_temps(hours: std::ops::Range<i64>, temp: f64) -> HashMap<i64, f64> {
        hours.map(|h| (h, temp)).collect()
    }

    #[test]
    fn deductions_lower_the_trained_bins() {
        let utc0 = FixedOffset::east_opt(0).unwrap();
        // 48 hourly samples of 2 kW total load; the second day has 1.5 kW of recorded heating.
        let load: Vec<_> = (0..48).map(|h| load_sample(h, 2000.0)).collect();
        let temps = flat_temps(0..48, 0.0);
        let deductions: HashMap<i64, f64> = (24..48).map(|h| (h, 1.5)).collect();

        let without = build_consumption_model(&load, &temps, &HashMap::new(), |_| utc0).unwrap();
        let with = build_consumption_model(&load, &temps, &deductions, |_| utc0).unwrap();
        // Without deductions the bin holds 2.0 kWh; with them, half the samples are 0.5 kWh.
        let (w0, w1) = (without.predict(0.0, 6, false), with.predict(0.0, 6, false));
        assert!(
            w1 < w0,
            "deducted training must predict less than raw ({w1} !< {w0})"
        );
    }

    #[test]
    fn deduction_larger_than_load_clamps_to_zero() {
        let utc0 = FixedOffset::east_opt(0).unwrap();
        let load: Vec<_> = (0..24).map(|h| load_sample(h, 1000.0)).collect();
        let temps = flat_temps(0..24, 10.0);
        // 5 kW of deduction against a 1 kW load: base clamps at 0, never negative.
        let deductions: HashMap<i64, f64> = (0..24).map(|h| (h, 5.0)).collect();
        let m = build_consumption_model(&load, &temps, &deductions, |_| utc0).unwrap();
        assert!(m.predict(10.0, 12, false) >= 0.0);
    }

    #[test]
    fn no_deductions_matches_previous_behavior() {
        let utc0 = FixedOffset::east_opt(0).unwrap();
        let load: Vec<_> = (0..24).map(|h| load_sample(h, 1500.0)).collect();
        let temps = flat_temps(0..24, 5.0);
        let m = build_consumption_model(&load, &temps, &HashMap::new(), |_| utc0).unwrap();
        // 1500 W → 1.5 kWh lands in the bins unchanged.
        assert!((m.predict(5.0, 3, false) - 1.5).abs() < 0.5);
    }

    #[test]
    fn under_half_temp_coverage_falls_back() {
        let utc0 = FixedOffset::east_opt(0).unwrap();
        let load: Vec<_> = (0..24).map(|h| load_sample(h, 1000.0)).collect();
        // Only 5 of 24 hours have a temperature — misaligned series ⇒ None.
        let temps = flat_temps(0..5, 5.0);
        assert!(build_consumption_model(&load, &temps, &HashMap::new(), |_| utc0).is_none());
    }
}
