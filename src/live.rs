//! Measured **current** telemetry for the dashboard's live energy-flow and live-vs-plan overlay.
//!
//! Read-only Growatt + outside-temperature reads from InfluxDB, best-effort per field (a missing or
//! stale feed leaves that field `None` rather than failing the whole response). The Growatt metrics
//! live in the `solar` bucket's `solar` measurement (one field per metric, in watts); the measured
//! outside temperature comes from the configured `outside` zone series.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::influxdb::{InfluxDB, InfluxQuery};
use crate::optimize::config::ControlConfig;

/// Ignore telemetry older than this (the live Growatt feed writes every few seconds, so anything
/// older means the feed has stalled — show it as unavailable rather than a frozen value).
const STALE_MIN: i64 = 10;

/// A snapshot of the house's measured power flows + battery + outside temperature, right now.
#[derive(Debug, Clone, Serialize)]
pub struct LiveTelemetry {
    pub at: DateTime<Utc>,
    /// PV generation (kW).
    pub solar_kw: Option<f64>,
    /// Grid power (kW): **positive = importing**, negative = exporting.
    pub grid_kw: Option<f64>,
    /// House consumption served by the inverter (kW).
    pub house_kw: Option<f64>,
    /// Battery power (kW): **positive = charging**, negative = discharging.
    pub battery_kw: Option<f64>,
    pub soc_pct: Option<f64>,
    pub soc_kwh: Option<f64>,
    pub outside_temp_c: Option<f64>,
}

/// Latest value (in its native unit) of a single `solar`-measurement field, if present and recent.
async fn latest_solar(db: &InfluxDB, field: &str) -> Option<f64> {
    let q = InfluxQuery::new("solar", "-30m", Some("now()"))
        .filter("_measurement", "solar")
        .filter("_field", field)
        .last();
    let row = db.read_rows(&q).await.ok()?.into_iter().last()?;
    // Recency guard: a stalled feed's last point can be hours old — don't present it as "now".
    let recent = row
        .get("_time")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| {
            Utc::now()
                .signed_duration_since(t.with_timezone(&Utc))
                .num_minutes()
        })
        .is_some_and(|m| m <= STALE_MIN);
    if !recent {
        return None;
    }
    row.get("_value")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Combine two optional one-directional power readings (watts) into a single signed kW value
/// (`a − b`), present if either side is known.
fn net_kw(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    (a.is_some() || b.is_some()).then(|| (a.unwrap_or(0.0) - b.unwrap_or(0.0)) / 1000.0)
}

/// Read the current measured telemetry. Each field is independent and best-effort.
pub async fn read_live(db: &InfluxDB, config: &ControlConfig) -> Result<LiveTelemetry> {
    let solar_kw = latest_solar(db, "InputPower").await.map(|w| w / 1000.0);
    let grid_kw = net_kw(
        latest_solar(db, "ACPowerToUser").await, // import
        latest_solar(db, "ACPowerToGrid").await, // export
    );
    let battery_kw = net_kw(
        latest_solar(db, "ChargePower").await,
        latest_solar(db, "DischargePower").await,
    );
    let house_kw = latest_solar(db, "INVPowerToLocalLoad")
        .await
        .map(|w| w / 1000.0);
    let soc_pct = latest_solar(db, "SOC")
        .await
        .filter(|s| (0.0..=100.0).contains(s));
    let soc_kwh = soc_pct.map(|p| p / 100.0 * config.battery.capacity_kwh);
    let outside_temp_c = db
        .read_zone_temperature_series("outside", "-1h", "now()", "5m")
        .await
        .ok()
        .and_then(|s| s.last().map(|x| x.value));

    Ok(LiveTelemetry {
        at: Utc::now(),
        solar_kw,
        grid_kw,
        house_kw,
        battery_kw,
        soc_pct,
        soc_kwh,
        outside_temp_c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_kw_combines_directions_and_handles_missing() {
        assert_eq!(net_kw(Some(2000.0), Some(500.0)), Some(1.5)); // import 2kW − export 0.5kW
        assert_eq!(net_kw(Some(0.0), Some(3000.0)), Some(-3.0)); // pure export → negative
        assert_eq!(net_kw(Some(1000.0), None), Some(1.0)); // one side known
        assert_eq!(net_kw(None, None), None); // neither known → unavailable
    }
}
