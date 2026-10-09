//! The "new features" page: a registry of the features shipped 2026-10-01..03 and, per feature, the
//! strongest honest comparison the stored data supports (`GET /api/features`).
//!
//! Everything here is pure: [`build`] takes the ledger rows, the accuracy history, the feature
//! samples, the cached PV-nowcast replay and `now`, and returns the page's payload. Each comparison
//! states what kind of evidence it is ([`Kind`]) — a replay is never presented as measured — and
//! says "not enough data (n=…)" below the sample thresholds in the `MIN_*` constants.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::feature_data::{
    AccuracyHistory, BinStat, FeatureSamples, NightStats, HIST_BINS_H, SAMPLE_RETENTION_DAYS,
    SUNNY_PV_KWH,
};
use crate::ledger::LedgerRow;
use crate::optimize::config::SiteConfig;
use crate::relay_duty::RelayDuty;

// ============================================================ Thresholds

/// Hourly terminal-SoC samples (outlook source) before a verdict is stated.
const MIN_TERMINAL_SAMPLES: usize = 12;
/// Replay samples in the 0-1 h bin before the nowcast verdict is stated.
const MIN_REPLAY_N: usize = 30;
/// Scored points in the 12-24 h bin (both arms) before the solar-scale verdict is stated.
const MIN_AB_POINTS: usize = 60;
/// Night targets after sunny days (both arms) before night bias is quoted in the verdict.
const MIN_NIGHT_POINTS: usize = 20;
/// Days of relay-duty rows before the events-vs-legacy verdict is stated.
const MIN_RELAY_DAYS: usize = 3;
/// Solved (non-skipped) priority-zone A/B plans before the verdict is stated.
const MIN_WARMTH_SAMPLES: usize = 6;
/// An RMSE change smaller than this (K) reads as "no change".
const RMSE_NEUTRAL_K: f64 = 0.01;
/// A nowcast RMSE change smaller than this (kW) reads as "no change".
const RMSE_NEUTRAL_KW: f64 = 0.02;
/// Heat below this (kWh) over a window is "no heat demand".
const NO_HEAT_KWH: f64 = 0.05;
/// A measured block is PV-covered when PV exceeds this (kW)…
const PV_COVER_KW: f64 = 0.1;
/// …and the grid import stays under this (kW).
const PV_COVER_IMPORT_KW: f64 = 0.05;

// ============================================================ Registry

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Live,
    /// Shipped but switched off in the live config (the evidence still accrues).
    Staged,
}

/// What kind of evidence a comparison is. The labels are the page's, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Both arms computed on the same live data (the strongest).
    AbLive,
    /// A counterfactual replay of stored history, plus live usage counts.
    AbReplay,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::AbLive => "A/B live",
            Kind::AbReplay => "A/B replay",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Release {
    pub sha: &'static str,
    /// UTC instant it went live (RFC 3339).
    pub at: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct FeatureInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub releases: &'static [Release],
    pub what: &'static str,
    pub status: Status,
    pub kind: Kind,
    /// The release proof, from the commit message.
    pub proof: &'static str,
    /// The key release-proof numbers, structured.
    pub baseline: &'static [(&'static str, f64)],
}

impl FeatureInfo {
    /// When the (first) release went live.
    pub fn live_since(&self) -> Option<DateTime<Utc>> {
        self.releases.first().and_then(|r| parse_utc(r.at?))
    }
}

fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

const fn rel(sha: &'static str, at: &'static str) -> Release {
    Release { sha, at: Some(at) }
}

pub const FEATURES: &[FeatureInfo] = &[
    FeatureInfo {
        id: "terminal_soc",
        name: "Terminal SoC at post-horizon prices",
        releases: &[rel("825da58", "2026-10-01T05:33:00Z")],
        what: "Leftover battery energy is valued at the day-type-median price AFTER the horizon, not the cheapest in-horizon block.",
        status: Status::Live,
        kind: Kind::AbLive,
        proof: "14-day replay to 2026-09-29: identical realized cost (-28.28 EUR, same dispatch), planned end SoC 3.9 -> 7.3 kWh; with a 12 h horizon 0.20 EUR cheaper over the window. Plan of the day: 0.043 -> 0.251 EUR/kWh.",
        baseline: &[
            ("legacy_eur_kwh", 0.043),
            ("outlook_eur_kwh", 0.251),
            ("end_soc_old_kwh", 3.9),
            ("end_soc_new_kwh", 7.3),
            ("replay_12h_delta_eur_14d", -0.20),
        ],
    },
    FeatureInfo {
        id: "pv_nowcast",
        name: "PV intraday nowcast",
        releases: &[rel("a7604db", "2026-10-01T07:37:00Z")],
        what: "Blends the last hour's measured/forecast PV ratio into the next 3 h of the calibrated Solcast curve.",
        status: Status::Live,
        kind: Kind::AbReplay,
        proof: "Hourly replay of 14 days of snapshots, identical sample sets: 0-1 h RMSE 1.789 -> 1.523 kW, 1-2 h 1.758 -> 1.681, 2-3 h 1.748 -> 1.727.",
        baseline: &[
            ("rmse_plain_0_1h_kw", 1.789),
            ("rmse_nowcast_0_1h_kw", 1.523),
            ("rmse_plain_1_2h_kw", 1.758),
            ("rmse_nowcast_1_2h_kw", 1.681),
            ("rmse_plain_2_3h_kw", 1.748),
            ("rmse_nowcast_2_3h_kw", 1.727),
        ],
    },
    FeatureInfo {
        id: "solar_scale",
        name: "Kalman per-zone solar scale",
        releases: &[rel("78f9c2d", "2026-10-01T09:42:00Z")],
        what: "Each zone's modelled solar gain is scaled by a live Kalman estimate, so a sunny-afternoon error does not leak into the night forecast.",
        status: Status::Live,
        kind: Kind::AbLive,
        proof: "14-day replay, 320 hourly origins, 6-24 h RMSE: entrance 0.56 -> 0.43 K, ground_closet 0.67 -> 0.51, kitchen 0.52 -> 0.42, room_2 0.66 -> 0.56, attic 0.84 -> 0.77; no zone worse by more than 0.02 K.",
        baseline: &[
            ("entrance_before_k", 0.56),
            ("entrance_after_k", 0.43),
            ("ground_closet_before_k", 0.67),
            ("ground_closet_after_k", 0.51),
            ("kitchen_before_k", 0.52),
            ("kitchen_after_k", 0.42),
        ],
    },
    FeatureInfo {
        id: "relay_duty",
        name: "Event-based relay duty",
        releases: &[
            rel("28fa2af", "2026-10-01T21:24:00Z"),
            rel("d083050", "2026-10-01T21:24:00Z"),
            rel("c1c9ffa", "2026-10-03T14:31:00Z"),
        ],
        what: "Reads the on-change heating relay as a true time-weighted duty instead of an hourly mean of edges. Shipped staged on `legacy`; the owner switched it to `events` on 2026-10-03.",
        status: Status::Staged,
        kind: Kind::AbLive,
        proof: "Last 7 days (2026-09-24..10-01): heat counted 13.97 -> 25.30 kWh (+81 %); phantom office 47 W / toilet 27 W gains fall to 0. It stayed on legacy until 2026-10-03 because the model over-responds to floor heat (19-zone post-fit RMSE 0.64 -> 0.69 K with events on); the owner accepted that trade-off.",
        baseline: &[
            ("legacy_kwh_7d", 13.97),
            ("events_kwh_7d", 25.30),
            ("delta_pct", 81.0),
        ],
    },
    FeatureInfo {
        id: "priority_zones",
        name: "Priority zones",
        releases: &[rel("8481684", "2026-10-02T21:41:00Z")],
        what: "A per-zone warmth value (EUR per K-hour above the floor) so cheap energy warms the family rooms toward the ceiling.",
        status: Status::Live,
        kind: Kind::AbLive,
        proof: "7-day replays on last winter's data, half values: +2.32 EUR (cold January week, from 75.50) and +2.26 EUR (December week, from 54.58), livingroom/kitchen ~21.7 C instead of 21.0; all extra heat in night-tariff hours. A late-September week buys no heat.",
        baseline: &[
            ("extra_cost_cold_week_eur", 2.32),
            ("base_cost_cold_week_eur", 75.50),
            ("extra_cost_december_week_eur", 2.26),
            ("base_cost_december_week_eur", 54.58),
        ],
    },
];

// ============================================================ Output shape

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictState {
    Helped,
    Neutral,
    Worse,
    /// A number to read, with no good/bad judgement.
    Info,
    Insufficient,
    /// The feature is configured off or is an offline tool.
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub state: VerdictState,
    pub text: String,
}

impl Verdict {
    fn new(state: VerdictState, text: impl Into<String>) -> Self {
        Self {
            state,
            text: text.into(),
        }
    }

    fn insufficient(n: usize, what: &str) -> Self {
        Self::new(
            VerdictState::Insufficient,
            format!("not enough data (n={n}): {what}"),
        )
    }
}

/// One named line of a [`Chart`]; `null` is a gap.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<Option<f64>>,
}

/// A category-axis chart: `x` labels (usually `YYYY-MM-DD`) and one value per label per series.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chart {
    pub title: String,
    pub y_unit: String,
    pub x: Vec<String>,
    pub series: Vec<ChartSeries>,
}

/// A compact table; cells are numbers, strings or `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Table {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseOut {
    pub sha: &'static str,
    pub at: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaselineOut {
    /// The release proof, as in the commit message.
    pub text: &'static str,
    pub values: BTreeMap<&'static str, f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureReport {
    pub id: &'static str,
    pub name: &'static str,
    pub shas: Vec<&'static str>,
    pub releases: Vec<ReleaseOut>,
    pub live_since: Option<DateTime<Utc>>,
    pub status: Status,
    pub what: &'static str,
    pub kind: Kind,
    pub kind_label: &'static str,
    /// The sample count the verdict rests on (`null` when there is none).
    pub n: Option<usize>,
    pub verdict: Verdict,
    pub baseline: BaselineOut,
    pub charts: Vec<Chart>,
    pub tables: Vec<Table>,
    /// Caveats the reader needs to interpret the comparison honestly.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeaturesReport {
    pub generated_at: DateTime<Utc>,
    pub features: Vec<FeatureReport>,
    /// Features that proved themselves and became the only behaviour: static text, no metrics.
    pub graduated: &'static [Graduated],
}

/// A feature that has left the evaluation page — the name, where it shipped and its final result.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Graduated {
    pub name: &'static str,
    pub sha: &'static str,
    /// Date it went live (`YYYY-MM-DD`); `null` for an offline tool.
    pub date: Option<&'static str>,
    pub result: &'static str,
}

pub const GRADUATED: &[Graduated] = &[
    Graduated {
        name: "PV-gated battery export",
        sha: "112ea92",
        date: Some("2026-10-01"),
        result: "0 dark-block exports in 826 blocks (423 dark) vs 4 of 4 missed before.",
    },
    Graduated {
        name: "Dispatch floor",
        sha: "330be56",
        date: Some("2026-10-01"),
        result: "Unactuated regular-mode dispatch 5.0 -> 0.3 per 100 regular blocks (2/40 -> 1/384).",
    },
    Graduated {
        name: "Model changes (entrance air change, glazing shading)",
        sha: "522adcb, 053892e",
        date: Some("2026-10-03"),
        result: "All-zone 6-24 h forecast RMSE 0.51 -> 0.30 K, entrance 0.86 -> 0.32 K (3 days before / 4 after).",
    },
    Graduated {
        name: "Decision ledger",
        sha: "33f4826",
        date: Some("2026-10-01"),
        result: "Scores every block; 9 days planned -31.08 vs realized -28.50 EUR.",
    },
    Graduated {
        name: "Winter kernel validation CLI",
        sha: "49b8430",
        date: None,
        result: "`backtest-heating`, the standard winter check.",
    },
];

/// One feature's computed comparison, before the registry fields are attached.
#[derive(Debug, Clone)]
struct Computed {
    n: Option<usize>,
    verdict: Verdict,
    charts: Vec<Chart>,
    tables: Vec<Table>,
    notes: Vec<String>,
}

impl Computed {
    fn only(n: Option<usize>, verdict: Verdict) -> Self {
        Self {
            n,
            verdict,
            charts: Vec::new(),
            tables: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// The cached `/api/pv/backtest` nowcast replay, as the endpoint serialized it.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplayScoreView {
    pub rmse_kw: f64,
    pub bias_kw: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplayBinView {
    pub lead_from_h: f64,
    pub lead_to_h: f64,
    pub n: usize,
    pub plain: ReplayScoreView,
    pub nowcast: ReplayScoreView,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NowcastReplayView {
    pub all: Vec<ReplayBinView>,
    pub n_ref: usize,
    pub n_ref_applied: usize,
}

impl NowcastReplayView {
    /// Pull the `nowcast` object out of a (cached) `/api/pv/backtest` payload.
    pub fn from_backtest(value: &Value) -> Option<Self> {
        serde_json::from_value(value.get("nowcast")?.clone()).ok()
    }
}

/// Everything [`build`] reads.
pub struct FeaturesInput<'a> {
    pub now: DateTime<Utc>,
    pub ledger_rows: &'a [LedgerRow],
    pub history: &'a AccuracyHistory,
    pub samples: &'a FeatureSamples,
    pub pv_replay: Option<&'a NowcastReplayView>,
    /// Per-local-hour low-tariff (NT) mask.
    pub low_tariff_mask: [bool; 24],
    pub site: &'a SiteConfig,
    /// Zones with a positive `warmth_value_eur_per_kh`.
    pub priority_zones: &'a [String],
    pub relay_duty_mode: RelayDuty,
}

/// A feature's status as deployed: the relay-duty feature is staged only while the config still reads
/// `legacy` — its registry status is the shipped default, the config decides what is live.
fn effective_status(info: &FeatureInfo, input: &FeaturesInput) -> Status {
    match (info.id, input.relay_duty_mode) {
        ("relay_duty", RelayDuty::Events) => Status::Live,
        _ => info.status,
    }
}

/// Build the whole page.
pub fn build(input: &FeaturesInput) -> FeaturesReport {
    let features = FEATURES
        .iter()
        .map(|info| {
            let computed = match info.id {
                "terminal_soc" => terminal_soc(input),
                "pv_nowcast" => pv_nowcast(input),
                "solar_scale" => solar_scale(input),
                "relay_duty" => relay_duty(input),
                "priority_zones" => priority_zones(info, input),
                _ => Computed::only(
                    None,
                    Verdict::insufficient(0, "no comparison is implemented for this feature"),
                ),
            };
            FeatureReport {
                id: info.id,
                name: info.name,
                shas: info.releases.iter().map(|r| r.sha).collect(),
                releases: info
                    .releases
                    .iter()
                    .map(|r| ReleaseOut {
                        sha: r.sha,
                        at: r.at,
                    })
                    .collect(),
                live_since: info.live_since(),
                status: effective_status(info, input),
                what: info.what,
                kind: info.kind,
                kind_label: info.kind.label(),
                n: computed.n,
                verdict: computed.verdict,
                baseline: BaselineOut {
                    text: info.proof,
                    values: info.baseline.iter().copied().collect(),
                },
                charts: computed.charts,
                tables: computed.tables,
                notes: computed.notes,
            }
        })
        .collect();
    FeaturesReport {
        generated_at: input.now,
        features,
        graduated: GRADUATED,
    }
}

// ============================================================ Shared helpers

fn day_of(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d").to_string()
}

/// Rows that feed totals: scored, not from a degraded or relaxed plan (the ledger's own rule).
fn eligible(row: &LedgerRow) -> bool {
    row.scored && !row.degraded && !row.relaxed
}

fn round_to(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

fn num(x: f64, places: i32) -> Value {
    json!(round_to(x, places))
}

fn num_opt(x: Option<f64>, places: i32) -> Value {
    x.map_or(Value::Null, |x| num(x, places))
}

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Pooled error statistics of [`BinStat`] cells (`sum = rmse² · n`).
#[derive(Debug, Clone, Copy, Default)]
struct Pool {
    n: usize,
    sq: f64,
    err: f64,
}

impl Pool {
    fn add(&mut self, s: &BinStat) {
        let n = s.n();
        self.n += n;
        self.sq += s.rmse_k() * s.rmse_k() * n as f64;
        self.err += s.bias_k() * n as f64;
    }
    fn rmse(&self) -> Option<f64> {
        (self.n > 0).then(|| (self.sq / self.n as f64).sqrt())
    }
    fn bias(&self) -> Option<f64> {
        (self.n > 0).then(|| self.err / self.n as f64)
    }
}

fn lead_label(bin: usize) -> String {
    let (from, to) = HIST_BINS_H[bin];
    format!("{from:.0}-{to:.0} h")
}

// ============================================================ Terminal SoC

fn terminal_soc(input: &FeaturesInput) -> Computed {
    #[derive(Default)]
    struct Day {
        n: usize,
        outlook: f64,
        legacy: f64,
        soc: f64,
    }
    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    let mut total = Day::default();
    let mut fallbacks = 0usize;
    for s in &input.samples.terminal {
        if s.source != "outlook" {
            fallbacks += 1;
            continue;
        }
        for day in [days.entry(day_of(s.t)).or_default(), &mut total] {
            day.n += 1;
            day.outlook += s.outlook_eur_kwh;
            day.legacy += s.legacy_eur_kwh;
            day.soc += s.end_soc_kwh;
        }
    }
    let mean = |sum: f64, n: usize| (n > 0).then(|| sum / n as f64);
    let verdict = if total.n < MIN_TERMINAL_SAMPLES {
        Verdict::insufficient(total.n, "hourly samples of the two valuations")
    } else {
        Verdict::new(
            VerdictState::Info,
            format!(
                "values leftover battery energy at {:.3} vs {:.3} EUR/kWh before (mean of {} hourly plans); planned end SoC {:.1} kWh. Realized-cost impact is a replay result (identical in September, -0.20 EUR/14 d at a 12 h horizon)",
                total.outlook / total.n as f64,
                total.legacy / total.n as f64,
                total.n,
                total.soc / total.n as f64
            ),
        )
    };
    let x: Vec<String> = days.keys().cloned().collect();
    let charts = vec![
        Chart {
            title: "Leftover-SoC value per kWh: outlook (live) vs legacy in-horizon median".into(),
            y_unit: "EUR/kWh".into(),
            x: x.clone(),
            series: vec![
                ChartSeries {
                    name: "outlook (live)".into(),
                    values: days.values().map(|d| mean(d.outlook, d.n)).collect(),
                },
                ChartSeries {
                    name: "legacy median".into(),
                    values: days.values().map(|d| mean(d.legacy, d.n)).collect(),
                },
            ],
        },
        Chart {
            title: "Planned end-of-horizon battery energy (live plan)".into(),
            y_unit: "kWh".into(),
            x,
            series: vec![ChartSeries {
                name: "end SoC".into(),
                values: days.values().map(|d| mean(d.soc, d.n)).collect(),
            }],
        },
    ];
    let table = Table {
        title: "Daily means of hourly plans".into(),
        columns: cols(&[
            "date",
            "plans",
            "outlook EUR/kWh",
            "legacy EUR/kWh",
            "end SoC kWh",
        ]),
        rows: days
            .iter()
            .map(|(date, d)| {
                vec![
                    json!(date),
                    json!(d.n),
                    num_opt(mean(d.outlook, d.n), 3),
                    num_opt(mean(d.legacy, d.n), 3),
                    num_opt(mean(d.soc, d.n), 1),
                ]
            })
            .collect(),
    };
    let mut notes = vec![
        "Both valuations are computed on the same plan every tick; only the outlook one is used. Whether it paid off in realized cost is not measurable per plan — that is the replay result.".into(),
    ];
    if fallbacks > 0 {
        notes.push(format!(
            "{fallbacks} sample(s) fell back to the legacy value (thin price history) and are not counted."
        ));
    }
    Computed {
        n: Some(total.n),
        verdict,
        charts,
        tables: vec![table],
        notes,
    }
}

// ============================================================ PV nowcast

fn pv_nowcast(input: &FeaturesInput) -> Computed {
    let mut tables = Vec::new();
    let mut notes = vec![
        "The accuracy comparison is a REPLAY of the last 7 days of snapshots (the cached `/api/pv/backtest?days=7`; both arms scored on identical samples), not a live measurement; the per-day table shows how often the nowcast actually fired live.".into(),
    ];
    let verdict;
    let mut n = None;
    match input.pv_replay {
        None => {
            verdict = Verdict::new(
                VerdictState::Insufficient,
                "not enough data: the PV-backtest replay is not available yet",
            );
        }
        Some(replay) => {
            tables.push(Table {
                title: "Replay: plain vs nowcast forecast error by lead".into(),
                columns: cols(&[
                    "lead",
                    "n",
                    "plain RMSE kW",
                    "nowcast RMSE kW",
                    "delta kW",
                    "plain bias kW",
                    "nowcast bias kW",
                ]),
                rows: replay
                    .all
                    .iter()
                    .map(|b| {
                        vec![
                            json!(format!("{:.0}-{:.0} h", b.lead_from_h, b.lead_to_h)),
                            json!(b.n),
                            num(b.plain.rmse_kw, 3),
                            num(b.nowcast.rmse_kw, 3),
                            num(b.nowcast.rmse_kw - b.plain.rmse_kw, 3),
                            num(b.plain.bias_kw, 3),
                            num(b.nowcast.bias_kw, 3),
                        ]
                    })
                    .collect(),
            });
            match replay.all.first() {
                Some(b) if b.n >= MIN_REPLAY_N => {
                    n = Some(b.n);
                    let delta = b.nowcast.rmse_kw - b.plain.rmse_kw;
                    let state = if delta < -RMSE_NEUTRAL_KW {
                        VerdictState::Helped
                    } else if delta > RMSE_NEUTRAL_KW {
                        VerdictState::Worse
                    } else {
                        VerdictState::Neutral
                    };
                    verdict = Verdict::new(
                        state,
                        format!(
                            "replay: 0-1 h RMSE {:.3} -> {:.3} kW ({:+.3}), n={} (applied on {} of {} reference hours)",
                            b.plain.rmse_kw, b.nowcast.rmse_kw, delta, b.n,
                            replay.n_ref_applied, replay.n_ref
                        ),
                    );
                }
                Some(b) => {
                    n = Some(b.n);
                    verdict = Verdict::insufficient(b.n, "replay samples in the 0-1 h bin");
                }
                None => {
                    verdict = Verdict::insufficient(0, "the replay returned no lead bins");
                }
            }
        }
    }

    #[derive(Default)]
    struct Day {
        samples: usize,
        applied: usize,
        ratio_sum: f64,
        ratio_n: usize,
    }
    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    for s in &input.samples.nowcast {
        let d = days.entry(day_of(s.t)).or_default();
        d.samples += 1;
        if s.applied {
            d.applied += 1;
            if let Some(r) = s.ratio {
                d.ratio_sum += r;
                d.ratio_n += 1;
            }
        }
    }
    let mean_ratio = |d: &Day| (d.ratio_n > 0).then(|| d.ratio_sum / d.ratio_n as f64);
    tables.push(Table {
        title: "Live: hourly nowcast outcomes per UTC day".into(),
        columns: cols(&["date", "hourly samples", "applied", "mean ratio (applied)"]),
        rows: days
            .iter()
            .map(|(date, d)| {
                vec![
                    json!(date),
                    json!(d.samples),
                    json!(d.applied),
                    num_opt(mean_ratio(d), 2),
                ]
            })
            .collect(),
    });
    let charts = vec![Chart {
        title: "Live: share of hourly plans the nowcast applied to".into(),
        y_unit: "ratio".into(),
        x: days.keys().cloned().collect(),
        series: vec![ChartSeries {
            name: "applied share".into(),
            values: days
                .values()
                .map(|d| (d.samples > 0).then(|| round_to(d.applied as f64 / d.samples as f64, 3)))
                .collect(),
        }],
    }];
    if days.is_empty() {
        notes.push("No live nowcast samples recorded yet.".into());
    }
    Computed {
        n,
        verdict,
        charts,
        tables,
        notes,
    }
}

// ============================================================ Solar scale

/// Pool one arm of the accuracy history per lead bin over `days` (an iterator over day entries).
fn pool_arms<'a>(
    days: impl Iterator<Item = &'a crate::feature_data::DayAccuracy>,
    zone_ok: impl Fn(&str) -> bool,
    arm: impl Fn(&crate::feature_data::ZoneArms) -> Option<&Vec<BinStat>>,
) -> [Pool; 4] {
    let mut pools = [Pool::default(); 4];
    for day in days {
        for (zone, arms) in &day.zones {
            if !zone_ok(zone) {
                continue;
            }
            if let Some(bins) = arm(arms) {
                for (pool, stat) in pools.iter_mut().zip(bins) {
                    pool.add(stat);
                }
            }
        }
    }
    pools
}

fn solar_scale(input: &FeaturesInput) -> Computed {
    let ab_days: Vec<(&String, &crate::feature_data::DayAccuracy)> = input
        .history
        .days
        .iter()
        .filter(|(_, d)| d.zones.values().any(|z| z.unscaled.is_some()))
        .collect();
    let without = pool_arms(
        ab_days.iter().map(|(_, d)| *d),
        |_| true,
        |z| z.unscaled.as_ref(),
    );
    let with = pool_arms(
        ab_days.iter().map(|(_, d)| *d),
        |_| true,
        |z| z.scaled_ab.as_ref(),
    );

    let primary = 2; // 12-24 h
    let n = without[primary].n.min(with[primary].n);
    let mut verdict = match (without[primary].rmse(), with[primary].rmse()) {
        (Some(w0), Some(w1)) if n >= MIN_AB_POINTS => {
            let delta = w1 - w0;
            let state = if delta < -RMSE_NEUTRAL_K {
                VerdictState::Helped
            } else if delta > RMSE_NEUTRAL_K {
                VerdictState::Worse
            } else {
                VerdictState::Neutral
            };
            let word = match state {
                VerdictState::Helped => "helped",
                VerdictState::Worse => "worse",
                _ => "no change",
            };
            Verdict::new(
                state,
                format!(
                    "{word}: {delta:+.2} K at {} (RMSE {w0:.2} -> {w1:.2} K), n={n} over {} day(s)",
                    lead_label(primary),
                    ab_days.len()
                ),
            )
        }
        _ => Verdict::insufficient(n, "scored points in the 12-24 h bin with both arms"),
    };

    // Night targets (local 22-06) after sunny vs other days, all zones, pooled over all leads.
    let night_pool = |arm: &dyn Fn(&crate::feature_data::ZoneArms) -> Option<NightStats>| {
        let mut pools = [Pool::default(), Pool::default()]; // [sunny, other]
        for (_, day) in &ab_days {
            for arms in day.zones.values() {
                if let Some(stats) = arm(arms) {
                    pools[0].add(&stats.sunny);
                    pools[1].add(&stats.other);
                }
            }
        }
        pools
    };
    let night_without = night_pool(&|z| z.night_unscaled);
    let night_with = night_pool(&|z| z.night_scaled_ab);
    let night_table = Table {
        title: "Night bias (local 22-06 targets), without vs with the solar scale".into(),
        columns: cols(&[
            "previous day",
            "n",
            "bias without K",
            "bias with K",
            "RMSE without K",
            "RMSE with K",
        ]),
        rows: [("sunny", 0usize), ("not sunny", 1)]
            .iter()
            .map(|(label, i)| {
                let (w0, w1) = (night_without[*i], night_with[*i]);
                vec![
                    json!(label),
                    json!(w0.n.min(w1.n)),
                    num_opt(w0.bias(), 3),
                    num_opt(w1.bias(), 3),
                    num_opt(w0.rmse(), 3),
                    num_opt(w1.rmse(), 3),
                ]
            })
            .collect(),
    };
    let sunny_n = night_without[0].n.min(night_with[0].n);
    if let (Some(b0), Some(b1)) = (night_without[0].bias(), night_with[0].bias()) {
        if sunny_n >= MIN_NIGHT_POINTS {
            verdict.text.push_str(&format!(
                "; night bias after sunny days {b0:+.2} -> {b1:+.2} K (n={sunny_n})"
            ));
        }
    }

    let table = Table {
        title: "Without (s=1) vs with the solar scale, all zones, by lead".into(),
        columns: cols(&[
            "lead",
            "n",
            "RMSE without K",
            "RMSE with K",
            "delta K",
            "bias without K",
            "bias with K",
        ]),
        rows: (0..HIST_BINS_H.len())
            .map(|b| {
                let (w0, w1) = (without[b], with[b]);
                vec![
                    json!(lead_label(b)),
                    json!(w0.n.min(w1.n)),
                    num_opt(w0.rmse(), 3),
                    num_opt(w1.rmse(), 3),
                    num_opt(w0.rmse().zip(w1.rmse()).map(|(a, b)| b - a), 3),
                    num_opt(w0.bias(), 3),
                    num_opt(w1.bias(), 3),
                ]
            })
            .collect(),
    };

    // Per zone over the 6-24 h bins.
    let mut zone_names: BTreeSet<&String> = BTreeSet::new();
    for (_, d) in &ab_days {
        zone_names.extend(d.zones.keys());
    }
    let mut zone_rows: Vec<(f64, Vec<Value>)> = Vec::new();
    for zone in zone_names {
        let pick = |arm: &dyn Fn(&crate::feature_data::ZoneArms) -> Option<&Vec<BinStat>>| {
            let pools = pool_arms(ab_days.iter().map(|(_, d)| *d), |z| z == zone, arm);
            let mut mid = Pool::default();
            for p in &pools[1..=2] {
                mid.n += p.n;
                mid.sq += p.sq;
                mid.err += p.err;
            }
            mid
        };
        let w0 = pick(&|z| z.unscaled.as_ref());
        let w1 = pick(&|z| z.scaled_ab.as_ref());
        if let (Some(a), Some(b)) = (w0.rmse(), w1.rmse()) {
            zone_rows.push((
                b - a,
                vec![
                    json!(zone),
                    json!(w0.n.min(w1.n)),
                    num(a, 3),
                    num(b, 3),
                    num(b - a, 3),
                ],
            ));
        }
    }
    zone_rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let zone_table = Table {
        title: "Per zone, 6-24 h (most improved first)".into(),
        columns: cols(&["zone", "n", "RMSE without K", "RMSE with K", "delta K"]),
        rows: zone_rows.into_iter().map(|(_, r)| r).collect(),
    };

    let chart = Chart {
        title: "12-24 h forecast RMSE per day, all zones".into(),
        y_unit: "K".into(),
        x: ab_days.iter().map(|(d, _)| (*d).clone()).collect(),
        series: vec![
            ChartSeries {
                name: "without solar scale".into(),
                values: ab_days
                    .iter()
                    .map(|(_, d)| {
                        pool_arms(std::iter::once(*d), |_| true, |z| z.unscaled.as_ref())[primary]
                            .rmse()
                            .map(|v| round_to(v, 3))
                    })
                    .collect(),
            },
            ChartSeries {
                name: "with solar scale".into(),
                values: ab_days
                    .iter()
                    .map(|(_, d)| {
                        pool_arms(std::iter::once(*d), |_| true, |z| z.scaled_ab.as_ref())[primary]
                            .rmse()
                            .map(|v| round_to(v, 3))
                    })
                    .collect(),
            },
        ],
    };
    Computed {
        n: Some(n),
        verdict,
        charts: vec![chart],
        tables: vec![table, zone_table, night_table],
        notes: vec![
            "Both arms are the same forecast from the same state and kernels on the same snapshots; only the solar-gain scale differs. Scored against measured hourly zone temperatures, one UTC day at a time.".into(),
            "Only days since the unscaled arm was recorded appear; the arm starts with the first deploy that carries it. Days are the UTC day a prediction was MADE.".into(),
            format!("Night = target local hour 22-06; \"sunny\" = the local day before it measured at least {SUNNY_PV_KWH} kWh of PV in the ledger (days with fewer than 90 scored blocks are unknown and left out)."),
        ],
    }
}

// ============================================================ Relay duty

fn relay_duty(input: &FeaturesInput) -> Computed {
    let rows = &input.samples.relay_duty;
    let (mut legacy, mut events) = (0.0, 0.0);
    let mut zones: BTreeMap<&String, [f64; 2]> = BTreeMap::new();
    for day in rows {
        for (zone, kwh) in &day.zones {
            legacy += kwh[0];
            events += kwh[1];
            let z = zones.entry(zone).or_default();
            z[0] += kwh[0];
            z[1] += kwh[1];
        }
    }
    let mode = match input.relay_duty_mode {
        RelayDuty::Legacy => "legacy",
        RelayDuty::Events => "events",
    };
    let verdict = if rows.len() < MIN_RELAY_DAYS {
        Verdict::insufficient(rows.len(), "days of both relay reads")
    } else if legacy + events < NO_HEAT_KWH {
        Verdict::new(
            VerdictState::Info,
            format!(
                "no heat demand yet (n={} days; the relays logged no heating)",
                rows.len()
            ),
        )
    } else if legacy < NO_HEAT_KWH {
        Verdict::new(
            VerdictState::Info,
            format!(
                "events read counts {events:.1} kWh where legacy read ~0 (n={} days); the live plan reads `{mode}`",
                rows.len()
            ),
        )
    } else {
        Verdict::new(
            VerdictState::Info,
            format!(
                "events read counts {:+.0} % heat vs legacy ({legacy:.1} -> {events:.1} kWh, n={} days); the live plan reads `{mode}`",
                (events - legacy) / legacy * 100.0,
                rows.len()
            ),
        )
    };
    let chart = Chart {
        title: "Heating energy read both ways off the same relays, per UTC day".into(),
        y_unit: "kWh".into(),
        x: rows.iter().map(|d| d.date.clone()).collect(),
        series: vec![
            ChartSeries {
                name: "legacy read".into(),
                values: rows
                    .iter()
                    .map(|d| Some(round_to(d.zones.values().map(|z| z[0]).sum(), 2)))
                    .collect(),
            },
            ChartSeries {
                name: "events read".into(),
                values: rows
                    .iter()
                    .map(|d| Some(round_to(d.zones.values().map(|z| z[1]).sum(), 2)))
                    .collect(),
            },
        ],
    };
    let zone_table = Table {
        title: format!("Per zone, all {} recorded day(s)", rows.len()),
        columns: cols(&["zone", "legacy kWh", "events kWh", "delta %"]),
        rows: zones
            .iter()
            .map(|(zone, z)| {
                vec![
                    json!(zone),
                    num(z[0], 2),
                    num(z[1], 2),
                    num_opt((z[0] > 0.0).then(|| (z[1] - z[0]) / z[0] * 100.0), 0),
                ]
            })
            .collect(),
    };
    Computed {
        n: Some(rows.len()),
        verdict,
        charts: vec![chart],
        tables: vec![zone_table],
        notes: vec![
            match input.relay_duty_mode {
                RelayDuty::Legacy => format!("Configured `heating.relay_duty`: `{mode}`. Staged: the events read feeds the true heat into the model, which over-responds to floor heat, so the live default stays `legacy` until the kernel gain is corrected."),
                RelayDuty::Events => format!("Configured `heating.relay_duty`: `{mode}` — the live plan uses the true heating energy. Trade-off accepted by the owner: the model over-responds to floor heat in the open-plan kitchen/livingroom, so heated rooms may forecast warm. The legacy column is kept as the comparison arm."),
            },
            "Both reads use the same relays and the same UTC day window; a day on which the Influx read failed is indistinguishable from a day with no heating.".into(),
        ],
    }
}

// ============================================================ Priority zones

/// Where one measured block's electricity came from, for splitting heat by tariff context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Supply {
    /// PV covered the house with nothing imported.
    PvSurplus,
    /// Night tariff.
    Nt,
    /// Day tariff.
    Vt,
}

fn block_supply(row: &LedgerRow, mask: &[bool; 24], site: &SiteConfig) -> Option<Supply> {
    let m = row.measured.as_ref()?;
    let dt_h = f64::from(row.dt_minutes) / 60.0;
    if m.pv_kwh >= PV_COVER_KW * dt_h && m.import_kwh <= PV_COVER_IMPORT_KW * dt_h {
        return Some(Supply::PvSurplus);
    }
    let local_hour = row.t.with_timezone(&site.offset_at(row.t)).hour() as usize;
    Some(if mask[local_hour] {
        Supply::Nt
    } else {
        Supply::Vt
    })
}

fn priority_zones(info: &FeatureInfo, input: &FeaturesInput) -> Computed {
    if input.priority_zones.is_empty() {
        return Computed::only(
            None,
            Verdict::new(
                VerdictState::Off,
                "feature off: every warmth_value_eur_per_kh is 0",
            ),
        );
    }
    // Measured heat per day per priority zone, by supply.
    let mut heat: BTreeMap<(String, String), [f64; 3]> = BTreeMap::new();
    let mut total_heat = 0.0;
    let mut by_supply = [0.0f64; 3];
    let release = info.live_since();
    for row in input
        .ledger_rows
        .iter()
        .filter(|r| eligible(r) && release.is_none_or(|rel| r.t >= rel))
    {
        let (Some(m), Some(supply)) = (
            row.measured.as_ref(),
            block_supply(row, &input.low_tariff_mask, input.site),
        ) else {
            continue;
        };
        let slot = supply as usize;
        for zone in input.priority_zones {
            if let Some(Some(kwh)) = m.heat_kwh.get(zone) {
                heat.entry((day_of(row.t), zone.clone())).or_default()[slot] += kwh;
                by_supply[slot] += kwh;
                total_heat += kwh;
            }
        }
    }
    // The hourly counterfactual rows (each is a full-horizon plan, so they overlap and are
    // averaged, not summed).
    #[derive(Default)]
    struct Day {
        n: usize,
        extra_cost: f64,
        extra_heat_kwh: f64,
        live_kh: f64,
    }
    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    let mut ab = Day::default();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for s in &input.samples.warmth {
        let (Some(with), Some(without)) = (s.cost_with_eur, s.cost_without_eur) else {
            *skipped
                .entry(s.skipped.clone().unwrap_or_else(|| "unknown".into()))
                .or_default() += 1;
            continue;
        };
        let extra_heat =
            s.heat_with_kwh.values().sum::<f64>() - s.heat_without_kwh.values().sum::<f64>();
        let live_kh = s.warmth_kh_with.values().sum::<f64>();
        for day in [days.entry(day_of(s.t)).or_default(), &mut ab] {
            day.n += 1;
            day.extra_cost += with - without;
            day.extra_heat_kwh += extra_heat;
            day.live_kh += live_kh;
        }
    }
    let verdict = if ab.n >= MIN_WARMTH_SAMPLES {
        Verdict::new(
            VerdictState::Info,
            format!(
                "warmth plans {:+.2} kWh more heat for {:+.3} EUR planned cost per plan ({} solved A/B plans, live plans hold {:.0} K·h above the floors); measured heat in priority zones {:.1} kWh (PV {:.1} / NT {:.1} / VT {:.1})",
                ab.extra_heat_kwh / ab.n as f64,
                ab.extra_cost / ab.n as f64,
                ab.n,
                ab.live_kh / ab.n as f64,
                total_heat,
                by_supply[0],
                by_supply[1],
                by_supply[2]
            ),
        )
    } else if total_heat < NO_HEAT_KWH {
        Verdict::new(
            VerdictState::Info,
            format!(
                "no heat demand yet ({} solved A/B plans, no measured heat in the priority zones)",
                ab.n
            ),
        )
    } else {
        Verdict::new(
            VerdictState::Info,
            format!(
                "measured heat in priority zones {:.1} kWh (PV {:.1} / NT {:.1} / VT {:.1}); not enough data for the A/B (n={} solved plans)",
                total_heat, by_supply[0], by_supply[1], by_supply[2], ab.n
            ),
        )
    };
    let mean = |sum: f64, n: usize| (n > 0).then(|| round_to(sum / n as f64, 4));
    let charts = vec![
        Chart {
            title: "A/B: extra planned cost per plan (with warmth minus without)".into(),
            y_unit: "EUR".into(),
            x: days.keys().cloned().collect(),
            series: vec![ChartSeries {
                name: "mean extra cost".into(),
                values: days.values().map(|d| mean(d.extra_cost, d.n)).collect(),
            }],
        },
        Chart {
            title: "A/B: extra planned heat per plan (with warmth minus without)".into(),
            y_unit: "kWh".into(),
            x: days.keys().cloned().collect(),
            series: vec![ChartSeries {
                name: "mean extra heat".into(),
                values: days.values().map(|d| mean(d.extra_heat_kwh, d.n)).collect(),
            }],
        },
    ];
    let heat_table = Table {
        title: "Measured heat per priority zone by supply (kWh)".into(),
        columns: cols(&["date", "zone", "PV-covered", "night tariff", "day tariff"]),
        rows: heat
            .iter()
            .map(|((date, zone), s)| {
                vec![
                    json!(date),
                    json!(zone),
                    num(s[0], 2),
                    num(s[1], 2),
                    num(s[2], 2),
                ]
            })
            .collect(),
    };
    let ab_table = Table {
        title: "A/B by day: hourly counterfactual plans".into(),
        columns: cols(&[
            "date",
            "solved plans",
            "mean extra EUR",
            "mean extra heat kWh",
            "mean live K·h above floor",
        ]),
        rows: days
            .iter()
            .map(|(date, d)| {
                vec![
                    json!(date),
                    json!(d.n),
                    num_opt(mean(d.extra_cost, d.n), 4),
                    num_opt(mean(d.extra_heat_kwh, d.n), 2),
                    num_opt(mean(d.live_kh, d.n), 1),
                ]
            })
            .collect(),
    };
    let mut notes = vec![
        format!(
            "Priority zones: {}. A/B: each hour one plan is re-solved with every warmth value 0 on the same inputs (the live plan is the other arm, and both share the loop's slow-input cache, kernels, block-0 relay pin and load tally; the counterfactual re-reads the thermal state, prices and weather a moment later, so tiny differences are input noise); each plan covers the whole 36 h horizon, so the rows overlap and are averaged, not summed. The zero-warmth plan reports no K·h figure, so \"bought\" is measured in planned heat kWh and EUR; the K·h shown is the live plan's.",
            input.priority_zones.join(", ")
        ),
        "Supply split of the measured heat: PV-covered = PV above 0.1 kW and no grid import in the block; otherwise the tariff at the block's local hour.".into(),
        format!("Samples older than {SAMPLE_RETENTION_DAYS} days are dropped."),
    ];
    if !skipped.is_empty() {
        let list: Vec<String> = skipped
            .iter()
            .map(|(why, n)| format!("{why}: {n}"))
            .collect();
        notes.push(format!(
            "Hours without a solved counterfactual — {}.",
            list.join(", ")
        ));
    }
    Computed {
        n: Some(ab.n),
        verdict,
        charts,
        tables: vec![ab_table, heat_table],
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_data::{
        DayAccuracy, NowcastSample, RelayDutyDay, TerminalSample, WarmthSample, ZoneArms,
    };
    use std::collections::HashMap;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn site() -> SiteConfig {
        serde_json::from_value(json!({"latitude": 49.5, "longitude": 17.4, "utc_offset_hours": 0}))
            .unwrap()
    }

    /// A scored ledger row (every field not named defaults).
    #[allow(clippy::too_many_arguments)]
    fn row(
        t: &str,
        slot: &str,
        planned: Value,
        measured: Value,
        misses: &[&str],
        cost: Option<(f64, f64)>,
    ) -> LedgerRow {
        let mut v = json!({
            "t": t, "dt_minutes": 15, "slot": slot, "scored": true,
            "planned": planned, "measured": measured, "misses": misses,
            "price_is_placeholder": false,
        });
        if let Some((p, r)) = cost {
            v["planned_cost_eur"] = json!(p);
            v["realized_cost_eur"] = json!(r);
        }
        serde_json::from_value(v).unwrap()
    }

    fn unscored(t: &str) -> LedgerRow {
        serde_json::from_value(json!({"t": t, "dt_minutes": 15, "scored": false})).unwrap()
    }

    struct Fixture {
        rows: Vec<LedgerRow>,
        history: AccuracyHistory,
        samples: FeatureSamples,
        replay: Option<NowcastReplayView>,
        priority: Vec<String>,
        mode: RelayDuty,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                rows: Vec::new(),
                history: AccuracyHistory::default(),
                samples: FeatureSamples::default(),
                replay: None,
                priority: vec!["livingroom".into()],
                mode: RelayDuty::Legacy,
            }
        }
        fn page(&self) -> FeaturesReport {
            let site = site();
            build(&FeaturesInput {
                now: utc("2026-10-04T12:00:00Z"),
                ledger_rows: &self.rows,
                history: &self.history,
                samples: &self.samples,
                pv_replay: self.replay.as_ref(),
                low_tariff_mask: std::array::from_fn(|h| !(8..20).contains(&h)),
                site: &site,
                priority_zones: &self.priority,
                relay_duty_mode: self.mode,
            })
        }
        fn feature(&self, id: &str) -> FeatureReport {
            self.page()
                .features
                .into_iter()
                .find(|f| f.id == id)
                .unwrap()
        }
    }

    // ---- registry

    #[test]
    fn registry_has_five_entries_with_the_specified_statuses() {
        assert_eq!(FEATURES.len(), 5);
        let count = |s: Status| FEATURES.iter().filter(|f| f.status == s).count();
        assert_eq!(count(Status::Staged), 1);
        assert_eq!(count(Status::Live), 4);
        let ids: BTreeSet<_> = FEATURES.iter().map(|f| f.id).collect();
        assert_eq!(ids.len(), 5, "ids are unique");
        for graduated in [
            "export_gate",
            "ledger",
            "dispatch_floor",
            "model_changes",
            "winter_cli",
        ] {
            assert!(
                !ids.contains(graduated),
                "{graduated} graduated off the page"
            );
        }
        let terminal = FEATURES.iter().find(|f| f.id == "terminal_soc").unwrap();
        assert_eq!(terminal.live_since(), Some(utc("2026-10-01T05:33:00Z")));
        let relay = FEATURES.iter().find(|f| f.id == "relay_duty").unwrap();
        assert_eq!(relay.status, Status::Staged);
        assert_eq!(relay.releases.len(), 3);
        for f in FEATURES {
            for r in f.releases {
                assert!(r.at.is_none_or(|t| parse_utc(t).is_some()), "{}", f.id);
            }
        }
    }

    #[test]
    fn every_feature_is_reported_with_the_exact_kind_labels() {
        let page = Fixture::new().page();
        assert_eq!(page.features.len(), 5);
        let kinds: HashMap<_, _> = page.features.iter().map(|f| (f.id, f.kind_label)).collect();
        assert_eq!(kinds["solar_scale"], "A/B live");
        assert_eq!(kinds["terminal_soc"], "A/B live");
        assert_eq!(kinds["relay_duty"], "A/B live");
        assert_eq!(kinds["priority_zones"], "A/B live");
        assert_eq!(kinds["pv_nowcast"], "A/B replay");
        // The JSON shape the UI reads.
        let v = serde_json::to_value(&page).unwrap();
        let first = &v["features"][0];
        for key in [
            "id",
            "name",
            "shas",
            "releases",
            "live_since",
            "status",
            "what",
            "kind",
            "kind_label",
            "n",
            "verdict",
            "baseline",
            "charts",
            "tables",
            "notes",
        ] {
            assert!(first.get(key).is_some(), "missing {key}");
        }
        assert_eq!(first["status"], "live");
        assert_eq!(first["kind"], "ab_live");
        assert!(first["baseline"]["values"]["legacy_eur_kwh"].is_number());
    }

    #[test]
    fn an_empty_world_says_not_enough_data_everywhere_it_should() {
        let page = Fixture::new().page();
        for f in &page.features {
            assert!(
                matches!(
                    f.verdict.state,
                    VerdictState::Insufficient | VerdictState::Info
                ),
                "{}: {:?}",
                f.id,
                f.verdict
            );
        }
    }

    #[test]
    fn the_graduated_list_is_static_text_with_a_sha_for_each_entry() {
        let v = serde_json::to_value(Fixture::new().page()).unwrap();
        let list = v["graduated"].as_array().unwrap();
        assert_eq!(list.len(), 5);
        for g in list {
            for key in ["name", "sha", "result"] {
                assert!(g[key].as_str().is_some_and(|t| !t.is_empty()), "{g}");
            }
        }
        assert_eq!(list[0]["sha"], "112ea92");
    }

    // ---- terminal SoC

    fn terminal(t: &str, outlook: f64, legacy: f64, source: &str, soc: f64) -> TerminalSample {
        TerminalSample {
            t: utc(t),
            outlook_eur_kwh: outlook,
            legacy_eur_kwh: legacy,
            source: source.into(),
            end_soc_kwh: soc,
        }
    }

    #[test]
    fn terminal_soc_means_the_two_valuations_per_day_and_ignores_fallback_samples() {
        let mut fx = Fixture::new();
        for h in 0..14 {
            fx.samples.terminal.push(terminal(
                &format!("2026-10-02T{h:02}:00:00Z"),
                0.25,
                0.05,
                "outlook",
                7.0,
            ));
        }
        fx.samples.terminal.push(terminal(
            "2026-10-02T20:00:00Z",
            0.9,
            0.9,
            "horizon_median",
            1.0,
        ));
        let f = fx.feature("terminal_soc");
        assert_eq!(f.n, Some(14));
        assert_eq!(f.verdict.state, VerdictState::Info);
        assert!(
            f.verdict.text.contains("0.250 vs 0.050"),
            "{}",
            f.verdict.text
        );
        assert!(f.verdict.text.contains("replay result"));
        assert_eq!(f.tables[0].rows[0][2], json!(0.25));
        assert_eq!(f.tables[0].rows[0][4], json!(7.0));
        assert!(f.notes.iter().any(|n| n.contains("1 sample(s) fell back")));
        assert_eq!(f.charts.len(), 2);
    }

    #[test]
    fn terminal_soc_needs_samples() {
        let mut fx = Fixture::new();
        fx.samples
            .terminal
            .push(terminal("2026-10-02T01:00:00Z", 0.25, 0.05, "outlook", 7.0));
        assert!(fx
            .feature("terminal_soc")
            .verdict
            .text
            .starts_with("not enough data (n=1)"));
    }

    // ---- PV nowcast

    fn replay(plain: f64, nowcast: f64, n: usize) -> NowcastReplayView {
        let bin = |from: f64, to: f64| ReplayBinView {
            lead_from_h: from,
            lead_to_h: to,
            n,
            plain: ReplayScoreView {
                rmse_kw: plain,
                bias_kw: 0.1,
            },
            nowcast: ReplayScoreView {
                rmse_kw: nowcast,
                bias_kw: 0.05,
            },
        };
        NowcastReplayView {
            all: vec![bin(0.0, 1.0), bin(1.0, 2.0), bin(2.0, 3.0)],
            n_ref: 100,
            n_ref_applied: 60,
        }
    }

    #[test]
    fn pv_nowcast_verdict_is_labelled_replay_and_follows_the_rmse_delta() {
        let mut fx = Fixture::new();
        fx.replay = Some(replay(1.789, 1.523, 120));
        let f = fx.feature("pv_nowcast");
        assert_eq!(f.verdict.state, VerdictState::Helped);
        assert!(f
            .verdict
            .text
            .starts_with("replay: 0-1 h RMSE 1.789 -> 1.523 kW"));
        assert_eq!(f.n, Some(120));
        fx.replay = Some(replay(1.5, 1.6, 120));
        assert_eq!(fx.feature("pv_nowcast").verdict.state, VerdictState::Worse);
        fx.replay = Some(replay(1.5, 1.505, 120));
        assert_eq!(
            fx.feature("pv_nowcast").verdict.state,
            VerdictState::Neutral
        );
        fx.replay = Some(replay(1.5, 1.0, 5));
        assert_eq!(
            fx.feature("pv_nowcast").verdict.state,
            VerdictState::Insufficient
        );
        fx.replay = None;
        assert!(fx
            .feature("pv_nowcast")
            .verdict
            .text
            .contains("replay is not available yet"));
    }

    #[test]
    fn pv_nowcast_live_samples_aggregate_per_day() {
        let mut fx = Fixture::new();
        let s = |t: &str, applied: bool, ratio: Option<f64>| NowcastSample {
            t: utc(t),
            applied,
            ratio,
            reason: None,
        };
        fx.samples.nowcast = vec![
            s("2026-10-02T08:00:00Z", true, Some(1.2)),
            s("2026-10-02T09:00:00Z", true, Some(1.0)),
            s("2026-10-02T10:00:00Z", false, None),
        ];
        let f = fx.feature("pv_nowcast");
        let live = f.tables.last().unwrap();
        assert_eq!(live.rows[0][1], json!(3));
        assert_eq!(live.rows[0][2], json!(2));
        assert_eq!(live.rows[0][3], json!(1.1));
        assert_eq!(f.charts[0].series[0].values[0], Some(0.667));
    }

    #[test]
    fn nowcast_replay_view_parses_the_backtest_payload() {
        let payload = json!({"nowcast": {
            "params": {}, "applied": [], "n_ref": 10, "n_ref_applied": 4,
            "all": [{"lead_from_h": 0.0, "lead_to_h": 1.0, "n": 7,
                     "plain": {"rmse_kw": 1.0, "bias_kw": 0.0},
                     "nowcast": {"rmse_kw": 0.9, "bias_kw": 0.0}}],
        }});
        let v = NowcastReplayView::from_backtest(&payload).unwrap();
        assert_eq!(v.all[0].n, 7);
        assert!(NowcastReplayView::from_backtest(&json!({})).is_none());
    }

    // ---- solar scale (history)

    fn bins(n: u32, rmse: f64, bias: f64) -> Vec<BinStat> {
        vec![BinStat(n, rmse, bias); 4]
    }

    fn day_with(zones: &[(&str, ZoneArms)]) -> DayAccuracy {
        DayAccuracy {
            zones: zones
                .iter()
                .map(|(z, a)| (z.to_string(), a.clone()))
                .collect(),
        }
    }

    fn ab_arms(without: f64, with: f64, n: u32) -> ZoneArms {
        ZoneArms {
            scaled: bins(n, with, 0.0),
            unscaled: Some(bins(n, without, 0.1)),
            scaled_ab: Some(bins(n, with, 0.0)),
            ..Default::default()
        }
    }

    #[test]
    fn solar_scale_pools_both_arms_per_bin_and_per_zone() {
        let mut fx = Fixture::new();
        fx.history.days.insert(
            "2026-10-04".into(),
            day_with(&[
                ("entrance", ab_arms(0.60, 0.40, 40)),
                ("kitchen", ab_arms(0.30, 0.30, 40)),
            ]),
        );
        let f = fx.feature("solar_scale");
        // 12-24 h bin: n = 80 per arm; pooled RMSE sqrt((40*.36+40*.09)/80) = 0.4743 vs
        // sqrt((40*.16+40*.09)/80) = 0.3536.
        assert_eq!(f.n, Some(80));
        assert_eq!(f.verdict.state, VerdictState::Helped, "{:?}", f.verdict);
        assert!(
            f.verdict
                .text
                .starts_with("helped: -0.12 K at 12-24 h (RMSE 0.47 -> 0.35 K), n=80"),
            "{}",
            f.verdict.text
        );
        let zone_table = &f.tables[1];
        assert_eq!(
            zone_table.rows[0][0],
            json!("entrance"),
            "most improved first"
        );
        assert_eq!(zone_table.rows[0][4], json!(-0.2));
        assert_eq!(f.tables[0].rows.len(), 4);
        assert_eq!(f.charts[0].x, vec!["2026-10-04"]);
    }

    #[test]
    fn solar_scale_ignores_days_without_the_arm_and_needs_points() {
        let mut fx = Fixture::new();
        // A day recorded before the arm existed contributes nothing.
        fx.history.days.insert(
            "2026-10-01".into(),
            day_with(&[(
                "entrance",
                ZoneArms {
                    scaled: bins(500, 0.5, 0.0),
                    ..Default::default()
                },
            )]),
        );
        let f = fx.feature("solar_scale");
        assert_eq!(f.n, Some(0));
        assert_eq!(f.verdict.state, VerdictState::Insufficient);
        // A thin A/B day is still "not enough data".
        fx.history.days.insert(
            "2026-10-04".into(),
            day_with(&[("entrance", ab_arms(0.6, 0.4, 5))]),
        );
        assert!(fx
            .feature("solar_scale")
            .verdict
            .text
            .starts_with("not enough data (n=5)"));
    }

    #[test]
    fn solar_scale_neutral_and_worse_thresholds() {
        let mut fx = Fixture::new();
        fx.history.days.insert(
            "2026-10-04".into(),
            day_with(&[("a", ab_arms(0.500, 0.505, 100))]),
        );
        assert_eq!(
            fx.feature("solar_scale").verdict.state,
            VerdictState::Neutral
        );
        fx.history.days.insert(
            "2026-10-04".into(),
            day_with(&[("a", ab_arms(0.40, 0.50, 100))]),
        );
        let v = fx.feature("solar_scale").verdict;
        assert_eq!(v.state, VerdictState::Worse);
        assert!(v.text.starts_with("worse: +0.10 K"));
    }

    // ---- relay duty

    fn relay_day(date: &str, zones: &[(&str, f64, f64)]) -> RelayDutyDay {
        RelayDutyDay {
            date: date.into(),
            zones: zones
                .iter()
                .map(|(z, l, e)| (z.to_string(), [*l, *e]))
                .collect(),
        }
    }

    #[test]
    fn relay_duty_compares_the_two_reads_and_shows_the_configured_mode() {
        let mut fx = Fixture::new();
        for d in ["2026-10-01", "2026-10-02", "2026-10-03"] {
            fx.samples
                .relay_duty
                .push(relay_day(d, &[("kitchen", 2.0, 4.0), ("office", 0.0, 1.0)]));
        }
        let f = fx.feature("relay_duty");
        assert_eq!(f.n, Some(3));
        assert_eq!(f.status, Status::Staged);
        assert!(
            f.verdict.text.starts_with(
                "events read counts +150 % heat vs legacy (6.0 -> 15.0 kWh, n=3 days)"
            ),
            "{}",
            f.verdict.text
        );
        assert!(f.verdict.text.contains("`legacy`"));
        let zones = &f.tables[0].rows;
        assert_eq!(zones[0][0], json!("kitchen"));
        assert_eq!(zones[0][3], json!(100.0));
        assert_eq!(zones[1][3], Value::Null, "no legacy base for a percentage");
        assert!(f.notes[0].contains("Configured `heating.relay_duty`: `legacy`"));
        fx.mode = RelayDuty::Events;
        let live = fx.feature("relay_duty");
        assert!(live.verdict.text.contains("`events`"));
        assert_eq!(
            live.status,
            Status::Live,
            "the config, not the registry, decides what is live"
        );
        assert!(live.notes[0].contains("the live plan uses the true heating energy"));
    }

    #[test]
    fn relay_duty_reports_no_heat_demand_and_waits_for_days() {
        let mut fx = Fixture::new();
        for d in ["2026-10-01", "2026-10-02"] {
            fx.samples.relay_duty.push(relay_day(d, &[]));
        }
        assert!(fx
            .feature("relay_duty")
            .verdict
            .text
            .starts_with("not enough data (n=2)"));
        fx.samples.relay_duty.push(relay_day("2026-10-03", &[]));
        assert!(fx
            .feature("relay_duty")
            .verdict
            .text
            .starts_with("no heat demand yet (n=3 days"));
    }

    // ---- priority zones

    fn warmth(t: &str, with: f64, without: f64, heat_with: f64, heat_without: f64) -> WarmthSample {
        WarmthSample {
            t: utc(t),
            cost_with_eur: Some(with),
            cost_without_eur: Some(without),
            heat_with_kwh: BTreeMap::from([("livingroom".to_string(), heat_with)]),
            heat_without_kwh: BTreeMap::from([("livingroom".to_string(), heat_without)]),
            warmth_kh_with: BTreeMap::from([("livingroom".to_string(), 40.0)]),
            ..Default::default()
        }
    }

    #[test]
    fn priority_zones_says_off_when_no_zone_has_a_warmth_value() {
        let mut fx = Fixture::new();
        fx.priority.clear();
        let f = fx.feature("priority_zones");
        assert_eq!(f.verdict.state, VerdictState::Off);
        assert_eq!(
            f.verdict.text,
            "feature off: every warmth_value_eur_per_kh is 0"
        );
    }

    #[test]
    fn priority_zones_reports_no_heat_demand_yet() {
        let mut fx = Fixture::new();
        fx.samples.warmth.push(WarmthSample {
            t: utc("2026-10-03T10:00:00Z"),
            skipped: Some("no heat planned".into()),
            identical: true,
            ..Default::default()
        });
        let f = fx.feature("priority_zones");
        assert!(
            f.verdict.text.starts_with("no heat demand yet"),
            "{}",
            f.verdict.text
        );
        assert!(f.notes.iter().any(|n| n.contains("no heat planned: 1")));
    }

    #[test]
    fn priority_zones_splits_measured_heat_by_supply_and_averages_the_ab() {
        let mut fx = Fixture::new();
        // site is UTC; the test mask is NT outside 08:00-20:00.
        let block = |t: &str, pv: f64, import: f64| {
            row(
                t,
                "regular",
                json!({}),
                json!({"pv_kwh": pv, "import_kwh": import, "heat_kwh": {"livingroom": 0.5, "kitchen": 9.0}}),
                &[],
                None,
            )
        };
        fx.rows = vec![
            block("2026-10-03T12:00:00Z", 0.5, 0.0), // PV-covered
            block("2026-10-03T12:15:00Z", 0.0, 0.8), // day tariff (VT)
            block("2026-10-03T23:00:00Z", 0.0, 0.8), // night tariff (NT)
        ];
        for h in 0..6 {
            fx.samples.warmth.push(warmth(
                &format!("2026-10-03T{:02}:00:00Z", 10 + h),
                1.2,
                1.0,
                3.0,
                1.0,
            ));
        }
        let f = fx.feature("priority_zones");
        assert_eq!(f.n, Some(6));
        let heat = &f.tables[1].rows;
        assert_eq!(heat.len(), 1, "only the priority zone, not the kitchen");
        assert_eq!(heat[0][2..], [json!(0.5), json!(0.5), json!(0.5)]);
        assert!(
            f.verdict.text.starts_with(
                "warmth plans +2.00 kWh more heat for +0.200 EUR planned cost per plan (6 solved A/B plans, live plans hold 40 K·h above the floors)"
            ),
            "{}",
            f.verdict.text
        );
        assert!(f.verdict.text.contains("PV 0.5 / NT 0.5 / VT 0.5"));
        assert_eq!(f.tables[0].rows[0][2], json!(0.2));
    }

    #[test]
    fn block_supply_follows_pv_cover_then_the_tariff_mask() {
        let mask: [bool; 24] = std::array::from_fn(|h| h < 6);
        let site = site();
        let r = |t: &str, pv: f64, import: f64| {
            row(
                t,
                "regular",
                json!({}),
                json!({"pv_kwh": pv, "import_kwh": import}),
                &[],
                None,
            )
        };
        assert_eq!(
            block_supply(&r("2026-10-03T12:00:00Z", 0.5, 0.0), &mask, &site),
            Some(Supply::PvSurplus)
        );
        // PV present but the house still imports: tariff decides.
        assert_eq!(
            block_supply(&r("2026-10-03T12:00:00Z", 0.5, 0.5), &mask, &site),
            Some(Supply::Vt)
        );
        assert_eq!(
            block_supply(&r("2026-10-03T03:00:00Z", 0.0, 0.5), &mask, &site),
            Some(Supply::Nt)
        );
        assert_eq!(
            block_supply(&unscored("2026-10-03T03:00:00Z"), &mask, &site),
            None
        );
    }

    #[test]
    fn pool_combines_cells_by_sample_weight() {
        let mut p = Pool::default();
        p.add(&BinStat(1, 3.0, 3.0));
        p.add(&BinStat(3, 1.0, -1.0));
        assert_eq!(p.n, 4);
        assert!((p.rmse().unwrap() - (12.0f64 / 4.0).sqrt()).abs() < 1e-12);
        assert!((p.bias().unwrap() - 0.0).abs() < 1e-12);
        assert!(Pool::default().rmse().is_none());
    }

    #[test]
    fn priority_zones_ignores_measured_heat_before_the_release() {
        let mut fx = Fixture::new();
        let block = |t: &str| {
            row(
                t,
                "regular",
                json!({}),
                json!({"pv_kwh": 0.0, "import_kwh": 0.8, "heat_kwh": {"livingroom": 0.5}}),
                &[],
                None,
            )
        };
        // Release 8481684 is 2026-10-02 21:41 UTC.
        fx.rows = vec![block("2026-10-02T12:00:00Z"), block("2026-10-02T22:00:00Z")];
        let f = fx.feature("priority_zones");
        let heat = &f.tables[1].rows;
        assert_eq!(heat.len(), 1);
        assert_eq!(heat[0][0], json!("2026-10-02"));
        assert_eq!(
            heat[0][2..],
            [json!(0.0), json!(0.5), json!(0.0)],
            "22:00 UTC is night tariff"
        );
    }

    #[test]
    fn solar_scale_quotes_night_bias_after_sunny_days_with_n() {
        let mut fx = Fixture::new();
        let night = |bias: f64, n: u32| NightStats {
            sunny: BinStat(n, bias.abs(), bias),
            other: BinStat(0, 0.0, 0.0),
        };
        let mut arms = ab_arms(0.5, 0.4, 100);
        arms.night_unscaled = Some(night(-0.60, 30));
        arms.night_scaled_ab = Some(night(-0.30, 30));
        fx.history
            .days
            .insert("2026-10-04".into(), day_with(&[("entrance", arms)]));
        let f = fx.feature("solar_scale");
        assert!(
            f.verdict
                .text
                .ends_with("night bias after sunny days -0.60 -> -0.30 K (n=30)"),
            "{}",
            f.verdict.text
        );
        let night_table = &f.tables[2];
        assert_eq!(night_table.rows[0][0], json!("sunny"));
        assert_eq!(night_table.rows[0][1], json!(30));
        assert_eq!(night_table.rows[0][2], json!(-0.6));
        assert_eq!(night_table.rows[0][3], json!(-0.3));
        // Too few night points: the verdict leaves the night figure out.
        let mut thin = ab_arms(0.5, 0.4, 100);
        thin.night_unscaled = Some(night(-0.6, 5));
        thin.night_scaled_ab = Some(night(-0.3, 5));
        fx.history
            .days
            .insert("2026-10-04".into(), day_with(&[("entrance", thin)]));
        assert!(!fx.feature("solar_scale").verdict.text.contains("night"));
    }
}
