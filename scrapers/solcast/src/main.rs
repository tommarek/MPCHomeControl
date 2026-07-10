//! Solcast → InfluxDB scraper.
//!
//! Fetches the Solcast rooftop forecast (30-min `pv_estimate`/`pv_estimate10`/`pv_estimate90`
//! records, kW, period-ending) and writes one snapshot point per covered `forecast_date` into the
//! house's `solar_forecast_history` series — the exact contract the MPC brain reads
//! (`src/solar_forecast.rs`): string fields `hourly_json` (+ `_p10`/`_p90`) keyed by LOCAL
//! **hour-ending** hour-of-day, tag `forecast_date`, field `source = "solcast"`, all on the same
//! point timestamp (the brain joins p10 to p50 by `(forecast_date, _time)`).
//!
//! **This scraper REPLACES the loxone Solcast fetcher** — the free tier is ~9 API calls/day and
//! only one owner can spend that budget (see the cut-over checklist in `docs/scrapers.md`). The
//! loxone `model+api` fallback writer keeps running: the brain's snapshot pick automatically
//! prefers `solcast`-sourced snapshots when present, so the two coexist.
//!
//! Budget guard: a **cron-like local-hour schedule** (`fetch_hours_local`), not an interval — a
//! restart mid-day cannot over-spend. Config load rejects `hours × sites > 9`; before each fetch
//! the scraper reads its own newest `source="solcast"` point and skips the slot if it is
//! <45 min old (non-fatal on read failure — availability beats a rare double call).
//!
//! Secrets: `SOLCAST_API_KEY` + `INFLUXDB_TOKEN` env only — never in the config file, never
//! logged. `--once` fetches immediately regardless of the schedule; `--dry-run` prints the lines.

use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

/// Skip a scheduled fetch when our own newest solcast snapshot is younger than this — the
/// restart-in-the-same-hour double-fire guard.
const FRESH_SKIP_MINUTES: i64 = 45;
/// The free-tier daily API budget this scraper must own alone.
const DAILY_BUDGET: usize = 9;

#[derive(Debug, Deserialize)]
struct Config {
    influx: InfluxConfig,
    solcast: SolcastConfig,
    /// IANA timezone for the local-hour curve keys and the fetch schedule.
    timezone: String,
}

#[derive(Debug, Deserialize)]
struct InfluxConfig {
    url: String,
    org: String,
    #[serde(default = "default_bucket")]
    bucket: String,
}

#[derive(Debug, Deserialize)]
struct SolcastConfig {
    /// Rooftop site ids; several arrays are fetched separately and their kW summed per period.
    site_ids: Vec<String>,
    /// Forecast horizon to request (hours).
    #[serde(default = "default_hours")]
    hours: u32,
    /// Local hours at which to fetch (cron-like; at most once per listed hour). Total calls/day =
    /// `fetch_hours_local.len() × site_ids.len()` and must stay ≤ the free-tier budget.
    fetch_hours_local: Vec<u32>,
    #[serde(default = "default_api_url")]
    api_url: String,
}

fn default_bucket() -> String {
    "solar".to_string()
}
fn default_hours() -> u32 {
    72
}
fn default_api_url() -> String {
    "https://api.solcast.com.au".to_string()
}

/// HTTP agent with explicit timeouts — ureq 2 sets NONE by default; a half-open socket in the
/// fetch, the freshness-guard query or the write would otherwise wedge the schedule loop forever
/// (process alive ⇒ no Docker restart, snapshots silently stop).
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .timeout_write(std::time::Duration::from_secs(30))
        .build()
}

/// One Solcast 30-min forecast record (kW, period-ending).
#[derive(Debug, Clone, Deserialize)]
struct SolcastRecord {
    pv_estimate: Option<f64>,
    pv_estimate10: Option<f64>,
    pv_estimate90: Option<f64>,
    period_end: DateTime<Utc>,
}

/// One day's hourly curves (local hour-ending keys → kW). The p10/p90 maps are emitted only when
/// EVERY contributing record carried the percentile — the brain's `p10_complete` logic must never
/// see a percentile curve silently mixing in point estimates.
#[derive(Debug, Default, PartialEq)]
struct DayCurves {
    p50: BTreeMap<u32, f64>,
    p10: Option<BTreeMap<u32, f64>>,
    p90: Option<BTreeMap<u32, f64>>,
}

/// Sum the per-site record lists per `period_end`, aggregate 30-min halves into local
/// **hour-ending** hourly means, and split per local `forecast_date`. Honest nulls: a record with
/// `pv_estimate: null` is skipped entirely; an hour keeps the mean of the halves present (1 or 2).
/// The hour ending exactly at local midnight keys as `(next date, hour 0)` — mirroring the brain's
/// reader. Pure.
fn hourly_curves(
    records_per_site: &[Vec<SolcastRecord>],
    tz: Tz,
) -> BTreeMap<NaiveDate, DayCurves> {
    // Sum kW across sites per period_end; a percentile survives only if every contributing
    // record has it (per period).
    #[derive(Default, Clone)]
    struct Period {
        p50: f64,
        p10: Option<f64>,
        p90: Option<f64>,
        first: bool,
    }
    let mut periods: BTreeMap<DateTime<Utc>, Period> = BTreeMap::new();
    for site in records_per_site {
        for rec in site {
            let Some(p50) = rec.pv_estimate else {
                continue; // null estimate: unknown, never 0-coerced
            };
            let e = periods.entry(rec.period_end).or_default();
            e.p50 += p50;
            e.p10 = match (e.first, e.p10, rec.pv_estimate10) {
                (false, _, v) => v, // first contributor sets it
                (true, Some(a), Some(b)) => Some(a + b),
                _ => None, // any contributor missing it drops it
            };
            e.p90 = match (e.first, e.p90, rec.pv_estimate90) {
                (false, _, v) => v,
                (true, Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
            e.first = true;
        }
    }

    // Group the halves by the local hour they END in: the half-hours ending at H−30min and H
    // belong to the hour ending at H (Solcast's own convention).
    #[derive(Default)]
    struct HourAcc {
        p50_sum: f64,
        p10_sum: Option<f64>,
        p90_sum: Option<f64>,
        n: usize,
        p10_complete: bool,
        p90_complete: bool,
    }
    let mut hours: BTreeMap<(NaiveDate, u32), HourAcc> = BTreeMap::new();
    for (end, p) in &periods {
        // The hour this half belongs to ends at `end` rounded UP to the hour boundary.
        let hour_end = if end.minute() == 0 && end.second() == 0 {
            *end
        } else {
            let floored = end
                .with_minute(0)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(*end);
            floored + Duration::hours(1)
        };
        let local = hour_end.with_timezone(&tz);
        let key = (local.date_naive(), local.hour());
        let acc = hours.entry(key).or_insert_with(|| HourAcc {
            p10_complete: true,
            p90_complete: true,
            ..Default::default()
        });
        acc.p50_sum += p.p50;
        acc.n += 1;
        match p.p10 {
            Some(v) => *acc.p10_sum.get_or_insert(0.0) += v,
            None => acc.p10_complete = false,
        }
        match p.p90 {
            Some(v) => *acc.p90_sum.get_or_insert(0.0) += v,
            None => acc.p90_complete = false,
        }
    }

    let mut days: BTreeMap<NaiveDate, DayCurves> = BTreeMap::new();
    // First pass: p50 always; track per-day percentile completeness.
    let mut day_p10_ok: BTreeMap<NaiveDate, bool> = BTreeMap::new();
    let mut day_p90_ok: BTreeMap<NaiveDate, bool> = BTreeMap::new();
    for ((date, hour), acc) in &hours {
        let day = days.entry(*date).or_default();
        day.p50.insert(*hour, acc.p50_sum / acc.n as f64);
        *day_p10_ok.entry(*date).or_insert(true) &= acc.p10_complete;
        *day_p90_ok.entry(*date).or_insert(true) &= acc.p90_complete;
    }
    // Second pass: percentile curves only for fully-covered days.
    for ((date, hour), acc) in &hours {
        let day = days.get_mut(date).unwrap();
        if day_p10_ok.get(date).copied().unwrap_or(false) {
            if let Some(sum) = acc.p10_sum {
                day.p10
                    .get_or_insert_with(BTreeMap::new)
                    .insert(*hour, sum / acc.n as f64);
            }
        }
        if day_p90_ok.get(date).copied().unwrap_or(false) {
            if let Some(sum) = acc.p90_sum {
                day.p90
                    .get_or_insert_with(BTreeMap::new)
                    .insert(*hour, sum / acc.n as f64);
            }
        }
    }
    days
}

/// A curve as the `hourly_json` blob the brain parses (`{"13":6.2,...}`, keys = local hour).
fn to_blob(curve: &BTreeMap<u32, f64>) -> String {
    let inner = curve
        .iter()
        .map(|(h, kw)| format!("\"{h}\":{kw}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
}

/// Escape a line-protocol STRING FIELD value (`\` and `"`).
fn escape_string_field(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// One snapshot point per forecast date, all at the same snapshot timestamp `ts` (the brain joins
/// p10 to p50 by `(forecast_date, _time)`).
fn to_lines(days: &BTreeMap<NaiveDate, DayCurves>, ts: i64) -> String {
    days.iter()
        .filter(|(_, d)| !d.p50.is_empty())
        .map(|(date, d)| {
            let mut fields = vec![
                format!("hourly_json=\"{}\"", escape_string_field(&to_blob(&d.p50))),
                "source=\"solcast\"".to_string(),
            ];
            if let Some(p10) = &d.p10 {
                fields.push(format!(
                    "hourly_json_p10=\"{}\"",
                    escape_string_field(&to_blob(p10))
                ));
            }
            if let Some(p90) = &d.p90 {
                fields.push(format!(
                    "hourly_json_p90=\"{}\"",
                    escape_string_field(&to_blob(p90))
                ));
            }
            format!(
                "solar_forecast_history,forecast_date={date} {} {ts}",
                fields.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Seconds until the next scheduled local fetch hour (top of the hour), wrapping past midnight.
fn next_slot_seconds(now_local_secs_of_day: i64, fetch_hours: &[u32]) -> i64 {
    let mut best = i64::MAX;
    for &h in fetch_hours {
        let slot = h as i64 * 3600;
        let delta = slot - now_local_secs_of_day;
        let delta = if delta > 0 { delta } else { delta + 86_400 };
        best = best.min(delta);
    }
    best
}

fn validate(config: &Config) -> Result<Tz> {
    ensure!(
        !config.solcast.site_ids.is_empty(),
        "solcast.site_ids must not be empty"
    );
    ensure!(
        !config.solcast.fetch_hours_local.is_empty(),
        "solcast.fetch_hours_local must not be empty"
    );
    ensure!(
        config.solcast.fetch_hours_local.iter().all(|&h| h < 24),
        "solcast.fetch_hours_local entries must be 0..=23"
    );
    let calls = config.solcast.fetch_hours_local.len() * config.solcast.site_ids.len();
    ensure!(
        calls <= DAILY_BUDGET,
        "fetch_hours_local ({}) × site_ids ({}) = {calls} API calls/day exceeds the free-tier \
         budget of {DAILY_BUDGET} — this scraper must be the sole budget owner",
        config.solcast.fetch_hours_local.len(),
        config.solcast.site_ids.len()
    );
    config
        .timezone
        .parse::<Tz>()
        .map_err(|e| anyhow::anyhow!("timezone {:?}: {e}", config.timezone))
}

/// Fetch one site's forecast records.
fn fetch_site(config: &Config, api_key: &str, site: &str) -> Result<Vec<SolcastRecord>> {
    #[derive(Deserialize)]
    struct Response {
        forecasts: Vec<SolcastRecord>,
    }
    let url = format!(
        "{}/rooftop_sites/{site}/forecasts",
        config.solcast.api_url.trim_end_matches('/')
    );
    let resp = http_agent()
        .get(&url)
        .query("format", "json")
        .query("hours", &config.solcast.hours.to_string())
        .set("Authorization", &format!("Bearer {api_key}"))
        .call();
    match resp {
        Ok(r) => Ok(r
            .into_json::<Response>()
            .context("solcast response is not the expected JSON")?
            .forecasts),
        Err(ureq::Error::Status(429, _)) => {
            anyhow::bail!("solcast rate-limited (429) — skipping this slot")
        }
        Err(e) => Err(e).context("solcast request failed"),
    }
}

/// The newest snapshot timestamp we ourselves wrote (`source="solcast"`), for the double-fire
/// guard. Best-effort: any failure reads as "stale" (fetch anyway).
fn newest_own_snapshot(config: &Config, token: &str) -> Option<DateTime<Utc>> {
    let flux = format!(
        r#"from(bucket:"{}") |> range(start: -2h)
           |> filter(fn:(r)=> r._measurement=="solar_forecast_history" and r._field=="source")
           |> filter(fn:(r)=> r._value=="solcast")
           |> keep(columns:["_time"]) |> sort(columns:["_time"]) |> last(column:"_time")"#,
        config.influx.bucket
    );
    let url = format!(
        "{}/api/v2/query?org={}",
        config.influx.url.trim_end_matches('/'),
        config.influx.org
    );
    let body = http_agent()
        .post(&url)
        .set("Authorization", &format!("Token {token}"))
        .set("Content-Type", "application/vnd.flux")
        .set("Accept", "application/csv")
        .send_string(&flux)
        .ok()?
        .into_string()
        .ok()?;
    // Annotated CSV: find the _time column of the first data row.
    let mut lines = body
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty());
    let header: Vec<&str> = lines.next()?.split(',').collect();
    let idx = header.iter().position(|c| c.trim() == "_time")?;
    let row: Vec<&str> = lines.next()?.split(',').collect();
    DateTime::parse_from_rfc3339(row.get(idx)?.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn write_influx(config: &Config, token: &str, body: &str) -> Result<()> {
    let url = format!(
        "{}/api/v2/write?org={}&bucket={}&precision=s",
        config.influx.url.trim_end_matches('/'),
        config.influx.org,
        config.influx.bucket
    );
    http_agent()
        .post(&url)
        .set("Authorization", &format!("Token {token}"))
        .set("Content-Type", "text/plain; charset=utf-8")
        .send_string(body)
        .context("influx write failed")?;
    Ok(())
}

fn scrape(config: &Config, api_key: &str, tz: Tz) -> Result<String> {
    let records: Vec<Vec<SolcastRecord>> = config
        .solcast
        .site_ids
        .iter()
        .map(|site| fetch_site(config, api_key, site))
        .collect::<Result<_>>()?;
    let days = hourly_curves(&records, tz);
    ensure!(!days.is_empty(), "solcast returned no forecast periods");
    Ok(to_lines(&days, Utc::now().timestamp()))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "scraper.json5".to_string());
    let once = args.iter().any(|a| a == "--once");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let config: Config = json5::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
    )
    .with_context(|| format!("parsing {path}"))?;
    let tz = validate(&config)?;
    let api_key = std::env::var("SOLCAST_API_KEY").context("SOLCAST_API_KEY not set")?;
    let token = if dry_run {
        String::new()
    } else {
        std::env::var("INFLUXDB_TOKEN").context("INFLUXDB_TOKEN not set")?
    };

    loop {
        if !once {
            // Sleep to the next scheduled local hour (cron-like; restart-safe on the budget).
            let now_local = Utc::now().with_timezone(&tz);
            let secs_of_day = now_local.num_seconds_from_midnight() as i64;
            let wait = next_slot_seconds(secs_of_day, &config.solcast.fetch_hours_local);
            eprintln!("[solcast] next fetch in {} min", wait / 60);
            std::thread::sleep(std::time::Duration::from_secs(wait as u64));
            // Double-fire guard: another instance (or a pre-restart self) already fetched.
            if !dry_run {
                if let Some(newest) = newest_own_snapshot(&config, &token) {
                    if Utc::now() - newest < Duration::minutes(FRESH_SKIP_MINUTES) {
                        eprintln!("[solcast] fresh snapshot exists ({newest}); skipping the slot");
                        continue;
                    }
                }
            }
        }
        match scrape(&config, &api_key, tz) {
            Ok(body) => {
                let dates = body.lines().count();
                if dry_run {
                    println!("{body}");
                    eprintln!("[solcast] dry-run: {dates} forecast-date snapshot(s), not written");
                } else {
                    match write_influx(&config, &token, &body) {
                        Ok(()) => eprintln!(
                            "[solcast] wrote {dates} forecast-date snapshot(s) to {}",
                            config.influx.bucket
                        ),
                        Err(e) => eprintln!("[solcast] {e:#}"),
                    }
                }
            }
            Err(e) => eprintln!("[solcast] {e:#}"),
        }
        if once {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(end: &str, p50: f64, p10: Option<f64>, p90: Option<f64>) -> SolcastRecord {
        SolcastRecord {
            pv_estimate: Some(p50),
            pv_estimate10: p10,
            pv_estimate90: p90,
            period_end: DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    fn prague() -> Tz {
        "Europe/Prague".parse().unwrap()
    }

    #[test]
    fn halves_aggregate_to_hour_ending_means() {
        // Prague summer = UTC+2: halves ending 09:30Z and 10:00Z belong to the local hour ending
        // 12:00 local (10:00Z).
        let site = vec![
            rec("2026-07-01T09:30:00Z", 3.0, Some(2.0), Some(4.0)),
            rec("2026-07-01T10:00:00Z", 5.0, Some(4.0), Some(6.0)),
        ];
        let days = hourly_curves(&[site], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()];
        assert_eq!(day.p50[&12], 4.0); // mean of the two halves
        assert_eq!(day.p10.as_ref().unwrap()[&12], 3.0);
        assert_eq!(day.p90.as_ref().unwrap()[&12], 5.0);
    }

    #[test]
    fn single_half_hour_still_emits_the_mean_of_one() {
        let site = vec![rec("2026-07-01T09:30:00Z", 3.0, None, None)];
        let days = hourly_curves(&[site], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()];
        assert_eq!(day.p50[&12], 3.0);
        assert!(day.p10.is_none()); // percentiles absent ⇒ no p10 blob at all
    }

    #[test]
    fn midnight_rolls_to_next_date_hour_zero() {
        // Prague summer: local midnight = 22:00Z. The half-hours ending 21:30Z and 22:00Z form
        // the hour ending 24:00 local July 1 ⇒ keyed as (July 2, hour 0), like the brain's reader.
        let site = vec![
            rec("2026-07-01T21:30:00Z", 0.2, None, None),
            rec("2026-07-01T22:00:00Z", 0.4, None, None),
        ];
        let days = hourly_curves(&[site], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 7, 2).unwrap()];
        assert!((day.p50[&0] - 0.3).abs() < 1e-12);
    }

    #[test]
    fn multi_site_sums_and_missing_period_is_not_zero_coerced() {
        let a = vec![
            rec("2026-07-01T09:30:00Z", 1.0, Some(0.5), None),
            rec("2026-07-01T10:00:00Z", 2.0, Some(1.5), None),
        ];
        // Site b misses the 10:00Z period entirely — that half keeps only site a's value.
        let b = vec![rec("2026-07-01T09:30:00Z", 1.0, Some(0.8), None)];
        let days = hourly_curves(&[a, b], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()];
        assert_eq!(day.p50[&12], 2.0); // (1+1 + 2)/2
        assert_eq!(day.p10.as_ref().unwrap()[&12], (1.3 + 1.5) / 2.0);
    }

    #[test]
    fn null_estimate_is_skipped_and_partial_percentiles_drop_the_day_blob() {
        let site = vec![
            SolcastRecord {
                pv_estimate: None, // null: skipped entirely
                pv_estimate10: Some(1.0),
                pv_estimate90: None,
                period_end: DateTime::parse_from_rfc3339("2026-07-01T09:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
            rec("2026-07-01T10:00:00Z", 2.0, Some(1.5), Some(2.5)),
            rec("2026-07-01T10:30:00Z", 3.0, None, Some(3.5)), // p10 missing here
        ];
        let days = hourly_curves(&[site], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()];
        assert_eq!(day.p50.len(), 2);
        // One record without p10 ⇒ the DAY has no p10 blob (never a mixed curve)...
        assert!(day.p10.is_none());
        // ...but p90 is complete across contributing records and survives.
        assert!(day.p90.is_some());
    }

    #[test]
    fn dst_changeover_day_keys_by_instantaneous_offset() {
        // Prague spring-forward 2026-03-29: 01:00Z is 03:00 local (offset jumps +1→+2 at 01:00Z).
        let site = vec![
            rec("2026-03-29T00:00:00Z", 0.1, None, None), // hour ending 01:00 local (+1)
            rec("2026-03-29T02:00:00Z", 0.2, None, None), // hour ending 04:00 local (+2)
        ];
        let days = hourly_curves(&[site], prague());
        let day = &days[&NaiveDate::from_ymd_opt(2026, 3, 29).unwrap()];
        assert!(day.p50.contains_key(&1));
        assert!(day.p50.contains_key(&4));
        assert!(
            !day.p50.contains_key(&3),
            "02:00Z-03:00 local never exists that day"
        );
    }

    #[test]
    fn line_protocol_matches_the_reader_contract() {
        let mut days = BTreeMap::new();
        days.insert(
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            DayCurves {
                p50: [(12u32, 4.0), (13u32, 5.5)].into_iter().collect(),
                p10: Some([(12u32, 3.0)].into_iter().collect()),
                p90: None,
            },
        );
        let line = to_lines(&days, 1_780_000_000);
        assert_eq!(
            line,
            "solar_forecast_history,forecast_date=2026-07-01 \
             hourly_json=\"{\\\"12\\\":4,\\\"13\\\":5.5}\",source=\"solcast\",\
             hourly_json_p10=\"{\\\"12\\\":3}\" 1780000000"
        );
    }

    #[test]
    fn next_slot_wraps_past_midnight() {
        // 23:30 local, slots at 5 and 12 → next is 05:00 tomorrow (5.5 h).
        assert_eq!(next_slot_seconds(23 * 3600 + 1800, &[5, 12]), 19800);
        // 04:00 local → 05:00 today.
        assert_eq!(next_slot_seconds(4 * 3600, &[5, 12]), 3600);
    }

    #[test]
    fn budget_validation_rejects_over_budget_schedules() {
        let cfg = |hours: Vec<u32>, sites: usize| Config {
            influx: InfluxConfig {
                url: "http://x".into(),
                org: "o".into(),
                bucket: default_bucket(),
            },
            solcast: SolcastConfig {
                site_ids: (0..sites).map(|i| format!("s{i}")).collect(),
                hours: 72,
                fetch_hours_local: hours,
                api_url: default_api_url(),
            },
            timezone: "Europe/Prague".into(),
        };
        assert!(validate(&cfg(vec![5, 8, 12, 17], 2)).is_ok()); // 8 ≤ 9
        assert!(validate(&cfg(vec![5, 8, 10, 12, 17], 2)).is_err()); // 10 > 9
        assert!(validate(&cfg(vec![25], 1)).is_err()); // bad hour
        assert!(validate(&cfg(vec![], 1)).is_err());
    }

    #[test]
    fn recorded_fixture_parses() {
        // A trimmed real-shape Solcast response — pins the field names against API drift.
        let fixture = r#"{
            "forecasts": [
                { "pv_estimate": 1.234, "pv_estimate10": 0.9, "pv_estimate90": 1.6,
                  "period_end": "2026-07-01T09:30:00.0000000Z", "period": "PT30M" },
                { "pv_estimate": 0, "pv_estimate10": 0, "pv_estimate90": 0.1,
                  "period_end": "2026-07-01T10:00:00.0000000Z", "period": "PT30M" }
            ]
        }"#;
        #[derive(Deserialize)]
        struct Response {
            forecasts: Vec<SolcastRecord>,
        }
        let r: Response = serde_json::from_str(fixture).unwrap();
        assert_eq!(r.forecasts.len(), 2);
        assert_eq!(r.forecasts[0].pv_estimate, Some(1.234));
        let days = hourly_curves(&[r.forecasts], prague());
        assert_eq!(days.len(), 1);
    }
}
