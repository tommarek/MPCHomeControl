//! Pieces shared by the read-only real-history backtest tools (`warmth_backtest`,
//! `heating_backtest`): the bounded ≤7-day chunked reads, the known-at-`T` market replay and the
//! per-origin state seed.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Duration, FixedOffset, NaiveDate, Timelike, Utc};
use nalgebra::DVector;

use crate::app::{fill_block_prices, placeholder_price_curve, tariff_prices};
use crate::estimate::hour_key;
use crate::influxdb::{PriceSample, TimeSample};
use crate::optimize::config::ControlConfig;
use crate::optimize::grid::BlockGrid;
use crate::optimize::price_forecast::day_type_median_price;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;

const FINE_SECONDS: f64 = 900.0;
const FINE_SECONDS_I: i64 = 900;
/// Influx-read chunk size (days) — the ≤7-day read bound.
const CHUNK_DAYS: i64 = 7;

/// How long to pause between successive per-day InfluxDB windows — the backtests' version of
/// `what_if.rs`'s "never run unbounded/rapid-fire queries on the live server" rule.
pub(crate) const INTER_DAY_PAUSE_S: u64 = 2;

// --- Bounded reads ---------------------------------------------------------------------------------

/// Split `[start, stop)` into consecutive spans of at most [`CHUNK_DAYS`] days.
pub(crate) fn chunk_windows(
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut s = start;
    while s < stop {
        let e = (s + Duration::days(CHUNK_DAYS)).min(stop);
        out.push((s, e));
        s = e;
    }
    out
}

pub(crate) fn floor_to_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    t - Duration::minutes(t.minute() as i64)
        - Duration::seconds(t.second() as i64)
        - Duration::nanoseconds(t.nanosecond() as i64)
}

pub(crate) async fn read_prices_chunked(
    db: &SourceClients,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Result<Vec<PriceSample>> {
    let mut all = Vec::new();
    let windows = chunk_windows(start, stop);
    let n = windows.len();
    for (i, (s, e)) in windows.into_iter().enumerate() {
        let mut samples = db
            .read_prices_range(&s.to_rfc3339(), &e.to_rfc3339())
            .await
            .with_context(|| format!("reading OTE prices {s}..{e}"))?;
        all.append(&mut samples);
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    Ok(all)
}

pub(crate) async fn read_growatt_chunked(
    db: &SourceClients,
    metric: &str,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
) -> Vec<TimeSample> {
    let mut all = Vec::new();
    let windows = chunk_windows(start, stop);
    let n = windows.len();
    for (i, (s, e)) in windows.into_iter().enumerate() {
        let mut samples = db
            .growatt_series(metric, &s.to_rfc3339(), &e.to_rfc3339(), "15m")
            .await
            .unwrap_or_default();
        all.append(&mut samples);
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    all
}

pub(crate) async fn read_soc_seed(
    db: &SourceClients,
    window_start: DateTime<Utc>,
    config: &ControlConfig,
) -> f64 {
    let stop = window_start + Duration::hours(1);
    db.growatt_series("SOC", &window_start.to_rfc3339(), &stop.to_rfc3339(), "15m")
        .await
        .ok()
        .and_then(|s| {
            s.first()
                .map(|x| x.value / 100.0 * config.battery.capacity_kwh)
        })
        .unwrap_or_else(|| config.battery.min_soc_pct / 100.0 * config.battery.capacity_kwh)
}

// --- The known-at-`T` market replay ---------------------------------------------------------------

/// The latest LOCAL calendar date whose day-ahead prices are published as of `t_local` — OTE
/// publishes day D's curve on D-1 at `publish_hour` local (default ~14:00): `t_local`'s own date
/// before that hour, the NEXT date from it on.
pub(crate) fn known_until_local_date(
    t_local: DateTime<FixedOffset>,
    publish_hour: u32,
) -> NaiveDate {
    if t_local.hour() >= publish_hour {
        t_local.date_naive() + Days::new(1)
    } else {
        t_local.date_naive()
    }
}

/// Is a block whose delivery falls on `delivery_local_date` REAL (published), given everything up
/// to and including `known_until` is published?
pub(crate) fn is_real_as_of(delivery_local_date: NaiveDate, known_until: NaiveDate) -> bool {
    delivery_local_date <= known_until
}

/// `(current, estimated, day_ago)` — the same shape `app::fill_block_prices` consumes.
type PriceFallbackInputs = (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>);

/// Real (published-as-of-`known_until`) spot-price samples from the FULL window array, as
/// `(time, price)` pairs — the day-type-median estimator's `history` argument. Built ONCE per hour
/// by [`prepare_hour`] and shared by [`known_at_t_price_inputs`]'s per-block fallback AND the
/// caller's post-horizon curve, instead of each independently re-scanning `real_spot_fine`.
fn history_known_at(
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    known_until: NaiveDate,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset,
) -> Vec<(DateTime<Utc>, f64)> {
    real_spot_fine
        .iter()
        .enumerate()
        .filter_map(|(i, &p)| {
            let at = array_start + Duration::seconds(FINE_SECONDS_I * i as i64);
            let real = is_real_as_of(at.with_timezone(&offset(at)).date_naive(), known_until);
            real.then_some(()).and(p).map(|v| (at, v))
        })
        .collect()
}

/// Build this plan's spot-price fallback inputs (`current`/`estimated`/`day_ago` — the same shape
/// `app::fill_block_prices` consumes live) from the FULL window's real (ground-truth) spot-price
/// array, respecting only what was KNOWN at `t` — a later day's real price never leaks into an
/// earlier plan. `real_spot_fine[i]` is the real spot price (EUR/kWh) `i` fine (900 s) steps after
/// `array_start`; `None` is a genuine data gap. `known_until`/`history` are the per-hour
/// [`history_known_at`] outputs.
#[allow(clippy::too_many_arguments)]
fn known_at_t_price_inputs(
    known_until: NaiveDate,
    history: &[(DateTime<Utc>, f64)],
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    t: DateTime<Utc>,
    n_fine: usize,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset,
    public_holidays: &[(u32, u32)],
    easter_holidays: bool,
) -> PriceFallbackInputs {
    let t_fine_index = (t - array_start).num_seconds().div_euclid(FINE_SECONDS_I);
    let at_index = |idx: i64| -> Option<f64> {
        usize::try_from(idx)
            .ok()
            .and_then(|i| real_spot_fine.get(i).copied().flatten())
    };

    let mut current = Vec::with_capacity(n_fine);
    let mut estimated = Vec::with_capacity(n_fine);
    let mut day_ago = Vec::with_capacity(n_fine);
    for b in 0..n_fine {
        let idx = t_fine_index + b as i64;
        let at = t + Duration::seconds(FINE_SECONDS_I * b as i64);
        let real = is_real_as_of(at.with_timezone(&offset(at)).date_naive(), known_until);
        current.push(if real { at_index(idx) } else { None });
        estimated.push(day_type_median_price(
            history,
            at,
            &offset,
            public_holidays,
            easter_holidays,
        ));
        // The day-ago candidate (the SAME clock block one day earlier) must ALSO have been
        // published as of `t` — live `block_prices`' `day_ago` only ever holds published prices,
        // and near the far edge of the horizon the day-ago delivery day can itself still be in the
        // future relative to `t` (a target 2+ days out). Without this gate the backtest would leak
        // a real price from a day that, at `t`, hadn't been auctioned yet.
        let at_day_ago = at - Duration::hours(24);
        let day_ago_real = is_real_as_of(
            at_day_ago.with_timezone(&offset(at_day_ago)).date_naive(),
            known_until,
        );
        day_ago.push(if day_ago_real {
            at_index(idx - 96)
        } else {
            None
        });
    }
    (current, estimated, day_ago)
}

/// Forward-fill `None` gaps with the last known (or first-available, before any value has been
/// seen) sample; returns the filled series and how many blocks were filled.
pub(crate) fn forward_fill(v: Vec<Option<f64>>) -> (Vec<f64>, usize) {
    let mut filled = 0usize;
    let mut last = v.iter().find_map(|&x| x).unwrap_or(0.0);
    let out = v
        .into_iter()
        .map(|x| match x {
            Some(val) => {
                last = val;
                val
            }
            None => {
                filled += 1;
                last
            }
        })
        .collect();
    (out, filled)
}

/// Everything one hour `t`'s plan needs from the market side, on the fine (15-min) lattice:
/// prices known-as-of `t`, the measured PV/load perfect-foresight slice, the export/inverter gates
/// and the block grid. `history` is the day-type-median estimator's input (real prices published
/// by `t`); `mask_fine` flags the placeholder-priced blocks.
pub(crate) struct HourPrep {
    pub(crate) grid: BlockGrid,
    pub(crate) history: Vec<(DateTime<Utc>, f64)>,
    pub(crate) import_fine: Vec<f64>,
    pub(crate) export_fine: Vec<f64>,
    pub(crate) mask_fine: Vec<bool>,
    pub(crate) export_allowed_fine: Vec<bool>,
    pub(crate) inverter_on_fine: Vec<bool>,
    /// Measured PV/load (kW), perfect foresight — the caller feeds these straight into
    /// `ForecastContext::pv_kw_override`/`load_kw_override`.
    pub(crate) pv_fine: Vec<f64>,
    pub(crate) load_fine: Vec<f64>,
}

/// Build [`HourPrep`] for hour `t`: the one known-at-`T` price/foresight/gate construction, so no
/// replay can see a later day's real price or a different idea of what hour `t` knew.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_hour(
    t: DateTime<Utc>,
    config: &ControlConfig,
    array_start: DateTime<Utc>,
    real_spot_fine: &[Option<f64>],
    pv_kw: &[f64],
    load_kw: &[f64],
    window_start: DateTime<Utc>,
    publish_hour: u32,
    offset: impl Fn(DateTime<Utc>) -> FixedOffset + Copy,
    public_holidays: &[(u32, u32)],
    export_floor: f64,
    inverter_off_price: f64,
) -> HourPrep {
    let grid = BlockGrid::multi_rate(
        t,
        config.horizon.hours,
        config.horizon.fine_hours,
        FINE_SECONDS,
    );
    let n_fine = grid.n_fine();

    // Built ONCE per hour, shared by the fallback chain below AND the caller's post-horizon
    // curve — see `history_known_at`'s doc.
    let known_until = known_until_local_date(t.with_timezone(&offset(t)), publish_hour);
    let history = history_known_at(array_start, real_spot_fine, known_until, offset);

    let (current, estimated, day_ago) = known_at_t_price_inputs(
        known_until,
        &history,
        array_start,
        real_spot_fine,
        t,
        n_fine,
        offset,
        public_holidays,
        config.site.easter_holidays,
    );
    let placeholder = placeholder_price_curve(t, offset(t), n_fine);
    let (spot_fine, mask_fine, _missing, _persisted, _estimated_count) =
        fill_block_prices(&current, &estimated, &day_ago, &placeholder);
    let (import_fine, export_fine) = tariff_prices(&config.tariff, &config.site, &spot_fine, t);
    let export_allowed_fine: Vec<bool> = spot_fine.iter().map(|&s| s >= export_floor).collect();
    let inverter_on_fine: Vec<bool> = spot_fine.iter().map(|&s| s >= inverter_off_price).collect();

    // Perfect foresight: the measured PV/load series stand in for the forecast directly.
    let meas_offset = ((t - window_start).num_seconds() / FINE_SECONDS_I) as usize;
    let pv_slice: Vec<f64> = (0..n_fine)
        .map(|i| pv_kw.get(meas_offset + i).copied().unwrap_or(0.0))
        .collect();
    let load_slice: Vec<f64> = (0..n_fine)
        .map(|i| load_kw.get(meas_offset + i).copied().unwrap_or(0.0))
        .collect();

    HourPrep {
        grid,
        history,
        import_fine,
        export_fine,
        mask_fine,
        export_allowed_fine,
        inverter_on_fine,
        pv_fine: pv_slice,
        load_fine: load_slice,
    }
}

// --- Per-origin state seed -------------------------------------------------------------------------

/// Seed each measured zone's air state at the latest sample at-or-before `at_hour` (falling back
/// to the earliest available sample, then to a flat 20 °C); other (wall/slab) states at the mean
/// of the seeded zones — the per-origin equivalent of [`crate::estimate::seed_state`], but pure
/// (no IO — the series are already in memory).
pub(crate) fn seed_from_series(
    net: &RcNetwork,
    ss: &StateSpace,
    zone_series: &HashMap<String, Vec<TimeSample>>,
    at_hour: i64,
) -> DVector<f64> {
    let mut per_zone: HashMap<&str, f64> = HashMap::new();
    for (zone, series) in zone_series {
        let mut candidate: Option<&TimeSample> = None;
        for s in series {
            let hk = hour_key(s.time);
            if hk <= at_hour {
                candidate = Some(s);
            } else {
                break;
            }
        }
        let picked = candidate.or_else(|| series.first());
        if let Some(s) = picked {
            per_zone.insert(zone.as_str(), s.value);
        }
    }
    // Summed in zone order so the seed is bit-reproducible run to run.
    let per_zone: BTreeMap<&str, f64> = per_zone.into_iter().collect();
    let base_c = if per_zone.is_empty() {
        20.0
    } else {
        per_zone.values().sum::<f64>() / per_zone.len() as f64
    };
    let mut x = DVector::from_element(ss.n_states(), crate::tools::c_to_k(base_c));
    for (zone, c) in &per_zone {
        if let Some(&node) = net.zone_indices.get(*zone) {
            if let Some(row) = ss.state_index(node) {
                x[row] = crate::tools::c_to_k(*c);
            }
        }
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn known_until_rolls_over_at_the_publish_hour() {
        let plus1 = FixedOffset::east_opt(3600).unwrap();
        // 13:00 local (before the 14:00 publish hour): only today's own date is known.
        let before = utc(2026, 9, 30, 12, 0).with_timezone(&plus1);
        assert_eq!(
            known_until_local_date(before, 14),
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
        // 14:00 local (at the publish hour): tomorrow is known too.
        let after = utc(2026, 9, 30, 13, 0).with_timezone(&plus1);
        assert_eq!(
            known_until_local_date(after, 14),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
    }

    #[test]
    fn is_real_as_of_matches_the_known_until_cutoff() {
        let known = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        assert!(is_real_as_of(
            NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(),
            known
        ));
        assert!(is_real_as_of(known, known));
        assert!(!is_real_as_of(
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            known
        ));
    }

    #[test]
    fn forward_fill_counts_gaps_and_holds_the_last_value() {
        let (v, n) = forward_fill(vec![None, Some(1.0), None, None, Some(4.0)]);
        assert_eq!(v, vec![1.0, 1.0, 1.0, 1.0, 4.0]);
        assert_eq!(n, 3);
    }

    #[test]
    fn chunk_windows_splits_into_at_most_seven_day_spans() {
        let start = utc(2026, 1, 1, 0, 0);
        let stop = start + Duration::days(16);
        let chunks = chunk_windows(start, stop);
        assert_eq!(chunks.len(), 3); // 7 + 7 + 2
        for (s, e) in &chunks {
            assert!((*e - *s) <= Duration::days(7));
        }
        assert_eq!(chunks[0].0, start);
        assert_eq!(chunks.last().unwrap().1, stop);
    }

    /// Acceptance (a): T before the publish hour — only today's own blocks are real; tomorrow's
    /// `current` is None even though our (hindsight) dataset already has tomorrow's actual price.
    #[test]
    fn known_at_t_price_inputs_before_publish_hour_only_today_is_real() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0); // day D midnight
        let t = array_start; // T = D 00:00, before the 14:00 publish hour
        let mut real_spot_fine = vec![Some(0.10); 96]; // day D
        real_spot_fine.extend(vec![Some(0.20); 96]); // day D+1 (unpublished as of T)
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let history = Vec::new();
        let (current, _estimated, _day_ago) = known_at_t_price_inputs(
            known_until,
            &history,
            array_start,
            &real_spot_fine,
            t,
            96 * 2,
            offset,
            &[],
            false,
        );
        assert!(current[0].is_some(), "today's own first block must be real");
        assert!(current[95].is_some(), "today's own last block must be real");
        assert!(
            current[96].is_none(),
            "tomorrow must NOT be real before the publish hour, even though the dataset has it"
        );
        assert!(current[191].is_none());
    }

    /// Acceptance (b): T at/after the publish hour — tomorrow is now real too.
    #[test]
    fn known_at_t_price_inputs_at_publish_hour_tomorrow_is_real() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0);
        let t = utc(2026, 9, 28, 14, 0); // T = D 14:00, AT the publish hour
        let mut real_spot_fine = vec![Some(0.10); 96];
        real_spot_fine.extend(vec![Some(0.20); 96]);
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let history = Vec::new();
        // A few fine steps past T is enough to cross the D -> D+1 midnight boundary.
        let idx_midnight = 96 - 56; // t is 14:00 (fine step 56 of the day); midnight is 40 steps on
        let (current, _estimated, _day_ago) = known_at_t_price_inputs(
            known_until,
            &history,
            array_start,
            &real_spot_fine,
            t,
            idx_midnight + 4,
            offset,
            &[],
            false,
        );
        assert!(
            current[idx_midnight].is_some(),
            "tomorrow must be real at/after the publish hour"
        );
    }

    /// Acceptance (c): a D+2 block's `day_ago` (the D+1 clock-twin) is None when D+1 is
    /// unpublished as of T, Some when it's published — regardless of the dataset having D+1's
    /// actual value either way (hindsight must never leak ahead of what T actually knew).
    #[test]
    fn known_at_t_price_inputs_day_ago_for_a_d_plus_2_block_respects_the_knowledge_cutoff() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(0).unwrap();
        let array_start = utc(2026, 9, 28, 0, 0); // day D midnight
        let t = array_start; // T = D 00:00
        let mut real_spot_fine = vec![Some(0.10); 96]; // D
        real_spot_fine.extend(vec![Some(0.20); 96]); // D+1
        real_spot_fine.extend(vec![Some(0.30); 96]); // D+2
        let history = Vec::new();
        let n_fine = 96 * 2 + 1; // reach the D+2 00:00 block
        let d_plus_2_idx = 192;

        // D+1 UNPUBLISHED as of T (only D itself is known): day_ago must be None.
        let known_until_unpublished = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let (_current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until_unpublished,
            &history,
            array_start,
            &real_spot_fine,
            t,
            n_fine,
            offset,
            &[],
            false,
        );
        assert_eq!(
            day_ago[d_plus_2_idx], None,
            "D+1 isn't known at T yet; day_ago must not leak its real price"
        );

        // D+1 PUBLISHED as of T: day_ago may use it.
        let known_until_published = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(); // D+1
        let (_current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until_published,
            &history,
            array_start,
            &real_spot_fine,
            t,
            n_fine,
            offset,
            &[],
            false,
        );
        assert_eq!(day_ago[d_plus_2_idx], Some(0.20));
    }

    /// The `day_ago` gate classifies by LOCAL date: at UTC+2 a block at D+1 22:30 UTC is D+2 00:30
    /// local, so its day-ago (D+1 00:30 local) is unpublished when only D is known — a UTC-date
    /// check would have leaked it.
    #[test]
    fn known_at_t_price_inputs_day_ago_gate_uses_the_local_date() {
        let offset = |_: DateTime<Utc>| FixedOffset::east_opt(2 * 3600).unwrap();
        let array_start = utc(2026, 9, 27, 22, 0); // D 00:00 local
        let t = array_start;
        let mut real_spot_fine = vec![Some(0.10); 96]; // D
        real_spot_fine.extend(vec![Some(0.20); 96]); // D+1
        real_spot_fine.extend(vec![Some(0.30); 96]); // D+2
        let known_until = known_until_local_date(t.with_timezone(&offset(t)), 14);
        let (current, _estimated, day_ago) = known_at_t_price_inputs(
            known_until,
            &[],
            array_start,
            &real_spot_fine,
            t,
            96 * 2 + 2,
            offset,
            &[],
            false,
        );
        let d_plus_2_0030_local = 96 * 2 + 1; // 2026-09-29 22:15 UTC
        assert_eq!(current[95], Some(0.10), "D 23:45 local is published");
        assert_eq!(current[96], None, "D+1 00:00 local is not");
        assert_eq!(day_ago[d_plus_2_0030_local], None);
    }

    #[test]
    fn seed_from_series_picks_the_latest_sample_at_or_before() {
        let model = crate::model::Model::from_json(
            r#"{
                materials: { m: { thermal_conductivity: 1, specific_heat_capacity: 1000, density: 1000 } },
                boundary_types: { wall: { layers: [ { material: "m", thickness: 0.2 } ] } },
                zones: { room: { volume: 50 } },
                boundaries: [ { boundary_type: "wall", zones: ["room", "outside"], area: 20 } ],
            }"#,
        )
        .unwrap();
        let net: RcNetwork = (&model).into();
        let ss: StateSpace = (&net).into();
        let t0 = utc(2026, 1, 1, 0, 0);
        let series = vec![
            TimeSample {
                time: t0,
                value: 18.0,
            },
            TimeSample {
                time: t0 + Duration::hours(2),
                value: 21.0,
            },
            TimeSample {
                time: t0 + Duration::hours(5),
                value: 23.0,
            },
        ];
        let zones = HashMap::from([("room".to_string(), series)]);
        let row = ss.state_index(net.zone_indices["room"]).unwrap();

        let x = seed_from_series(&net, &ss, &zones, hour_key(t0 + Duration::hours(3)));
        assert!((crate::tools::k_to_c(x[row]) - 21.0).abs() < 1e-9);

        // Before the first sample: falls back to the earliest available.
        let x_early = seed_from_series(&net, &ss, &zones, hour_key(t0 - Duration::hours(10)));
        assert!((crate::tools::k_to_c(x_early[row]) - 18.0).abs() < 1e-9);
    }
}
