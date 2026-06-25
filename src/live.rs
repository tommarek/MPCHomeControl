//! Measured **current** telemetry for the dashboard's live energy-flow and live-vs-plan overlay.
//!
//! Read-only Growatt + outside-temperature reads from InfluxDB, best-effort per field (a missing or
//! stale feed leaves that field `None` rather than failing the whole response). The Growatt metrics
//! live in the `solar` bucket's `solar` measurement (one field per metric, in watts); the measured
//! outside temperature comes from the configured `outside` zone series.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::optimize::config::ControlConfig;
use crate::source::SourceClients;

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

/// Latest value (in its native unit) of a single Growatt metric, if present and recent. The metric's
/// location resolves through the pluggable data-source layer (`db`'s configured signal map): the
/// `solar`-bucket field by default, or a config override. The `STALE_MIN` recency window means a
/// stalled feed's old point is not presented as "now".
async fn latest_metric(db: &SourceClients, metric: &str) -> Option<f64> {
    db.growatt_latest(metric, STALE_MIN).await
}

/// Net signed kW from two optional one-directional watt readings (`a − b`); `None` if both are
/// absent or the result is non-finite (corrupt telemetry — matches the other guards, never `NaN`).
fn net_kw(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    (a.is_some() || b.is_some())
        .then(|| (a.unwrap_or(0.0) - b.unwrap_or(0.0)) / 1000.0)
        .filter(|kw| kw.is_finite())
}

/// Read the current measured telemetry. Each field is independent and best-effort.
pub async fn read_live(db: &SourceClients, config: &ControlConfig) -> Result<LiveTelemetry> {
    // PV generation is physically ≥ 0; drop a corrupt negative/non-finite reading.
    let solar_kw = latest_metric(db, "InputPower")
        .await
        .map(|w| w / 1000.0)
        .filter(|kw| kw.is_finite() && *kw >= 0.0);
    let grid_kw = net_kw(
        latest_metric(db, "ACPowerToUser").await, // import
        latest_metric(db, "ACPowerToGrid").await, // export
    );
    let battery_kw = net_kw(
        latest_metric(db, "ChargePower").await,
        latest_metric(db, "DischargePower").await,
    );
    let house_kw = latest_metric(db, "INVPowerToLocalLoad")
        .await
        .map(|w| w / 1000.0)
        .filter(|kw| kw.is_finite() && *kw >= 0.0);
    let soc_pct = latest_metric(db, "SOC")
        .await
        .filter(|s| (0.0..=100.0).contains(s));
    // A non-positive configured capacity (no battery, or a misconfig) has no meaningful kWh figure —
    // report `None` rather than a misleading `0` for every SoC%. Then drop any non-finite/negative
    // result, so we never serialize a physically impossible SoC.
    let soc_kwh = soc_pct
        .filter(|_| config.battery.capacity_kwh > 0.0)
        .map(|p| p / 100.0 * config.battery.capacity_kwh)
        .filter(|k| k.is_finite() && *k >= 0.0);
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
        assert_eq!(net_kw(Some(f64::INFINITY), Some(1.0)), None); // corrupt → dropped, not inf
        assert_eq!(net_kw(Some(f64::NAN), None), None); // corrupt → dropped, not NaN
    }
}
