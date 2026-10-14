# Spec: Room target temperatures editable from HA, with a yearly cost projection

Work dir: ~/.cache/fable-pipeline/comfort-targets-ui/  (scout.md, spec.md, tester/, refuter/)
Repos: brain = ~/git/MPCHomeControl (branch accuracy-2026-10), HA = ~/git/selfhosted (branch mpc-dashboard)

## Request (user's words)
"Could we add a feature of adjusting the [target] temps in ha UI. But also project the additional
costs/savings? Ideally over last year or something like that?"

## Problem & why
Every room's comfort target lives only in `config.json5` (`comfort.rooms.<zone>.heat.target` /
`.cool.target`). Changing one today = edit the file, restart the brain (deploy script), commit — we
did exactly that for the downstairs bathroom (21 → 22 °C, commit c678cd5) minutes before this
request. The owner wants to make that change from the phone-first HA room page, and to see what a
change costs or saves per year BEFORE committing to it, so comfort vs money is a visible trade-off
rather than a guess.

## Users & context
The house owner (and family) via the HA dashboard (`home.markovi.online`, behind HA login), phone
first. ~15 rooms. A change is rare (weeks apart), not interactive-scale.

## Decisions (decided by [user] unless noted)
1. **config.json5 stays the single source of truth.** The brain rewrites the LIVE file on the server
   and hot-reloads it. No separate override store. [user]
2. **Heat AND cool targets** are editable. Band widths (`below`/`above`/`delta`), `priority`,
   `windows` are not (MVP). [user]
3. **Projection = heat-loss model × last 12 months** of measured outside temperature and real prices,
   seconds to compute, labelled as a model estimate. Not a planner replay. [user]
4. **Changes are permanent until changed again** (no expiry). [user]
5. **Repo sync: pull live config before every deploy.** The deploy script refuses to ship a
   `config.json5.new` whose comfort targets differ from the live file unless the operator passes
   an explicit `ALLOW_CONFIG_DRIFT=1`; the documented procedure is: copy the live file into the
   repo, commit, deploy. [user]
6. **Targeted line edit, comments kept.** The brain changes only the `target:` number of the matching
   room line (one room per line in `comfort.rooms`), writes atomically (temp + rename), and
   re-parses + re-validates the whole file BEFORE the swap; on any failure the file is untouched
   and the API returns the error. [user]
7. **UI on the room page (Místnosti), per room card.** [user]
8. Write path = dashboard JS → HA service (`rest_command`, server side, same as the EV preference)
   → brain `POST`. The HA proxy stays GET-only. [lead; Scout: proxy.py has no POST]
9. Step 0.5 °C. Heat target clamp 15.0–26.0 °C, cool target clamp 20.0–30.0 °C, and the existing
   config validation (heat band below cool band etc.) still applies. [lead]
10. Projection cost basis = HOUSE BILL: only the extra heat that leaves the house (to outside +
    ground), `g_house` from the RC steady-state solve (research.md). `g_room` (the room's own heater)
    is exposed in the API but not shown. Interior rooms legitimately come out near zero. [user]
11. HA write path: one `rest_command.mpc_comfort_set_target` (zone, heat_target/cool_target) added
    to the panel's service allowlist for the `rooms` screen (controls.py DOMAIN_SERVICES gains a
    `rest_command` domain scoped to that one service name; panel JS enforces the same). [lead]
12. Projection baseline = the target currently in the file. Sign convention: positive = costs more
    per year, negative = saves. Shown in EUR/yr and CZK/yr. [lead]

## Behaviour

### Brain API (new)
- `GET /api/comfort` → per room: `zone, heat: {target, low, high}, cool: {target, low, high} | null,
  priority, file_target_heat/cool` (the same numbers; kept simple), plus `config_fingerprint`.
  Also the projection table (below) so the UI needs one read.
- `POST /api/comfort/<zone>` body `{ "heat_target": 22.0 }` and/or `{ "cool_target": 25.0 }`
  (`deny_unknown_fields`). Validates clamp + step, rewrites the line, re-parses + validates the full
  config; on success swaps the live `ControlConfig` (next MPC tick uses it — no restart), updates
  `config_fingerprint`, returns the room's new entry. Errors: 404 unknown zone; 422 out of range /
  wrong step / cool target set on a room with no `cool` block / validation failure (message from the
  validator); 500 if the file write fails (file untouched). Writes are serialised (one mutex).
- The `GET /api/version` `config_fingerprint` changes after a successful POST.
- Projection, part of `GET /api/comfort`: per room, `cost_per_k_eur_per_year` for +1 K on the heat
  target (and `cool_cost_per_k_eur_per_year` where a cool block exists AND an hvac unit serves the
  room; else null), the window used (`from`, `to`, hours of data), and `method: "heat_loss_model"`.
  The UI multiplies by the chosen ΔT (linear). Computed off the planning path, cached, refreshed at
  most daily (`MPC_COMFORT_PROJECTION_TTL` default 24 h); if data covers < 300 days the response
  carries `coverage_days` and the number is scaled to a year with a `partial: true` flag.

### Projection method (heat-loss model)
For room r and +1 K on its heat target, holding every other room at its own target:
- Steady-state Laplacian solve on the RC graph (research.md option A): fixed = outside, ground and
  the other heated rooms' air nodes; 1 W injected at r's `heating` marker nodes; `g_house` = flux
  reaching outside + ground per K of r's air. Use `g_house`. Unit W/K.
- Per hour h of the last 12 months with measured outside temp `T_out(h)`: the room heats only when
  the outside is below its balance point; use `heating_hour(h) = T_out(h) < T_target − T_balance_offset`
  where the offset comes from the configured internal gain / g_r (document it; zero gain → offset 0).
- heating_hour(h): `T_out(h) < T_t − (gain_r − g_gnd·(T_t − T_gnd))/g_out` (research.md)
- extra heat per hour = `g_house × 1 K × heating_hour(h)` (W → kWh/h). Electricity = heat ÷
  `heating.cop` (1.0 today, i.e. direct electric underfloor). Priced at that hour's real import price
  (`tariff_prices`: OTE spot + distribution, VT/NT) — read from `ote_prices`.
- Sum over the year → EUR/yr per +1 K. CZK via `tariff.eur_czk_rate`.
- Cooling, when applicable: same with cooling hours (`T_out > T_target`) and the unit's cooling COP.
- Caveat text in the API (`note`) and UI: "Model estimate from last year's weather and prices;
  the planner shifts heating to cheap hours, so the real cost is likely lower. ±20 %."
- Reads are bounded like the backtests (≤7-day chunks, one series at a time, paused) and run only
  on the daily refresh / first request, never on a tick. NO unbounded Influx query (memory:
  mpchc-influx-heavy-query-io-storm).

### HA dashboard (Místnosti room card)
- Shows "Cíl topení 22.0 °C" with −/+ steppers (0.5), and, where a cool block exists, "Cíl chlazení".
  Stepping does NOT write; it updates a preview line "≈ +180 Kč/rok (+7 €)" (or "− … ušetří") from
  the per-K number × ΔT, relative to the saved value.
- "Uložit" writes via the HA service and re-reads `/api/comfort`; on error shows the brain's message.
  "Zpět" discards. If the saved value differs from the one at page load (someone else changed it),
  the re-read wins.
- The projection block also shows the window ("podle počasí a cen 10/2025–10/2026") and the caveat.
- Czech labels, matches existing card style, phone width.
- Dashboard tests (`node dashboard_test.js`) cover: preview math and sign, stepper clamp, the error
  path, the "no cool block" case.

### Deploy procedure (CLAUDE.md)
- New step before a brain deploy: `scp` the live `config.json5` to the repo, `git diff`, commit if
  changed. `deploy-live-with-rollback.sh` compares the `comfort:` block of `config.json5.new` against
  the live file and aborts on drift unless `ALLOW_CONFIG_DRIFT=1`.

## Examples
- [user, today] bathroom `ground_bathroom` heat 21.0 → 22.0: POST `{"heat_target": 22.0}` → file line
  becomes `ground_bathroom:      { heat: { target: 22.0, below: 1.0, above: 3.0 }, cool: …` with
  the rest of the line and every comment in the file byte-identical; next tick plans to 22.
- [lead] POST `{"heat_target": 22.3}` → 422 "heat_target must be a multiple of 0.5".
- [lead] POST `{"heat_target": 14}` → 422 "heat_target must be within 15.0–26.0".
- [lead] POST `{"cool_target": 24}` on `storage` (no cool block) → 422.
- [lead] POST on `nonexistent` → 404.
- [lead, revised after research] Projection: a 6 m² bathroom with g_house ≈ 5–8 W/K, ~4000 heating
  hours/yr, mean heating-hour price 0.20 EUR/kWh → ≈ 4–7 EUR/yr per +1 K; the living room (large
  exterior envelope) tens of EUR; an interior toilet near 0. Numbers must be this order.

## Acceptance criteria
1. `POST /api/comfort/ground_bathroom {"heat_target": 21.5}` on a copy of the repo config.json5
   changes exactly that number; `diff` shows one line, comments intact; `ControlConfig::load` of the
   result succeeds; `GET /api/comfort` and the next plan use 21.5 (test: `current_plan` band).
2. Invalid writes (step, range, unknown zone, cool on a room without cool, a value that fails config
   validation, e.g. heat above cool) return 4xx with a message and leave the file byte-identical.
3. A write that fails at the filesystem level leaves the original file intact (temp+rename), 500.
4. `config_fingerprint` in `/api/version` changes after a successful write; the MPC loop's next tick
   logs that it reloaded the comfort block (one log line) and plans with the new target.
5. `GET /api/comfort` returns the per-room per-K yearly projection, computed from ≥ 300 days of
   measured outside temps + prices when available, cached (second call is instant), and never
   runs on the tick path. The read uses ≤7-day chunks.
6. The projection for each heated room is ≥ 0, finite, and within an order of magnitude of the
   sanity figures above; a room with a large exterior envelope costs more per K than an interior
   one; a room with `max_heat_kw: 0` (storage) returns null with a reason. A unit test on the real
   model.json5 prints g_house/g_room per room and asserts g_house ≤ g_room and both finite.
7. HA room card: steppers, live preview (sign + CZK + EUR), Save → brain updated → card shows the
   saved value; error shown on a 422. `node dashboard_test.js` passes with new cases.
8. The HA `rest_command` for the comfort write exists; the proxy is unchanged (still GET-only).
9. Deploy guard: `deploy-live-with-rollback.sh` aborts when the live and new comfort blocks differ
   (unless `ALLOW_CONFIG_DRIFT=1`); CLAUDE.md documents "pull live config first".
10. `cargo test`, `cargo clippy --all-targets -D warnings`, `cargo fmt --check` clean. API docs
    (`docs/api.md`) and `docs/configuration.md` updated; `API_VERSION` unchanged (additive).

## Constraints
- Brain stays read-only toward the house: this is a config edit, not actuation. Never writes Influx.
- No unbounded Influx queries; projection reads bounded + chunked + off-path.
- Comments in config.json5 are preserved byte-for-byte outside the edited number.
- Czech UI text; phone width.
- No Claude attribution in commits.

## Non-goals
Editing band widths/priority/windows; expiring overrides; replaying the real planner for the
projection; editing model.json5; a dedicated summary page; cool targets on rooms without hvac units
(stored but shown as inert).

## Priorities when trading off
Safety of the live file (never corrupt / never lose comments) > correctness of the projection's
order of magnitude > simplicity > UI polish.

## Data sources (from research)
- Outside temperature: `db.read_zone_temperature_series("outside", …, "1h")` (config maps outside →
  teplomer/TC; NOT loxone/venek, which reads 44 °C in sun). Prices: `read_prices_chunked`. Chunks via
  `backtest_common::chunk_windows` with `INTER_DAY_PAUSE_S` between chunks.

## Assumptions (unconfirmed)
- Each `comfort.rooms` entry is and stays one line; the brain refuses (422, explains) otherwise.
- ≥ 12 months of `temperature_outside` and `ote_prices` exist in Influx (if not, partial flag).
- Internal gains for the balance point come from `heating.zones.<zone>.internal_gain_w`.
- `heating.cop` 1.0 applies to every heated room.

## How the user will verify
Open Místnosti on the phone, step the bathroom to 22.5, see "≈ +… Kč/rok", save, check
`/api/version` fingerprint changed and the next plan's bathroom band; step back to 22.0.

## Tier: full · Risk: HIGH
(prod config file rewritten by the brain; a corrupt config = brain won't start after a restart)
