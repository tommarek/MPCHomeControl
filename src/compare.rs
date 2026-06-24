//! Shadow-vs-loxone comparison — the confidence metric.
//!
//! `mpc_home_control` runs read-only alongside the live `loxone_smart_home`, so it never logs its
//! own past recommendations to a series. This module therefore compares the *current* state: the
//! shadow's recommended Growatt mode + heating for the coming block (from the latest published plan)
//! against what loxone is *actually* doing right now (its `growatt_status` mode + SoC and the
//! per-zone heating relays). A dashboard polling this over time accumulates the agreement record —
//! the right model for a read-only observer. Strictly read-only: it only reads InfluxDB.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::app::TimestampedPlan;
use crate::influxdb::{InfluxDB, InfluxQuery};
use crate::optimize::config::ControlConfig;
use crate::rc_network::RcNetwork;

/// kW above which the shadow's recommended heating counts as "on" for the relay comparison.
const HEAT_ON_KW: f64 = 0.05;
/// Reject loxone telemetry older than this — a dark feed should read as *unavailable*, not show its
/// last value as if it were current. Generous enough not to false-null a slow-but-alive feed.
const TELEMETRY_MAX_AGE_MIN: i64 = 30;

/// Map a Growatt mode name to a canonical family, so the shadow's recommendation and loxone's live
/// `growatt_status.current_mode` compare directly. Both now speak loxone's vocabulary (`regular` /
/// `charge_from_grid` / `discharge_to_grid` / `sell_production` / `battery_hold` / `inverter_off`);
/// this also folds the historical Growatt-slot names and a couple of synonyms. `None` for an
/// unrecognised string (e.g. loxone's `high_load_protected`, a protection state the shadow doesn't
/// model), so agreement is reported as "unknown" (`null`) rather than a misleading mismatch.
fn canonical_mode(mode: &str) -> Option<&'static str> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "regular" | "load_first" | "self" | "self_use" | "self_consumption" => Some("regular"),
        "charge_from_grid" | "ac_charge" | "grid_charge" | "battery_first" => {
            Some("charge_from_grid")
        }
        "discharge_to_grid" | "discharge" => Some("discharge_to_grid"),
        "sell_production" | "grid_first" | "sell" | "export" => Some("sell_production"),
        "battery_hold" | "hold" => Some("battery_hold"),
        "inverter_off" | "off" => Some("inverter_off"),
        _ => None,
    }
}

/// One zone's heating agreement: shadow recommendation vs the loxone relay, right now.
#[derive(Debug, Clone, Serialize)]
pub struct ZoneHeatCompare {
    pub zone: String,
    pub shadow_on: bool,
    /// The loxone heating relay state (`Some(true/false)`, or `None` if it couldn't be read).
    pub loxone_on: Option<bool>,
    pub agree: Option<bool>,
}

/// The shadow's current recommendation vs loxone's live actuals.
#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub at: DateTime<Utc>,
    /// When the shadow plan being compared was computed (`None` if the loop hasn't published yet).
    pub shadow_plan_at: Option<DateTime<Utc>>,
    /// Shadow's recommended battery mode for the coming block (loxone vocabulary; see `classify_mode`).
    pub shadow_mode: Option<String>,
    /// Loxone's actual current Growatt mode (`growatt_status.current_mode`).
    pub loxone_mode: Option<String>,
    pub mode_agree: Option<bool>,
    /// Loxone's measured battery state of charge (kWh = `battery_soc` % × capacity).
    pub loxone_soc_kwh: Option<f64>,
    /// The shadow plan's battery SoC at the end of the coming block (kWh) — where it's heading.
    pub shadow_next_soc_kwh: Option<f64>,
    pub heating: Vec<ZoneHeatCompare>,
    /// Fraction of zones (with both states known) where shadow and loxone agree on heating on/off.
    pub heating_agreement_pct: Option<f64>,
}

/// Read the most recent raw `_value` of a measurement/field (with optional extra tag filters) over a
/// short look-back, as a string. Used for loxone's `growatt_status` fields and the heating relays.
async fn latest_value(
    db: &InfluxDB,
    bucket: &str,
    measurement: &str,
    field: &str,
    tags: &[(&str, &str)],
) -> Option<String> {
    let mut q = InfluxQuery::new(bucket, "-3h", Some("now()"))
        .filter("_measurement", measurement)
        .filter("_field", field);
    for (k, v) in tags {
        q = q.filter(k, v);
    }
    // The query already reduces to the single most-recent point per series (`last()`); take it.
    let rows = db.read_rows(&q.last()).await.ok()?;
    let row = rows.into_iter().last()?;
    // Recency guard: a value older than TELEMETRY_MAX_AGE_MIN means the loxone feed has gone dark, so
    // report it as unavailable rather than comparing against a stale reading. A missing/unparseable
    // `_time`, or a future timestamp from minor clock skew (negative age), lets the value through.
    let stale = row
        .get("_time")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| {
            Utc::now()
                .signed_duration_since(t.with_timezone(&Utc))
                .num_minutes()
        })
        .is_some_and(|age_min| age_min > TELEMETRY_MAX_AGE_MIN);
    if stale {
        return None;
    }
    row.get("_value").cloned()
}

/// Build the shadow-vs-loxone comparison from the latest published plan and loxone's live telemetry.
pub async fn compare(
    db: &InfluxDB,
    config: &ControlConfig,
    net: &RcNetwork,
    latest: Option<&TimestampedPlan>,
) -> Result<CompareReport> {
    let shadow_mode = latest.map(|tp| tp.plan.first_step.mode.slot.clone());
    let shadow_next_soc_kwh = latest.and_then(|tp| tp.plan.timeline.first().map(|b| b.soc_kwh));

    // Loxone live actuals (best-effort: a missing feed leaves the field `None`, not an error).
    let loxone_mode = latest_value(db, "solar", "growatt_status", "current_mode", &[]).await;
    let cap = config.battery.capacity_kwh;
    let loxone_soc_kwh = latest_value(db, "solar", "growatt_status", "battery_soc", &[])
        .await
        .and_then(|s| s.parse::<f64>().ok())
        .map(|pct| pct / 100.0 * cap)
        // Reject malformed telemetry (NaN / out-of-range) rather than reporting a nonsense SoC.
        .filter(|soc| soc.is_finite() && (0.0..=cap).contains(soc));

    // Compare on the canonical mode family (the two systems name modes differently); a mode that
    // doesn't map to a known family leaves agreement unknown rather than asserting a false mismatch.
    let mode_agree = match (
        shadow_mode.as_deref().and_then(canonical_mode),
        loxone_mode.as_deref().and_then(canonical_mode),
    ) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };

    // Per-zone heating: shadow recommendation (first-step heat) vs the loxone heating relay.
    let shadow_heat = latest
        .map(|tp| tp.plan.first_step.heat_kw.clone())
        .unwrap_or_default();
    let mut zones: Vec<String> = net
        .marker_indices
        .keys()
        .filter(|(_, m)| m == "heating")
        .map(|(z, _)| z.clone())
        .collect();
    zones.sort();
    zones.dedup();

    let mut heating = Vec::new();
    let (mut agree_n, mut both_n) = (0u32, 0u32);
    for zone in zones {
        let shadow_on = shadow_heat.get(&zone).copied().unwrap_or(0.0) > HEAT_ON_KW;
        let loxone_on = match db.zone_room(&zone) {
            Some(room) => latest_value(db, "loxone", "relay", room, &[("tag1", "heating")])
                .await
                .and_then(|s| s.parse::<f64>().ok())
                .map(|v| v > 0.5),
            None => None,
        };
        let agree = loxone_on.map(|lx| lx == shadow_on);
        if let Some(matched) = agree {
            both_n += 1;
            agree_n += u32::from(matched);
        }
        heating.push(ZoneHeatCompare {
            zone,
            shadow_on,
            loxone_on,
            agree,
        });
    }
    let heating_agreement_pct =
        (both_n > 0).then(|| 100.0 * f64::from(agree_n) / f64::from(both_n));

    Ok(CompareReport {
        at: Utc::now(),
        shadow_plan_at: latest.map(|tp| tp.computed_at),
        shadow_mode,
        loxone_mode,
        mode_agree,
        loxone_soc_kwh,
        shadow_next_soc_kwh,
        heating,
        heating_agreement_pct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_mode_unifies_vocabularies() {
        // The shadow now speaks loxone's vocabulary, so most modes match directly…
        assert_eq!(canonical_mode("battery_hold"), Some("battery_hold"));
        assert_eq!(canonical_mode("CHARGE_FROM_GRID"), Some("charge_from_grid")); // case-insensitive
                                                                                  // …and the historical Growatt-slot synonyms still fold in.
        assert_eq!(
            canonical_mode("grid_first"),
            canonical_mode("sell_production")
        );
        assert_eq!(canonical_mode("load_first"), canonical_mode("regular"));
        // loxone's protection state isn't modelled → unknown, not a false mismatch.
        assert_eq!(canonical_mode("high_load_protected"), None);
        assert_eq!(canonical_mode("something_new"), None);
    }
}
