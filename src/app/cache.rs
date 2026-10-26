//! The slow-changing inputs a live tick reuses ([`PlanCache`]): the trailing PV calibration, the
//! OTE price history, and the live internal-gain re-fit.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use uom::si::f64::Angle;

use super::flat_consumption;
use crate::forecast::calibration::{Calibration, PvBandCalibration};
use crate::forecast::consumption::ConsumptionModel;
use crate::live_inputs::train_consumption;
use crate::optimize::config::ControlConfig;
use crate::pv_backtest::backtest_pv;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::validate::{self, BacktestConfig, GainFit};

/// The slow-changing plan inputs cached across the per-minute re-plans: the consumption model and
/// PV calibration are trained from days of history and don't change minute to minute. The loop
/// refreshes this every few minutes and reuses it, so only the fast state (zone temps, SoC) and the
/// horizon-aligned forecasts (weather, prices, PV) are re-read each minute.
#[derive(Debug, Clone)]
pub struct PlanCache {
    pub consumption: ConsumptionModel,
    pub calibration: PvBandCalibration,
    /// Per-zone internal gains (W) used by the plan. The MPC loop re-fits these from a trailing
    /// window (see [`fit_live_internal_gains`]) on its own slow cadence and writes them here; absent
    /// that, [`build_cache`] seeds them from the calibrated `heating` config values.
    pub internal_gains: HashMap<String, crate::optimize::config::GainProfile>,
    /// Fitted scheduled-load magnitudes (W, ≥ 0), aligned 1:1 to `config.scheduled_loads`. The MPC
    /// loop writes its live re-fit here; [`build_cache`] seeds them to zero (no effect) until the
    /// first fit lands.
    pub scheduled_w: Vec<f64>,
    /// Inputs that fell back while building this cache (neutral PV calibration, flat consumption).
    /// `current_plan` folds these into `placeholder_inputs` so a degraded cache is never presented
    /// as fully-calibrated, and the loop retries a degraded cache on a short back-off.
    pub fallbacks: Vec<String>,
    /// Bounded (≤30 day: 28 back + 2 forward) day-ahead price history for the day-type-median outlook/unpublished-block
    /// estimator (`optimize::price_forecast`), `(time, price)` — the field name carries the unit
    /// deliberately (every consumer expects EUR/kWh; storing raw OTE EUR/MWh here would be a
    /// silent 1000x): EUR/kWh, the SAME spot-price scale
    /// `fill_block_prices`'s `current`/`day_ago` use (tariffing, where needed, applies afterward —
    /// see `estimate_outlook_prices`'s and `fill_block_prices`'s own call sites). Refreshed at most
    /// [`PRICE_HISTORY_TTL`] — INDEPENDENT of `PlanCache`'s own (shorter) refresh cadence, since 28
    /// days of settled history changes slowly and a bounded-but-large query has no business running
    /// every few minutes (see `memory/`: an unbounded price query once caused a failsafe).
    pub price_history_eur_kwh: Vec<(DateTime<Utc>, f64)>,
    /// When [`Self::price_history_eur_kwh`] was last ATTEMPTED — set on both success and failure
    /// (leaving it unset on failure would retry every `build_cache` call during an InfluxDB outage
    /// instead of backing off); `None` means never attempted (forces a
    /// refresh on the next [`build_cache`]).
    pub price_history_fetched_at: Option<DateTime<Utc>>,
    /// When [`build_cache`] produced this cache. The loop keeps an old cache when a rebuild times
    /// out, so its age is what tells a plan its slow inputs have gone stale.
    pub built_at: std::time::Instant,
}

/// Past this age a kept cache is flagged in the plan: twice the loop's refresh TTL, i.e. at least
/// one refresh missed.
const SLOW_INPUTS_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(2 * crate::mpc_loop::CACHE_TTL.as_secs());

/// The placeholder for a cache older than [`SLOW_INPUTS_STALE_AFTER`], `None` while it is fresh.
pub fn stale_slow_inputs_note(age: std::time::Duration) -> Option<String> {
    (age > SLOW_INPUTS_STALE_AFTER).then(|| {
        format!(
            "slow inputs (stale: last refreshed {} min ago)",
            age.as_secs() / 60
        )
    })
}

/// Minimum spacing between `price_history_eur_kwh` refreshes — the history changes slowly (settled
/// day-ahead prices), and re-querying it every `PlanCache` cycle (as often as every couple of
/// minutes under [`crate::mpc_loop::DEGRADED_CACHE_RETRY`]) would be needless InfluxDB load for no
/// benefit.
const PRICE_HISTORY_TTL: Duration = Duration::hours(1);
/// How far back `price_history_eur_kwh` reads — enough same-day-type history for the `K = 4`
/// median even on a house with only Saturdays/Sundays sparsely represented, bounded so the query
/// can never grow unbounded (see [`PlanCache::price_history_eur_kwh`]'s doc).
const PRICE_HISTORY_DAYS: i64 = 28;
/// Wall-clock budget for the price-history read itself — shorter than `InfluxClient::query`'s own
/// 30 s `QUERY_TIMEOUT` (`influxdb.rs`): this is a background cache refresh, not a plan tick, and
/// must not sit for that long during an InfluxDB overload.
const PRICE_HISTORY_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether `price_history_eur_kwh` needs a refresh: never attempted (`None`), or
/// [`PRICE_HISTORY_TTL`] has elapsed since the last ATTEMPT (success or failure). Pure — extracted
/// from [`build_cache`] so the "at most hourly" gate is directly unit-testable without a
/// live/mocked `SourceClients`.
fn price_history_is_stale(fetched_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    fetched_at.is_none_or(|t| now - t >= PRICE_HISTORY_TTL)
}

/// Minimum scored (clean daylight) hours before the PV backtest ratio is trusted as a calibration.
/// Below this, one cloudy afternoon could fit a clamped 0.5×/2.0× scale from noise. `pub(crate)`:
/// also the gate `pv_backtest`'s nowcast replay uses to fit its own trailing per-date calibration
/// the same way this live cache does.
pub(crate) const CALIBRATION_MIN_SCORED_HOURS: usize = 24;
/// Minimum clean hours in one local-time band before its own ratio is trusted over the overall.
pub(crate) const CALIBRATION_MIN_BAND_HOURS: usize = 8;

/// Build the cacheable slow inputs — the 7-day PV-calibration backtest and the trailing-window
/// consumption training (the two heaviest reads). Refreshed periodically by the MPC loop. The
/// internal gains start at the config baseline; the loop overwrites them with its live re-fit.
/// Every fallback taken here is recorded in `fallbacks` — the loop path has no other way to know.
/// `previous` is the cache being replaced, when there is one. A component that FAILS to refresh
/// keeps the previous good value instead of collapsing to its fallback: a transient Influx blip at
/// refresh time would otherwise throw away a well-trained consumption model and a real PV
/// calibration, replace them with a flat 0.4 kWh/h curve and a neutral multiplier, and — since the
/// loop stores the refreshed cache unconditionally — actuate a plan built on them for a full TTL.
/// A stale-but-real model beats a fresh-but-fabricated one; the substitution is still recorded in
/// `fallbacks`, so the degradation stays visible.
pub async fn build_cache(
    db: &SourceClients,
    net: &RcNetwork,
    config: &ControlConfig,
    previous: Option<&PlanCache>,
) -> PlanCache {
    let mut fallbacks = Vec::new();
    let calibration = match backtest_pv(db, &config.site, 7, &config.pv.nowcast).await {
        Ok(bt) if bt.scored_hours >= CALIBRATION_MIN_SCORED_HOURS => {
            // Shape-aware: per-band ratios where a band has enough clean hours, the totals ratio
            // elsewhere — a totals-only scalar corrects energy but not the shoulder-of-day timing
            // the battery's morning/evening decisions ride on.
            PvBandCalibration::from_backtest(
                bt.band_solcast_kwh,
                bt.band_actual_kwh,
                bt.band_clean_hours,
                Calibration::from_totals_default(bt.total_solcast_kwh, bt.total_actual_kwh),
                CALIBRATION_MIN_BAND_HOURS,
            )
        }
        Ok(bt) => {
            fallbacks.push(format!(
                "PV calibration ({} scored hours < {CALIBRATION_MIN_SCORED_HOURS}; neutral)",
                bt.scored_hours
            ));
            PvBandCalibration::neutral()
        }
        Err(_) => match previous.map(|p| p.calibration) {
            // A failed READ is not evidence the calibration changed — keep the last good one.
            Some(prev) => {
                fallbacks
                    .push("PV calibration (backtest failed; kept the previous fit)".to_string());
                prev
            }
            None => {
                fallbacks.push("PV calibration (backtest failed; neutral)".to_string());
                PvBandCalibration::neutral()
            }
        },
    };
    let consumption = match train_consumption(db, net, config).await {
        Ok(Some(m)) => m,
        _ => match previous.map(|p| p.consumption.clone()) {
            Some(prev) => {
                fallbacks
                    .push("consumption (training failed; kept the previous model)".to_string());
                prev
            }
            None => {
                fallbacks.push("consumption (training failed; flat 0.4 kWh/h)".to_string());
                flat_consumption()
            }
        },
    };
    // Bounded (≤30 day: 28 back + 2 forward), hourly-refreshed price history for the day-type median estimator (see
    // `PlanCache::price_history_eur_kwh`'s doc) — refreshed independently of everything else
    // above, because 28 days of settled prices doesn't need re-reading every cache cycle.
    let stale = price_history_is_stale(
        previous.and_then(|p| p.price_history_fetched_at),
        Utc::now(),
    );
    let (price_history_eur_kwh, price_history_fetched_at) = if stale {
        let now = Utc::now();
        // A SHORT timeout of its own: `InfluxClient::query`'s own
        // 30 s `QUERY_TIMEOUT` is sized for a plan tick, not a background cache refresh — during
        // an InfluxDB overload this read must not sit for that long. On EITHER a timeout or a read
        // error the OLD history is kept (an empty/stale history just means every block falls back
        // to persistence — see `estimate_outlook_prices` / `fill_block_prices` — so keeping a
        // slightly stale one is strictly better than discarding real data over a transient blip),
        // but the attempt is STAMPED regardless of outcome: leaving `price_history_fetched_at`
        // unchanged on failure would retry on every `build_cache` call during an outage (as often
        // as every couple of minutes under the loop's degraded-cache back-off), hammering an
        // already-struggling InfluxDB; stamping `now` makes the next attempt wait the full hourly
        // TTL either way.
        // Stop 48 h PAST `now`, not AT it: stopping at `now` would miss today's own remaining
        // published blocks and tomorrow's day-ahead curve once the ~14:00 OTE auction ran — real,
        // already-published prices the estimator should see. Safe to include:
        // `day_type_median_price` only ever uses a sample whose date is STRICTLY BEFORE the
        // estimate's own target date, so a future-relative-to-`now` sample is simply unused until
        // some LATER target date makes it real history. Total span stays bounded (≤ 30 days: 28
        // back + 2 forward).
        let read = tokio::time::timeout(
            PRICE_HISTORY_QUERY_TIMEOUT,
            db.read_prices_range(
                &crate::live_inputs::flux_time(now - Duration::days(PRICE_HISTORY_DAYS)),
                &crate::live_inputs::flux_time(now + Duration::hours(48)),
            ),
        )
        .await;
        match read {
            Ok(Ok(samples)) => (
                samples
                    .into_iter()
                    // EUR/MWh -> EUR/kWh (`live_inputs.rs`'s `align_blocks_15min` applies the
                    // SAME `/ 1000.0` to the horizon's own `current`/`day_ago` prices — this
                    // history must match that scale, or the day-type median comes out 1000x
                    // real prices).
                    .map(|s| (s.time, s.price_eur_mwh / 1000.0))
                    .collect(),
                Some(now),
            ),
            Ok(Err(_)) | Err(_) => (
                previous
                    .map(|p| p.price_history_eur_kwh.clone())
                    .unwrap_or_default(),
                Some(now),
            ),
        }
    } else {
        let p = previous.expect("stale is false only when previous carries a fetch timestamp");
        (p.price_history_eur_kwh.clone(), p.price_history_fetched_at)
    };

    PlanCache {
        consumption,
        calibration,
        internal_gains: config.heating.internal_gains(),
        // Configured magnitudes (fixed `power_w` used as-is, fitted loads 0) until the live re-fit
        // overwrites the fitted ones.
        scheduled_w: config
            .scheduled_loads
            .iter()
            .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
            .collect(),
        fallbacks,
        price_history_eur_kwh,
        price_history_fetched_at,
        built_at: std::time::Instant::now(),
    }
}

/// Re-fit the live per-zone internal gains from a trailing window of measured temperatures and the
/// recorded heating relays — the self-correction that lets the model track changes in occupant
/// behaviour (more/fewer people, appliance and fireplace use) without any config or model edit.
///
/// Returns `Some(fit)` on a successful fit — including **empty** gains, which is the legitimate answer
/// "the data shows no extra gain is needed" (e.g. summer, or a fireplace that's stopped being used),
/// so the caller should trust it. The fit also carries the per-scheduled-load magnitudes (W, aligned
/// to `config.scheduled_loads`). Returns `None` only when the fit can't run (no data / sensors down),
/// so the caller keeps its last-good values rather than discarding them.
pub async fn fit_live_internal_gains(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    config: &ControlConfig,
    latitude: Angle,
    longitude: Angle,
) -> Option<GainFit> {
    let window_days = config.internal_gain_window_days.max(3);
    let cfg = BacktestConfig {
        warmup_hours: 48, // relax the unknown slab seed before scoring
        window_hours: (window_days * 24 - 48).max(24),
        ground_temperature_c: config.site.ground_temperature_c,
        cloud_cover: 0.5,
    };
    let local_offset = config.site.offset_at(Utc::now());
    let start = format!("-{window_days}d");
    match validate::fit_internal_gains(
        db,
        net,
        ss,
        &config.heating,
        &config.scheduled_loads,
        local_offset,
        Some(&config.site),
        latitude,
        longitude,
        &cfg,
        &start,
        "now()",
    )
    .await
    {
        Ok(fit) => Some(fit),
        Err(e) => {
            eprintln!("[mpc] internal-gain re-fit failed ({e}); keeping previous gains");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::*;

    /// A cache within two refresh TTLs is silent; past that the plan names its age.
    #[test]
    fn stale_slow_inputs_note_flags_a_cache_past_two_ttls() {
        let min = |m: u64| std::time::Duration::from_secs(m * 60);
        assert_eq!(stale_slow_inputs_note(min(0)), None);
        assert_eq!(stale_slow_inputs_note(min(30)), None);
        assert_eq!(
            stale_slow_inputs_note(min(47)).as_deref(),
            Some("slow inputs (stale: last refreshed 47 min ago)")
        );
    }

    /// `price_history` refreshes at most [`PRICE_HISTORY_TTL`] (hourly) —
    /// never on every `build_cache` cycle, which can run as often as every couple of minutes.
    #[test]
    fn price_history_is_stale_respects_the_hourly_ttl() {
        let now = utc("2024-01-15T12:00:00Z");
        assert!(price_history_is_stale(None, now), "never fetched -> stale");
        assert!(
            !price_history_is_stale(Some(now - Duration::minutes(30)), now),
            "30 min ago, well under the 1 h TTL -> NOT stale"
        );
        assert!(
            !price_history_is_stale(Some(now - Duration::minutes(59)), now),
            "just under the TTL -> NOT stale"
        );
        assert!(
            price_history_is_stale(Some(now - Duration::hours(1)), now),
            "exactly the TTL -> stale (>=), so a refresh always eventually happens"
        );
        assert!(
            price_history_is_stale(Some(now - Duration::hours(2)), now),
            "well past the TTL -> stale"
        );
    }

    /// A FAILED refresh attempt must still stamp `price_history_fetched_at`
    /// (`build_cache` does this on both `Ok` and `Err`/timeout) — otherwise the next `build_cache`
    /// call (as soon as `DEGRADED_CACHE_RETRY` later) sees `stale` again and retries immediately,
    /// hammering an already-struggling InfluxDB on every rebuild instead of backing off to the
    /// hourly TTL. This tests the pure staleness fn's side of that contract: an attempt stamped
    /// "just now" (whether it succeeded or not) must read as NOT stale a few minutes later.
    #[test]
    fn price_history_is_stale_backs_off_after_a_stamped_failed_attempt() {
        let attempted_at = utc("2024-01-15T12:00:00Z");
        let soon_after = attempted_at + Duration::minutes(2); // e.g. the next degraded-cache retry
        assert!(
            !price_history_is_stale(Some(attempted_at), soon_after),
            "an attempt stamped 2 min ago (success OR failure) must NOT be stale yet — the next \
             rebuild must wait out the hourly TTL, not retry immediately"
        );
        // An hour later, it's stale again regardless of whether that stamped attempt succeeded.
        assert!(price_history_is_stale(
            Some(attempted_at),
            attempted_at + Duration::hours(1)
        ));
    }
}
