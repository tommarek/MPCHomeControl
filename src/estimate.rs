//! Drive the thermal model with measured / forecast data, and estimate the current state.
//!
//! Shared machinery for two jobs: the backtest ([`crate::validate`]) and **state estimation** for
//! the live MPC. Both read the measured outside temperature and the open-meteo cloud cover onto an
//! hourly grid and roll the state-space model forward with heating off. The estimator returns the
//! converged final state — a real `x0` for the optimizer, so the slow wall/slab masses no longer
//! start at an arbitrary flat guess.

use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use nalgebra::DVector;
use uom::si::{
    f64::{Angle, Power, Ratio, ThermodynamicTemperature},
    power::{kilowatt, watt},
    ratio::ratio,
    thermodynamic_temperature::degree_celsius,
};

use crate::influxdb::{InfluxDB, TimeSample};
use crate::rc_network::RcNetwork;
use crate::state_space::StateSpace;
use crate::tools::sun::calculate_tilted_irradiance;

/// Open-meteo cloud-cover series location (written into InfluxDB by loxone_smart_home).
const WEATHER_BUCKET: &str = "weather_forecast";
const WEATHER_MEASUREMENT: &str = "weather_forecast";

/// The unix-hour bucket of an instant (matches Flux `aggregateWindow(every: 1h)` stop boundaries).
pub(crate) fn hour_key(t: DateTime<Utc>) -> i64 {
    t.timestamp().div_euclid(3600)
}

/// Forward-filled values over `hours` from `samples` (samples must be non-empty, sorted). A grid
/// hour with no sample carries the most recent earlier value; grid hours before the first sample
/// take the first sample's value. On an hour collision the last sample wins (benign — the source
/// series is one point per hour).
pub(crate) fn resample_ffill(hours: &[i64], samples: &[TimeSample]) -> Vec<f64> {
    debug_assert!(
        !samples.is_empty(),
        "resample_ffill requires a non-empty series"
    );
    let by_hour: HashMap<i64, f64> = samples
        .iter()
        .map(|s| (hour_key(s.time), s.value))
        .collect();
    let mut last = samples[0].value;
    hours
        .iter()
        .map(|h| {
            if let Some(&v) = by_hour.get(h) {
                last = v;
            }
            last
        })
        .collect()
}

/// Per-hour known inputs that drive the passive model: measured outside temperature, open-meteo
/// cloud cover (for the solar gain), and the ground boundary, on a regular hourly grid.
#[derive(Clone)]
pub struct DriveData {
    /// Hourly grid timestamps (UTC), one per grid point.
    pub grid_times: Vec<DateTime<Utc>>,
    /// Unix-hour keys, parallel to `grid_times`.
    pub hours: Vec<i64>,
    /// Forward-filled measured outside temperature (°C) per grid hour.
    pub outside_c: Vec<f64>,
    /// Forward-filled cloud cover (ratio 0..1) per grid hour.
    pub cloud: Vec<f64>,
    /// Ground temperature (°C) under the slab.
    pub ground_c: f64,
    /// Per-zone underfloor-heating power (kW) per grid hour, from the recorded relays (empty =
    /// heating off / passive). Injected at each zone's `heating` marker node in [`drive`].
    pub heating_kw: HashMap<String, Vec<f64>>,
    /// Constant per-zone internal heat gain (W) — occupants, appliances, cooking, fireplace — that
    /// the physics model doesn't otherwise have. Injected at each zone's air node in [`drive`].
    pub internal_gain_w: HashMap<String, f64>,
}

/// Read the measured outside temperature and open-meteo cloud cover over `[start, now]` onto an
/// hourly grid. The outside series defines the grid; cloud is forward-filled onto it, falling back
/// to `fallback_cloud` if no cloud series is available.
pub async fn read_drive_data(
    db: &InfluxDB,
    start: &str,
    stop: &str,
    ground_c: f64,
    fallback_cloud: f64,
) -> Result<DriveData> {
    let outside = db
        .read_zone_temperature_series("outside", start, stop, "1h")
        .await
        .context("reading outside temperature series")?;
    ensure!(
        outside.len() >= 2,
        "not enough outside-temperature samples ({}) to drive the model",
        outside.len()
    );

    let first = hour_key(outside[0].time);
    let last = hour_key(outside[outside.len() - 1].time);
    let hours: Vec<i64> = (first..=last).collect();
    let grid_times: Vec<DateTime<Utc>> = hours
        .iter()
        .map(|h| Utc.timestamp_opt(h * 3600, 0).single().context("grid time"))
        .collect::<Result<_>>()?;
    let outside_c = resample_ffill(&hours, &outside);

    // Open-meteo cloud cover (percent) -> ratio; fall back to a constant if unavailable.
    let cloud_samples = db
        .read_series(
            WEATHER_BUCKET,
            WEATHER_MEASUREMENT,
            "cloudcover",
            &[("room", "outside"), ("type", "hour")],
            start,
            stop,
            "1h",
        )
        .await
        .unwrap_or_default();
    let cloud = if cloud_samples.is_empty() {
        vec![fallback_cloud.clamp(0.0, 1.0); hours.len()]
    } else {
        resample_ffill(&hours, &cloud_samples)
            .iter()
            .map(|pct| (pct / 100.0).clamp(0.0, 1.0))
            .collect()
    };

    Ok(DriveData {
        grid_times,
        hours,
        outside_c,
        cloud,
        ground_c,
        heating_kw: HashMap::new(),
        internal_gain_w: HashMap::new(),
    })
}

/// Seed the model state from measurements: each measured zone's air node at its first sample, all
/// other (wall/slab) states at the mean of those. Returns the seed and the per-zone measured
/// series (reused by the backtest for scoring).
pub async fn seed_state(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    start: &str,
    stop: &str,
) -> Result<(DVector<f64>, HashMap<String, Vec<TimeSample>>)> {
    let mut zone_series: HashMap<String, Vec<TimeSample>> = HashMap::new();
    for zone in net.zone_indices.keys() {
        if zone == "outside" || zone == "ground" || ss.state_index(net.zone_indices[zone]).is_none()
        {
            continue;
        }
        match db
            .read_zone_temperature_series(zone, start, stop, "1h")
            .await
        {
            Ok(series) if !series.is_empty() => {
                zone_series.insert(zone.clone(), series);
            }
            Ok(_) => eprintln!("  estimate: zone '{zone}' has no temperature data, skipping"),
            Err(e) => eprintln!("  estimate: zone '{zone}' read failed ({e}), skipping"),
        }
    }
    ensure!(
        !zone_series.is_empty(),
        "no measured zone temperatures to seed from"
    );

    let seeds: Vec<f64> = zone_series
        .values()
        .filter_map(|s| s.first())
        .map(|s| s.value)
        .collect();
    let base_c = seeds.iter().sum::<f64>() / seeds.len() as f64;
    let mut x = DVector::from_element(
        ss.n_states(),
        ThermodynamicTemperature::new::<degree_celsius>(base_c)
            .get::<uom::si::thermodynamic_temperature::kelvin>(),
    );
    for (zone, series) in &zone_series {
        if let (Some(&node), Some(first)) = (net.zone_indices.get(zone), series.first()) {
            if let Some(s) = ss.state_index(node) {
                x[s] = ThermodynamicTemperature::new::<degree_celsius>(first.value)
                    .get::<uom::si::thermodynamic_temperature::kelvin>();
            }
        }
    }
    Ok((x, zone_series))
}

/// Roll the model forward over the hourly grid from `x0`, driven by `data` (outside temp + solar
/// with per-hour cloud, heating off). Returns the state at each grid time (length `grid_times`).
pub fn drive(
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    x0: &DVector<f64>,
    data: &DriveData,
) -> Vec<DVector<f64>> {
    let outside = net.zone_indices.get("outside").copied();
    let ground = net.zone_indices.get("ground").copied();
    let disc = ss.discretize(3600.0); // uniform 1-hour grid
    let mut x = x0.clone();
    let mut trajectory = Vec::with_capacity(data.grid_times.len());
    trajectory.push(x.clone());
    for h in 0..data.grid_times.len().saturating_sub(1) {
        let mut u = ss.zero_input();
        if let Some(node) = outside {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(data.outside_c[h]),
            );
        }
        if let Some(node) = ground {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(data.ground_c),
            );
        }
        // Solar at the hour midpoint with the hour's cloud cover (the hourly representative).
        let when = data.grid_times[h] + Duration::minutes(30);
        let cloud = Ratio::new::<ratio>(data.cloud[h]);
        for surf in &net.solar_surfaces {
            let irradiance = calculate_tilted_irradiance(
                latitude,
                longitude,
                &when,
                cloud,
                surf.tilt,
                surf.azimuth,
            );
            ss.set_flux(&mut u, surf.node, irradiance * surf.area);
        }
        // Recorded underfloor heating (active backtest): inject each zone's hourly power at its
        // `heating` marker node(s), split equally across them (a split floor has several) — matching
        // the kernel construction in `optimize::thermal`. Empty `heating_kw` leaves the free response.
        for (zone, powers) in &data.heating_kw {
            if let (Some(nodes), Some(&kw)) = (
                net.marker_indices
                    .get_vec(&(zone.clone(), "heating".to_string())),
                powers.get(h),
            ) {
                let per_node = Power::new::<kilowatt>(kw / nodes.len() as f64);
                for &node in nodes {
                    ss.set_flux(&mut u, node, per_node);
                }
            }
        }
        // Constant internal gains (occupants / appliances / fireplace) at each zone's air node — a
        // node distinct from the heating markers and solar surfaces, so this never overwrites them.
        for (zone, &gain_w) in &data.internal_gain_w {
            if let Some(&node) = net.zone_indices.get(zone) {
                ss.set_flux(&mut u, node, Power::new::<watt>(gain_w));
            }
        }
        x = ss.step(&disc, &x, &u);
        trajectory.push(x.clone());
    }
    trajectory
}

/// Estimate the current thermal state by driving the model over the last `history_hours` from a
/// measured seed, returning the converged final state — a real `x0` for the optimizer. The slow
/// slab masses relax toward the measured-driven solution, so the result is far better than a flat
/// seed even though only zone-air is observed.
pub async fn estimate_initial_state(
    db: &InfluxDB,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    history_hours: i64,
    ground_c: f64,
) -> Result<DVector<f64>> {
    ensure!(history_hours > 0, "history window must be positive");
    let start = format!("-{history_hours}h");
    let data = read_drive_data(db, &start, "now()", ground_c, 0.5).await?;
    let (seed, _series) = seed_state(db, net, ss, &start, "now()").await?;
    let trajectory = drive(net, ss, latitude, longitude, &seed, &data);
    Ok(trajectory.last().cloned().unwrap_or(seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(hour: i64, value: f64) -> TimeSample {
        TimeSample {
            time: Utc.timestamp_opt(hour * 3600, 0).single().unwrap(),
            value,
        }
    }

    #[test]
    fn resample_forward_fills_gaps() {
        let samples = vec![sample(0, 10.0), sample(2, 12.0)]; // hour 1 missing
        let hours = vec![0, 1, 2, 3];
        assert_eq!(
            resample_ffill(&hours, &samples),
            vec![10.0, 10.0, 12.0, 12.0]
        );
    }

    #[test]
    fn hour_key_is_stop_boundary_bucket() {
        // 13:00:00Z -> hour bucket; 12:59 falls in the previous bucket.
        let t = Utc.timestamp_opt(13 * 3600, 0).single().unwrap();
        assert_eq!(hour_key(t), 13);
        assert_eq!(hour_key(t - Duration::minutes(1)), 12);
    }
}
