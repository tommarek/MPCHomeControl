//! The pure mapping from the MPC plan to per-controller [`ControlCommand`]s — IO-free and unit-tested.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use controller_protocol::{
    BatteryPayload, BatterySlot, ControlCommand, LoadChannel, LoxoneWrite, Payload, SCHEMA_VERSION,
};

use crate::config::PublisherConfig;
use crate::plan::{LatestResponse, TimelineBlock};

/// Parse the plan's `slot` string into the protocol enum. Unknown strings (and `"regular"`) map to
/// the safe self-consumption default.
pub fn parse_slot(slot: &str) -> BatterySlot {
    match slot {
        "charge_from_grid" => BatterySlot::ChargeFromGrid,
        "discharge_to_grid" => BatterySlot::DischargeToGrid,
        "sell_production" => BatterySlot::SellProduction,
        "battery_hold" => BatterySlot::BatteryHold,
        "inverter_off" => BatterySlot::InverterOff,
        _ => BatterySlot::Regular,
    }
}

/// A battery command whose plan block starts this far in the past is refused. Unlike the heating /
/// loxone writes (which describe "state now"), a battery command programs an explicit inverter
/// `slot_window` at the block's local HH:MM — re-issuing one for a long-past block would leave a
/// stale timeslot armed in the inverter. One block (900 s) + generous solve/poll/skew slack.
const MAX_BLOCK_AGE_SECONDS: i64 = 1200;

/// True when the plan's first block is too old to safely program the battery timeslot.
fn battery_block_stale(block_start: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - block_start).num_seconds() > MAX_BLOCK_AGE_SECONDS
}

/// One block's inputs, in the shape every payload builder needs — sourced from either `first_step`
/// (the CURRENT command, `apply_at: None`, built by [`commands`]) or `timeline[1]` (item G's NEXT
/// command, `apply_at: Some(block.t)`, built by [`next_commands`]). [`commands_for`] is the only place
/// that turns a block into payloads, so the two commands can never drift apart in how they're built —
/// the publisher test `next_command_payload_matches_what_current_would_build_for_the_same_block`
/// checks exactly that.
struct BlockInputs<'a> {
    t: DateTime<Utc>,
    heat_kw: &'a HashMap<String, f64>,
    controllable_load_kw: &'a HashMap<String, f64>,
    slot: &'a str,
    export_enabled: bool,
    inverter_on: bool,
    charge_kw: f64,
    discharge_kw: f64,
    soc_kwh: Option<f64>,
    /// This block's planned EV charge power (kW) per charger NAME — `TimelineBlock::ev_charge_kw`
    /// (rework cycle 4, item 4). Sourced from the SAME block (`covering_block`'s pick, or the frozen
    /// `next_step`) as every other actuated field here, so it freezes exactly like `heat_kw`/
    /// `charge_kw` — unlike the old `api.data.ev[i].charge_kw[block_index]` lookup, which read a
    /// separate, NEVER-frozen array and could still change inside the freeze window even after the
    /// loop pinned everything else.
    ev_charge_kw: &'a HashMap<String, f64>,
}

/// Build the commands for the configured controllers from one block's inputs, addressed with the
/// given envelope fields. Shared by [`commands`] (current, `apply_at: None`, unchanged behaviour) and
/// [`next_commands`] (item G, `apply_at: Some(block.t)`).
fn commands_for(
    api: &LatestResponse,
    block: &BlockInputs,
    cfg: &PublisherConfig,
    seq: u64,
    apply_at: Option<DateTime<Utc>>,
    valid_until: DateTime<Utc>,
) -> Vec<(String, ControlCommand)> {
    let plan_id = api.computed_at.to_rfc3339();

    let envelope = |controller_id: &str, payload: Payload| ControlCommand {
        schema_version: SCHEMA_VERSION.to_string(),
        controller_id: controller_id.to_string(),
        issued_at: api.computed_at,
        block_start: block.t,
        valid_until,
        plan_id: plan_id.clone(),
        command_seq: seq,
        apply_at,
        payload,
    };

    let mut out = Vec::new();

    if let Some(b) = &cfg.battery {
        let payload = Payload::Battery(BatteryPayload {
            slot: parse_slot(block.slot),
            export_enabled: block.export_enabled,
            inverter_on: block.inverter_on,
            charge_kw: block.charge_kw,
            discharge_kw: block.discharge_kw,
            min_soc_kwh: b.min_soc_kwh,
            max_soc_kwh: b.max_soc_kwh,
            soc_kwh: block.soc_kwh,
        });
        out.push((b.controller_id.clone(), envelope(&b.controller_id, payload)));
    }

    if let Some(b) = &cfg.boiler {
        // One channel per controllable load, with the coming block's planned draw as the setpoint and
        // an `enabled` flag from the on-threshold (the load-shift on/off decision). A generic
        // `Payload::Load`, like the EV path — the boiler controller reads it.
        let mut channels: Vec<LoadChannel> = block
            .controllable_load_kw
            .iter()
            .map(|(name, &power_kw)| LoadChannel {
                channel: name.clone(),
                power_kw,
                enabled: power_kw > b.on_threshold_kw,
                target_c: None,
                target_soc: None,
            })
            .collect();
        channels.sort_by(|a, b| a.channel.cmp(&b.channel)); // deterministic order
        out.push((
            b.controller_id.clone(),
            envelope(&b.controller_id, Payload::Load { channels }),
        ));
    }

    if let Some(lx) = &cfg.loxone {
        // The unified Loxone datagram: map each wired plan field to its exact virtual-input key. The
        // controller is a generic writer, so adding a domain is a config row here, not a code change.
        let mut writes: Vec<LoxoneWrite> = Vec::new();
        if let Some(h) = &lx.heating {
            // Iterate the CONFIGURED zone keys, not the plan's heat_kw: a zone that drops out of
            // the plan (model/config release removed its marker or heating entry) must get an
            // explicit 0 — the loxone VI holds its last value and MPCActive stays alive through
            // the other zones, so omitting the write would leave that relay latched at its last
            // state (possibly ON) indefinitely, overriding native room control.
            for (zone, key) in &h.zone_keys {
                let power_kw = block.heat_kw.get(zone).copied().unwrap_or(0.0);
                writes.push(LoxoneWrite {
                    key: key.clone(),
                    value: f64::from(power_kw > h.on_threshold_kw), // relay 1/0
                });
            }
        }
        if let Some(e) = &lx.ev {
            // ALWAYS write the EV key — explicit 0 when nothing is schedulable. Omission cannot
            // "hand control back" on the unified path: MPCActive is a GLOBAL gate the heating keys
            // keep alive, the controller re-sends the last datagram every 10 s, and the VI holds
            // its last value — so a skipped write would latch the previous setpoint (e.g. 7 kW
            // mid-charge when the SoC feed went stale, or after an unplug) indefinitely. The cost:
            // while MPC is alive, an untracked/guest car sees EvChargePower=0 — Miniserver-side
            // logic must own that case (a per-domain MPCEvActive pulse is the richer alternative).
            let value = api
                .data
                .ev
                .iter()
                .find(|c| c.controllable_now)
                .and_then(|c| block.ev_charge_kw.get(&c.name).copied())
                .unwrap_or(0.0);
            writes.push(LoxoneWrite {
                key: e.power_key.clone(),
                value,
            });
        }
        writes.sort_by(|a, b| a.key.cmp(&b.key)); // deterministic order
        out.push((
            lx.controller_id.clone(),
            envelope(&lx.controller_id, Payload::Loxone { writes }),
        ));
    }

    out
}

/// item 1 (rework cycle 2, finding 1): the timeline block that COVERS `now` — `[t, t+dt)` — never
/// `first_step` blindly. Right after a quarter-hour mark (before the brain's own post-mark tick
/// lands, at second :20) `first_step`/`timeline[0]` is still the PRE-mark plan's OLD block, one that
/// just ended — publishing it as "current" is exactly the OFF-glitch finding 1 found. Search is over
/// TWO candidates: block 0 (`timeline[0]`), and block 1 — but block 1's candidate is `next_step`
/// when present, falling back to `timeline[1]` only for an older brain that predates that field
/// (item 3, rework cycle 2). This matters once `next_step` can be FROZEN (item 3): before a mark,
/// `next_step` and `timeline[1]` always agree (both mirror the tick's own fresh solve) so this
/// resolves to `timeline[0]` either way, today's behaviour, unchanged; right after a mark and before
/// the brain's re-plan, `next_step` may hold the FROZEN value while raw `timeline[1]` has already
/// drifted to the new tick's fresh (unfrozen) opinion — reading `next_step` keeps the current command
/// identical to what the controller already applied as the NEXT command at the mark, closing the
/// exact coherence gap `next_step`/`frozen` would otherwise reopen.
fn covering_block<'a>(
    timeline: &'a [TimelineBlock],
    next_step: Option<&'a TimelineBlock>,
    now: DateTime<Utc>,
) -> Option<(usize, &'a TimelineBlock)> {
    let candidates: [Option<&TimelineBlock>; 2] =
        [timeline.first(), next_step.or_else(|| timeline.get(1))];
    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(i, b)| b.map(|b| (i, b)))
        .find(|(_, b)| b.t <= now && now < b.t + Duration::minutes(i64::from(b.dt_minutes)))
}

/// Build the CURRENT commands for the configured controllers from one plan poll: sourced from
/// [`covering_block`] (item 1), not `first_step` blindly. `seq` is the producer's monotonic counter;
/// `now` is the publish instant (the deadman is `now + deadman_seconds`). When no block in
/// `timeline[0..2]` covers `now` (the plan is older than a block — e.g. a wedged loop, or a poll that
/// raced a large clock/plan skew), publishes NOTHING for this poll — every domain, heating included —
/// and logs it; the controllers simply keep repeating their last-applied value (no glitch) until a
/// fresher plan arrives.
pub fn commands(
    api: &LatestResponse,
    cfg: &PublisherConfig,
    seq: u64,
    now: DateTime<Utc>,
) -> Vec<(String, ControlCommand)> {
    let Some((_, cb)) = covering_block(&api.data.timeline, api.data.next_step.as_ref(), now) else {
        eprintln!(
            "[publisher] no timeline block covers now ({now}) — the plan is older than a block; \
             publishing nothing new (heating included) this poll, controllers keep their last value"
        );
        return Vec::new();
    };
    let block = BlockInputs {
        t: cb.t,
        heat_kw: &cb.heat_kw,
        controllable_load_kw: &cb.controllable_load_kw,
        slot: &cb.slot,
        export_enabled: cb.export_enabled,
        inverter_on: cb.inverter_on,
        charge_kw: cb.charge_kw,
        discharge_kw: cb.discharge_kw,
        soc_kwh: Some(cb.soc_kwh),
        ev_charge_kw: &cb.ev_charge_kw,
    };
    let valid_until = now + Duration::seconds(cfg.deadman_seconds.max(0));
    let mut out = commands_for(api, &block, cfg, seq, None, valid_until);

    if let Some(b) = &cfg.battery {
        if battery_block_stale(cb.t, now) {
            eprintln!(
                "[publisher] battery block_start {} is >{}s old — skipping the battery command \
                 (stale timeslot); the controller will deadman-revert",
                cb.t, MAX_BLOCK_AGE_SECONDS
            );
            out.retain(|(id, _)| id != &b.controller_id);
        }
    }
    out
}

/// Build the NEXT commands (item G) from the plan's `next_step` — item 3 (rework cycle 2, findings
/// 5/2): sourced from `next_step`, NOT `timeline[1]` directly, and emitted ONLY when
/// `next_step.frozen` is `true` — the brain's pre-mark freeze window has pinned it to the value the
/// FIRST tick inside that window decided, held for the rest of the window regardless of what a later
/// tick's fresh solve says (see `TimelineBlock::frozen`'s doc on the brain side). Before item 3 this
/// function could promote a value a later tick would have decided differently, so what the controller
/// applied at the mark could silently diverge from what the loop itself latches at rollover; gating on
/// `frozen` makes the two structurally identical — the SAME frozen value is both what gets promoted
/// here and what `mpc_loop`'s rollover adopts. Outside the freeze window (or on an older brain that
/// predates this field) `frozen` is `false` and this returns empty — the controllers simply keep
/// repeating their last-applied value across the mark (no glitch), exactly like before item G existed.
///
/// `apply_at = Some(next_step.t)`, so a controller HOLDS each one pending and applies it only once its
/// own clock reaches that instant — never on receipt, never early. `valid_until = apply_at +
/// deadman_seconds` (item 5, rework cycle 2, finding 3) — the SAME deadman window the current command
/// uses, not `apply_at + the block's own duration` (900 s for a 15-min block): the earlier formula
/// stretched a promoted command's failsafe window from the configured ~120 s to a full 15 minutes,
/// delaying `MPCActive` handback / the Growatt revert by up to ~13 minutes after a brain/publisher
/// death right after a mark. This reuses the protocol's ordinary `accept`/deadman freshness check
/// rather than a second staleness rule (a controller that never got around to applying a next command
/// before it aged out simply drops it, the same fail-safe direction as every other freshness check in
/// this protocol). A newer poll's next command always supersedes an earlier one via its higher
/// `command_seq`.
///
/// Unlike [`commands`], there is no `MAX_BLOCK_AGE_SECONDS` battery-timeslot guard here: that guard
/// exists because a battery command programs an explicit inverter `slot_window`, and this function's
/// `valid_until` is already bounded to `deadman_seconds` (well under the block width in any sane
/// config — `PublisherConfig::validate` requires `deadman_seconds > poll_seconds`, and a poll cadence
/// wider than a block would make the whole next-command mechanism pointless), which is far tighter.
/// Empty when there is no `next_step` at all (a degenerate/very short horizon, or an older brain that
/// predates the field) or it isn't frozen — nothing safe to promote yet.
/// `now` guards the rework-cycle-3, rule-3 requirement: never re-publish a `/next` whose `apply_at`
/// is already in the past (the refuter's 06:15:18 stale re-send, found live on the dry-run pair —
/// a poll landing just after the mark, before this tick's own fresh plan has rolled the block
/// forward yet). Once `now >= nb.t` the block is due or overdue; [`commands`]'s own `covering_block`
/// already treats `next_step` as a covering candidate regardless of `apply_at`, so it becomes the
/// CURRENT command on its own — publishing it again here as "next" would just be the redundant,
/// confusing re-send rule 3 forbids.
pub fn next_commands(
    api: &LatestResponse,
    cfg: &PublisherConfig,
    seq: u64,
    now: DateTime<Utc>,
) -> Vec<(String, ControlCommand)> {
    let Some(nb) = api.data.next_step.as_ref().filter(|ns| ns.frozen) else {
        return Vec::new();
    };
    if now >= nb.t {
        return Vec::new();
    }
    let block = BlockInputs {
        t: nb.t,
        heat_kw: &nb.heat_kw,
        controllable_load_kw: &nb.controllable_load_kw,
        slot: &nb.slot,
        export_enabled: nb.export_enabled,
        inverter_on: nb.inverter_on,
        charge_kw: nb.charge_kw,
        discharge_kw: nb.discharge_kw,
        soc_kwh: Some(nb.soc_kwh),
        ev_charge_kw: &nb.ev_charge_kw,
    };
    let valid_until = nb.t + Duration::seconds(cfg.deadman_seconds.max(0));
    commands_for(api, &block, cfg, seq, Some(nb.t), valid_until)
}

/// Rework cycle 3, rule 3: the publisher's own record of what it has ALREADY promoted as a given
/// block's CURRENT payload, per controller — kept across polls by the poll loop (`main.rs`). Once a
/// snapshot has been promoted for block k (`apply_at` reached, or a `/next` received with `apply_at
/// <= now < block end`), that snapshot is authoritative for block k's CURRENT command until k ends: a
/// later brain tick that re-decides the same block differently (the exact defect the refuter
/// demonstrated live — finding 1 — a brain not yet running rules 1/2, or a genuine race between this
/// tick's solve and the mark) can never reach a controller as a second, different actuation for a
/// block the mark already committed to. Only the PAYLOAD is pinned; the envelope (`issued_at`,
/// `valid_until`, `command_seq`) still refreshes every poll, so the deadman keeps advancing normally.
///
/// Keyed by block start (not a single slot) because [`next_commands`]'s promotion for the UPCOMING
/// block must be recordable before the mark, while the CURRENT block's own promotion (covering
/// `now`) still needs to stay live — both coexist for the ~2-minute freeze window each mark. The poll
/// loop prunes entries once their block has ended (`prune_before`) so this can't grow unbounded.
#[derive(Default)]
pub struct Promoted {
    blocks: HashMap<DateTime<Utc>, HashMap<String, Payload>>,
}

impl Promoted {
    /// Record (or refresh) this poll's promotion for `block_start` — called by the poll loop right
    /// after `next_commands()`, or right after `commands()`'s covering-block fallback when nothing
    /// was ever promoted for the block that's now due (rule 3's "applies only when no snapshot
    /// exists for the current block").
    pub fn record(&mut self, block_start: DateTime<Utc>, commands: &[(String, ControlCommand)]) {
        let entry = self.blocks.entry(block_start).or_default();
        for (id, cmd) in commands {
            entry.insert(id.clone(), cmd.payload.clone());
        }
    }

    /// Drop every recorded block strictly before `oldest_to_keep` (a block that has definitely
    /// ended) — bounds the map's size across a long-running process.
    pub fn prune_before(&mut self, oldest_to_keep: DateTime<Utc>) {
        self.blocks.retain(|&t, _| t >= oldest_to_keep);
    }
}

/// Apply this poll's [`Promoted`] record for `block_start` onto `out` (the fresh candidate
/// [`commands`]/[`next_commands`] output for that same block): a controller with a promoted payload
/// gets that payload's ACTUATION verbatim instead of whatever this poll's plan says for it now (see
/// [`merge_actuation`] — telemetry fields, e.g. `BatteryPayload::soc_kwh`, are taken from the fresh
/// candidate instead of frozen at the mark, rework cycle 4 item 2); a controller with none (added
/// after the promotion) keeps its freshly-built payload unchanged. A no-op when nothing was ever
/// promoted for `block_start` — the ordinary, unpinned output stands.
pub fn apply_promotion(
    out: &mut [(String, ControlCommand)],
    promoted: &Promoted,
    block_start: DateTime<Utc>,
) {
    let Some(payloads) = promoted.blocks.get(&block_start) else {
        return;
    };
    for (id, cmd) in out.iter_mut() {
        if let Some(p) = payloads.get(id) {
            cmd.payload = merge_actuation(p, &cmd.payload);
        }
    }
}

/// Merge the promoted snapshot's ACTUATION (`frozen`, wins — rule 3) with this poll's freshly-built
/// payload (`fresh`): non-actuation telemetry fields are taken from `fresh` instead of the frozen
/// snapshot, so a promoted block's outgoing command still carries the controller's current SoC
/// reading rather than one stamped at the mark and never updated for the rest of the block. The
/// ONLY such field today is [`BatteryPayload::soc_kwh`] — see [`Payload::actuation_eq`]'s doc for
/// why it's excluded from the same-block actuation comparison; this is the publisher-side mirror of
/// that exclusion (rework cycle 4, item 2). Every other payload kind has no telemetry field, so the
/// frozen snapshot is used verbatim.
fn merge_actuation(frozen: &Payload, fresh: &Payload) -> Payload {
    match (frozen, fresh) {
        (Payload::Battery(f), Payload::Battery(g)) => Payload::Battery(BatteryPayload {
            soc_kwh: g.soc_kwh,
            ..f.clone()
        }),
        _ => frozen.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BatteryPub, BoilerPub, LoxoneEvMap, LoxoneHeatingMap, LoxonePub, MqttConfig,
        PublisherConfig,
    };
    use std::collections::HashMap;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn api_json() -> LatestResponse {
        // A realistic /api/plan/latest envelope (extra fields present to prove they're ignored).
        let json = r#"{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {
                "total_cost_eur": 1.23,
                "first_step": {
                    "hour_start": "2026-06-23T12:00:00Z",
                    "heat_kw": { "livingroom": 2.4, "office": 0.0 },
                    "cool_kw": {},
                    "controllable_load_kw": { "water heat-pump": 2.0 },
                    "battery_charge_kw": 3.0,
                    "battery_discharge_kw": 0.0,
                    "grid_import_kw": 3.0,
                    "grid_export_kw": 0.0,
                    "mode": {
                        "slot": "charge_from_grid",
                        "export_enabled": false,
                        "inverter_on": true,
                        "charge_kw": 3.0,
                        "discharge_kw": 0.0
                    }
                },
                "timeline": [
                    { "t": "2026-06-23T12:00:00Z", "dt_minutes": 15, "soc_kwh": 6.1,
                      "slot": "charge_from_grid", "export_enabled": false, "inverter_on": true,
                      "charge_kw": 3.0, "discharge_kw": 0.0,
                      "heat_kw": { "livingroom": 2.4, "office": 0.0 },
                      "controllable_load_kw": { "water heat-pump": 2.0 },
                      "ev_charge_kw": { "garage": 3.6 } },
                    { "t": "2026-06-23T12:15:00Z", "dt_minutes": 15, "soc_kwh": 6.4,
                      "slot": "regular", "export_enabled": true, "inverter_on": true,
                      "charge_kw": 0.0, "discharge_kw": 1.2,
                      "heat_kw": { "livingroom": 0.0, "office": 1.8 },
                      "controllable_load_kw": { "water heat-pump": 0.0 },
                      "ev_charge_kw": { "garage": 0.0 } }
                ],
                "next_step": { "t": "2026-06-23T12:15:00Z", "dt_minutes": 15, "soc_kwh": 6.4,
                      "slot": "regular", "export_enabled": true, "inverter_on": true,
                      "charge_kw": 0.0, "discharge_kw": 1.2,
                      "heat_kw": { "livingroom": 0.0, "office": 1.8 },
                      "controllable_load_kw": { "water heat-pump": 0.0 },
                      "ev_charge_kw": { "garage": 0.0 }, "frozen": true },
                "ev": [
                    { "name": "garage", "controllable_now": true, "target_pct": 80.0 },
                    { "name": "street", "controllable_now": false, "target_pct": 90.0 }
                ]
            }
        }"#;
        serde_json::from_str(json).unwrap()
    }

    fn cfg() -> PublisherConfig {
        PublisherConfig {
            mpc_url: "http://x/api/plan/latest".into(),
            poll_seconds: 30,
            deadman_seconds: 120,
            max_plan_age_seconds: 900,
            armed: false,
            mqtt: MqttConfig::default(),
            battery: Some(BatteryPub {
                controller_id: "growatt".into(),
                min_soc_kwh: 2.0,
                max_soc_kwh: 10.0,
            }),
            boiler: None,
            loxone: None,
        }
    }

    #[test]
    fn builds_battery_command() {
        let now = utc("2026-06-23T12:00:05Z");
        let cmds = commands(&api_json(), &cfg(), 7, now);
        assert_eq!(cmds.len(), 1);

        let battery = &cmds.iter().find(|(id, _)| id == "growatt").unwrap().1;
        assert_eq!(battery.command_seq, 7);
        assert_eq!(battery.plan_id, "2026-06-23T12:00:00+00:00");
        assert_eq!(battery.valid_until, utc("2026-06-23T12:02:05Z")); // now + 120 s
        match &battery.payload {
            Payload::Battery(b) => {
                assert_eq!(b.slot, BatterySlot::ChargeFromGrid);
                assert_eq!(b.charge_kw, 3.0);
                assert!(!b.export_enabled && b.inverter_on);
                assert_eq!(b.min_soc_kwh, 2.0);
                assert_eq!(b.soc_kwh, Some(6.1)); // from timeline[0]
            }
            _ => panic!("expected a battery payload"),
        }
    }

    #[test]
    fn builds_boiler_load_command_from_controllable_loads() {
        let mut c = cfg();
        c.boiler = Some(BoilerPub {
            controller_id: "boiler".into(),
            on_threshold_kw: 0.05,
        });
        let cmds = commands(&api_json(), &c, 5, utc("2026-06-23T12:00:05Z"));
        let boiler = &cmds.iter().find(|(id, _)| id == "boiler").unwrap().1;
        match &boiler.payload {
            Payload::Load { channels } => {
                assert_eq!(channels.len(), 1);
                assert_eq!(channels[0].channel, "water heat-pump");
                assert_eq!(channels[0].power_kw, 2.0); // first block's planned draw
                assert!(channels[0].enabled); // 2.0 > 0.05
                assert_eq!(channels[0].target_soc, None);
            }
            _ => panic!("expected a load payload"),
        }
    }

    #[test]
    fn builds_unified_loxone_command_from_heating_and_ev() {
        let mut c = cfg();
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([
                    ("livingroom".to_string(), "MPCHeatObyvak".to_string()),
                    ("office".to_string(), "MPCHeatPracovna".to_string()),
                    // a zone with no key is simply not written
                ]),
            }),
            ev: Some(LoxoneEvMap {
                power_key: "EvChargePower".into(),
            }),
        });
        let cmds = commands(&api_json(), &c, 9, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                // Sorted by key: EvChargePower=3.6 (garage first block), MPCHeatObyvak=1
                // (livingroom 2.4 > 0.05), MPCHeatPracovna=0 (office 0.0).
                assert_eq!(writes.len(), 3);
                assert_eq!(writes[0].key, "EvChargePower");
                assert_eq!(writes[0].value, 3.6);
                assert_eq!(writes[1].key, "MPCHeatObyvak");
                assert_eq!(writes[1].value, 1.0);
                assert_eq!(writes[2].key, "MPCHeatPracovna");
                assert_eq!(writes[2].value, 0.0);
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    #[test]
    fn rejects_malformed_or_colliding_loxone_keys() {
        let base = |keys: Vec<(&str, &str)>, ev: Option<&str>| {
            let mut c = cfg();
            c.loxone = Some(LoxonePub {
                controller_id: "loxone".into(),
                heating: Some(LoxoneHeatingMap {
                    on_threshold_kw: 0.05,
                    zone_keys: keys
                        .into_iter()
                        .map(|(z, k)| (z.to_string(), k.to_string()))
                        .collect(),
                }),
                ev: ev.map(|p| LoxoneEvMap {
                    power_key: p.to_string(),
                }),
            });
            c
        };
        // a delimiter in a zone key would be silently dropped by translate
        assert!(base(vec![("livingroom", "MPC;bad")], None)
            .validate()
            .is_err());
        // two zones mapped to the same virtual input collide in the datagram
        assert!(base(
            vec![("livingroom", "MPCHeatX"), ("office", "MPCHeatX")],
            None
        )
        .validate()
        .is_err());
        // an empty ev power_key would vanish
        assert!(base(vec![("livingroom", "MPCHeatObyvak")], Some(""))
            .validate()
            .is_err());
        // a clean config passes
        assert!(
            base(vec![("livingroom", "MPCHeatObyvak")], Some("EvChargePower"))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn loxone_omits_zones_without_a_key() {
        let mut c = cfg();
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            // only livingroom is mapped; office (also in the plan's heat_kw) is intentionally absent
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([("livingroom".to_string(), "MPCHeatObyvak".to_string())]),
            }),
            ev: None,
        });
        let cmds = commands(&api_json(), &c, 1, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(writes.len(), 1);
                assert_eq!(writes[0].key, "MPCHeatObyvak");
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    #[test]
    fn omits_a_controller_when_unconfigured() {
        let c = cfg();
        let cmds = commands(&api_json(), &c, 1, utc("2026-06-23T12:00:05Z"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, "growatt");
    }

    #[test]
    fn slot_parsing_defaults_to_regular() {
        assert_eq!(parse_slot("regular"), BatterySlot::Regular);
        assert_eq!(parse_slot("inverter_off"), BatterySlot::InverterOff);
        assert_eq!(parse_slot("nonsense"), BatterySlot::Regular);
    }

    #[test]
    fn battery_command_skipped_when_block_start_is_stale() {
        // item 1: the covering-block search (`timeline[0..2]`) makes a FINE (15-min) block's own age
        // top out under 900s — always well under MAX_BLOCK_AGE_SECONDS (1200s) — so this guard can
        // only still fire for a genuinely-covering HOURLY block (a degenerate `fine_hours: 0` config;
        // block 1 is otherwise always fine by design). Block 0 here spans a full hour so `now` at
        // +1100s and +1300s both still COVER it (the same deltas the pre-item-1 test used, now
        // sourced from the covering block itself rather than a blindly-trusted `first_step`).
        let mut api = api_json();
        api.data.timeline = vec![TimelineBlock {
            t: utc("2026-06-23T12:00:00Z"),
            dt_minutes: 60,
            soc_kwh: 6.1,
            slot: "charge_from_grid".into(),
            export_enabled: false,
            inverter_on: true,
            charge_kw: 3.0,
            discharge_kw: 0.0,
            heat_kw: HashMap::from([("livingroom".to_string(), 2.4)]),
            controllable_load_kw: HashMap::new(),
            ev_charge_kw: HashMap::new(),
            frozen: false,
        }];
        let mut c = cfg();
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([("livingroom".to_string(), "MPCHeatObyvak".to_string())]),
            }),
            ev: None,
        });
        let fresh = commands(&api, &c, 7, utc("2026-06-23T12:18:20Z")); // +1100 s, still covered
        assert!(fresh.iter().any(|(id, _)| id == "growatt"));
        let stale = commands(&api, &c, 7, utc("2026-06-23T12:21:40Z")); // +1300 s, still covered
        assert!(
            !stale.iter().any(|(id, _)| id == "growatt"),
            "battery command must be skipped for a >1200 s old covering block"
        );
        assert!(
            stale.iter().any(|(id, _)| id == "loxone"),
            "loxone (state-now) must still be emitted"
        );
    }

    /// item 1 / finding 1: right after a quarter-hour mark, a poll of the SAME (pre-mark) plan — the
    /// brain hasn't reticked yet — must build the CURRENT command from `timeline[1]` (the block that
    /// now covers `now`), not `timeline[0]`/`first_step` (the block that just ended). This is the
    /// publisher-level half of the fix the Refuter's `probe-D2-loxone-glitch.rs` demonstrated: without
    /// it, this exact poll re-publishes the OLD block's relay value and glitches the mechanical relay.
    #[test]
    fn post_mark_poll_before_the_brains_retick_uses_the_covering_block_not_first_step() {
        let mut c = cfg();
        c.battery = None;
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([("livingroom".to_string(), "MPCHeatObyvak".to_string())]),
            }),
            ev: None,
        });
        // api_json(): timeline[0] = [12:00,12:15) relay ON (2.4kW); timeline[1] = [12:15,12:30) relay
        // OFF (0.0kW) — first_step still mirrors timeline[0] (the brain hasn't reticked).
        let mark = utc("2026-06-23T12:15:00Z");
        let cmds = commands(&api_json(), &c, 1, mark + chrono::Duration::seconds(5));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(
                    writes
                        .iter()
                        .find(|w| w.key == "MPCHeatObyvak")
                        .unwrap()
                        .value,
                    0.0,
                    "must read timeline[1] (the covering block), not the stale first_step/timeline[0]"
                );
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    /// item 3 coherence: once `next_step` can be FROZEN, the covering-block search must read IT for
    /// the block-1 position, not raw `timeline[1]` — `mpc_loop` deliberately leaves an ordinary
    /// `timeline` row showing the tick's own fresh (possibly-diverged) solve and only overrides
    /// `next_step`. Reading the wrong one here would reopen exactly the brain/publisher divergence
    /// item 3 exists to close (finding 5): the post-mark CURRENT command would disagree with the NEXT
    /// command already applied at the mark.
    #[test]
    fn post_mark_poll_uses_the_frozen_next_step_not_a_diverged_raw_timeline1() {
        let mut api = api_json();
        // The later tick's raw timeline[1] "office" entry disagrees (0.0kW) with what's frozen into
        // next_step (1.8kW, api_json()'s default) — exactly what a drifting fresh solve inside the
        // freeze window looks like.
        if let Some(t1) = api.data.timeline.get_mut(1) {
            t1.heat_kw.insert("office".to_string(), 0.0);
        }
        let mut c = cfg();
        c.battery = None;
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([("office".to_string(), "MPCHeatPracovna".to_string())]),
            }),
            ev: None,
        });
        let mark = utc("2026-06-23T12:15:00Z");
        let cmds = commands(&api, &c, 1, mark + chrono::Duration::seconds(5));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(
                    writes
                        .iter()
                        .find(|w| w.key == "MPCHeatPracovna")
                        .unwrap()
                        .value,
                    1.0,
                    "must read the FROZEN next_step (1.8kW -> on), not the diverged raw timeline[1] \
                     (0.0kW -> off)"
                );
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    /// item 1(b): when NO block in `timeline[0..2]` covers `now` (the plan is older than a block),
    /// `commands()` publishes nothing at all for this poll — every domain, not just heating.
    #[test]
    fn commands_is_empty_when_no_timeline_block_covers_now() {
        let api = api_json(); // timeline spans only [12:00, 12:30)
        let cmds = commands(&api, &cfg(), 1, utc("2026-06-23T13:00:00Z")); // 30 min past the plan
        assert!(
            cmds.is_empty(),
            "no covering block must yield an empty command set: {cmds:?}"
        );
    }

    /// The EV write matrix: scheduled → setpoint; done-but-tracked → explicit 0; SoC-unknown → omitted.
    /// item 1: `timeline` now needs a block COVERING the tests' `now` (12:00:05) — `commands()` no
    /// longer reads `first_step` directly, so an empty `timeline` would find no covering block and
    /// build nothing at all.
    /// `ev_charge_kw` is the raw JSON object text for the covering timeline block's
    /// `ev_charge_kw` map (rework cycle 4, item 4 — the loxone EV write now reads THIS, keyed by
    /// charger name, not `chargers`' own `charge_kw` array), e.g. `"garage": 3.6`; pass `""` for
    /// no scheduled charge at all.
    fn ev_api(chargers: &str, ev_charge_kw: &str) -> LatestResponse {
        let json = format!(
            r#"{{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {{
                "first_step": {{
                    "hour_start": "2026-06-23T12:00:00Z",
                    "heat_kw": {{}},
                    "controllable_load_kw": {{}},
                    "mode": {{ "slot": "regular", "export_enabled": true, "inverter_on": true,
                              "charge_kw": 0.0, "discharge_kw": 0.0 }}
                }},
                "timeline": [ {{ "t": "2026-06-23T12:00:00Z", "dt_minutes": 15, "soc_kwh": 0.0,
                                 "slot": "regular", "export_enabled": true, "inverter_on": true,
                                 "charge_kw": 0.0, "discharge_kw": 0.0, "heat_kw": {{}},
                                 "controllable_load_kw": {{}},
                                 "ev_charge_kw": {{{ev_charge_kw}}} }} ],
                "ev": [{chargers}]
            }}
        }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn ev_write_matrix_on_the_unified_loxone_path() {
        let mut c = cfg();
        c.battery = None;
        // Scheduled charger → first-block setpoint.
        let scheduled = r#"{ "name": "a", "controllable_now": true, "charge_kw": [3.6], "target_pct": 80.0, "soc_pct": 55.0 }"#;
        // Target reached (empty plan): explicit 0 kW.
        let done = r#"{ "name": "b", "controllable_now": true, "charge_kw": [], "target_pct": 80.0, "soc_pct": 80.0 }"#;
        // Controllable but SoC unknown (empty plan, no soc): still an explicit 0 (see below).
        let unknown =
            r#"{ "name": "c", "controllable_now": true, "charge_kw": [], "target_pct": 80.0 }"#;

        // The scheduled charger drives the setpoint when present.
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: None,
            ev: Some(LoxoneEvMap {
                power_key: "EvChargePower".into(),
            }),
        });
        let sched = ev_api(scheduled, "\"a\": 3.6");
        let cmds = commands(&sched, &c, 1, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(
                    (writes[0].key.as_str(), writes[0].value),
                    ("EvChargePower", 3.6)
                );
            }
            _ => panic!("expected a loxone payload"),
        }
        let done_only = ev_api(done, "");
        let cmds = commands(&done_only, &c, 2, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(writes.len(), 1);
                assert_eq!(
                    (writes[0].key.as_str(), writes[0].value),
                    ("EvChargePower", 0.0)
                );
            }
            _ => panic!("expected a loxone payload"),
        }
        // The unified path ALWAYS writes the EV key — the SoC-unknown charger gets an explicit 0
        // (the VI holds its last value under a global MPCActive, so omission would latch the
        // previous setpoint; the untracked case is owned Miniserver-side).
        let unknown_only = ev_api(unknown, "");
        let cmds = commands(&unknown_only, &c, 3, utc("2026-06-23T12:00:05Z"));
        let lx = &cmds.iter().find(|(id, _)| id == "loxone").unwrap().1;
        match &lx.payload {
            Payload::Loxone { writes } => {
                assert_eq!(writes.len(), 1);
                assert_eq!(
                    (writes[0].key.as_str(), writes[0].value),
                    ("EvChargePower", 0.0),
                    "SoC-unknown charger gets an explicit 0 on the unified path"
                );
            }
            _ => panic!("expected a loxone payload"),
        }
    }

    #[test]
    fn validate_rejects_bad_cadence_ids_and_soc_band() {
        // deadman <= poll oscillates into failsafe every cycle.
        let mut c = cfg();
        c.deadman_seconds = 30;
        assert!(c.validate().is_err(), "deadman <= poll must be rejected");
        // max_plan_age below 3x poll gates fresh plans as stale.
        let mut c = cfg();
        c.max_plan_age_seconds = 60;
        assert!(
            c.validate().is_err(),
            "max_plan_age < 3x poll must be rejected"
        );
        // Duplicate controller ids race on one topic.
        let mut c = cfg();
        c.loxone = Some(LoxonePub {
            controller_id: "growatt".into(), // collides with the battery block
            heating: None,
            ev: None,
        });
        assert!(
            c.validate().is_err(),
            "duplicate controller_id must be rejected"
        );
        // Inverted SoC band.
        let mut c = cfg();
        c.battery = Some(BatteryPub {
            controller_id: "growatt".into(),
            min_soc_kwh: 11.0,
            max_soc_kwh: 10.0,
        });
        assert!(c.validate().is_err(), "min_soc > max_soc must be rejected");
    }

    // ---- item G: the NEXT command (built from timeline[1]) ----

    fn loxone_cfg() -> PublisherConfig {
        let mut c = cfg();
        c.loxone = Some(LoxonePub {
            controller_id: "loxone".into(),
            heating: Some(LoxoneHeatingMap {
                on_threshold_kw: 0.05,
                zone_keys: HashMap::from([
                    ("livingroom".to_string(), "MPCHeatObyvak".to_string()),
                    ("office".to_string(), "MPCHeatPracovna".to_string()),
                ]),
            }),
            ev: Some(LoxoneEvMap {
                power_key: "EvChargePower".into(),
            }),
        });
        c
    }

    #[test]
    fn next_command_apply_at_comes_from_next_step_and_valid_until_is_the_deadman() {
        let cmds = next_commands(&api_json(), &loxone_cfg(), 8, utc("2026-06-23T12:13:00Z"));
        assert_eq!(cmds.len(), 2); // battery + loxone
        for (_, cmd) in &cmds {
            // next_step.t = 12:15:00Z
            assert_eq!(cmd.apply_at, Some(utc("2026-06-23T12:15:00Z")));
            assert_eq!(cmd.block_start, utc("2026-06-23T12:15:00Z"));
            // item 5: apply_at + deadman_seconds (loxone_cfg()/cfg() sets 120s) -- NOT apply_at + the
            // block's own 15-minute duration (the pre-item-5 bug, finding 3).
            assert_eq!(cmd.valid_until, utc("2026-06-23T12:17:00Z"));
            assert_eq!(cmd.command_seq, 8);
        }
    }

    #[test]
    fn next_commands_empty_when_timeline_has_no_block_1() {
        // item 3: next_commands() is sourced from `next_step`, not `timeline`, so this now needs
        // `next_step` itself cleared (truncating `timeline` alone no longer starves it).
        let mut api = api_json();
        api.data.next_step = None;
        assert!(next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:13:00Z")).is_empty());
        api.data.timeline.clear();
        assert!(next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:13:00Z")).is_empty());
    }

    /// item 3: `next_commands()` emits nothing while `next_step` exists but isn't frozen yet (outside
    /// the brain's pre-mark freeze window) — the acceptance criterion the brief calls out by name.
    #[test]
    fn next_commands_empty_when_next_step_is_not_frozen() {
        let mut api = api_json();
        if let Some(ns) = api.data.next_step.as_mut() {
            ns.frozen = false;
        }
        assert!(
            next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:13:00Z")).is_empty(),
            "an unfrozen next_step must not be promoted"
        );
    }

    /// The publisher test the brief calls for: the next command's payload equals what the
    /// CURRENT-command builder (`commands`) would produce for that same block — i.e. `commands_for`
    /// is genuinely shared, not just coincidentally in agreement. Constructed by building a SECOND
    /// api whose `first_step`/`timeline[0]` are block 1's own values (so `commands()` builds "what
    /// current would look like for that block"), then comparing payloads (envelope fields like
    /// `apply_at`/`valid_until`/`block_start` legitimately differ and are excluded).
    #[test]
    fn next_command_payload_matches_what_current_would_build_for_the_same_block() {
        let api = api_json();
        let cfg = loxone_cfg();

        let as_if_current_json = r#"{
            "computed_at": "2026-06-23T12:00:00Z",
            "age_seconds": 4,
            "data": {
                "first_step": {
                    "hour_start": "2026-06-23T12:15:00Z",
                    "heat_kw": { "livingroom": 0.0, "office": 1.8 },
                    "controllable_load_kw": {},
                    "mode": { "slot": "regular", "export_enabled": true, "inverter_on": true,
                              "charge_kw": 0.0, "discharge_kw": 1.2 }
                },
                "timeline": [ { "t": "2026-06-23T12:15:00Z", "dt_minutes": 15, "soc_kwh": 6.4,
                                 "slot": "regular", "export_enabled": true, "inverter_on": true,
                                 "charge_kw": 0.0, "discharge_kw": 1.2,
                                 "heat_kw": { "livingroom": 0.0, "office": 1.8 },
                                 "controllable_load_kw": {} } ],
                "ev": [
                    { "name": "garage", "controllable_now": true, "charge_kw": [0.0], "target_pct": 80.0 },
                    { "name": "street", "controllable_now": false, "charge_kw": [0.0], "target_pct": 90.0 }
                ]
            }
        }"#;
        let as_if_current: LatestResponse = serde_json::from_str(as_if_current_json).unwrap();

        let current_for_block1 = commands(&as_if_current, &cfg, 1, utc("2026-06-23T12:15:00Z"));
        let next = next_commands(&api, &cfg, 1, utc("2026-06-23T12:13:00Z"));

        assert_eq!(current_for_block1.len(), next.len());
        for (id, cur_cmd) in &current_for_block1 {
            let (_, next_cmd) = next.iter().find(|(nid, _)| nid == id).unwrap();
            assert_eq!(
                cur_cmd.payload, next_cmd.payload,
                "payload for controller {id:?} must match the current-command builder's output \
                 for the same block"
            );
        }
    }

    #[test]
    fn next_commands_use_block_1_ev_charge_kw_not_block_0() {
        // garage: timeline[0].ev_charge_kw["garage"] = 3.6, next_step.ev_charge_kw["garage"] = 0.0
        // (rework cycle 4, item 4: read off the block's OWN ev_charge_kw map, not a separate
        // never-frozen per-charger array) — current (block 0) reads 3.6, next (block 1) reads 0.0.
        let cur = commands(&api_json(), &loxone_cfg(), 1, utc("2026-06-23T12:00:05Z"));
        let cur_lx = &cur.iter().find(|(id, _)| id == "loxone").unwrap().1;
        let Payload::Loxone { writes } = &cur_lx.payload else {
            panic!("expected loxone payload")
        };
        assert_eq!(
            writes
                .iter()
                .find(|w| w.key == "EvChargePower")
                .unwrap()
                .value,
            3.6
        );

        let next = next_commands(&api_json(), &loxone_cfg(), 1, utc("2026-06-23T12:13:00Z"));
        let next_lx = &next.iter().find(|(id, _)| id == "loxone").unwrap().1;
        let Payload::Loxone { writes } = &next_lx.payload else {
            panic!("expected loxone payload")
        };
        assert_eq!(
            writes
                .iter()
                .find(|w| w.key == "EvChargePower")
                .unwrap()
                .value,
            0.0,
            "next command must read charge_kw[1], not charge_kw[0]"
        );
    }

    #[test]
    fn next_battery_command_has_no_max_block_age_guard() {
        // Unlike `commands`, `next_commands` has no MAX_BLOCK_AGE_SECONDS check against `now` (it
        // doesn't even take `now`) — freshness is entirely `apply_at`/`valid_until`, checked by the
        // controller. A battery block is always included when configured, however "old" block 1's
        // start is relative to whenever this happens to be called.
        let cmds = next_commands(&api_json(), &loxone_cfg(), 1, utc("2026-06-23T12:13:00Z"));
        assert!(cmds.iter().any(|(id, _)| id == "growatt"));
    }

    /// Rework cycle 3, rule 3: never re-publish a `/next` whose `apply_at` has already passed — the
    /// refuter's 06:15:18 live case (a poll landing at/after the mark, before this tick's own solve
    /// has rolled `next_step` forward to the FOLLOWING block yet). `commands()`'s own covering-block
    /// logic already promotes the due block as CURRENT on its own.
    #[test]
    fn next_commands_does_not_republish_a_stale_apply_at() {
        let api = api_json(); // next_step.t = 12:15:00Z
        assert!(
            next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:15:00Z")).is_empty(),
            "apply_at == now (due) must not be republished as /next"
        );
        assert!(
            next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:15:18Z")).is_empty(),
            "apply_at in the past must not be republished as /next"
        );
        assert!(
            !next_commands(&api, &loxone_cfg(), 1, utc("2026-06-23T12:14:59Z")).is_empty(),
            "sanity: still due to publish a moment before apply_at"
        );
    }

    /// Rework cycle 3, rule 3 — the core mechanism: once a block's payload has been promoted (here,
    /// by recording `commands()`'s own first-ever output for a block, mirroring the "no snapshot
    /// exists yet, so the covering-block fallback IS the promotion" case), a LATER poll whose plan
    /// disagrees about the SAME block must still publish the ORIGINALLY promoted payload, not the
    /// diverged one — this is what stops the refuter's finding 1 (a brain not yet running rules 1/2
    /// re-deciding block 0 differently one tick after the mark) from ever reaching a controller as a
    /// second, different actuation.
    #[test]
    fn apply_promotion_overrides_a_later_diverged_plan_for_the_same_block() {
        let cfg = loxone_cfg();
        let now = utc("2026-06-23T12:00:05Z");
        let first = commands(&api_json(), &cfg, 1, now); // block 0, discharge_kw 0.0 (charge_from_grid)
        let block_start = first.first().unwrap().1.block_start;

        let mut promoted = Promoted::default();
        promoted.record(block_start, &first);

        // A later poll's plan disagrees about the SAME block 0 (a diverged re-solve, mirroring the
        // live 06:15:48 case: same block_start, different slot/discharge_kw).
        let mut diverged = api_json();
        diverged.data.timeline[0].slot = "discharge_to_grid".to_string();
        diverged.data.timeline[0].discharge_kw = 3.32;
        let mut second = commands(&diverged, &cfg, 2, now + Duration::seconds(48));
        assert_ne!(
            first
                .iter()
                .find(|(id, _)| id == "growatt")
                .unwrap()
                .1
                .payload,
            second
                .iter()
                .find(|(id, _)| id == "growatt")
                .unwrap()
                .1
                .payload,
            "sanity: the diverged plan really does build a different battery payload"
        );

        apply_promotion(&mut second, &promoted, block_start);
        assert_eq!(
            first
                .iter()
                .find(|(id, _)| id == "growatt")
                .unwrap()
                .1
                .payload,
            second
                .iter()
                .find(|(id, _)| id == "growatt")
                .unwrap()
                .1
                .payload,
            "the promoted payload must win over the later, diverged plan for the same block"
        );
        // The envelope keeps refreshing (deadman/seq) even though the payload is pinned.
        assert_eq!(second.first().unwrap().1.command_seq, 2);
    }

    /// `apply_promotion` is a no-op when nothing was ever recorded for the block in question.
    #[test]
    fn apply_promotion_is_a_noop_for_an_unpromoted_block() {
        let cfg = loxone_cfg();
        let now = utc("2026-06-23T12:00:05Z");
        let mut cmds = commands(&api_json(), &cfg, 1, now);
        let before = cmds.clone();
        let promoted = Promoted::default();
        let block_start = cmds.first().unwrap().1.block_start;
        apply_promotion(&mut cmds, &promoted, block_start);
        assert_eq!(before, cmds);
    }

    /// Rework cycle 4, item 5 — the acceptance test the brief named but was still missing: THREE
    /// CONSECUTIVE blocks, each with one genuinely distinct command, and for every one of them a
    /// later diverged re-solve for that SAME block that must not override what was already
    /// promoted. Proves `Promoted` correctly isolates blocks across a realistic multi-block rollover
    /// sequence — not just a single block in isolation (every other rule-3 test here) — i.e. a
    /// promotion recorded for block N+1 never disturbs block N's already-settled payload, and each
    /// block still ends up with its OWN distinct command rather than all three collapsing to one.
    #[test]
    fn three_consecutive_blocks_each_promote_exactly_one_distinct_command() {
        let make = |seq: u64, block_start: DateTime<Utc>, slot: BatterySlot| {
            (
                "growatt".to_string(),
                ControlCommand {
                    schema_version: SCHEMA_VERSION.to_string(),
                    controller_id: "growatt".to_string(),
                    issued_at: block_start,
                    block_start,
                    valid_until: block_start + Duration::seconds(120),
                    plan_id: "plan-1".to_string(),
                    command_seq: seq,
                    apply_at: None,
                    payload: Payload::Battery(BatteryPayload {
                        slot,
                        export_enabled: true,
                        inverter_on: true,
                        charge_kw: 0.0,
                        discharge_kw: 0.0,
                        min_soc_kwh: 2.0,
                        max_soc_kwh: 10.0,
                        soc_kwh: None,
                    }),
                },
            )
        };

        let b0 = utc("2026-06-23T12:00:00Z");
        let b1 = b0 + Duration::minutes(15);
        let b2 = b1 + Duration::minutes(15);
        // (block_start, this block's FIRST-promoted slot, a LATER diverged re-solve's slot).
        let blocks = [
            (
                b0,
                BatterySlot::ChargeFromGrid,
                BatterySlot::DischargeToGrid,
            ),
            (b1, BatterySlot::SellProduction, BatterySlot::Regular),
            (b2, BatterySlot::BatteryHold, BatterySlot::InverterOff),
        ];

        let mut promoted = Promoted::default();
        let mut settled = Vec::new();
        for (i, &(block_start, first_slot, diverged_slot)) in blocks.iter().enumerate() {
            let seq = i as u64 * 10 + 1;
            let mut first = vec![make(seq, block_start, first_slot)];
            apply_promotion(&mut first, &promoted, block_start); // nothing promoted yet: a no-op
            promoted.record(block_start, &first);

            let mut second = vec![make(seq + 1, block_start, diverged_slot)];
            apply_promotion(&mut second, &promoted, block_start);
            assert_eq!(
                second[0].1.payload, first[0].1.payload,
                "block {i} ({block_start}): a later diverged re-solve must not override the \
                 already-promoted payload"
            );
            settled.push(first[0].1.payload.clone());
        }

        // Each block genuinely settled on its OWN command — not all three collapsing to one.
        assert_ne!(settled[0], settled[1], "block 0 and block 1 must differ");
        assert_ne!(settled[1], settled[2], "block 1 and block 2 must differ");
        assert_ne!(settled[0], settled[2], "block 0 and block 2 must differ");
    }

    /// `Promoted::prune_before` drops a block once it has ended, so a long-running process's map
    /// can't grow forever.
    #[test]
    fn promoted_prune_before_drops_ended_blocks() {
        let cfg = loxone_cfg();
        let now = utc("2026-06-23T12:00:05Z");
        let cur = commands(&api_json(), &cfg, 1, now);
        let mut promoted = Promoted::default();
        let old_block = utc("2026-06-23T11:45:00Z");
        promoted.record(old_block, &cur);
        let new_block = utc("2026-06-23T12:00:00Z");
        promoted.record(new_block, &cur);
        promoted.prune_before(new_block);
        assert!(promoted.blocks.contains_key(&new_block));
        assert!(!promoted.blocks.contains_key(&old_block));
    }
}
