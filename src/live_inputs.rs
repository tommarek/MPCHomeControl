//! Read the live MPC inputs from InfluxDB, aligned to the planning horizon.
//!
//! Each reader returns `Option`: `Some` when real data covers the horizon, `None` when the caller
//! should fall back (and flag it). The pure alignment/binning is factored out and unit-tested; the
//! IO wrappers stay thin and reuse [`crate::influxdb`] + [`crate::estimate`] helpers. Read-only.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, FixedOffset, SecondsFormat, Timelike, Utc, Weekday};

use crate::estimate::{hour_key, resample_ffill};
use crate::forecast::consumption::ConsumptionModel;
use crate::influxdb::{InfluxDB, PriceSample};

const SOLAR_BUCKET: &str = "solar";
const WEATHER_BUCKET: &str = "weather_forecast";
/// Growatt battery state-of-charge lives in its own measurement, not `solar`.
const GROWATT_MEASUREMENT: &str = "growatt_status";

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
    let block_of = |t: DateTime<Utc>| (t.timestamp() - start.timestamp()).div_euclid(BLOCK_SECONDS);
    let mut by_block: HashMap<i64, f64> = HashMap::new();
    for s in samples {
        by_block.entry(block_of(s.time)).or_insert(s.price_eur_mwh);
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
    db: &InfluxDB,
    start: DateTime<Utc>,
    blocks: usize,
) -> Result<Option<Vec<Option<f64>>>> {
    // Read the future day-ahead curve with an explicit stop (an open-ended range stops at now()).
    let stop = flux_time(start + Duration::seconds(BLOCK_SECONDS * blocks as i64));
    let samples = db.read_prices_range(&flux_time(start), &stop).await?;
    Ok(align_blocks_15min(&samples, start, blocks))
}

/// The open-meteo outside-temperature (°C) and cloud-cover (fraction 0..1) forecasts per hour over
/// the horizon, forward-filled onto the grid. `None` if no temperature forecast is available.
pub async fn weather_forecast(
    db: &InfluxDB,
    start: DateTime<Utc>,
    horizon: usize,
) -> Result<Option<(Vec<f64>, Vec<f64>)>> {
    let start_str = flux_time(start);
    let stop_str = flux_time(start + Duration::hours(horizon as i64));
    let tags = [("room", "outside"), ("type", "hour")];
    let temp = db
        .read_series(
            WEATHER_BUCKET,
            WEATHER_BUCKET,
            "temperature_2m",
            &tags,
            &start_str,
            &stop_str,
            "1h",
        )
        .await
        .unwrap_or_default();
    if temp.is_empty() {
        return Ok(None);
    }
    let cloud = db
        .read_series(
            WEATHER_BUCKET,
            WEATHER_BUCKET,
            "cloudcover",
            &tags,
            &start_str,
            &stop_str,
            "1h",
        )
        .await
        .unwrap_or_default();

    let hours: Vec<i64> = (0..horizon)
        .map(|k| hour_key(start + Duration::hours(k as i64)))
        .collect();
    let temperature_c = resample_ffill(&hours, &temp);
    let cloud_cover = if cloud.is_empty() {
        vec![0.3; horizon]
    } else {
        resample_ffill(&hours, &cloud)
            .iter()
            .map(|pct| (pct / 100.0).clamp(0.0, 1.0))
            .collect()
    };
    Ok(Some((temperature_c, cloud_cover)))
}

/// Train the consumption model from the last `history_days` of measured house load
/// (`INVPowerToLocalLoad`, W→kWh) joined by hour with the measured outside temperature. Retraining
/// from this trailing window each cycle is the consumption self-correction. `None` if no usable
/// samples (the caller keeps a fallback model).
pub async fn train_consumption(
    db: &InfluxDB,
    history_days: i64,
    utc_offset_hours: i32,
) -> Result<Option<ConsumptionModel>> {
    let start = format!("-{history_days}d");
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
    let offset = FixedOffset::east_opt(utc_offset_hours * 3600).context("invalid UTC offset")?;

    let total = load.len();
    let mut model = ConsumptionModel::new();
    let mut matched = 0usize;
    for s in &load {
        // Need the outside temperature for that hour to bin the sample.
        let Some(&temperature) = temp_by_hour.get(&hour_key(s.time)) else {
            continue;
        };
        let local = s.time.with_timezone(&offset);
        let is_weekend = matches!(local.weekday(), Weekday::Sat | Weekday::Sun);
        // hourly-mean W / 1000 = mean kW = kWh over the 1 h window.
        model.add_sample(temperature, local.hour(), is_weekend, s.value / 1000.0);
        matched += 1;
    }
    // If most load hours lack an outside-temp match the series are misaligned; the model would be
    // trained on a biased subset, so fall back to the flat model (the caller flags it).
    if matched * 2 < total {
        eprintln!(
            "  consumption: only {matched}/{total} load hours had an outside-temp match; using fallback"
        );
        return Ok(None);
    }
    model.build();
    Ok(Some(model))
}

/// The battery's current energy (kWh) from the live `battery_soc` (%) × capacity, or `None` if no
/// telemetry (the caller keeps the default spec's initial SoC).
pub async fn battery_soc_kwh(db: &InfluxDB, max_soc_kwh: f64) -> Result<Option<f64>> {
    let soc = db
        .read_series(
            SOLAR_BUCKET,
            GROWATT_MEASUREMENT,
            "battery_soc",
            &[],
            "-2h",
            "now()",
            "1h",
        )
        .await
        .unwrap_or_default();
    Ok(soc
        .last()
        .map(|s| (s.value / 100.0).clamp(0.0, 1.0) * max_soc_kwh))
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
}
