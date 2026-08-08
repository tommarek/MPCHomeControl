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
    // Each OTE sample is stamped at its block start; map it to a block index from `start`. The
    // caller queries `[start − 1 h, stop)`, so a sample BEFORE the horizon gets a NEGATIVE index —
    // deliberate: on an hourly market the sample covering block 0 is stamped at the previous top of
    // the hour. Negative (and any out-of-range) keys feed the period inference and the fill below
    // but never land in the output, whose loop reads only `0..blocks`.
    let block_of = |t: DateTime<Utc>| (t.timestamp() - start.timestamp()).div_euclid(BLOCK_SECONDS);
    // Samples arrive sorted by time, so `insert` keeps the **latest** value for a block — a corrected
    // / re-published price overwrites an earlier one in the same block.
    let mut by_block: HashMap<i64, f64> = HashMap::new();
    for s in samples {
        by_block.insert(block_of(s.time), s.price_eur_mwh);
    }
    // A sample covers its own PERIOD, which is not necessarily one block. `data_sources.prices` is
    // documented as remappable so "a house on a different market reads prices without a code
    // change" — but on an HOURLY market each sample landed in one 15-minute block and left the other
    // three `None`, which `app` then fills with the synthetic placeholder curve AND flags
    // `price_is_placeholder`, which hard-disables battery arbitrage. Three quarters of every horizon
    // planned against an invented price with arbitrage banned, while the real prices sat in the DB,
    // and the only symptom was a log line indistinguishable from a normal pre-auction gap.
    //
    // The period is the MINIMUM spacing between consecutive samples (min, not median: it must never
    // exceed the true period, or a genuine mid-horizon hole would be filled and a stale feed
    // masked). One sample ⇒ no evidence of a period ⇒ no fill, exactly as before.
    let mut keys: Vec<i64> = by_block.keys().copied().collect();
    keys.sort_unstable();
    // The aligner supports exactly TWO markets: 15-minute (period 1) and hourly (period 4). The
    // inferred minimum spacing is trusted only when it IS one of those; anything else — a uniformly
    // sparse 15-min feed whose every other sample was lost (min gap 2), or two lone samples
    // spanning a long hole — degrades to period 1, i.e. no fill beyond each sample's own block.
    // Smearing a stale price across an unexplained gap would come back as `Some(price)`, be marked
    // real (`price_is_placeholder = false`), and open the arbitrage gate against a number the
    // market never published for that block.
    let period = match keys.windows(2).map(|w| w[1] - w[0]).min() {
        Some(4) => 4,
        _ => 1,
    };
    Some(
        (0..blocks as i64)
            .map(|b| {
                // The sample whose own period covers this block, if any.
                by_block
                    .get(&b)
                    .or_else(|| {
                        keys.partition_point(|&k| k <= b)
                            .checked_sub(1)
                            .map(|i| keys[i])
                            .filter(|&k| b - k < period)
                            .and_then(|k| by_block.get(&k))
                    })
                    .map(|p| p / 1000.0) // EUR/MWh -> EUR/kWh
            })
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
    // The range starts ONE HOUR before the horizon: on an hourly market the sample that covers
    // block 0 is stamped at the top of the hour, which is usually BEFORE the horizon start — a
    // query from `start` could then never fill the leading blocks, and they fell to the placeholder
    // curve (arbitrage banned) even with the price published. An hour is the longest period the
    // aligner supports, so one hour of look-back always suffices; out-of-range samples only feed
    // the fill and never land in the output directly.
    let stop = flux_time(start + Duration::seconds(BLOCK_SECONDS * blocks as i64));
    let samples = db
        .read_prices_range(&flux_time(start - Duration::hours(1)), &stop)
        .await?;
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
    /// Grid hours with an actual cloud sample; `0` means the whole cloud channel is the flat 30 %
    /// fallback (the caller flags it — temperature alone succeeding used to hide a dead cloud feed).
    pub cloud_covered_hours: usize,
    /// Per-hour solar input, best-available-first: measured direct+diffuse radiation → global
    /// (shortwave) with a cloud split → the cloud model. Aligned to the horizon hours.
    pub solar: Vec<crate::tools::sun::SolarInput>,
    /// Hours whose solar input came from RADIATION fields (direct+diffuse or shortwave); the rest
    /// fell back to the cloud model. `0` = the radiation feed is absent entirely.
    pub radiation_covered_hours: usize,
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
    // Floor the range start to the containing hour: forecast points are stamped at the hour they
    // forecast (`_start`), so a mid-hour plan start (3 of 4 ticks) would exclude the CURRENT
    // hour's point and back-fill hour 0 with the next hour's values.
    let range_start =
        start - Duration::minutes(start.minute() as i64) - Duration::seconds(start.second() as i64);
    let start_str = flux_time(range_start);
    let stop_str = flux_time(start + Duration::hours(horizon as i64));
    // Radiation is read HOUR-ENDING (`h + 1` below), so the last horizon hour needs the sample
    // stamped `start + horizon`. Flux's range stop is EXCLUSIVE, so `stop_str` would drop exactly
    // that point and silently push the final hour onto the cloud-model fallback every cycle.
    let rad_stop_str = flux_time(start + Duration::hours(horizon as i64 + 1));
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
    // Radiation fields (may be absent until the writer stores them — empty is normal).
    use crate::source::RadiationField;
    let direct = db
        .weather_radiation_series(RadiationField::Direct, &start_str, &rad_stop_str, "1h")
        .await
        .unwrap_or_default();
    let diffuse = db
        .weather_radiation_series(RadiationField::Diffuse, &start_str, &rad_stop_str, "1h")
        .await
        .unwrap_or_default();
    let shortwave = db
        .weather_radiation_series(RadiationField::Shortwave, &start_str, &rad_stop_str, "1h")
        .await
        .unwrap_or_default();

    let hours: Vec<i64> = (0..horizon)
        .map(|k| hour_key(start + Duration::hours(k as i64)))
        .collect();
    let temp_keys: std::collections::HashSet<i64> = temp.iter().map(|s| hour_key(s.time)).collect();
    let covered_hours = hours.iter().filter(|h| temp_keys.contains(h)).count();
    let cloud_keys: std::collections::HashSet<i64> =
        cloud.iter().map(|s| hour_key(s.time)).collect();
    let cloud_covered_hours = hours.iter().filter(|h| cloud_keys.contains(h)).count();
    let temperature_c = resample_ffill(&hours, &temp);
    let cloud_cover = if cloud.is_empty() {
        vec![0.3; horizon]
    } else {
        resample_ffill(&hours, &cloud)
            .iter()
            .map(|pct| (pct / 100.0).clamp(0.0, 1.0))
            .collect()
    };
    // Per-hour solar chain: radiation where a real sample exists for THAT hour (no forward-fill —
    // smearing daylight radiation across a gap invents sunshine), else the cloud model.
    let key_map = |s: &[crate::influxdb::TimeSample]| -> HashMap<i64, f64> {
        let mut m = HashMap::new();
        for x in s {
            m.entry(hour_key(x.time)).or_insert(x.value);
        }
        m
    };
    let (direct_by, diffuse_by, shortwave_by) =
        (key_map(&direct), key_map(&diffuse), key_map(&shortwave));
    let mut radiation_covered_hours = 0usize;
    // Open-meteo's averaged radiation fields are the mean of the PRECEDING hour (the value
    // stamped T covers (T-1h, T]) — so forecast hour [h, h+1) reads the sample stamped h+1
    // (hour-ENDING), unlike the instantaneous temperature/cloud series. Mirrors estimate.rs.
    let solar: Vec<crate::tools::sun::SolarInput> = hours
        .iter()
        .zip(&cloud_cover)
        .map(|(h, &cloud)| {
            use crate::tools::sun::SolarInput;
            let h = &(h + 1);
            match (direct_by.get(h), diffuse_by.get(h), shortwave_by.get(h)) {
                (Some(&d), Some(&f), _) => {
                    radiation_covered_hours += 1;
                    SolarInput::Radiation {
                        direct_h: d,
                        diffuse_h: f,
                    }
                }
                (_, _, Some(&g)) => {
                    radiation_covered_hours += 1;
                    SolarInput::Ghi { ghi: g, cloud }
                }
                _ => SolarInput::Cloud { cloud },
            }
        })
        .collect();
    Ok(Some(WeatherForecast {
        temperature_c,
        cloud_cover,
        covered_hours,
        cloud_covered_hours,
        solar,
        radiation_covered_hours,
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
/// A **controllable** load IS deducted: `optimize_unified` re-adds `rated_kw · on` as a decision
/// variable, so leaving its history in the base load double-counts that appliance's electricity in
/// every in-window block, biasing battery dispatch, the grid-import cap and the load-shift choice.
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
    // Through the signal map (same reason as `pv_backtest::read_pv_kw`): the SoC read a few lines
    // below already uses `growatt_latest` so "the plan and /api/live can never disagree", and this
    // one bypassed it. A remapped `data_sources.growatt.INVPowerToLocalLoad` yielded an empty series
    // → `Ok(None)` → the planner silently trains on the flat fallback consumption model for the
    // whole horizon, biasing battery dispatch, the grid-import cap and load shifting.
    let load = db
        .growatt_series("INVPowerToLocalLoad", &start, "now()", "1h")
        .await
        .unwrap_or_default();
    if load.is_empty() {
        return Ok(None);
    }
    // Keep-first (see `estimate::keep_first_by_hour`): the trailing partial `stop=now()` window
    // shares the completed hour's key, and taking the later partial mean would bin that hour's
    // training row against a fraction of an hour's outside temperature.
    let temp_by_hour = crate::estimate::keep_first_by_hour(
        &db.read_zone_temperature_series("outside", &start, "now()", "1h")
            .await
            .unwrap_or_default(),
    );
    // Per-hour deductions (kWh over the hour = mean kW): what the LP re-adds as decisions.
    // De-duplicated hour keys: a stop=now() series' trailing partial window shares the last
    // completed hour's key, and accumulating per raw sample would count that hour's deduction
    // twice (the same collision the keep-first buckets guard elsewhere).
    let mut hours: Vec<i64> = load.iter().map(|s| hour_key(s.time)).collect();
    hours.dedup();
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
    // Only SCHEDULED chargers are deducted: the LP re-adds those as decision variables over the
    // whole horizon. A `monitored` charger never becomes an `EvSpec` — only its *current* draw is
    // folded as a ~1 h nowcast — so deducting its history here would remove that consumption from
    // the base load without anything re-adding it beyond the first hour.
    for charger in &config.chargers {
        if charger.control == crate::optimize::config::EvControl::Monitored {
            continue;
        }
        let Some(loc) = charger.sources.get("power") else {
            continue;
        };
        match db.read_locator_series(loc, &start, "now()", "1h").await {
            Ok(series) => {
                // Keep-first per hour (the trailing partial window shares the completed hour's key).
                let mut by_hour: HashMap<i64, f64> = HashMap::new();
                for s in &series {
                    by_hour.entry(hour_key(s.time)).or_insert(s.value);
                }
                for (h, kw) in by_hour {
                    // The power locator is scaled to kW by its `scale` (see config).
                    *deduction_kwh.entry(h).or_insert(0.0) += kw.max(0.0);
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

    // Controllable loads: same reasoning as the chargers — the LP re-adds them as decisions. A
    // `sensor` gives the measured draw directly; without one, fall back to the configured rating
    // over the hours its window says it could have run (the honest approximation available, and the
    // one the calibration already uses for the thermal side). An unarmed (`controllable: false`)
    // load stays in the base load, where it belongs.
    for load in config.scheduled_loads.iter().filter(|l| l.controllable) {
        let rated_kw = load.power_w.unwrap_or(0.0) / 1000.0;
        if rated_kw <= 0.0 {
            continue;
        }
        let measured = match &load.sensor {
            Some(loc) => match db.read_locator_series(loc, &start, "now()", "1h").await {
                Ok(series) => {
                    let mut by_hour: HashMap<i64, f64> = HashMap::new();
                    for s in &series {
                        by_hour.entry(hour_key(s.time)).or_insert(s.value);
                    }
                    Some(by_hour)
                }
                Err(e) => {
                    eprintln!(
                        "  consumption: controllable load {:?} sensor unavailable ({e}); deducting \
                         its rated draw over the window instead",
                        crate::optimize::coordinator::load_name(load)
                    );
                    None
                }
            },
            None => None,
        };
        match measured {
            Some(by_hour) => {
                for (h, w) in by_hour {
                    *deduction_kwh.entry(h).or_insert(0.0) += (w / 1000.0).max(0.0);
                }
            }
            None => {
                // No sensor: spread the load's DUTY across its window rather than its full rated
                // draw. It runs only `run_hours` of a window that is usually far longer, so
                // deducting the rating in every in-window hour over-subtracts by
                // `window_hours / run_hours` (a 2 kW boiler, 8 h window, 3 h target ⇒ 5 kWh/night
                // too much) and `build_consumption_model`'s `.max(0)` clamp then collapses those
                // bins toward zero — the model would forecast almost no night base load at all.
                let run_hours = load.run_hours.unwrap_or(0.0);
                for &h in temp_by_hour.keys() {
                    // The MIDPOINT of the hour this key covers. Keys are stop-stamped, so key `h`
                    // covers `[h−1h, h)` — which is exactly why `build_consumption_model` bins with
                    // `s.time − 30 min`. Evaluating the window at `h` itself tested the hour
                    // STARTING there, shifting the whole duty deduction one hour late: for a
                    // 22:00–06:00 boiler it over-deducted the 21:00 bin and left the boiler's energy
                    // in the 05:00 bin, training the base load wrong at both ends of every window.
                    let t = DateTime::from_timestamp(h * 3600 - 1800, 0).unwrap_or_default();
                    let local = t.with_timezone(&config.site.offset_at(t));
                    if load.unit_profile(local.month(), local.hour() * 60 + local.minute()) != 0.0 {
                        let window_hours = load.window_hours(local.month());
                        let duty = if window_hours > 0.0 {
                            (run_hours / window_hours).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        *deduction_kwh.entry(h).or_insert(0.0) += rated_kw * duty;
                    }
                }
            }
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
    // Keep-first per hour, like the deduction buckets: a `stop=now()` series ends with a *partial*
    // window carrying the last completed hour's key. Training it too would bin that hour twice —
    // the second time on a sub-hour mean while still subtracting a whole hour's deduction.
    let mut seen_hours: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for s in load {
        let key = hour_key(s.time);
        if !seen_hours.insert(key) {
            continue;
        }
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
    /// A 15-minute feed must be unchanged, an HOURLY feed must fill its own hour (not leave three
    /// quarters to the placeholder curve, which also bans battery arbitrage), and a genuine GAP
    /// must stay a gap so a stale feed is never masked.
    #[test]
    fn price_alignment_fills_a_samples_own_period_but_not_a_gap() {
        let t0 = chrono::DateTime::from_timestamp(0, 0).unwrap();
        let s = |mins: i64, p: f64| PriceSample {
            time: t0 + Duration::minutes(mins),
            price_eur_mwh: p,
        };
        // Native 15-minute feed: one sample per block, nothing inferred.
        let q = align_blocks_15min(&[s(0, 100.0), s(15, 200.0), s(30, 300.0)], t0, 4).unwrap();
        assert_eq!(q, vec![Some(0.1), Some(0.2), Some(0.3), None]);
        // Hourly feed: each sample covers its four blocks.
        let h = align_blocks_15min(&[s(0, 100.0), s(60, 200.0)], t0, 8).unwrap();
        assert_eq!(
            h,
            vec![
                Some(0.1),
                Some(0.1),
                Some(0.1),
                Some(0.1),
                Some(0.2),
                Some(0.2),
                Some(0.2),
                Some(0.2),
            ]
        );
        // An hourly sample stamped BEFORE the horizon start covers the horizon's leading blocks —
        // the case the 1 h query look-back exists for (block 0 was otherwise unfillable).
        let lead = align_blocks_15min(&[s(-30, 100.0), s(30, 200.0)], t0, 4).unwrap();
        assert_eq!(lead, vec![Some(0.1), Some(0.1), Some(0.2), Some(0.2)]);
        // Two samples spanning a long gap are NOT a market period: nothing fills, each sample
        // covers only its own block.
        let far = align_blocks_15min(&[s(0, 100.0), s(600, 200.0)], t0, 44).unwrap();
        assert_eq!(
            far.iter().filter(|v| v.is_some()).count(),
            2,
            "no smear across a hole"
        );
        // A uniformly sparse 15-minute feed (every other sample lost) reads as a GAPPY 15-minute
        // feed — not as a 30-minute market that would fill the losses with stale prices.
        let sparse = align_blocks_15min(&[s(0, 100.0), s(30, 200.0), s(60, 300.0)], t0, 6).unwrap();
        assert_eq!(
            sparse,
            vec![Some(0.1), None, Some(0.2), None, Some(0.3), None],
            "lost samples stay lost"
        );
        // A hole larger than the period stays None — the placeholder path must still see it.
        let g = align_blocks_15min(&[s(0, 100.0), s(15, 200.0), s(60, 300.0)], t0, 6).unwrap();
        assert_eq!(
            g,
            vec![Some(0.1), Some(0.2), None, None, Some(0.3), None],
            "a genuine gap must not be forward-filled"
        );
    }
}
