//! What one more Kelvin on a room's comfort target costs per year: a steady-state heat-loss model
//! over the last 365 days of measured outside temperature and real import prices.
//!
//! Two parts. The pure part ([`steady_state_conductance`], [`hourly_sum`]) works on the RC network
//! and plain numbers and is unit-tested offline. The IO part ([`yearly_projection`]) reads the
//! history in bounded 7-day chunks with a pause between them and must only ever run off the
//! planning path (the API computes it in a background task and caches it).
//!
//! Conductances are plain `f64` W/K here, like the pricing helpers use plain EUR floats.
//!
//! Method: put 1 W into the room (at its underfloor slab for heating, at its air node for
//! cooling), hold the outside, the ground and every OTHER heated room at the reference
//! temperature, and solve the network's steady state. The room's own conductance `g_room` is
//! `1 / θ_air`; the share of that heat that leaves the house (to outside + ground) gives
//! `g_house`. Raising the target 1 K then costs `g_house` more watts in every hour the room needs
//! heat; heat shared with neighbours that are heated anyway is not a house cost.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use nalgebra::{DMatrix, DVector};
use petgraph::visit::EdgeRef;
use serde::Serialize;
use uom::si::thermal_conductance::watt_per_kelvin;

use crate::backtest_common::{chunk_windows, read_prices_chunked, INTER_DAY_PAUSE_S};
use crate::estimate::hour_key;
use crate::optimize::config::ControlConfig;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;

/// Days of history read.
pub const WINDOW_DAYS: i64 = 365;
/// Below this many matched days the figure is flagged `partial`.
pub const PARTIAL_BELOW_DAYS: f64 = 300.0;
const METHOD: &str = "heat_loss_model";
const NOTE: &str = "Odhad z modelu podle loňského počasí a cen; plánovač přesouvá topení do levných hodin, skutečné náklady budou spíš nižší. ±20 %.";
const HEATING_MARKER: &str = "heating";

/// Where the 1 W of the steady-state solve is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inject {
    /// Split equally over the zone's `heating` slab marker nodes.
    Slab,
    /// The zone's air node (an air-conditioning unit).
    Air,
}

/// Steady-state conductances of one room, W per K of its air temperature.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Conductances {
    /// All heat the room can shed (to outside, ground and the neighbours held at reference).
    pub g_room: f64,
    /// Of that, straight to the outside.
    pub g_out: f64,
    /// Of that, to the ground.
    pub g_gnd: f64,
    /// `g_out + g_gnd`: what leaves the house, the basis of the cost.
    pub g_house: f64,
}

/// Solve the steady state for `zone` with `heated_zones` (the other heated rooms) held at the
/// reference temperature along with the outside and the ground.
pub fn steady_state_conductance(
    net: &RcNetwork,
    zone: &str,
    heated_zones: &[String],
    inject: Inject,
) -> Result<Conductances> {
    let graph = &net.graph;
    let air = *net
        .zone_indices
        .get(zone)
        .ok_or_else(|| anyhow!("zone {zone:?} is not in the model"))?;
    let outside = net.zone_indices.get("outside").copied();
    let ground = net.zone_indices.get("ground").copied();

    let mut fixed: HashSet<_> = graph
        .node_indices()
        .filter(|&n| !graph[n].heat_capacity.is_finite())
        .collect();
    for other in heated_zones.iter().filter(|z| z.as_str() != zone) {
        if let Some(&n) = net.zone_indices.get(other) {
            fixed.insert(n);
        }
    }
    anyhow::ensure!(!fixed.contains(&air), "zone {zone:?} has no free air node");

    let free: Vec<_> = graph
        .node_indices()
        .filter(|n| !fixed.contains(n))
        .collect();
    let pos: HashMap<_, _> = free.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let n = free.len();

    let mut l = DMatrix::<f64>::zeros(n, n);
    for e in graph.edge_references() {
        let g = e.weight().conductance.get::<watt_per_kelvin>();
        let (a, b) = (e.source(), e.target());
        for (x, y) in [(a, b), (b, a)] {
            if let Some(&i) = pos.get(&x) {
                l[(i, i)] += g;
                if let Some(&j) = pos.get(&y) {
                    l[(i, j)] -= g;
                }
            }
        }
    }

    let sources = match inject {
        Inject::Air => vec![air],
        Inject::Slab => net
            .marker_indices
            .get_vec(&(zone.to_string(), HEATING_MARKER.to_string()))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|node| pos.contains_key(node))
            .collect(),
    };
    anyhow::ensure!(!sources.is_empty(), "zone {zone:?} has no heating marker");
    let mut q = DVector::<f64>::zeros(n);
    for node in &sources {
        q[pos[node]] += 1.0 / sources.len() as f64;
    }

    let theta = l
        .cholesky()
        .ok_or_else(|| anyhow!("steady-state matrix for {zone:?} is not positive definite"))?
        .solve(&q);

    let (mut f_out, mut f_gnd) = (0.0, 0.0);
    for e in graph.edge_references() {
        let g = e.weight().conductance.get::<watt_per_kelvin>();
        for (x, y) in [(e.source(), e.target()), (e.target(), e.source())] {
            let Some(&i) = pos.get(&x) else { continue };
            if Some(y) == outside {
                f_out += g * theta[i];
            } else if Some(y) == ground {
                f_gnd += g * theta[i];
            }
        }
    }
    let t_air = theta[pos[&air]];
    anyhow::ensure!(
        t_air.is_finite() && t_air > 0.0,
        "steady-state solve for {zone:?} gave a non-positive air rise"
    );
    let g_room = 1.0 / t_air;
    let (g_out, g_gnd) = (f_out * g_room, f_gnd * g_room);
    Ok(Conductances {
        g_room,
        g_out,
        g_gnd,
        g_house: g_out + g_gnd,
    })
}

/// One matched hour of history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HourInput {
    /// Measured outside temperature (°C).
    pub t_out: f64,
    /// Import price (EUR/kWh): spot plus distribution.
    pub import_eur_kwh: f64,
}

/// What a room's heating needs in a steady hour: the heat lost to outside and ground at target
/// `target`, minus the internal gain (W). The room heats when this is positive.
fn heat_demand_w(c: &Conductances, target: f64, t_out: f64, t_ground: f64, gain_w: f64) -> f64 {
    c.g_out * (target - t_out) + c.g_gnd * (target - t_ground) - gain_w
}

/// Yearly-style sum over `hours` of the electricity cost (EUR) and the number of hours of raising a
/// heat target by 1 K: `g_house × 1 K / cop` in every hour the room heats, at that hour's price.
/// A room heats when the outside is colder than its target (the heating season) AND its losses to
/// outside and ground exceed the internal gain.
pub fn hourly_sum(
    hours: &[HourInput],
    c: &Conductances,
    target: f64,
    t_ground: f64,
    gain_w: f64,
    cop: f64,
) -> (f64, usize) {
    let mut eur = 0.0;
    let mut count = 0;
    for h in hours {
        if h.t_out < target && heat_demand_w(c, target, h.t_out, t_ground, gain_w) > 0.0 {
            eur += c.g_house / 1000.0 / cop * h.import_eur_kwh;
            count += 1;
        }
    }
    (eur, count)
}

/// Electricity cost (EUR) and hours of the cooling an air-conditioned room needs to hold its cool
/// target 1 K lower: `g_house × 1 K / COP(t_out)` in every hour the outside is above the target.
/// `cop_at` maps the outside temperature to the unit's cooling COP.
pub fn cooling_sum(
    hours: &[HourInput],
    c: &Conductances,
    cool_target: f64,
    cop_at: impl Fn(f64) -> f64,
) -> (f64, usize) {
    let mut eur = 0.0;
    let mut count = 0;
    for h in hours.iter().filter(|h| h.t_out > cool_target) {
        eur += c.g_house / 1000.0 / cop_at(h.t_out).max(0.1) * h.import_eur_kwh;
        count += 1;
    }
    (eur, count)
}

/// One room's projection. Costs are per +1 K on the target and per year; positive costs more,
/// negative saves.
#[derive(Debug, Clone, Serialize)]
pub struct RoomProjection {
    pub zone: String,
    pub heat_cost_per_k_eur_per_year: Option<f64>,
    pub heat_cost_per_k_czk_per_year: Option<f64>,
    /// Hours of the window in which the room needs heat at its current target.
    pub heating_hours: Option<usize>,
    /// Why the heat figures are null (the dashboard shows it instead of the preview).
    pub reason: Option<String>,
    /// Slab-injection conductances (W/K).
    pub g_room_w_per_k: Option<f64>,
    pub g_house_w_per_k: Option<f64>,
    /// Raising the COOL target 1 K saves cooling energy, so this is negative. Null unless an hvac
    /// unit serves the room.
    pub cool_cost_per_k_eur_per_year: Option<f64>,
    pub cool_cost_per_k_czk_per_year: Option<f64>,
    pub cooling_hours: Option<usize>,
}

/// The cached projection served by `GET /api/comfort`.
#[derive(Debug, Clone, Serialize)]
pub struct ComfortProjection {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub coverage_days: f64,
    pub partial: bool,
    pub method: &'static str,
    pub note: &'static str,
    /// CZK per EUR (the tariff's rate), for the dashboard's CZK preview.
    pub eur_czk_rate: f64,
    /// Keyed by zone.
    pub rooms: std::collections::BTreeMap<String, RoomProjection>,
}

/// Zones that actually heat: equipment with power and a heating marker in the model.
fn heated_zones(config: &ControlConfig, net: &RcNetwork) -> Vec<String> {
    let mut zones: Vec<String> = config
        .heating
        .zones
        .iter()
        .filter(|(zone, z)| {
            z.max_heat_kw > 0.0
                && net
                    .marker_indices
                    .contains_key(&((*zone).clone(), HEATING_MARKER.to_string()))
        })
        .map(|(zone, _)| zone.clone())
        .collect();
    zones.sort();
    zones
}

/// Per-room projection from matched hours already in memory (no IO).
pub fn project_rooms(
    config: &ControlConfig,
    net: &RcNetwork,
    hours: &[HourInput],
    scale_to_year: f64,
) -> Vec<RoomProjection> {
    let heated = heated_zones(config, net);
    let t_ground = config.site.ground_temperature_c;
    let hvac = config.hvac.as_ref();
    let served: HashSet<String> = hvac
        .map(|h| h.served_zones().into_iter().collect())
        .unwrap_or_default();
    let cop = config.heating.cop.max(0.1);
    let mut zones: Vec<&String> = config.heating.comfort.rooms.keys().collect();
    zones.sort();
    zones
        .into_iter()
        .map(|zone| {
            let room = &config.heating.comfort.rooms[zone];
            let mut p = RoomProjection {
                zone: zone.clone(),
                heat_cost_per_k_eur_per_year: None,
                heat_cost_per_k_czk_per_year: None,
                heating_hours: None,
                reason: None,
                g_room_w_per_k: None,
                g_house_w_per_k: None,
                cool_cost_per_k_eur_per_year: None,
                cool_cost_per_k_czk_per_year: None,
                cooling_hours: None,
            };
            if !heated.contains(zone) {
                // Shown verbatim in the Czech dashboard.
                p.reason = Some(match config.heating.zones.get(zone) {
                    None => "místnost nemá topení".to_string(),
                    Some(z) if z.max_heat_kw <= 0.0 => {
                        "místnost nemá topení (max_heat_kw 0)".to_string()
                    }
                    Some(_) => "model nemá pro místnost podlahové topení".to_string(),
                });
            } else {
                match steady_state_conductance(net, zone, &heated, Inject::Slab) {
                    Ok(c) => {
                        let gain = config.heating.zones[zone].internal_gain_w.max(0.0);
                        let (eur, n) = hourly_sum(hours, &c, room.heat.target, t_ground, gain, cop);
                        let eur = eur * scale_to_year;
                        p.g_room_w_per_k = Some(c.g_room);
                        p.g_house_w_per_k = Some(c.g_house);
                        p.heating_hours = Some(n);
                        p.heat_cost_per_k_eur_per_year = Some(eur);
                        p.heat_cost_per_k_czk_per_year = Some(config.tariff.eur_to_czk(eur));
                    }
                    Err(e) => p.reason = Some(format!("výpočet tepelné ztráty selhal: {e:#}")),
                }
            }
            if let (true, Some(cool), Some(hvac)) =
                (served.contains(zone), room.cool.as_ref(), hvac)
            {
                let unit = hvac.units.values().find(|u| u.zones.contains(zone));
                if let (Some(unit), Ok(c)) = (
                    unit,
                    steady_state_conductance(net, zone, &heated, Inject::Air),
                ) {
                    let (eur, n) =
                        cooling_sum(hours, &c, cool.target, |t| unit.cooling_cop.cop_at(t));
                    let eur = -eur * scale_to_year;
                    p.cooling_hours = Some(n);
                    p.cool_cost_per_k_eur_per_year = Some(eur);
                    p.cool_cost_per_k_czk_per_year = Some(config.tariff.eur_to_czk(eur));
                }
            }
            p
        })
        .collect()
}

/// Join measured hourly outside temperatures (stamped at the window's stop) with the real import
/// price of the same hour. Hours missing either side are skipped.
pub fn match_hours(
    temps: &[crate::influxdb::TimeSample],
    prices: &[crate::influxdb::PriceSample],
    config: &ControlConfig,
) -> Vec<HourInput> {
    let mask = config.tariff.low_tariff_mask();
    let mut temp_by_hour: HashMap<i64, f64> = HashMap::new();
    for t in temps {
        // Flux stamps the hourly mean at the window's stop, so it belongs to the hour before.
        temp_by_hour.entry(hour_key(t.time) - 1).or_insert(t.value);
    }
    let mut price_by_hour: HashMap<i64, (f64, usize)> = HashMap::new();
    for p in prices {
        let e = price_by_hour.entry(hour_key(p.time)).or_default();
        e.0 += p.price_eur_mwh;
        e.1 += 1;
    }
    let mut keys: Vec<i64> = temp_by_hour.keys().copied().collect();
    keys.sort_unstable();
    keys.into_iter()
        .filter_map(|k| {
            let (sum, n) = *price_by_hour.get(&k)?;
            let hour_start = DateTime::from_timestamp(k * 3600, 0)?;
            let local_hour = chrono::Timelike::hour(
                &hour_start.with_timezone(&config.site.offset_at(hour_start)),
            );
            Some(HourInput {
                t_out: temp_by_hour[&k],
                import_eur_kwh: sum / n as f64 / 1000.0
                    + config.tariff.distribution_eur(local_hour, &mask),
            })
        })
        .collect()
}

/// Read the last [`WINDOW_DAYS`] of outside temperature and prices (bounded 7-day chunks, paused)
/// and project every room. Off the planning path only.
pub async fn yearly_projection(
    db: &SourceClients,
    config: &ControlConfig,
    net: &RcNetwork,
) -> Result<ComfortProjection> {
    let to = Utc::now();
    let from = to - Duration::days(WINDOW_DAYS);
    let windows = chunk_windows(from, to);
    let n = windows.len();
    let mut temps = Vec::new();
    for (i, (s, e)) in windows.into_iter().enumerate() {
        temps.extend(
            db.read_zone_temperature_series("outside", &s.to_rfc3339(), &e.to_rfc3339(), "1h")
                .await
                .with_context(|| format!("reading outside temperature {s}..{e}"))?,
        );
        if i + 1 < n {
            tokio::time::sleep(std::time::Duration::from_secs(INTER_DAY_PAUSE_S)).await;
        }
    }
    let prices = read_prices_chunked(db, from, to).await?;
    let hours = match_hours(&temps, &prices, config);
    let coverage_days = hours.len() as f64 / 24.0;
    anyhow::ensure!(
        !hours.is_empty(),
        "no hour has both an outside temperature and a price"
    );
    let scale = WINDOW_DAYS as f64 / coverage_days;
    let eur_czk_rate = config.tariff.eur_czk_rate;
    let (config, net) = (config.clone(), net.clone());
    let rooms = tokio::task::spawn_blocking(move || project_rooms(&config, &net, &hours, scale))
        .await
        .context("projection task")?;
    Ok(ComfortProjection {
        from,
        to,
        coverage_days,
        partial: coverage_days < PARTIAL_BELOW_DAYS,
        method: METHOD,
        note: NOTE,
        eur_czk_rate,
        rooms: rooms.into_iter().map(|r| (r.zone.clone(), r)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    fn load() -> (ControlConfig, RcNetwork) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config = ControlConfig::load(root.join("config.json5")).unwrap();
        let model = Model::load(root.join("model.json5")).unwrap();
        (config, RcNetwork::from(&model))
    }

    #[test]
    fn hourly_sum_counts_only_heating_hours_at_their_price() {
        let c = Conductances {
            g_room: 20.0,
            g_out: 6.0,
            g_gnd: 2.0,
            g_house: 8.0,
        };
        // target 20, ground 10: demand = 6*(20-t) + 2*10 - gain(0)
        let hours = [
            HourInput {
                t_out: 0.0,
                import_eur_kwh: 0.2,
            }, // 120+20 > 0
            HourInput {
                t_out: 19.0,
                import_eur_kwh: 0.5,
            }, // 6+20 > 0
            HourInput {
                t_out: 25.0,
                import_eur_kwh: 9.0,
            }, // -30+20 < 0
        ];
        let (eur, n) = hourly_sum(&hours, &c, 20.0, 10.0, 0.0, 1.0);
        assert_eq!(n, 2);
        assert!((eur - (8.0 / 1000.0 * 0.2 + 8.0 / 1000.0 * 0.5)).abs() < 1e-12);
        // an internal gain big enough to cover the loss removes the hour
        let (_, n) = hourly_sum(&hours, &c, 20.0, 10.0, 30.0, 1.0);
        assert_eq!(n, 1);
        // COP divides the electricity
        let (eur2, _) = hourly_sum(&hours, &c, 20.0, 10.0, 0.0, 2.0);
        assert!((eur2 - eur / 2.0).abs() < 1e-12);
    }

    #[test]
    fn cooling_sum_uses_hot_hours_and_cop() {
        let c = Conductances {
            g_room: 20.0,
            g_out: 6.0,
            g_gnd: 2.0,
            g_house: 8.0,
        };
        let hours = [
            HourInput {
                t_out: 30.0,
                import_eur_kwh: 0.1,
            },
            HourInput {
                t_out: 20.0,
                import_eur_kwh: 0.1,
            },
        ];
        let (eur, n) = cooling_sum(&hours, &c, 25.0, |_| 4.0);
        assert_eq!(n, 1);
        assert!((eur - 8.0 / 1000.0 / 4.0 * 0.1).abs() < 1e-12);
    }

    #[test]
    fn real_house_g_house_never_exceeds_g_room() {
        let (config, net) = load();
        let heated = heated_zones(&config, &net);
        assert!(heated.len() > 5);
        println!(
            "{:<22} {:>9} {:>9} {:>8} {:>8}",
            "zone", "g_room", "g_house", "g_out", "g_gnd"
        );
        for zone in &heated {
            let c = steady_state_conductance(&net, zone, &heated, Inject::Slab)
                .unwrap_or_else(|e| panic!("{zone}: {e:#}"));
            println!(
                "{zone:<22} {:>9.2} {:>9.2} {:>8.2} {:>8.2}",
                c.g_room, c.g_house, c.g_out, c.g_gnd
            );
            for v in [c.g_room, c.g_house, c.g_out, c.g_gnd] {
                assert!(v.is_finite() && v >= 0.0, "{zone}: {c:?}");
            }
            assert!(c.g_room > 0.0 && c.g_house > 0.0, "{zone}: {c:?}");
            assert!(c.g_house <= c.g_room * (1.0 + 1e-9), "{zone}: {c:?}");
        }
    }

    #[test]
    fn projection_of_synthetic_year_is_sane_and_storage_is_null() {
        let (config, net) = load();
        // A mild synthetic year: sinusoid around 9 degC, flat price.
        let hours: Vec<HourInput> = (0..365 * 24)
            .map(|h| HourInput {
                t_out: 9.0 - 11.0 * (h as f64 / (365.0 * 24.0) * std::f64::consts::TAU).cos(),
                import_eur_kwh: 0.2,
            })
            .collect();
        let rooms = project_rooms(&config, &net, &hours, 1.0);
        println!("{:<22} {:>10} {:>8}", "zone", "EUR/K/yr", "hours");
        for r in &rooms {
            println!(
                "{:<22} {:>10} {:>8}",
                r.zone,
                r.heat_cost_per_k_eur_per_year
                    .map_or("null".into(), |v| format!("{v:.2}")),
                r.heating_hours.map_or("-".into(), |v| v.to_string())
            );
            if let Some(v) = r.heat_cost_per_k_eur_per_year {
                assert!(v.is_finite() && v >= 0.0, "{}", r.zone);
            }
        }
        let storage = rooms.iter().find(|r| r.zone == "storage").unwrap();
        assert!(storage.heat_cost_per_k_eur_per_year.is_none());
        assert_eq!(
            storage.reason.as_deref(),
            Some("místnost nemá topení (max_heat_kw 0)")
        );
        let living = rooms.iter().find(|r| r.zone == "livingroom").unwrap();
        assert!(living.heat_cost_per_k_eur_per_year.unwrap() > 0.0);
        // No hvac units today: cooling is null everywhere.
        assert!(rooms
            .iter()
            .all(|r| r.cool_cost_per_k_eur_per_year.is_none()));
    }

    #[test]
    fn serialized_projection_has_the_dashboard_contract_shape() {
        let (config, net) = load();
        let hours: Vec<HourInput> = (0..24 * 30)
            .map(|h| HourInput {
                t_out: 5.0 + (h % 24) as f64 * 0.2,
                import_eur_kwh: 0.2,
            })
            .collect();
        let rooms = project_rooms(&config, &net, &hours, 12.0);
        let p = ComfortProjection {
            from: Utc::now() - Duration::days(365),
            to: Utc::now(),
            coverage_days: 365.0,
            partial: false,
            method: METHOD,
            note: NOTE,
            eur_czk_rate: config.tariff.eur_czk_rate,
            rooms: rooms.into_iter().map(|r| (r.zone.clone(), r)).collect(),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v["rooms"].is_object());
        assert!(v["eur_czk_rate"].as_f64().unwrap() > 0.0);
        let bath = &v["rooms"]["ground_bathroom"];
        assert!(bath["heat_cost_per_k_eur_per_year"].as_f64().unwrap() > 0.0);
        assert!(bath["cool_cost_per_k_eur_per_year"].is_null());
        assert!(bath["reason"].is_null() && bath["heating_hours"].as_u64().unwrap() > 0);
        assert!(v["rooms"]["storage"]["reason"].is_string());
        if let Ok(out) = std::env::var("COMFORT_SHAPE_OUT") {
            let room = crate::web::comfort_room_entry(&config, "ground_bathroom").unwrap();
            let body = serde_json::json!({
                "config_fingerprint": "x", "rooms": [room], "projection": v,
                "projection_status": "ready", "projection_error": null
            });
            std::fs::write(out, body.to_string()).unwrap();
        }
    }

    #[test]
    fn match_hours_joins_stop_stamped_temps_with_hour_prices() {
        let (config, _) = load();
        let t0 = DateTime::from_timestamp(1_700_000_000 - 1_700_000_000 % 3600, 0).unwrap();
        // temp stamped at t0+1h belongs to the hour starting t0
        let temps = vec![crate::influxdb::TimeSample {
            time: t0 + Duration::hours(1),
            value: 5.0,
        }];
        let prices = vec![
            crate::influxdb::PriceSample {
                time: t0,
                price_eur_mwh: 100.0,
            },
            crate::influxdb::PriceSample {
                time: t0 + Duration::minutes(15),
                price_eur_mwh: 200.0,
            },
        ];
        let h = match_hours(&temps, &prices, &config);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].t_out, 5.0);
        assert!(h[0].import_eur_kwh > 0.15);
        assert!(match_hours(&temps, &[], &config).is_empty());
    }
}
