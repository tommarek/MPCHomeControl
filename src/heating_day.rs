//! `GET /api/heating/day` — everything the dashboard's one-day heating chart needs for ONE
//! site-local calendar day, except the relay history (which Home Assistant owns): per room the
//! measured temperature, the plan's predicted temperature, the planned heat, the comfort band, and
//! 15-minute context slots (price, outside temperature, PV, house load).
//!
//! API CONTRACT: the Home Assistant dashboard in ~/git/selfhosted consumes this API. This endpoint
//! is purely additive (no `API_VERSION` bump); a change to its shape must be mirrored there.
//!
//! This module is **pure and IO-free**: [`build`] takes plain-data inputs (the measured series the
//! handler read best-effort, the latest plan's timeline, the ledger rows) and returns the report.
//! `web::get_heating_day` is the thin handler that gathers them. Honesty rule as everywhere: what the
//! brain does not know is `null` or absent, never a default.
//!
//! Time conventions: slots are stamped at their START; measured series are stamped at the END of the
//! window they average (InfluxDB `aggregateWindow` default, same as `/api/history` and
//! `/api/zones/series`); `predicted` is stamped at the block END (the plan's `temp_c` is the
//! temperature at the end of the block).

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::app::TimelineBlock;
use crate::ledger::LedgerRow;
use crate::optimize::config::SiteConfig;

/// How many days back a caller may ask for.
pub const MAX_DAYS_BACK: i64 = 13;
/// How many days ahead (tomorrow).
pub const MAX_DAYS_AHEAD: i64 = 1;
/// Slot length (minutes).
pub const SLOT_MINUTES: i64 = 15;
/// Planned heat at or below this (kW) is "zero" and omitted from `plan_heat`.
const PLAN_HEAT_EPS_KW: f64 = 0.001;

/// One local calendar day as UTC instants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayWindow {
    pub date: NaiveDate,
    /// Local midnight, as UTC.
    pub start: DateTime<Utc>,
    /// The NEXT local midnight, as UTC (23 h / 25 h after `start` on DST days).
    pub end: DateTime<Utc>,
}

/// The site-local calendar date of `t`.
pub fn local_date(t: DateTime<Utc>, site: &SiteConfig) -> NaiveDate {
    t.with_timezone(&site.offset_at(t)).date_naive()
}

/// The UTC instant of local midnight at the start of `date`. DST-correct: the offset is validated at
/// the candidate instant itself, not borrowed from a neighbouring day.
fn local_midnight(date: NaiveDate, site: &SiteConfig) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).unwrap_or_default();
    let probe = naive.and_utc();
    let mut candidates: Vec<FixedOffset> = [-1, 0, 1]
        .into_iter()
        .map(|d| site.offset_at(probe + Duration::days(d)))
        .collect();
    candidates.sort_by_key(|o| std::cmp::Reverse(o.local_minus_utc()));
    candidates.dedup();
    for off in candidates {
        let u = probe - Duration::seconds(i64::from(off.local_minus_utc()));
        if site.offset_at(u) == off {
            return u;
        }
    }
    // Local midnight falls in a DST gap (not the case for the Czech zone): the offset at the naive
    // instant is the best available.
    probe - Duration::seconds(i64::from(site.offset_at(probe).local_minus_utc()))
}

/// The window of one local day.
pub fn day_window(date: NaiveDate, site: &SiteConfig) -> DayWindow {
    DayWindow {
        date,
        start: local_midnight(date, site),
        end: local_midnight(date.succ_opt().unwrap_or(date), site),
    }
}

/// Parse and range-check the `date` query value against site-local `today`. `Err` is the 400 message.
pub fn parse_date(raw: &str, today: NaiveDate) -> Result<NaiveDate, String> {
    let well_formed = raw.len() == 10
        && raw.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    let date = well_formed
        .then(|| NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok())
        .flatten()
        .ok_or_else(|| format!("invalid date {raw:?}: expected YYYY-MM-DD"))?;
    let (lo, hi) = (
        today - Duration::days(MAX_DAYS_BACK),
        today + Duration::days(MAX_DAYS_AHEAD),
    );
    if date < lo || date > hi {
        return Err(format!(
            "date {date} out of range: allowed {lo} .. {hi} (site-local, today {today})"
        ));
    }
    Ok(date)
}

/// A measured series: `(timestamp, value)`, ascending, stamped at the END of each averaging window.
pub type Series = Vec<(DateTime<Utc>, f64)>;

/// A zone's comfort band as `/api/zones` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Band {
    pub low: f64,
    pub high: f64,
    pub target: f64,
}

/// One controlled zone's inputs.
#[derive(Debug, Clone)]
pub struct ZoneInput {
    pub zone: String,
    pub heated: bool,
    pub max_heat_kw: Option<f64>,
    pub heat: Option<Band>,
    /// Measured temperature, 15-min means (empty when the read failed).
    pub measured: Series,
}

/// Everything [`build`] needs.
pub struct DayInputs<'a> {
    pub window: DayWindow,
    pub now: DateTime<Utc>,
    pub eur_czk_rate: f64,
    /// The latest plan's timeline (empty when there is none).
    pub plan: &'a [TimelineBlock],
    /// Ledger rows (any day; filtered here).
    pub ledger: &'a [LedgerRow],
    pub pv_kw: Series,
    pub house_kw: Series,
    pub outside_c: Series,
    pub zones: Vec<ZoneInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slot {
    pub t: String,
    pub source: &'static str,
    pub import_price: Option<f64>,
    pub price_is_placeholder: bool,
    pub outside_c: Option<f64>,
    pub pv_kw: Option<f64>,
    pub house_kw: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneDay {
    pub zone: String,
    pub heated: bool,
    pub max_heat_kw: Option<f64>,
    pub heat: Option<Band>,
    /// `["t_end", °C]`
    pub measured: Vec<(String, f64)>,
    /// `["t_end", °C]`
    pub predicted: Vec<(String, f64)>,
    /// `["t", dt_minutes, kW, "plan" | "ledger"]`
    pub plan_heat: Vec<(String, u32, f64, &'static str)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayReport {
    pub date: String,
    pub start: String,
    pub end: String,
    pub now: String,
    pub eur_czk_rate: f64,
    pub slots: Vec<Slot>,
    pub zones: Vec<ZoneDay>,
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn round_to(v: f64, decimals: i32) -> f64 {
    let k = 10f64.powi(decimals);
    (v * k).round() / k
}

/// Index of the slot a window-END-stamped sample belongs to: the slot containing the window's
/// interior, so a sample stamped exactly on a slot boundary belongs to the slot that just ended, and
/// the truncated last window (stamped at `now`) to the slot containing `now`.
fn slot_index(stamp: DateTime<Utc>, start: DateTime<Utc>, n: usize) -> Option<usize> {
    let secs = (stamp - start).num_seconds();
    if secs <= 0 {
        return None;
    }
    let idx = ((secs - 1) / (SLOT_MINUTES * 60)) as usize;
    (idx < n).then_some(idx)
}

/// Bin an end-stamped series onto the slot lattice (later samples win), rounding to `decimals`.
fn bin(series: &Series, start: DateTime<Utc>, n: usize, decimals: i32) -> Vec<Option<f64>> {
    let mut out = vec![None; n];
    for &(t, v) in series {
        if let (Some(i), true) = (slot_index(t, start, n), v.is_finite()) {
            out[i] = Some(round_to(v, decimals));
        }
    }
    out
}

fn block_minutes(b: &TimelineBlock) -> i64 {
    if b.dt_minutes == 0 {
        SLOT_MINUTES
    } else {
        i64::from(b.dt_minutes)
    }
}

/// The plan block covering instant `t` (`b.t <= t < b.t + dt`).
fn plan_block_at(plan: &[TimelineBlock], t: DateTime<Utc>) -> Option<&TimelineBlock> {
    plan.iter()
        .find(|b| b.t <= t && t < b.t + Duration::minutes(block_minutes(b)))
}

/// The ledger row covering `t`, if any (the last one on a duplicate).
fn ledger_row_at(rows: &[&LedgerRow], t: DateTime<Utc>) -> Option<usize> {
    rows.iter().rposition(|r| {
        let dt = if r.dt_minutes == 0 {
            SLOT_MINUTES
        } else {
            i64::from(r.dt_minutes)
        };
        r.t <= t && t < r.t + Duration::minutes(dt)
    })
}

/// Build the day report.
pub fn build(inp: &DayInputs) -> DayReport {
    let DayWindow { date, start, end } = inp.window;
    let now = inp.now;
    let n = ((end - start).num_minutes() / SLOT_MINUTES).max(0) as usize;

    // Ledger rows whose block starts inside the day, ascending.
    let mut day_rows: Vec<&LedgerRow> = inp
        .ledger
        .iter()
        .filter(|r| r.t >= start && r.t < end)
        .collect();
    day_rows.sort_by_key(|r| r.t);

    let pv = bin(&inp.pv_kw, start, n, 3);
    let house = bin(&inp.house_kw, start, n, 3);
    let outside = bin(&inp.outside_c, start, n, 1);

    let slots: Vec<Slot> = (0..n)
        .map(|i| {
            let t = start + Duration::minutes(SLOT_MINUTES * i as i64);
            let slot_end = t + Duration::minutes(SLOT_MINUTES);
            let has_measured = pv[i].is_some() || house[i].is_some() || outside[i].is_some();
            let measured = slot_end <= now || (t <= now && has_measured);
            if measured {
                let (price, placeholder) = match ledger_row_at(&day_rows, t) {
                    Some(k) => (day_rows[k].import_price, day_rows[k].price_is_placeholder),
                    // The block in progress is recorded only once it ends: its price is the plan's.
                    None => plan_block_at(inp.plan, t)
                        .map(|b| (Some(b.import_price), b.price_is_placeholder))
                        .unwrap_or((None, false)),
                };
                return Slot {
                    t: ts(t),
                    source: "measured",
                    import_price: price.filter(|p| p.is_finite()).map(|p| round_to(p, 5)),
                    price_is_placeholder: placeholder,
                    outside_c: outside[i],
                    pv_kw: pv[i],
                    house_kw: house[i],
                };
            }
            match plan_block_at(inp.plan, t) {
                Some(b) => Slot {
                    t: ts(t),
                    source: "plan",
                    import_price: Some(round_to(b.import_price, 5)).filter(|p| p.is_finite()),
                    price_is_placeholder: b.price_is_placeholder,
                    outside_c: b.outside_c.map(|v| round_to(v, 1)),
                    pv_kw: Some(round_to(b.pv_kw, 3)).filter(|v| v.is_finite()),
                    house_kw: Some(round_to(b.load_kw, 3)).filter(|v| v.is_finite()),
                },
                None => Slot {
                    t: ts(t),
                    source: "none",
                    import_price: None,
                    price_is_placeholder: false,
                    outside_c: None,
                    pv_kw: None,
                    house_kw: None,
                },
            }
        })
        .collect();

    let measured_until = end.min(now);
    let zones = inp
        .zones
        .iter()
        .map(|z| {
            let measured = z
                .measured
                .iter()
                .filter(|(t, v)| *t > start && *t <= measured_until && v.is_finite())
                .map(|(t, v)| (ts(*t), round_to(*v, 2)))
                .collect();

            // Predicted: the latest plan's end-of-block temperature for blocks ending in (now, end].
            let predicted = inp
                .plan
                .iter()
                .filter_map(|b| {
                    let b_end = b.t + Duration::minutes(block_minutes(b));
                    let v = *b.temp_c.get(&z.zone)?;
                    (b_end > now && b_end > start && b_end <= end && v.is_finite())
                        .then(|| (ts(b_end), round_to(v, 2)))
                })
                .collect();

            // Planned heat: the ledger's decision for blocks it holds, the latest plan for the
            // blocks it does not (the block in progress and the future).
            let mut plan_heat: Vec<(DateTime<Utc>, u32, f64, &'static str)> = Vec::new();
            for r in &day_rows {
                if let Some(&kw) = r.planned.heat_kw.get(&z.zone) {
                    let dt = if r.dt_minutes == 0 { 15 } else { r.dt_minutes };
                    if kw.is_finite() && kw > PLAN_HEAT_EPS_KW {
                        plan_heat.push((r.t, dt, round_to(kw, 3), "ledger"));
                    }
                }
            }
            for b in inp.plan {
                let b_end = b.t + Duration::minutes(block_minutes(b));
                if b.t < start || b.t >= end || b_end <= now {
                    continue;
                }
                if day_rows.iter().any(|r| r.t == b.t) {
                    continue; // the ledger already holds this block's decision
                }
                if let Some(&kw) = b.heat_kw.get(&z.zone) {
                    if kw.is_finite() && kw > PLAN_HEAT_EPS_KW {
                        plan_heat.push((b.t, block_minutes(b) as u32, round_to(kw, 3), "plan"));
                    }
                }
            }
            plan_heat.sort_by_key(|e| e.0);

            ZoneDay {
                zone: z.zone.clone(),
                heated: z.heated,
                max_heat_kw: z.max_heat_kw,
                heat: z.heat,
                measured,
                predicted,
                plan_heat: plan_heat
                    .into_iter()
                    .map(|(t, dt, kw, src)| (ts(t), dt, kw, src))
                    .collect(),
            }
        })
        .collect();

    DayReport {
        date: date.format("%Y-%m-%d").to_string(),
        start: ts(start),
        end: ts(end),
        now: ts(now),
        eur_czk_rate: inp.eur_czk_rate,
        slots,
        zones,
    }
}

/// The CZK/EUR rate to report: a past day uses what the ledger rows of that day carry (the rate the
/// plans of that time were priced with), today/tomorrow the live plan's; each falls back to the other,
/// then to the configured tariff.
pub fn pick_rate(
    window: &DayWindow,
    now: DateTime<Utc>,
    ledger: &[LedgerRow],
    plan_rate: Option<f64>,
    config_rate: f64,
) -> f64 {
    let ok = |r: f64| r.is_finite() && r > 0.0;
    let ledger_rate = ledger
        .iter()
        .filter(|r| r.t >= window.start && r.t < window.end && ok(r.eur_czk_rate))
        .max_by_key(|r| r.t)
        .map(|r| r.eur_czk_rate);
    let plan_rate = plan_rate.filter(|r| ok(*r));
    let (first, second) = if window.end <= now {
        (ledger_rate, plan_rate)
    } else {
        (plan_rate, ledger_rate)
    };
    first.or(second).unwrap_or(config_rate)
}

/// Pull the `{low, high, target}` heat band out of a `/api/zones` entry.
pub fn band_from_zone_entry(entry: &Value) -> Option<Band> {
    let h = entry.get("heat")?;
    Some(Band {
        low: h.get("low")?.as_f64()?,
        high: h.get("high")?.as_f64()?,
        target: h.get("target")?.as_f64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn prague() -> SiteConfig {
        serde_json::from_value(serde_json::json!({
            "latitude": 49.5, "longitude": 17.4, "utc_offset_hours": 1,
            "timezone": "Europe/Prague",
        }))
        .unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn block(start: &str, dt: u32, price: f64, heat: &[(&str, f64)]) -> TimelineBlock {
        TimelineBlock {
            t: t(start),
            dt_minutes: dt,
            import_price: price,
            export_price: 0.0,
            price_is_placeholder: false,
            pv_kw: 1.5,
            load_kw: 0.4,
            soc_kwh: 0.0,
            charge_kw: 0.0,
            discharge_kw: 0.0,
            grid_import_kw: 0.0,
            grid_export_kw: 0.0,
            curtail_kw: 0.0,
            heat_kw: heat.iter().map(|(z, v)| (z.to_string(), *v)).collect(),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            temp_c: HashMap::from([("kitchen".to_string(), 21.9)]),
            outside_c: Some(7.0),
            slot: "regular".into(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    fn ledger_row(start: &str, price: Option<f64>, heat: &[(&str, f64)]) -> LedgerRow {
        let mut r: LedgerRow = serde_json::from_str("{}").unwrap();
        r.t = t(start);
        r.dt_minutes = 15;
        r.import_price = price;
        r.eur_czk_rate = 24.5;
        r.planned.heat_kw = heat.iter().map(|(z, v)| (z.to_string(), *v)).collect();
        r
    }

    fn kitchen(measured: Series) -> ZoneInput {
        ZoneInput {
            zone: "kitchen".into(),
            heated: true,
            max_heat_kw: Some(1.2),
            heat: Some(Band {
                low: 19.5,
                high: 21.0,
                target: 20.0,
            }),
            measured,
        }
    }

    fn inputs<'a>(
        date: &str,
        now: &str,
        plan: &'a [TimelineBlock],
        ledger: &'a [LedgerRow],
    ) -> DayInputs<'a> {
        DayInputs {
            window: day_window(d(date), &prague()),
            now: t(now),
            eur_czk_rate: 25.0,
            plan,
            ledger,
            pv_kw: vec![],
            house_kw: vec![],
            outside_c: vec![],
            zones: vec![],
        }
    }

    #[test]
    fn day_boundaries_normal_and_dst() {
        let site = prague();
        let w = day_window(d("2026-10-13"), &site);
        assert_eq!(w.start, t("2026-10-12T22:00:00Z"));
        assert_eq!(w.end, t("2026-10-13T22:00:00Z"));
        // Spring forward 2026-03-29: 23 h = 92 slots.
        let w = day_window(d("2026-03-29"), &site);
        assert_eq!(w.start, t("2026-03-28T23:00:00Z"));
        assert_eq!(w.end, t("2026-03-29T22:00:00Z"));
        let r = build(&inputs("2026-03-29", "2026-06-01T00:00:00Z", &[], &[]));
        assert_eq!(r.slots.len(), 92);
        // Fall back 2026-10-25: 25 h = 100 slots.
        let w = day_window(d("2026-10-25"), &site);
        assert_eq!(w.start, t("2026-10-24T22:00:00Z"));
        assert_eq!(w.end, t("2026-10-25T23:00:00Z"));
        let r = build(&inputs("2026-10-25", "2026-12-01T00:00:00Z", &[], &[]));
        assert_eq!(r.slots.len(), 100);
        let r = build(&inputs("2026-10-13", "2026-12-01T00:00:00Z", &[], &[]));
        assert_eq!(r.slots.len(), 96);
    }

    #[test]
    fn local_date_follows_the_site_offset() {
        // 22:30 UTC on 12 Oct is already 13 Oct in Prague (CEST).
        assert_eq!(
            local_date(t("2026-10-12T22:30:00Z"), &prague()),
            d("2026-10-13")
        );
    }

    #[test]
    fn date_range_and_format_are_validated() {
        let today = d("2026-10-13");
        assert_eq!(parse_date("2026-10-13", today), Ok(d("2026-10-13")));
        assert_eq!(parse_date("2026-09-30", today), Ok(d("2026-09-30"))); // -13
        assert_eq!(parse_date("2026-10-14", today), Ok(d("2026-10-14"))); // +1
        assert!(parse_date("2026-09-29", today).is_err()); // -14
        assert!(parse_date("2026-10-15", today).is_err()); // +2
        for bad in [
            "",
            "tomorrow",
            "2026-1-3",
            "2026-13-01",
            "20261013",
            "2026-02-30",
        ] {
            assert!(parse_date(bad, today).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn source_follows_now() {
        // now = 12:07 local (10:07Z). Plan from 12:00 local on, 15-min blocks.
        let plan: Vec<_> = (0..8)
            .map(|i| {
                block(
                    &format!("2026-10-13T{:02}:{:02}:00Z", 10 + i / 4, (i % 4) * 15),
                    15,
                    0.2,
                    &[],
                )
            })
            .collect();
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &[]);
        // Measured data up to the in-progress window (stamped at `now`, the truncated last window).
        inp.pv_kw = vec![
            (t("2026-10-13T09:45:00Z"), 1.0),
            (t("2026-10-13T10:00:00Z"), 2.0),
            (t("2026-10-13T10:07:00Z"), 3.0),
        ];
        let r = build(&inp);
        let by = |s: &str| r.slots.iter().find(|x| x.t == s).unwrap().clone();
        // Ended before now -> measured; the value is the window stamped at its end.
        assert_eq!(by("2026-10-13T09:45:00Z").source, "measured");
        assert_eq!(by("2026-10-13T09:45:00Z").pv_kw, Some(2.0));
        assert_eq!(by("2026-10-13T09:30:00Z").pv_kw, Some(1.0));
        // The slot containing now has measured data -> measured (truncated window).
        let cur = by("2026-10-13T10:00:00Z");
        assert_eq!(cur.source, "measured");
        // Next slot is the future -> plan.
        assert_eq!(by("2026-10-13T10:15:00Z").source, "plan");
        assert_eq!(by("2026-10-13T10:15:00Z").outside_c, Some(7.0));
        // Beyond the plan -> none, all null.
        let none = by("2026-10-13T12:00:00Z");
        assert_eq!(none.source, "none");
        assert_eq!(none.import_price, None);
        assert_eq!(none.pv_kw, None);
        assert_eq!(none.house_kw, None);
        // No measured data in the current slot -> falls to the plan.
        let mut inp3 = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &[]);
        inp3.pv_kw = vec![(t("2026-10-13T10:00:00Z"), 2.0)];
        let r3 = build(&inp3);
        let cur = r3
            .slots
            .iter()
            .find(|x| x.t == "2026-10-13T10:00:00Z")
            .unwrap();
        assert_eq!(cur.source, "plan");
        assert_eq!(cur.pv_kw, Some(1.5));
        assert_eq!(cur.house_kw, Some(0.4));
    }

    #[test]
    fn ledger_gives_past_price_and_plan_heat_with_origin() {
        let ledger = vec![
            ledger_row("2026-10-13T08:00:00Z", Some(0.31), &[("kitchen", 1.2)]),
            ledger_row("2026-10-13T08:15:00Z", Some(0.28), &[]),
            ledger_row("2026-10-13T08:30:00Z", None, &[("kitchen", 0.0)]),
        ];
        let plan = vec![
            block("2026-10-13T10:00:00Z", 15, 0.2, &[("kitchen", 0.6)]),
            block("2026-10-13T10:15:00Z", 15, 0.2, &[("kitchen", 0.0)]),
            // Hourly block: 4 slots, one plan_heat entry with dt 60.
            block("2026-10-13T10:30:00Z", 60, 0.25, &[("kitchen", 1.2)]),
        ];
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &ledger);
        inp.zones = vec![kitchen(vec![
            (t("2026-10-13T08:15:00Z"), 21.7),
            (t("2026-10-13T10:07:00Z"), 21.4),
            (t("2026-10-13T12:00:00Z"), 99.0), // beyond now: dropped
        ])];
        let r = build(&inp);
        let by = |s: &str| r.slots.iter().find(|x| x.t == s).unwrap().clone();
        assert_eq!(by("2026-10-13T08:00:00Z").import_price, Some(0.31));
        assert_eq!(by("2026-10-13T08:15:00Z").import_price, Some(0.28));
        assert_eq!(by("2026-10-13T08:30:00Z").import_price, None);
        // In-progress slot has no ledger row yet: the plan's price.
        assert_eq!(by("2026-10-13T10:00:00Z").import_price, Some(0.2));
        let z = &r.zones[0];
        assert_eq!(
            z.plan_heat,
            vec![
                ("2026-10-13T08:00:00Z".to_string(), 15, 1.2, "ledger"),
                ("2026-10-13T10:00:00Z".to_string(), 15, 0.6, "plan"),
                ("2026-10-13T10:30:00Z".to_string(), 60, 1.2, "plan"),
            ]
        );
        assert_eq!(z.measured.len(), 2);
        assert_eq!(z.measured[0], ("2026-10-13T08:15:00Z".to_string(), 21.7));
        // Predicted: block ends after now, stamped at the block END; hourly block = one point.
        assert_eq!(
            z.predicted.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
            vec![
                "2026-10-13T10:15:00Z",
                "2026-10-13T10:30:00Z",
                "2026-10-13T11:30:00Z"
            ]
        );
    }

    #[test]
    fn hourly_plan_block_splits_into_four_slots() {
        let plan = vec![block("2026-10-13T12:00:00Z", 60, 0.33, &[])];
        let r = build(&inputs("2026-10-13", "2026-10-13T10:00:00Z", &plan, &[]));
        let hour: Vec<_> = r
            .slots
            .iter()
            .filter(|s| s.t.starts_with("2026-10-13T12:"))
            .collect();
        assert_eq!(hour.len(), 4);
        assert!(hour
            .iter()
            .all(|s| s.source == "plan" && s.import_price == Some(0.33)));
        assert_eq!(
            r.slots
                .iter()
                .find(|s| s.t == "2026-10-13T13:00:00Z")
                .unwrap()
                .source,
            "none"
        );
    }

    #[test]
    fn rate_prefers_ledger_for_past_days_and_plan_for_today() {
        let ledger = vec![ledger_row("2026-10-10T08:00:00Z", Some(0.3), &[])];
        let past = day_window(d("2026-10-10"), &prague());
        let now = t("2026-10-13T10:00:00Z");
        assert_eq!(pick_rate(&past, now, &ledger, Some(25.0), 26.0), 24.5);
        let today = day_window(d("2026-10-13"), &prague());
        assert_eq!(pick_rate(&today, now, &ledger, Some(25.0), 26.0), 25.0);
        assert_eq!(pick_rate(&today, now, &[], None, 26.0), 26.0);
    }

    #[test]
    fn fixture_json_shape() {
        let plan = vec![block("2026-10-13T10:15:00Z", 15, 0.22, &[("kitchen", 1.2)])];
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &[]);
        inp.zones = vec![kitchen(vec![(t("2026-10-13T10:00:00Z"), 21.73)])];
        let r = build(&inp);
        let v = serde_json::to_value(&r).unwrap();
        let slot = v["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["t"] == "2026-10-13T10:15:00Z")
            .unwrap();
        assert_eq!(
            slot,
            &serde_json::json!({"t":"2026-10-13T10:15:00Z","source":"plan","import_price":0.22,
                "price_is_placeholder":false,"outside_c":7.0,"pv_kw":1.5,"house_kw":0.4})
        );
        assert_eq!(
            v["zones"][0],
            serde_json::json!({"zone":"kitchen","heated":true,"max_heat_kw":1.2,
                "heat":{"low":19.5,"high":21.0,"target":20.0},
                "measured":[["2026-10-13T10:00:00Z",21.73]],
                "predicted":[["2026-10-13T10:30:00Z",21.9]],
                "plan_heat":[["2026-10-13T10:15:00Z",15,1.2,"plan"]]})
        );
        assert!(v["slots"].as_array().unwrap().len() == 96);
        assert_eq!(v["start"], "2026-10-12T22:00:00Z");
    }

    #[test]
    fn outside_c_round_trips_and_old_json_still_loads() {
        let b = block("2026-10-13T10:00:00Z", 15, 0.2, &[]);
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["outside_c"], 7.0);
        let back: TimelineBlock = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.outside_c, Some(7.0));
        // A plan saved before the field existed: key absent -> None, and not re-emitted.
        let mut old = v;
        old.as_object_mut().unwrap().remove("outside_c");
        let back: TimelineBlock = serde_json::from_value(old).unwrap();
        assert_eq!(back.outside_c, None);
        assert!(serde_json::to_value(&back)
            .unwrap()
            .get("outside_c")
            .is_none());
    }
}
