//! The "new features" page (`GET /api/features`): a registry of the shipped features still under
//! evaluation with, per feature, the strongest honest comparison the stored data supports, plus the
//! static list of features that graduated (became the only behaviour).
//!
//! Everything here is pure: [`build`] takes the ledger rows, the feature samples and `now`, and
//! returns the page's payload. Each comparison states what kind of evidence it is ([`Kind`]) and
//! says "not enough data (n=…)" below the sample thresholds in the `MIN_*` constants.

use std::collections::BTreeMap;

use chrono::{DateTime, Timelike, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::feature_data::{FeatureSamples, SAMPLE_RETENTION_DAYS};
use crate::ledger::LedgerRow;
use crate::optimize::config::SiteConfig;
use crate::tools::round_to;

// ============================================================ Thresholds

/// Solved (non-skipped) priority-zone A/B plans before the verdict is stated.
const MIN_PULL_SAMPLES: usize = 6;
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
}

/// What kind of evidence a comparison is. The labels are the page's, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Both arms computed on the same live data (the strongest).
    AbLive,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::AbLive => "A/B live",
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

pub const FEATURES: &[FeatureInfo] = &[FeatureInfo {
    id: "comfort_targets",
    name: "Comfort targets",
    releases: &[rel("9a1431c", "2026-10-11T07:47:00Z")],
    what: "Every room has a heat target and band (and a prepared cool target) instead of a bare floor and ceiling; a per-room priority times one global price pulls the rooms toward their targets, and the cheap energy fills the gap.",
    status: Status::Live,
    kind: Kind::AbLive,
    proof: "Jan-2026 replay (backtest-comfort): pull 0.04 = +1.6 EUR/week vs the old floor-and-warmth config, every priority room within ~0.2 K of its target on average.",
    baseline: &[("pull_eur_per_kh", 0.04), ("replay_extra_eur_per_week", 1.6)],
}];

// ============================================================ Output shape

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictState {
    /// A number to read, with no good/bad judgement.
    Info,
    Insufficient,
    /// The feature is configured off.
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
    Graduated {
        name: "PV intraday nowcast",
        sha: "a7604db",
        date: Some("2026-10-01"),
        result: "Replay over 7 days, identical samples: 0-1 h RMSE 1.483 -> 1.383 kW, 1-2 h 1.396 -> 1.321, 2-3 h 1.385 -> 1.303; 0-1 h bias +0.64 -> +0.36 kW (n=79).",
    },
    Graduated {
        name: "Event-based relay duty",
        sha: "28fa2af, d083050, c1c9ffa",
        date: Some("2026-10-03"),
        result: "17 days both ways off the same relays: legacy mis-read per-zone heat (livingroom 4.5 vs 0.19 kWh, ground_hall 0.9 vs 0.04); events counts 41.3 vs 33.8 kWh. Legacy read removed.",
    },
    Graduated {
        name: "Terminal SoC at post-horizon prices",
        sha: "825da58",
        date: Some("2026-10-01"),
        result: "249 hourly plans valued leftover energy at 0.165 vs 0.080 EUR/kWh (legacy fell to 0.01 on 2026-10-09); 14-day replay cost identical, -0.20 EUR at a 12 h horizon; planned end SoC 7.8 kWh.",
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

/// Everything [`build`] reads.
pub struct FeaturesInput<'a> {
    pub now: DateTime<Utc>,
    pub ledger_rows: &'a [LedgerRow],
    pub samples: &'a FeatureSamples,
    /// Per-local-hour low-tariff (NT) mask.
    pub low_tariff_mask: [bool; 24],
    pub site: &'a SiteConfig,
    /// Zones that carry a pull (`comfort.pull_eur_per_kh × priority > 0`).
    pub priority_zones: &'a [String],
}

/// Build the whole page.
pub fn build(input: &FeaturesInput) -> FeaturesReport {
    let features = FEATURES
        .iter()
        .map(|info| {
            let computed = match info.id {
                "comfort_targets" => comfort_targets(info, input),
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
                status: info.status,
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

fn num(x: f64, places: i32) -> Value {
    json!(round_to(x, places))
}

fn num_opt(x: Option<f64>, places: i32) -> Value {
    x.map_or(Value::Null, |x| num(x, places))
}

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
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

fn comfort_targets(info: &FeatureInfo, input: &FeaturesInput) -> Computed {
    if input.priority_zones.is_empty() {
        return Computed::only(
            None,
            Verdict::new(
                VerdictState::Off,
                "feature off: comfort.pull_eur_per_kh is 0 (or every room has priority 0)",
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
    for s in &input.samples.pull {
        let (Some(with), Some(without)) = (s.cost_with_eur, s.cost_without_eur) else {
            *skipped
                .entry(s.skipped.clone().unwrap_or_else(|| "unknown".into()))
                .or_default() += 1;
            continue;
        };
        let extra_heat =
            s.heat_with_kwh.values().sum::<f64>() - s.heat_without_kwh.values().sum::<f64>();
        let live_kh = s.pull_kh_with.values().sum::<f64>();
        for day in [days.entry(day_of(s.t)).or_default(), &mut ab] {
            day.n += 1;
            day.extra_cost += with - without;
            day.extra_heat_kwh += extra_heat;
            day.live_kh += live_kh;
        }
    }
    let verdict = if ab.n >= MIN_PULL_SAMPLES {
        Verdict::new(
            VerdictState::Info,
            format!(
                "the pull plans {:+.2} kWh more heat for {:+.3} EUR planned cost per plan ({} solved A/B plans, live plans sit {:.0} K·h below the targets); measured heat in pulled zones {:.1} kWh (PV {:.1} / NT {:.1} / VT {:.1})",
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
                "no heat demand yet ({} solved A/B plans, no measured heat in the pulled zones)",
                ab.n
            ),
        )
    } else {
        Verdict::insufficient(
            ab.n,
            &format!(
                "solved A/B plans; measured heat in pulled zones {:.1} kWh (PV {:.1} / NT {:.1} / VT {:.1})",
                total_heat, by_supply[0], by_supply[1], by_supply[2]
            ),
        )
    };
    let mean = |sum: f64, n: usize| (n > 0).then(|| round_to(sum / n as f64, 4));
    let charts = vec![
        Chart {
            title: "A/B: extra planned cost per plan (pull on minus pull off)".into(),
            y_unit: "EUR".into(),
            x: days.keys().cloned().collect(),
            series: vec![ChartSeries {
                name: "mean extra cost".into(),
                values: days.values().map(|d| mean(d.extra_cost, d.n)).collect(),
            }],
        },
        Chart {
            title: "A/B: extra planned heat per plan (pull on minus pull off)".into(),
            y_unit: "kWh".into(),
            x: days.keys().cloned().collect(),
            series: vec![ChartSeries {
                name: "mean extra heat".into(),
                values: days.values().map(|d| mean(d.extra_heat_kwh, d.n)).collect(),
            }],
        },
    ];
    let heat_table = Table {
        title: "Measured heat per pulled zone by supply (kWh)".into(),
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
            "mean live K·h below target",
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
            "Pulled zones: {}. A/B: each hour one plan is re-solved with the pull 0 on the same inputs (the live plan is the other arm, and both share the loop's slow-input cache, kernels, block-0 relay pin and load tally; the counterfactual re-reads the thermal state, prices and weather a moment later, so tiny differences are input noise); each plan covers the whole 36 h horizon, so the rows overlap and are averaged, not summed. The pull-off plan reports no K·h figure, so \"bought\" is measured in planned heat kWh and EUR; the K·h shown is the live plan's.",
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
    use crate::feature_data::PullSample;
    use std::collections::BTreeSet;

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
        samples: FeatureSamples,
        priority: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                rows: Vec::new(),
                samples: FeatureSamples::default(),
                priority: vec!["livingroom".into()],
            }
        }
        fn page(&self) -> FeaturesReport {
            let site = site();
            build(&FeaturesInput {
                now: utc("2026-10-13T12:00:00Z"),
                ledger_rows: &self.rows,
                samples: &self.samples,
                low_tariff_mask: std::array::from_fn(|h| !(8..20).contains(&h)),
                site: &site,
                priority_zones: &self.priority,
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
    fn registry_holds_only_the_features_still_under_evaluation() {
        let ids: BTreeSet<_> = FEATURES.iter().map(|f| f.id).collect();
        assert_eq!(ids, BTreeSet::from(["comfort_targets"]));
        let comfort = &FEATURES[0];
        assert_eq!(comfort.status, Status::Live);
        assert_eq!(comfort.live_since(), Some(utc("2026-10-11T07:47:00Z")));
        for f in FEATURES {
            for r in f.releases {
                assert!(r.at.is_none_or(|t| parse_utc(t).is_some()), "{}", f.id);
            }
        }
    }

    #[test]
    fn every_feature_is_reported_in_the_shape_the_ui_reads() {
        let page = Fixture::new().page();
        assert_eq!(page.features.len(), FEATURES.len());
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
        assert_eq!(first["kind_label"], "A/B live");
        assert_eq!(first["shas"], json!(["9a1431c"]));
        assert!(first["baseline"]["values"]["pull_eur_per_kh"].is_number());
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
        assert_eq!(list.len(), 8);
        for g in list {
            for key in ["name", "sha", "result"] {
                assert!(g[key].as_str().is_some_and(|t| !t.is_empty()), "{g}");
            }
        }
        assert_eq!(list[0]["sha"], "112ea92");
        let names: BTreeSet<_> = list.iter().map(|g| g["name"].as_str().unwrap()).collect();
        for graduated in [
            "PV intraday nowcast",
            "Event-based relay duty",
            "Terminal SoC at post-horizon prices",
        ] {
            assert!(names.contains(graduated), "{graduated}");
        }
    }

    // ---- comfort targets

    fn pull_sample(
        t: &str,
        with: f64,
        without: f64,
        heat_with: f64,
        heat_without: f64,
    ) -> PullSample {
        PullSample {
            t: utc(t),
            cost_with_eur: Some(with),
            cost_without_eur: Some(without),
            heat_with_kwh: BTreeMap::from([("livingroom".to_string(), heat_with)]),
            heat_without_kwh: BTreeMap::from([("livingroom".to_string(), heat_without)]),
            pull_kh_with: BTreeMap::from([("livingroom".to_string(), 40.0)]),
            ..Default::default()
        }
    }

    #[test]
    fn comfort_targets_says_off_when_no_room_carries_a_pull() {
        let mut fx = Fixture::new();
        fx.priority.clear();
        let f = fx.feature("comfort_targets");
        assert_eq!(f.verdict.state, VerdictState::Off);
        assert_eq!(
            f.verdict.text,
            "feature off: comfort.pull_eur_per_kh is 0 (or every room has priority 0)"
        );
    }

    #[test]
    fn comfort_targets_reports_no_heat_demand_yet() {
        let mut fx = Fixture::new();
        fx.samples.pull.push(PullSample {
            t: utc("2026-10-12T10:00:00Z"),
            skipped: Some("no heat planned".into()),
            identical: true,
            ..Default::default()
        });
        let f = fx.feature("comfort_targets");
        assert!(
            f.verdict.text.starts_with("no heat demand yet"),
            "{}",
            f.verdict.text
        );
        assert!(f.notes.iter().any(|n| n.contains("no heat planned: 1")));
    }

    #[test]
    fn comfort_targets_splits_measured_heat_by_supply_and_averages_the_ab() {
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
            block("2026-10-12T12:00:00Z", 0.5, 0.0), // PV-covered
            block("2026-10-12T12:15:00Z", 0.0, 0.8), // day tariff (VT)
            block("2026-10-12T23:00:00Z", 0.0, 0.8), // night tariff (NT)
        ];
        for h in 0..6 {
            fx.samples.pull.push(pull_sample(
                &format!("2026-10-12T{:02}:00:00Z", 10 + h),
                1.2,
                1.0,
                3.0,
                1.0,
            ));
        }
        let f = fx.feature("comfort_targets");
        assert_eq!(f.n, Some(6));
        let heat = &f.tables[1].rows;
        assert_eq!(heat.len(), 1, "only the pulled zone, not the kitchen");
        assert_eq!(heat[0][2..], [json!(0.5), json!(0.5), json!(0.5)]);
        assert!(
            f.verdict.text.starts_with(
                "the pull plans +2.00 kWh more heat for +0.200 EUR planned cost per plan (6 solved A/B plans, live plans sit 40 K·h below the targets)"
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
            block_supply(&r("2026-10-12T12:00:00Z", 0.5, 0.0), &mask, &site),
            Some(Supply::PvSurplus)
        );
        // PV present but the house still imports: tariff decides.
        assert_eq!(
            block_supply(&r("2026-10-12T12:00:00Z", 0.5, 0.5), &mask, &site),
            Some(Supply::Vt)
        );
        assert_eq!(
            block_supply(&r("2026-10-12T03:00:00Z", 0.0, 0.5), &mask, &site),
            Some(Supply::Nt)
        );
        assert_eq!(
            block_supply(&unscored("2026-10-12T03:00:00Z"), &mask, &site),
            None
        );
    }

    #[test]
    fn comfort_targets_counts_only_measured_heat_since_its_release() {
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
        fx.rows = vec![
            block("2026-10-11T07:30:00Z"),
            block("2026-10-12T12:00:00Z"),
            block("2026-10-12T22:00:00Z"),
        ];
        let f = fx.feature("comfort_targets");
        let heat = &f.tables[1].rows;
        assert_eq!(heat.len(), 1, "the pre-release block is left out");
        assert_eq!(heat[0][0], json!("2026-10-12"));
        assert_eq!(
            heat[0][2..],
            [json!(0.0), json!(0.5), json!(0.5)],
            "12:00 UTC is day tariff, 22:00 UTC night tariff"
        );
    }
}
