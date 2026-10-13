//! `GET /api/energy/day` — everything the dashboard's one-day energy chart needs for ONE site-local
//! calendar day: per 15-minute slot the battery mode, price, PV, house load (total and its heating /
//! EV parts), battery charge / discharge / SoC, grid import / export and the cost, as MEASURED (the
//! decision ledger's scored rows plus the measured SoC) for the past and as PLAN (the latest plan)
//! for the rest, with what the plan of that time had decided next to every measured slot.
//!
//! API CONTRACT: the Home Assistant dashboard in ~/git/selfhosted consumes this API. This endpoint
//! is purely additive (no `API_VERSION` bump); a change to its shape must be mirrored there.
//!
//! **Pure and IO-free**, like [`crate::heating_day`] whose window / date / slot helpers it reuses:
//! [`build`] takes the SoC series the handler read best-effort, the latest plan's timeline and the
//! ledger rows. Honesty rule: what the brain does not know is `null`, never a zero (an unscored
//! ledger row gives null flows, not 0 kW).

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::app::TimelineBlock;
use crate::heating_day::{bin, ledger_row_at, plan_block_at, DayWindow, Series, SLOT_MINUTES};
use crate::ledger::{block_cost_eur, LedgerRow};
use crate::tools::{rfc3339_secs as ts, round_to};

/// Slot length in hours.
const SLOT_H: f64 = SLOT_MINUTES as f64 / 60.0;

/// Everything [`build`] needs.
pub struct DayInputs<'a> {
    pub window: DayWindow,
    pub now: DateTime<Utc>,
    pub eur_czk_rate: f64,
    /// Configured battery capacity (kWh); `None` when there is no battery.
    pub battery_capacity_kwh: Option<f64>,
    /// Electrical-to-thermal factor of the heating equipment (`heating.cop`).
    pub heating_cop: f64,
    /// Battery wear (EUR per kWh discharged) for the plan slots' cost.
    pub wear_eur_per_kwh: f64,
    /// The latest plan's timeline (empty when there is none).
    pub plan: &'a [TimelineBlock],
    /// Ledger rows (any day; filtered here).
    pub ledger: &'a [LedgerRow],
    /// Measured battery SoC (kWh), 15-min means stamped at the window END (empty without battery or
    /// when the read failed).
    pub soc_kwh: Series,
}

/// What the plan of that time decided for a measured slot (from the ledger row).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Planned {
    pub mode: Option<String>,
    pub pv_kw: f64,
    /// The plan's BASE load (no heating / EV).
    pub load_kw: f64,
    pub heat_kw: f64,
    pub ev_kw: f64,
    pub charge_kw: f64,
    pub discharge_kw: f64,
    pub import_kw: f64,
    pub export_kw: f64,
    pub cost_eur: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slot {
    pub t: String,
    pub source: &'static str,
    pub mode: Option<String>,
    pub export_enabled: Option<bool>,
    pub inverter_on: Option<bool>,
    pub import_price: Option<f64>,
    pub export_price: Option<f64>,
    pub price_is_placeholder: bool,
    pub pv_kw: Option<f64>,
    pub load_kw: Option<f64>,
    pub heat_kw: Option<f64>,
    pub ev_kw: Option<f64>,
    pub charge_kw: Option<f64>,
    pub discharge_kw: Option<f64>,
    pub import_kw: Option<f64>,
    pub export_kw: Option<f64>,
    pub soc_kwh: Option<f64>,
    pub cost_eur: Option<f64>,
    pub planned: Option<Planned>,
    pub misses: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Totals {
    pub pv_kwh: f64,
    pub load_kwh: f64,
    pub heat_kwh: f64,
    pub ev_kwh: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub charge_kwh: f64,
    pub discharge_kwh: f64,
    pub cost_eur: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MeasuredTotals {
    #[serde(flatten)]
    pub totals: Totals,
    pub planned_cost_eur: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DayTotals {
    pub measured: MeasuredTotals,
    pub plan: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayReport {
    pub date: String,
    pub start: String,
    pub end: String,
    pub now: String,
    pub eur_czk_rate: f64,
    pub battery_capacity_kwh: Option<f64>,
    /// `heat_kw` is heating power as the plan / ledger state it (thermal); electricity = `/ heating_cop`.
    pub heating_cop: f64,
    pub slots: Vec<Slot>,
    pub totals: DayTotals,
}

/// Rounded, finite or `None`; `+ 0.0` turns the `-0.0` an empty `sum()` yields into `0.0`.
fn kw(v: f64, decimals: i32) -> Option<f64> {
    v.is_finite().then(|| round_to(v, decimals) + 0.0)
}

/// Sum of a per-item map; `None` when no entry is known. Non-finite entries are unknown.
fn sum_known<'a>(vals: impl Iterator<Item = Option<f64>> + 'a) -> Option<f64> {
    vals.flatten()
        .filter(|v| v.is_finite())
        .fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn empty_slot(t: DateTime<Utc>, source: &'static str) -> Slot {
    Slot {
        t: ts(t),
        source,
        mode: None,
        export_enabled: None,
        inverter_on: None,
        import_price: None,
        export_price: None,
        price_is_placeholder: false,
        pv_kw: None,
        load_kw: None,
        heat_kw: None,
        ev_kw: None,
        charge_kw: None,
        discharge_kw: None,
        import_kw: None,
        export_kw: None,
        soc_kwh: None,
        cost_eur: None,
        planned: None,
        misses: Vec::new(),
    }
}

fn price(p: Option<f64>) -> Option<f64> {
    p.filter(|p| p.is_finite()).map(|p| round_to(p, 5))
}

fn measured_slot(t: DateTime<Utc>, row: Option<&LedgerRow>, plan: Option<&TimelineBlock>) -> Slot {
    let mut s = empty_slot(t, "measured");
    let Some(r) = row else {
        // The block in progress is recorded by the ledger only once it ends: the plan's decision,
        // prices and flags stand in, the flows stay unknown.
        if let Some(b) = plan {
            s.mode = non_empty(&b.slot);
            s.export_enabled = Some(b.export_enabled);
            s.inverter_on = Some(b.inverter_on);
            s.import_price = price(Some(b.import_price));
            s.export_price = price(Some(b.export_price));
            s.price_is_placeholder = b.price_is_placeholder;
        }
        return s;
    };
    s.mode = non_empty(&r.slot);
    s.export_enabled = Some(r.export_enabled);
    s.inverter_on = Some(r.inverter_on);
    s.import_price = price(r.import_price);
    s.export_price = price(r.export_price);
    s.price_is_placeholder = r.price_is_placeholder;
    s.misses = r.misses.clone();

    let dt_h = if r.dt_minutes == 0 {
        SLOT_H
    } else {
        f64::from(r.dt_minutes) / 60.0
    };
    if let (true, Some(m)) = (r.scored, r.measured.as_ref()) {
        let rate = |kwh: f64| kw(kwh / dt_h, 3);
        s.pv_kw = rate(m.pv_kwh);
        s.load_kw = rate(m.load_kwh);
        s.charge_kw = rate(m.charge_kwh);
        s.discharge_kw = rate(m.discharge_kwh);
        s.import_kw = rate(m.import_kwh);
        s.export_kw = rate(m.export_kwh);
        s.heat_kw = sum_known(m.heat_kwh.values().copied()).and_then(rate);
        s.ev_kw = sum_known(m.ev_kwh.values().copied()).and_then(rate);
        s.cost_eur = r.realized_cost_eur.and_then(|c| kw(c, 4));
    }
    let p = &r.planned;
    s.planned = Some(Planned {
        mode: non_empty(&r.slot),
        pv_kw: round_to(p.pv_kw, 3),
        load_kw: round_to(p.load_kw, 3),
        heat_kw: kw(p.heat_kw.values().sum::<f64>(), 3).unwrap_or(0.0),
        ev_kw: kw(p.ev_charge_kw.values().sum::<f64>(), 3).unwrap_or(0.0),
        charge_kw: round_to(p.charge_kw, 3),
        discharge_kw: round_to(p.discharge_kw, 3),
        import_kw: round_to(p.grid_import_kw, 3),
        export_kw: round_to(p.grid_export_kw, 3),
        cost_eur: r.planned_cost_eur.and_then(|c| kw(c, 4)),
    });
    s
}

fn plan_slot(t: DateTime<Utc>, b: &TimelineBlock, wear_eur_per_kwh: f64) -> Slot {
    let mut s = empty_slot(t, "plan");
    s.mode = non_empty(&b.slot);
    s.export_enabled = Some(b.export_enabled);
    s.inverter_on = Some(b.inverter_on);
    s.import_price = price(Some(b.import_price));
    s.export_price = price(Some(b.export_price));
    s.price_is_placeholder = b.price_is_placeholder;
    s.pv_kw = kw(b.pv_kw, 3);
    s.load_kw = kw(b.load_kw, 3);
    s.heat_kw = kw(b.heat_kw.values().sum::<f64>(), 3);
    s.ev_kw = kw(b.ev_charge_kw.values().sum::<f64>(), 3);
    s.charge_kw = kw(b.charge_kw, 3);
    s.discharge_kw = kw(b.discharge_kw, 3);
    s.import_kw = kw(b.grid_import_kw, 3);
    s.export_kw = kw(b.grid_export_kw, 3);
    s.soc_kwh = kw(b.soc_kwh, 2);
    // The block's net cost for this slot's share of it (a slot is 15 min; hourly blocks split).
    s.cost_eur = kw(
        block_cost_eur(
            b.import_price,
            b.export_price,
            wear_eur_per_kwh,
            b.grid_import_kw * SLOT_H,
            b.grid_export_kw * SLOT_H,
            b.discharge_kw * SLOT_H,
        ),
        4,
    );
    s
}

fn add(acc: &mut Totals, s: &Slot) {
    let put = |a: &mut f64, v: Option<f64>| *a += v.unwrap_or(0.0) * SLOT_H;
    put(&mut acc.pv_kwh, s.pv_kw);
    put(&mut acc.load_kwh, s.load_kw);
    put(&mut acc.heat_kwh, s.heat_kw);
    put(&mut acc.ev_kwh, s.ev_kw);
    put(&mut acc.import_kwh, s.import_kw);
    put(&mut acc.export_kwh, s.export_kw);
    put(&mut acc.charge_kwh, s.charge_kw);
    put(&mut acc.discharge_kwh, s.discharge_kw);
    acc.cost_eur += s.cost_eur.unwrap_or(0.0);
}

fn rounded(t: &mut Totals) {
    for v in [
        &mut t.pv_kwh,
        &mut t.load_kwh,
        &mut t.heat_kwh,
        &mut t.ev_kwh,
        &mut t.import_kwh,
        &mut t.export_kwh,
        &mut t.charge_kwh,
        &mut t.discharge_kwh,
    ] {
        *v = round_to(*v, 3);
    }
    t.cost_eur = round_to(t.cost_eur, 3);
}

/// Build the day report.
pub fn build(inp: &DayInputs) -> DayReport {
    let DayWindow { date, start, end } = inp.window;
    let now = inp.now;
    let n = ((end - start).num_minutes() / SLOT_MINUTES).max(0) as usize;

    let mut day_rows: Vec<&LedgerRow> = inp
        .ledger
        .iter()
        .filter(|r| r.t >= start && r.t < end)
        .collect();
    day_rows.sort_by_key(|r| r.t);
    let soc = bin(&inp.soc_kwh, start, n, 2);

    let slots: Vec<Slot> = (0..n)
        .map(|i| {
            let t = start + Duration::minutes(SLOT_MINUTES * i as i64);
            let slot_end = t + Duration::minutes(SLOT_MINUTES);
            let row = ledger_row_at(&day_rows, t).map(|k| day_rows[k]);
            let plan = plan_block_at(inp.plan, t);
            // Same rule as /api/heating/day: ended slots are measured; the slot containing now is
            // measured once there is measured data for it (here: a ledger row or a SoC sample).
            let measured = slot_end <= now || (t <= now && (row.is_some() || soc[i].is_some()));
            if measured {
                let mut s = measured_slot(t, row, plan);
                s.soc_kwh = soc[i];
                s
            } else {
                match plan {
                    Some(b) => plan_slot(t, b, inp.wear_eur_per_kwh),
                    None => empty_slot(t, "none"),
                }
            }
        })
        .collect();

    let mut totals = DayTotals::default();
    for s in &slots {
        match s.source {
            "measured" => {
                add(&mut totals.measured.totals, s);
                totals.measured.planned_cost_eur +=
                    s.planned.as_ref().and_then(|p| p.cost_eur).unwrap_or(0.0);
            }
            "plan" => add(&mut totals.plan, s),
            _ => {}
        }
    }
    rounded(&mut totals.measured.totals);
    totals.measured.planned_cost_eur = round_to(totals.measured.planned_cost_eur, 3);
    rounded(&mut totals.plan);

    DayReport {
        date: date.format("%Y-%m-%d").to_string(),
        start: ts(start),
        end: ts(end),
        now: ts(now),
        eur_czk_rate: inp.eur_czk_rate,
        battery_capacity_kwh: inp.battery_capacity_kwh,
        heating_cop: inp.heating_cop,
        slots,
        totals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heating_day::{day_window, parse_date};
    use crate::ledger::Measured;
    use crate::optimize::config::SiteConfig;
    use chrono::NaiveDate;
    use std::collections::HashMap;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn prague() -> SiteConfig {
        serde_json::from_value(serde_json::json!({
            "latitude": 49.5, "longitude": 17.4, "utc_offset_hours": 1,
            "timezone": "Europe/Prague",
        }))
        .unwrap()
    }

    fn block(start: &str, dt: u32) -> TimelineBlock {
        TimelineBlock {
            t: t(start),
            dt_minutes: dt,
            import_price: 0.2,
            export_price: 0.1,
            price_is_placeholder: false,
            pv_kw: 1.5,
            load_kw: 0.4,
            soc_kwh: 6.3,
            charge_kw: 0.0,
            discharge_kw: 2.0,
            grid_import_kw: 4.0,
            grid_export_kw: 0.0,
            curtail_kw: 0.0,
            heat_kw: HashMap::from([("a".to_string(), 1.0), ("b".to_string(), 0.5)]),
            cool_kw: HashMap::new(),
            hvac_heat_kw: HashMap::new(),
            controllable_load_kw: HashMap::new(),
            ev_charge_kw: HashMap::from([("tesla".to_string(), 3.0)]),
            temp_c: HashMap::new(),
            outside_c: None,
            slot: "regular".into(),
            export_enabled: true,
            inverter_on: true,
            frozen: false,
        }
    }

    /// A scored ledger row: 0.25 kWh PV, 0.5 load, import 0.3 kWh at 0.22 EUR.
    fn row(start: &str) -> LedgerRow {
        let mut r: LedgerRow = serde_json::from_str("{}").unwrap();
        r.t = t(start);
        r.dt_minutes = 15;
        r.slot = "charge_from_grid".into();
        r.export_enabled = true;
        r.inverter_on = true;
        r.import_price = Some(0.22);
        r.export_price = Some(0.19);
        r.eur_czk_rate = 24.5;
        r.scored = true;
        r.measured = Some(Measured {
            pv_kwh: 0.25,
            load_kwh: 0.5,
            charge_kwh: 0.1,
            discharge_kwh: 0.0,
            import_kwh: 0.3,
            export_kwh: 0.0,
            heat_kwh: HashMap::from([
                ("a".to_string(), Some(0.1)),
                ("b".to_string(), Some(0.05)),
                ("c".to_string(), None),
            ]),
            ev_kwh: HashMap::from([("tesla".to_string(), None)]),
        });
        r.planned.pv_kw = 1.1;
        r.planned.load_kw = 0.4;
        r.planned.grid_import_kw = 2.0;
        r.planned.heat_kw = HashMap::from([("a".to_string(), 0.6)]);
        r.planned_cost_eur = Some(-0.01);
        r.realized_cost_eur = Some(-0.02);
        r.misses = vec!["charge shortfall".into()];
        r
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
            battery_capacity_kwh: Some(10.0),
            heating_cop: 3.0,
            wear_eur_per_kwh: 0.05,
            plan,
            ledger,
            soc_kwh: vec![],
        }
    }

    fn by<'a>(r: &'a DayReport, s: &str) -> &'a Slot {
        r.slots.iter().find(|x| x.t == s).unwrap()
    }

    #[test]
    fn slot_counts_including_dst() {
        for (date, n) in [("2026-10-13", 96), ("2026-03-29", 92), ("2026-10-25", 100)] {
            let r = build(&inputs(date, "2026-12-01T00:00:00Z", &[], &[]));
            assert_eq!(r.slots.len(), n, "{date}");
        }
    }

    #[test]
    fn sources_follow_now() {
        let plan: Vec<_> = (0..8)
            .map(|i| {
                block(
                    &format!("2026-10-13T{:02}:{:02}:00Z", 10 + i / 4, (i % 4) * 15),
                    15,
                )
            })
            .collect();
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &[]);
        let r = build(&inp);
        assert_eq!(by(&r, "2026-10-13T09:45:00Z").source, "measured");
        // In progress, no ledger row and no SoC yet -> plan.
        assert_eq!(by(&r, "2026-10-13T10:00:00Z").source, "plan");
        assert_eq!(by(&r, "2026-10-13T10:15:00Z").source, "plan");
        assert_eq!(by(&r, "2026-10-13T12:00:00Z").source, "none");
        assert_eq!(by(&r, "2026-10-13T12:00:00Z").pv_kw, None);
        // A SoC sample for the slot in progress makes it measured; the plan fills mode and price.
        inp.soc_kwh = vec![(t("2026-10-13T10:07:00Z"), 5.0)];
        let r = build(&inp);
        let cur = by(&r, "2026-10-13T10:00:00Z");
        assert_eq!(cur.source, "measured");
        assert_eq!(cur.soc_kwh, Some(5.0));
        assert_eq!(cur.mode.as_deref(), Some("regular"));
        assert_eq!(cur.import_price, Some(0.2));
        assert_eq!(cur.pv_kw, None);
    }

    #[test]
    fn ledger_row_gives_measured_flows_cost_planned_and_misses() {
        let ledger = vec![row("2026-10-13T08:00:00Z")];
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &[], &ledger);
        inp.soc_kwh = vec![(t("2026-10-13T08:15:00Z"), 6.3)];
        let r = build(&inp);
        let s = by(&r, "2026-10-13T08:00:00Z");
        assert_eq!(s.source, "measured");
        assert_eq!(s.mode.as_deref(), Some("charge_from_grid"));
        assert_eq!(s.pv_kw, Some(1.0));
        assert_eq!(s.load_kw, Some(2.0));
        assert_eq!(s.import_kw, Some(1.2));
        assert_eq!(s.charge_kw, Some(0.4));
        // Known entries only: a 0.1 + b 0.05 kWh -> 0.6 kW; the EV has no known entry -> null.
        assert_eq!(s.heat_kw, Some(0.6));
        assert_eq!(s.ev_kw, None);
        assert_eq!(s.soc_kwh, Some(6.3));
        assert_eq!(s.cost_eur, Some(-0.02));
        assert_eq!(s.misses, vec!["charge shortfall".to_string()]);
        let p = s.planned.as_ref().unwrap();
        assert_eq!((p.pv_kw, p.import_kw, p.heat_kw), (1.1, 2.0, 0.6));
        assert_eq!(p.cost_eur, Some(-0.01));
        // A measured slot with no ledger row (e.g. a gap) has null flows and no planned.
        let gap = by(&r, "2026-10-13T08:15:00Z");
        assert_eq!(gap.source, "measured");
        assert!(gap.pv_kw.is_none() && gap.planned.is_none() && gap.misses.is_empty());
    }

    #[test]
    fn unscored_row_is_null_not_zero() {
        let mut r0 = row("2026-10-13T08:00:00Z");
        r0.scored = false;
        r0.measured = None;
        r0.realized_cost_eur = None;
        r0.misses = vec![];
        let ledger = vec![r0];
        let r = build(&inputs("2026-10-13", "2026-10-13T10:07:00Z", &[], &ledger));
        let s = by(&r, "2026-10-13T08:00:00Z");
        for v in [
            s.pv_kw,
            s.load_kw,
            s.heat_kw,
            s.ev_kw,
            s.charge_kw,
            s.discharge_kw,
            s.import_kw,
            s.export_kw,
            s.cost_eur,
        ] {
            assert_eq!(v, None);
        }
        // The decision and what was planned are still known.
        assert_eq!(s.mode.as_deref(), Some("charge_from_grid"));
        assert_eq!(s.planned.as_ref().unwrap().pv_kw, 1.1);
        assert_eq!(r.totals.measured.totals.pv_kwh, 0.0);
        // An empty per-zone map is 0.0, never -0.0 (serialised "-0.0").
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("-0.0,") && !json.contains("-0.0}"));
    }

    #[test]
    fn hourly_plan_block_splits_into_four_slots_and_costs_sum() {
        let plan = vec![block("2026-10-13T12:00:00Z", 60)];
        let r = build(&inputs("2026-10-13", "2026-10-13T10:00:00Z", &plan, &[]));
        let hour: Vec<_> = r
            .slots
            .iter()
            .filter(|s| s.t.starts_with("2026-10-13T12:"))
            .collect();
        assert_eq!(hour.len(), 4);
        // 4 kW import * 0.25 h * 0.2 + 0.05 * 2 kW * 0.25 h = 0.2 + 0.025
        assert!(hour
            .iter()
            .all(|s| s.source == "plan" && s.cost_eur == Some(0.225) && s.soc_kwh == Some(6.3)));
        let sum: f64 = hour.iter().map(|s| s.cost_eur.unwrap()).sum();
        assert!((sum - 0.9).abs() < 1e-9); // the whole block: (0.8 + 0.1) EUR
        assert_eq!(by(&r, "2026-10-13T13:00:00Z").source, "none");
        let s = hour[0];
        assert_eq!(s.heat_kw, Some(1.5));
        assert_eq!(s.ev_kw, Some(3.0));
        assert_eq!(s.load_kw, Some(0.4));
        assert_eq!(r.totals.plan.cost_eur, 0.9);
        assert_eq!(r.totals.plan.import_kwh, 4.0);
    }

    #[test]
    fn totals_split_measured_and_plan() {
        let ledger = vec![row("2026-10-13T08:00:00Z"), row("2026-10-13T08:15:00Z")];
        let plan = vec![block("2026-10-13T12:00:00Z", 15)];
        let r = build(&inputs(
            "2026-10-13",
            "2026-10-13T10:00:00Z",
            &plan,
            &ledger,
        ));
        let m = &r.totals.measured;
        assert_eq!(m.totals.pv_kwh, 0.5);
        assert_eq!(m.totals.load_kwh, 1.0);
        assert_eq!(m.totals.import_kwh, 0.6);
        assert_eq!(m.totals.cost_eur, -0.04);
        assert_eq!(m.planned_cost_eur, -0.02);
        assert_eq!(r.totals.plan.pv_kwh, 0.375);
    }

    #[test]
    fn fixture_json_shape() {
        let ledger = vec![row("2026-10-13T08:00:00Z")];
        let plan = vec![block("2026-10-13T12:00:00Z", 15)];
        let mut inp = inputs("2026-10-13", "2026-10-13T10:07:00Z", &plan, &ledger);
        inp.soc_kwh = vec![(t("2026-10-13T08:15:00Z"), 6.3)];
        let v = serde_json::to_value(build(&inp)).unwrap();
        let find = |s: &str| {
            v["slots"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["t"] == s)
                .unwrap()
                .clone()
        };
        println!("MEASURED {}", find("2026-10-13T08:00:00Z"));
        println!("PLAN {}", find("2026-10-13T12:00:00Z"));
        println!("TOTALS {}", v["totals"]);
        assert_eq!(v["battery_capacity_kwh"], 10.0);
        assert_eq!(v["totals"]["measured"]["planned_cost_eur"], -0.01);
        assert!(find("2026-10-13T12:00:00Z")["planned"].is_null());
        assert_eq!(
            find("2026-10-13T08:00:00Z")["planned"]["mode"],
            "charge_from_grid"
        );
        // Whole-day payload stays small.
        assert!(serde_json::to_string(&v).unwrap().len() < 80_000);
    }

    #[test]
    fn date_range_is_validated() {
        let today = d("2026-10-13");
        assert!(parse_date("2026-09-30", today).is_ok());
        assert!(parse_date("2026-10-14", today).is_ok());
        assert!(parse_date("2026-09-29", today).is_err());
        assert!(parse_date("2026-10-15", today).is_err());
    }
}
