//! Drive the thermal model with measured / forecast data, and estimate the current state.
//!
//! Shared machinery for two jobs: the backtest ([`crate::validate`]) and **state estimation** for
//! the live MPC. Both read the measured outside temperature and the open-meteo cloud cover onto an
//! hourly grid and roll the state-space model forward with heating off. The estimator returns the
//! converged final state — a real `x0` for the optimizer that accounts for the slow wall/slab masses
//! rather than an arbitrary flat guess.

use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use nalgebra::DVector;
use uom::si::{
    f64::{Angle, Power, Ratio, ThermodynamicTemperature},
    power::watt,
    ratio::ratio,
    thermodynamic_temperature::degree_celsius,
};

use crate::influxdb::TimeSample;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;
use crate::tools::sun::calculate_tilted_irradiance;

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
    /// Per-zone internal heat gain PROFILE (W per daypart) — occupants, appliances, cooking,
    /// fireplace — that the physics model doesn't otherwise have. [`drive`] evaluates it at each
    /// step's local minute and injects the result at the zone's air node.
    pub internal_gain_w: HashMap<String, crate::optimize::config::GainProfile>,
    /// Scheduled heat fluxes (e.g. a water heat-pump that cools its room on a seasonal schedule) —
    /// only the direction + schedule; the magnitude is `scheduled_w`. Applied at each load's zone air
    /// node in [`drive`] as `scheduled_w[i] × unit_profile(local time)`, combined with the internal
    /// gain on the same node. Empty = none.
    pub scheduled_loads: Vec<crate::optimize::config::ScheduledLoad>,
    /// Fitted magnitude (W, ≥ 0) of each [`Self::scheduled_loads`] entry, aligned 1:1. The probe in
    /// [`crate::validate::fit_gains`] overwrites a single entry to measure its response.
    pub scheduled_w: Vec<f64>,
    /// Optional **measured** electrical-power series (W) per [`Self::scheduled_loads`] entry, aligned
    /// 1:1 and each (when `Some`) on [`Self::hours`]. A load with a `sensor` derives its flux from this
    /// measured draw (`sign × P_elec[h] × power_factor`, still gated by the windows/months) instead of
    /// `scheduled_w × unit_profile`; `None` ⇒ no sensor (the magnitude path is used). Empty ⇒ all-None.
    pub sensor_power_w: Vec<Option<Vec<f64>>>,
    /// Site-local civil-time offset, for evaluating the scheduled-load windows (month / minute-of-day).
    pub local_offset: chrono::FixedOffset,
}

/// Read the measured outside temperature and open-meteo cloud cover over `[start, now]` onto an
/// hourly grid. The outside series defines the grid; cloud is forward-filled onto it, falling back
/// to `fallback_cloud` if no cloud series is available.
pub async fn read_drive_data(
    db: &SourceClients,
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

    // Through the pluggable weather locator (honours `data_sources` remaps) with the forecast's
    // `_start` stamping: open-meteo points are stamped at the hour they FORECAST, so cloud[i]
    // covers [hours[i], hours[i]+1h) — indexed at `h` in the drive, unlike the stop-stamped
    // measured series below.
    let cloud_samples = db
        .weather_cloud_series(start, stop, "1h")
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
        scheduled_loads: Vec::new(),
        scheduled_w: Vec::new(),
        sensor_power_w: Vec::new(),
        local_offset: chrono::FixedOffset::east_opt(0).unwrap(),
    })
}

/// Seed the model state from measurements: each measured zone's air node at its first sample, all
/// other (wall/slab) states at the mean of those. Returns the seed and the per-zone measured
/// series (reused by the backtest for scoring).
pub async fn seed_state(
    db: &SourceClients,
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
        // Index convention for the step [grid_times[h], grid_times[h+1]): the MEASURED series
        // (outside temp, relay heating, sensor power) are hourly means stamped at the window
        // *stop* (influx aggregateWindow), so the sample covering this interval sits at h+1 —
        // using h applied everything one hour late (an hour of morning sun landed in the model
        // an hour after the real house saw it). The forecast-stamped cloud series covers [h, h+1)
        // at index h (see read_drive_data), and solar uses the interval midpoint.
        let mut u = ss.zero_input();
        if let Some(node) = outside {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(data.outside_c[h + 1]),
            );
        }
        if let Some(node) = ground {
            ss.set_boundary_temp(
                &mut u,
                node,
                ThermodynamicTemperature::new::<degree_celsius>(data.ground_c),
            );
        }
        // Solar at the hour midpoint (the hourly representative).
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
            ss.set_flux(&mut u, surf.node, irradiance * surf.area * surf.absorptance);
        }
        // Marker-node fluxes accumulate in one map (set_flux overwrites): recorded underfloor
        // heating plus the mass share of transmitted window solar can land on the same slab nodes.
        let mut marker_flux_w: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
        // Recorded underfloor heating (active backtest): inject each zone's hourly power at its
        // `heating` marker node(s), split equally when a zone's floor has several. Empty `heating_kw`
        // leaves the free response.
        for (zone, powers) in &data.heating_kw {
            if let (Some(nodes), Some(&kw)) = (
                net.marker_indices
                    .get_vec(&(zone.clone(), "heating".to_string())),
                powers.get(h + 1),
            ) {
                let per_node_w = kw * 1000.0 / nodes.len() as f64;
                for &node in nodes {
                    *marker_flux_w.entry(node).or_insert(0.0) += per_node_w;
                }
            }
        }
        // Combined per-zone air-node flux: the constant internal gain (occupants / appliances /
        // fireplace) plus any scheduled loads active now (e.g. a water heat-pump that cools its room
        // on a seasonal schedule). The air node is distinct from the heating markers and solar
        // surfaces, so this never overwrites them; accumulating into one map then writing once means
        // an internal gain and a scheduled load on the same zone *combine* rather than clobber.
        let local = when.with_timezone(&data.local_offset);
        let (month, minute) = (local.month(), local.hour() * 60 + local.minute());
        let mut air_flux_w: HashMap<&str, f64> = HashMap::new();
        for (zone, gain) in &data.internal_gain_w {
            *air_flux_w.entry(zone.as_str()).or_insert(0.0) += gain.at(minute);
        }
        // Transmitted window solar — the same aperture model + air/floor-mass split the forecast
        // applies (coordinator's known_thermal_inputs), so the calibration/backtest drive and the
        // live plan see identical physics.
        for w in &net.window_surfaces {
            let irradiance =
                calculate_tilted_irradiance(latitude, longitude, &when, cloud, w.tilt, w.azimuth);
            let gain_w = (irradiance * w.area * w.g).get::<watt>();
            match net
                .marker_indices
                .get_vec(&(w.zone.clone(), "heating".to_string()))
                .filter(|nodes| !nodes.is_empty())
            {
                Some(nodes) => {
                    *air_flux_w.entry(w.zone.as_str()).or_insert(0.0) +=
                        gain_w * crate::rc_network::WINDOW_SOLAR_TO_AIR;
                    let per_node = gain_w * (1.0 - crate::rc_network::WINDOW_SOLAR_TO_AIR)
                        / nodes.len() as f64;
                    for &node in nodes {
                        *marker_flux_w.entry(node).or_insert(0.0) += per_node;
                    }
                }
                None => *air_flux_w.entry(w.zone.as_str()).or_insert(0.0) += gain_w,
            }
        }
        // Flush the accumulated marker fluxes (recorded heating + window mass share) into the
        // input vector — set_flux overwrites, hence the single write per node.
        for (&node, &flux_w) in &marker_flux_w {
            ss.set_flux(&mut u, node, Power::new::<watt>(flux_w));
        }
        for (i, load) in data.scheduled_loads.iter().enumerate() {
            // The signed unit profile (±1 active, 0 outside the window/months) — the seasonal duct stays
            // authoritative for *when* the load is on. A sensor-driven load derives its magnitude from
            // the measured electrical draw (`P_elec × power_factor`); all others use the fitted/configured
            // `scheduled_w` magnitude. Both carry the sign through `unit_profile`.
            let profile = load.unit_profile(month, minute);
            if profile == 0.0 {
                continue;
            }
            let magnitude = match data.sensor_power_w.get(i).and_then(Option::as_ref) {
                Some(series) => {
                    series.get(h + 1).copied().unwrap_or(0.0) * load.power_factor.unwrap_or(1.0)
                }
                None => data.scheduled_w.get(i).copied().unwrap_or(0.0),
            };
            *air_flux_w.entry(load.zone.as_str()).or_insert(0.0) += magnitude * profile;
        }
        for (zone, flux_w) in air_flux_w {
            if let Some(&node) = net.zone_indices.get(zone) {
                ss.set_flux(&mut u, node, Power::new::<watt>(flux_w));
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
/// seed even though only zone-air is observed. The zone-air states are then re-anchored to the
/// latest measured temperature, so the result reflects disturbances the model can't see (e.g. open
/// windows) rather than the free-running prediction.
///
/// The drive uses the **recorded heating relays**, the live-fitted internal gains and the scheduled
/// loads over the window — not a passive free-run. In heating season several kWh/day go into each
/// slab; a passive drive would relax exactly the unobserved slab states (the ones this estimator
/// exists to recover) toward a heating-free trajectory, leaving them far too cold and making the LP
/// over-buy heating every tick. `cache` supplies the live-fitted gains/magnitudes when the loop has
/// them; the config baseline covers the on-demand path.
#[allow(clippy::too_many_arguments)] // the model, site, window, config and live-fit cache are all distinct
pub async fn estimate_initial_state(
    db: &SourceClients,
    net: &RcNetwork,
    ss: &StateSpace,
    latitude: Angle,
    longitude: Angle,
    history_hours: i64,
    config: &crate::optimize::config::ControlConfig,
    cache: Option<&crate::app::PlanCache>,
) -> Result<DVector<f64>> {
    ensure!(history_hours > 0, "history window must be positive");
    let start = format!("-{history_hours}h");
    let ground_c = config.site.ground_temperature_c;
    let mut data = read_drive_data(db, &start, "now()", ground_c, 0.5).await?;
    // The drive grid ends at the LAST outside-temperature sample — if that feed stalls, the whole
    // "current" state is really the state as of hours ago (and the re-anchor guard below would
    // keep zone samples newer than the grid from applying). Erroring here routes current_plan to
    // its flagged fallback seed + degraded gate instead of silently planning from the past.
    let now_h = chrono::Utc::now().timestamp().div_euclid(3600);
    if let Some(&last_h) = data.hours.last() {
        ensure!(
            now_h - last_h <= 3,
            "outside-temperature series is stale (last sample {}h ago) — cannot estimate the current state",
            now_h - last_h
        );
    }
    // Real inputs over the window: recorded relay duty × max_heat_kw per zone, the live-fitted (or
    // config-baseline) internal gains, and the scheduled loads at their live-fitted magnitudes.
    data.heating_kw =
        crate::validate::read_heating_kw(db, net, &config.heating, &data.hours, &start, "now()")
            .await;
    data.internal_gain_w = cache
        .map(|c| c.internal_gains.clone())
        .unwrap_or_else(|| config.heating.internal_gains());
    data.scheduled_loads = config.scheduled_loads.clone();
    data.scheduled_w = cache
        .map(|c| c.scheduled_w.clone())
        .filter(|w| w.len() == config.scheduled_loads.len())
        .unwrap_or_else(|| {
            config
                .scheduled_loads
                .iter()
                .map(|l| l.power_w.unwrap_or(0.0) * l.power_factor.unwrap_or(1.0))
                .collect()
        });
    // Sensor-driven loads: the measured draw (like the calibration drive) rather than the
    // forecast magnitude, so the estimator and the fit see the same fluxes.
    data.sensor_power_w = crate::validate::read_sensor_power_w(
        db,
        &config.scheduled_loads,
        &data.hours,
        &start,
        "now()",
    )
    .await;
    data.local_offset = config.site.offset_at(chrono::Utc::now());
    let (seed, series) = seed_state(db, net, ss, &start, "now()").await?;
    let trajectory = drive(net, ss, latitude, longitude, &seed, &data);
    let mut x = trajectory.last().cloned().unwrap_or(seed);
    // Re-anchor each zone's AIR node to its latest measured temperature. The drive recovers the
    // unobservable wall/slab masses, but the air node itself is measured — pinning it to the most
    // recent reading captures disturbances the model can't see (e.g. windows left open overnight)
    // that would otherwise leave the free-running estimate too warm. Zones without measured data
    // keep the driven value (seed_state only returns a series for zones that have data).
    //
    // Recency guard: only anchor to a FRESH sample. A dead sensor's last reading can be many
    // hours old (anywhere in the 72 h window), and pinning the air node to e.g. a warm afternoon
    // value overnight would beat the driven estimate every tick until the sensor returns — the
    // driven value is the better guess once the reading is stale.
    const ANCHOR_FRESH_HOURS: i64 = 2;
    let now_h = chrono::Utc::now().timestamp().div_euclid(3600);
    for (zone, samples) in &series {
        if let (Some(&node), Some(last)) = (net.zone_indices.get(zone), samples.last()) {
            if now_h - last.time.timestamp().div_euclid(3600) > ANCHOR_FRESH_HOURS {
                eprintln!(
                    "[estimate] zone {zone}: last temperature sample is stale ({}); keeping the driven value",
                    last.time.format("%Y-%m-%d %H:%M")
                );
                continue;
            }
            if let Some(s) = ss.state_index(node) {
                x[s] = ThermodynamicTemperature::new::<degree_celsius>(last.value)
                    .get::<uom::si::thermodynamic_temperature::kelvin>();
            }
        }
    }
    Ok(x)
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
