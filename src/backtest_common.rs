//! Pieces shared by the read-only real-history backtest tools (`terminal_backtest`,
//! `warmth_backtest`, `heating_backtest`, `solar_scale_backtest`).

use crate::optimize::config::ControlConfig;
use crate::rc_network::RcNetwork;
use crate::source::SourceClients;
use crate::state_space::StateSpace;

/// How long to pause between successive per-day InfluxDB windows — the backtests' version of
/// `what_if.rs`'s "never run unbounded/rapid-fire queries on the live server" rule.
pub(crate) const INTER_DAY_PAUSE_S: u64 = 2;

/// Build the slow plan inputs — a 7-day PV-calibration backtest plus the configured
/// `consumption_history_days` windowed-mean consumption training (`app::build_cache`), and the
/// kernel cache — ONCE, so two or more `current_plan` solves over the same tick see identical slow
/// inputs (apples-to-apples) instead of each independently re-running ~2.3 s of training queries.
pub(crate) async fn shared_plan_cache(
    db: &SourceClients,
    net: &RcNetwork,
    config: &ControlConfig,
    ss: &StateSpace,
) -> (
    crate::app::PlanCache,
    std::sync::Arc<crate::optimize::thermal::KernelSet>,
) {
    let cache = crate::app::build_cache(db, net, config, None).await;
    let kernels = std::sync::Arc::new(crate::app::build_kernel_cache(config, net, ss));
    (cache, kernels)
}
