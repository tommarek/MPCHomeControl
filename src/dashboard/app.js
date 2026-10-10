/* MPC Home Control — dashboard app. Vanilla JS, hash routing, polling, ECharts. Read-only. */
'use strict';

// ---------- tiny helpers ----------
const $ = (sel, root = document) => root.querySelector(sel);
const h = (html) => { const t = document.createElement('template'); t.innerHTML = html.trim(); return t.content.firstChild; };
const clamp = (x, lo, hi) => Math.max(lo, Math.min(hi, x));
const css = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();
// Escape any backend/InfluxDB-sourced string before it goes into innerHTML (defence in depth — the
// data is the house's own internal feed, but never trust a string into markup).
const esc = (s) => { const d = document.createElement('div'); d.textContent = s == null ? '' : String(s); return d.innerHTML.replace(/"/g, '&quot;'); };

const fmt = {
  n: (x, d = 1) => (x == null || !isFinite(x)) ? '—' : x.toFixed(d),
  kw: (x, d = 2) => (x == null || !isFinite(x)) ? '—' : `${x.toFixed(d)}`,
  signedkw: (x, d = 2) => (x == null || !isFinite(x)) ? '—' : `${x >= 0 ? '+' : ''}${x.toFixed(d)}`,
  temp: (x) => (x == null || !isFinite(x)) ? '—' : `${x.toFixed(1)}°`,
  pct: (x) => (x == null || !isFinite(x)) ? '—' : `${Math.round(x)}%`,
  czk: (x, d = 0) => (x == null || !isFinite(x)) ? '—' : `${x.toFixed(d)} Kč`,
  eur: (x, d = 2) => (x == null || !isFinite(x)) ? '—' : `€${x.toFixed(d)}`,
  hm: (iso) => { const d = new Date(iso); return isNaN(d) ? '—' : d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false }); },
  ago: (s) => s == null ? '—' : s < 90 ? `${Math.round(s)}s ago` : s < 5400 ? `${Math.round(s / 60)}m ago` : `${Math.round(s / 3600)}h ago`,
};

// loxone_smart_home's battery-action vocabulary (growatt_status.current_mode). Export-enabled is a
// SEPARATE toggle (shown independently), not a mode — see the dashboard's export badges.
const MODE = {
  regular:           { label: 'Normal',        color: '#93a1bd', desc: 'self-consumption — solar covers the house, battery as needed' },
  charge_from_grid:  { label: 'Charge (grid)', color: '#4f9cff', desc: 'charging the battery from cheap grid power' },
  discharge_to_grid: { label: 'Discharge',     color: '#fbbf24', desc: 'the battery is discharging to the grid' },
  sell_production:   { label: 'Selling',       color: '#34d399', desc: 'exporting surplus solar to the grid' },
  battery_hold:      { label: 'Hold',          color: '#a78bfa', desc: 'holding the battery for a pricier block' },
  inverter_off:      { label: 'Inverter off',  color: '#fb7185', desc: 'inverter paused — grid prices are negative' },
};
const modeOf = (slot) => MODE[slot] || MODE.regular;
const FP_ROOMS_EP = '/api/rooms';
// each room's whole plan, fetched only while the time bar is in the plan (fpLoadFullPlan)
const FP_ROOMS_FULL_EP = '/api/rooms?hours=48';
// the measured battery / PV and room temperatures over the bar's whole past (a day, and the window
// before its first step)
const FP_HISTORY_EP = '/api/history?hours=25';
const FP_SERIES_EP = '/api/zones/series?hours=25';
const modeLabel = (slot) => MODE[slot]?.label || slot || '—'; // raw string for an unmapped loxone mode
const modeLegend = () => Object.values(MODE).map((m) => `<span><i style="background:${m.color}33;border:1px solid ${m.color}"></i>${m.label}</span>`).join('');

// ---------- host adapter ----------
// The one seam between this app and whatever page embeds it. Served by the brain (the default) it is
// all same-origin; a host such as Home Assistant defines `window.MPC_HOST` BEFORE loading this file:
//   apiBase   prefix for every '/api/...' request ('' = same origin)
//   assetBase where '/static/<file>' assets (three.min.js, fp3d.js, floorplan.json) live ('/static')
//   fetch     (url, init) => Promise<Response> — HA passes hass.fetchWithAuth
//   hass      the host's Home Assistant state, or null — STATES ONLY: a minimal `{states}` object, so
//             read `.states` and call nothing else on it. The host keeps it current and fires a
//             'mpc-hass' CustomEvent on `window` (detail: the new hass) each time it changes
//   readOnly  true removes every write control (the EV preferences) and makes apiPost/apiDelete
//             refuse without a network call; the host sets it where it has its own controls (default false)
//   controlLink  {label, href} — what readOnly shows in place of the controls, as a link that opens in
//             the TOP window (out of the iframe); without it a plain "only on the brain dashboard" note
//   covers / pergola / history  the HA panel's shading and doors (display-only; see fpCoverMap): the
//             entity -> opening mapping, the pergola's footprint (or null) and an async recorder read
//   adapter   set to 1 by this file once the adapter is initialised, so a host can tell that this
//             app.js speaks the contract (an older app.js leaves it undefined)
// Missing fields fall back to the defaults. Every request goes through mpcApi / mpcAsset below.
const MPC_HOST_DEFAULTS = { apiBase: '', assetBase: '/static', fetch: (url, init) => window.fetch(url, init), hass: null, readOnly: false };
window.MPC_HOST = Object.assign({}, MPC_HOST_DEFAULTS, window.MPC_HOST);
window.MPC_HOST.adapter = 1;
const mpcHost = () => window.MPC_HOST;
const mpcUrl = (path) => mpcHost().apiBase + path; // '/api/x' -> apiBase + '/api/x' (also for plain <a href>s)
const mpcApi = (path, init) => mpcHost().fetch(mpcUrl(path), init);
const mpcAsset = (file) => `${mpcHost().assetBase}/${file}`;
const mpcHass = () => mpcHost().hass; // phase 2 reads the host's entity states here (display-only)

// ---------- API ----------
async function api(path) {
  try {
    const r = await mpcApi(path, { cache: 'no-store' });
    // 503 carries a MEANINGFUL body on /readyz ({ready:false, plan_available, last_tick_age_seconds})
    // — that is the entire point of the probe. Discarding it left `data` undefined, the caller kept
    // the last-good `{ready:true}`, and the status dot stayed green while the MPC loop was wedged:
    // the one condition the indicator exists to signal was the one it hid.
    if (r.status === 503) {
      // Carry `status` even when the body won't parse (a proxy-generated 503, a truncated
      // response): without it the object had no `data`, no `error` AND no `status`, so refresh()'s
      // fallback kept the previous `{ready:true}` and the dot stayed green through an outage.
      //
      // The body is surfaced as `data` ONLY when it is a real payload. Every data endpoint's 503 is
      // `{error: "...warming up"}`, and handing that object to a screen made `data || []` truthy —
      // so `.map()` threw and the whole screen rendered blank (silently, inside refresh()'s catch)
      // on every restart. /readyz's 503 body IS the payload, which is why it is kept.
      const j = await r.json().catch(() => undefined);
      const body = j && j.data !== undefined ? j.data : j;
      const payload = body && typeof body === 'object' && !Array.isArray(body) && body.error !== undefined
        ? undefined
        : body;
      return { ok: false, warming: true, status: 503, data: payload };
    }
    if (!r.ok) return { ok: false, status: r.status };
    const j = await r.json();
    // data endpoints use the {computed_at, age_seconds, data} envelope; probes return bare json.
    return { ok: true, data: j.data !== undefined ? j.data : j, age: j.age_seconds, at: j.computed_at };
  } catch (e) { return { ok: false, error: String(e) }; }
}
async function loadAll(paths) {
  const entries = await Promise.all(paths.map(async (p) => [p, await api(p)]));
  return Object.fromEntries(entries);
}
// The dashboard's only writable calls: set / clear an EV preference (the MPC persists it to its
// own file). When the server runs with MPC_API_TOKEN set, mutating calls need an X-MPC-Token
// header — prompt once, remember it locally, retry.
async function apiSend(method, path, body) {
  if (mpcHost().readOnly) return false; // an embedding host forbids writes: no request, no token prompt
  const doSend = (token) => mpcApi(path, {
    method,
    headers: {
      ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
      ...(token ? { 'X-MPC-Token': token } : {}),
    },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  try {
    let r = await doSend(localStorage.getItem('mpcApiToken') || '');
    if (r.status === 401) {
      const token = prompt('This MPC requires an API token for writes (MPC_API_TOKEN):');
      if (!token) return false;
      r = await doSend(token);
      if (r.ok) localStorage.setItem('mpcApiToken', token);
    }
    return r.ok;
  } catch (e) { console.error(method, e); return false; }
}
const apiPost = (path, body) => apiSend('POST', path, body);
const apiDelete = (path) => apiSend('DELETE', path);

// ---------- ECharts manager ----------
const charts = {};
function chart(id) {
  const dom = document.getElementById(id);
  if (!dom) return null;
  if (!charts[id]) charts[id] = echarts.init(dom, null, { renderer: 'canvas' });
  return charts[id];
}
function disposeCharts() { Object.values(charts).forEach((c) => c.dispose()); for (const k in charts) delete charts[k]; }
window.addEventListener('resize', () => Object.values(charts).forEach((c) => c.resize()));

function baseOption() {
  const muted = css('--muted'), border = css('--border'), surface = css('--surface-2'), text = css('--text');
  return {
    textStyle: { color: muted, fontFamily: css('--font') },
    grid: { left: 48, right: 52, top: 30, bottom: 36, containLabel: true },
    tooltip: { trigger: 'axis', confine: true, backgroundColor: surface, borderColor: border, textStyle: { color: text }, axisPointer: { lineStyle: { color: border } }, valueFormatter: (v) => typeof v === 'number' ? v.toFixed(2) : v },
    legend: { textStyle: { color: muted }, top: 0, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
    xAxis: { type: 'time', axisLine: { lineStyle: { color: border } }, axisLabel: { color: muted }, splitLine: { show: false } },
  };
}
const yAxis = (name, opts = {}) => Object.assign({ type: 'value', name, nameTextStyle: { color: css('--faint') }, axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } }, opts);
const grad = (hex) => new echarts.graphic.LinearGradient(0, 0, 0, 1, [{ offset: 0, color: hex + 'aa' }, { offset: 1, color: hex + '08' }]);

// A vertical "now" divider separating measured history (solid) from forecast (dashed) on a time
// axis; pass `true` to print a small "now" tag on it.
const nowMark = (labeled) => ({ silent: true, symbol: 'none', label: labeled ? { show: true, formatter: 'now', color: css('--faint'), fontSize: 10, position: 'insideEndTop' } : { show: false }, lineStyle: { color: css('--faint'), type: 'dashed', width: 1 }, data: [{ xAxis: Date.now() }] });
// Array payload from the store, or []. A 503 body (`{error: ...}`) is filtered out in api(), but
// keep the shape check here too: `data || []` was truthy for any object, so one `.map()` on a
// warming-up endpoint threw and blanked the whole screen inside refresh()'s catch.
const arrData = (store, key) => (Array.isArray(store[key]?.data) ? store[key].data : []);

// Measured history series ([[iso, value]]) from /api/history (or `ep`); [] when it has no data yet.
const histData = (store, key, ep = '/api/history') => store[ep]?.data?.[key] || [];

// item F: blocks past `horizon.fine_hours` are 1h wide (`dt_minutes: 60`) instead of 15 min, so the
// plan's block array is no longer uniformly spaced. Expand each such block into 4 flat 15-min
// sub-points (same values, quartered timestamps) so every chart below — built assuming a uniform
// 15-min grid (`blockEnd`, `modeBands`, every `tl.map((b) => [b.t, …])` series) — keeps working
// UNCHANGED: visually identical to one wide bar/segment, since the values don't change across the
// sub-points. Each expanded entry carries `_i`, its ORIGINAL block index, for a caller that needs
// to look up a separate parallel array (e.g. an EV charger's per-block schedule) by block position
// rather than by a field already on the block object itself. Text/number widgets (KPIs, the insight
// engine, the per-block table) read the RAW timeline instead — expanding there would inflate counts
// and durations.
function expandTimeline(tl) {
  const out = [];
  tl.forEach((b, i) => {
    const steps = Math.max(1, Math.round((b.dt_minutes ?? 15) / 15));
    const start = new Date(b.t).getTime();
    for (let k = 0; k < steps; k++) {
      out.push(Object.assign({}, b, { t: new Date(start + k * 15 * 60000).toISOString(), _i: i }));
    }
  });
  return out;
}

// item H: relay-driven (underfloor) heating switches a WHOLE 15-min block on or off — there is no
// such thing as "0.4 kW" on a mechanical relay. Only the solver's near-term fix-and-round window
// actually pins an integral decision; beyond it `heat_kw` is the relaxed LP's AVERAGE power over the
// block (see docs/api.md), which must never be shown as if it were a real, continuously-variable
// setpoint ("the plan shows 0.1 kW for room 1 etc it doesn't make sense" — the user's words). Pure
// and unit-tested (see dashboard_test.js) so the fix-and-round window's near-binary values and the
// far horizon's continuous ones share exactly one formula: `dutyPct`/`onBlocks` read as a clean 0 or
// 100% / 0 or 4 wherever `heat_kw` is already ~0 or ~max (the near term), and continuously otherwise.
// `kw` is the raw average, unrounded, for a tooltip/diagnostic that needs the real number.
// HVAC (`hvac_heat_kw`/`cool_kw`) modulates continuously and never goes through this — it keeps kW.
function relayDuty(heatKw, maxHeatKw) {
  const kw = isFinite(heatKw) ? heatKw : 0;
  if (!isFinite(maxHeatKw) || maxHeatKw <= 0) return { dutyPct: 0, onBlocks: 0, kw };
  const frac = clamp(kw / maxHeatKw, 0, 1);
  return { dutyPct: frac * 100, onBlocks: frac * 4, kw };
}

// Whether a RAW (unexpanded) timeline block's start falls inside the near-term window the brain
// actually PINS to an integral 0/max decision — used only to choose the tooltip's wording ("on"/"off"
// vs "N% duty"); the plotted duty-% value is the same `relayDuty` formula either side of this line,
// since a near-term block's heat_kw is already ~0/~max by construction. item 9 (rework cycle 2,
// finding 9): this is blocks 0 and 1 (30 minutes — `HEAT_COOL_PIN_BLOCKS` on the brain side, item G's
// covering-block current command and frozen-gated next command), NOT the wider `BINARY_HEAT_BLOCKS`
// (2 hours) window this used to claim — blocks 2..BINARY_HEAT_BLOCKS keep a real relay/mode variable
// but are never pinned to an extreme, so they can be genuinely fractional. `blockT`/`planStartT`
// accept anything `Date` does (an ISO string or an epoch-ms number, the latter being what an ECharts
// time-axis tooltip callback hands back).
function isNearTermBlock(blockT, planStartT) {
  if (blockT == null || planStartT == null) return false;
  const minutesIn = (new Date(blockT).getTime() - new Date(planStartT).getTime()) / 60000;
  return minutesIn < 30;
}

// item 9 (rework cycle 2, finding 9): the near-term "on"/"off" tooltip wording must match the
// PUBLISHER's actual relay rule (`on_threshold_kw`, default 0.05 kW — see docs/controllers.md), an
// ABSOLUTE kW threshold, not an arbitrary duty-PERCENTAGE cutoff: the dashboard previously said "off"
// below 50% duty while the publisher already switches the relay ON above ~2.5% (0.05 kW of a typical
// 2 kW circuit) — contradicting the actual actuated behaviour right in the near-term window where the
// wording claims to be exact. Every "is this zone heating" test in the dashboard goes through here, and
// src/rooms.rs HEATER_ON_KW holds the same value for the live rooms.
const HEAT_ON_KW = 0.05;
function isRelayOn(kw) {
  return isFinite(kw) && kw > HEAT_ON_KW;
}

// item M: classify ONE timeline block's heating for the on-period timeline. A 15-min ("fine")
// block is a real relay decision, near-binary by construction (docs/api.md), so any nonzero duty
// there reads as a clean on period. A wider (hourly) block is the relaxed LP's block-AVERAGE
// power — `n of 4 quarter-hours on` — so it's only "exact" at the extremes (0 or a full 4/4,
// which really is certain); 1-3 quarters is genuinely undecided until the block is re-planned at
// finer granularity, so it's flagged `exact: false` and never merged across a block boundary.
function heatingBlockClass(heatKw, maxHeatKw, dtMinutes) {
  const d = relayDuty(heatKw, maxHeatKw);
  if ((dtMinutes ?? 15) <= 15) {
    // item 9's rule again: "on" is the PUBLISHER's absolute kW cutoff, not a duty-percentage one.
    return isRelayOn(d.kw) ? { on: true, exact: true, kw: d.kw } : { on: false, kw: d.kw };
  }
  const quarters = clamp(Math.round(d.onBlocks), 0, 4);
  if (quarters <= 0) return { on: false, kw: d.kw };
  if (quarters >= 4) return { on: true, exact: true, kw: d.kw };
  return { on: true, exact: false, quarters, kw: d.kw };
}

// item M: one zone's RAW (unexpanded) timeline -> merged on-periods for the heating-schedule
// Gantt chart. Consecutive EXACT-on blocks (contiguous `end === next start`) merge into a single
// period; a fractional (1-3 quarters) hourly block always stands alone, flushing any open exact
// period first, since its own quarter-hour split isn't decided yet — merging it with a neighbour
// would draw a length that isn't real. Pure and unit-tested (dashboard_test.js) against the exact
// formula the chart renders.
function mergeOnPeriods(tl, zone, maxHeatKw) {
  const periods = [];
  let cur = null;
  const flush = () => { if (cur) periods.push(cur); cur = null; };
  (tl || []).forEach((b) => {
    const dt = b.dt_minutes ?? 15;
    const start = b.t;
    const end = new Date(new Date(start).getTime() + dt * 60000).toISOString();
    const cls = heatingBlockClass(b.heat_kw?.[zone] ?? 0, maxHeatKw, dt);
    if (!cls.on) { flush(); return; }
    if (cls.exact) {
      if (cur && cur.exact && new Date(cur.end).getTime() === new Date(start).getTime()) {
        cur.end = end; cur.minutes += dt; cur.kwh += cls.kw * (dt / 60);
      } else {
        flush();
        cur = { start, end, minutes: dt, kwh: cls.kw * (dt / 60), exact: true };
      }
    } else {
      flush();
      periods.push({ start, end, minutes: dt, kwh: cls.kw * (dt / 60), exact: false, quarters: cls.quarters });
    }
  });
  flush();
  return periods;
}

// item M: local-midnight day dividers, offset in from the API. `/api/version` doesn't carry the
// site's UTC offset today (only `Site::offset_at` server-side, not wired to any endpoint) and this
// build stays within the dashboard files — so this reads the VIEWING BROWSER's own live offset
// (DST-aware, never a hardcoded +2: the same "trust the Date object" convention `fmt.hm`/`xfmt`
// already use for every other local-time label in this file).
const siteOffsetMinutes = () => -new Date().getTimezoneOffset();
const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

// Every local midnight in [startMs, endMs], as {ms, label} (e.g. "Wed 24 Sep"). `offsetMin` is
// UTC -> local minutes (+120 for Prague summer). Pure: derives the calendar date from UTC getters
// on a shifted instant, so the result never depends on the RUNNING environment's own timezone —
// testable in plain Node regardless of the machine/CI's local TZ.
function localMidnights(startMs, endMs, offsetMin) {
  const day = 86400000, off = (offsetMin || 0) * 60000;
  if (!isFinite(startMs) || !isFinite(endMs) || endMs <= startMs) return [];
  const out = [];
  for (let t = Math.ceil((startMs + off) / day) * day - off; t <= endMs; t += day) {
    if (t < startMs) continue;
    const local = new Date(t + off);
    out.push({ ms: t, label: `${WEEKDAYS[local.getUTCDay()]} ${local.getUTCDate()} ${MONTHS[local.getUTCMonth()]}` });
  }
  return out;
}

// item L: /api/model/solar splits each surface's irradiance into beam (direct sun) and diffuse
// (sky light scattered by the atmosphere, present even when the sun is behind the surface — e.g. a
// north-facing wall at mid-morning) — "☀ on surfaces" used to sum both under one sun icon, reading
// as direct sun even on a purely diffuse-lit face. Non-finite inputs (missing/late API data) read as
// 0, never NaN in the label.
function solarSplitText(beamW, diffuseW) {
  const beam = isFinite(beamW) ? Math.max(0, beamW) : 0;
  const diffuse = isFinite(diffuseW) ? Math.max(0, diffuseW) : 0;
  return `${Math.round(beam + diffuse)} W — ${Math.round(beam)} W direct · ${Math.round(diffuse)} W diffuse sky`;
}

// `YYYY-MM-DD` for the given epoch ms, in the CALLER's local time zone (whatever `new Date()`
// resolves to — the browser's own clock). The dashboard is viewed from the house, so the browser's
// local zone IS the site's zone; this must match the server's own site-local `by_day[].date` (see
// docs/api.md) for "today"/"yesterday" to find the right row rather than silently reading nothing or
// the wrong day.
function localDateKey(ms) {
  const d = new Date(ms);
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}

// The decision ledger's Home-screen card: a last-24h per-mode planned-vs-measured table, today's
// and yesterday's planned-vs-realized cost, and the latest misses — built from `/api/ledger`'s
// `data` (envelope stripped by the caller). `nowIso` anchors the 24h window: `by_mode` in `data` is
// scoped to the whole `?days=` query (48h, for the yesterday comparison), not the 24h this card
// wants, so the mode table is re-aggregated here from the raw per-block `rows` instead of reusing
// the server's `by_mode`. Only SCORED, non-degraded, non-relaxed rows feed the table (both planned
// and measured sides) — mixing an unscored row's planned kWh into a total whose measured side only
// ever sees scored rows would bias the comparison toward "didn't happen" when it's really just
// "not scored yet" (same rule the server's own `summarize` applies). `today`/`yesterday` are looked
// up by the actual SITE-LOCAL calendar date (via `localDateKey`), not "the newest date(s) present in
// `by_day`" — a quiet night with no scored blocks yet must read as "no scored blocks yet", never as
// yesterday's numbers silently relabelled "today". No DOM — pure data in, data out (see
// `dashboard_test.js`).
function ledgerSummaryRows(data, nowIso) {
  const rows = Array.isArray(data?.rows) ? data.rows : [];
  const byDay = Array.isArray(data?.by_day) ? data.by_day : [];
  const misses = Array.isArray(data?.misses) ? data.misses : [];

  const now = new Date(nowIso).getTime();
  const cutoff = isFinite(now) ? now - 24 * 3600 * 1000 : -Infinity;
  const recent = rows.filter((r) => {
    const t = new Date(r.t).getTime();
    return isFinite(t) && t >= cutoff && t <= now && r.scored && !r.degraded && !r.relaxed;
  });

  const byMode = {};
  for (const r of recent) {
    const key = r.slot || '—';
    const acc = byMode[key] || (byMode[key] = {
      mode: key, n: 0,
      chargePlanned: 0, chargeMeasured: 0,
      dischargePlanned: 0, dischargeMeasured: 0,
      exportPlanned: 0, exportMeasured: 0,
    });
    acc.n += 1;
    const dtH = (r.dt_minutes || 0) / 60;
    acc.chargePlanned += (r.planned?.charge_kw || 0) * dtH;
    acc.dischargePlanned += (r.planned?.discharge_kw || 0) * dtH;
    acc.exportPlanned += (r.planned?.grid_export_kw || 0) * dtH;
    if (r.measured) {
      acc.chargeMeasured += r.measured.charge_kwh || 0;
      acc.dischargeMeasured += r.measured.discharge_kwh || 0;
      acc.exportMeasured += r.measured.export_kwh || 0;
    }
  }
  const modeRows = Object.values(byMode).sort((a, b) => b.n - a.n);

  const byDate = {};
  for (const d of byDay) byDate[d.date] = d;
  const todayKey = isFinite(now) ? localDateKey(now) : null;
  // Calendar arithmetic, not "now − 24 h": the hour after a spring-forward day would otherwise land
  // two calendar days back.
  const yesterdayKey = isFinite(now) ? localDateKey(new Date(new Date(now).setDate(new Date(now).getDate() - 1))) : null;
  const today = (todayKey && byDate[todayKey]) || null;
  const yesterday = (yesterdayKey && byDate[yesterdayKey]) || null;
  const latestMisses = misses.slice(0, 3);

  return {
    modeRows,
    today,
    yesterday,
    latestMisses,
    empty: modeRows.length === 0 && !today && !yesterday && latestMisses.length === 0,
  };
}

// One ledger day's cost line ("today"/"yesterday" in the card). `d.n_scored === 0` (or `d` itself
// missing — no entry at all for that site-local date yet) must say so honestly rather than printing
// a `planned_cost_czk` total of 0 next to "not yet scored", which reads as a real (if boring) outcome
// instead of "nothing to show". Pure except for the injected `fmt` (see `dashboard_test.js`).
function ledgerCostLine(label, d, fmt) {
  if (!d || !d.n_scored) {
    return `<div>cost ${label}: no scored blocks yet</div>`;
  }
  const realized = d.realized_cost_czk == null
    ? 'not yet scored'
    : `${fmt.czk(d.realized_cost_czk)} (${fmt.eur(d.realized_cost_eur)})`;
  return `<div>cost ${label}: planned ${fmt.czk(d.planned_cost_czk)} → realized ${realized}</div>`;
}

// Render the ledger card from the `/api/ledger?days=2` store entry — the only DOM-touching half of
// the card (see `ledgerSummaryRows` for the pure data shaping).
function renderLedgerCard(store) {
  const el = $('#ledger-card');
  if (!el) return;
  const data = store['/api/ledger?days=2']?.data;
  const s = ledgerSummaryRows(data || {}, new Date().toISOString());
  if (s.empty) {
    el.innerHTML = `<div class="insight">no scored blocks yet</div>`;
    return;
  }
  const kwhCell = (p, m) => `${fmt.kw(p, 1)} → ${fmt.kw(m, 1)} kWh`;
  const modeTable = s.modeRows.length
    ? `<table class="tbl">
        <thead><tr><th>Mode</th><th class="num">n</th><th class="num">Charge</th><th class="num">Discharge</th><th class="num">Export</th></tr></thead>
        <tbody>${s.modeRows.map((m) => `<tr>
          <td>${esc(modeLabel(m.mode))}</td><td class="num">${m.n}</td>
          <td class="num">${kwhCell(m.chargePlanned, m.chargeMeasured)}</td>
          <td class="num">${kwhCell(m.dischargePlanned, m.dischargeMeasured)}</td>
          <td class="num">${kwhCell(m.exportPlanned, m.exportMeasured)}</td>
        </tr>`).join('')}</tbody>
      </table>`
    : `<div class="insight">no scored blocks in the last 24 h</div>`;
  const missesHtml = s.latestMisses.length
    ? `<ul class="reasons" style="margin-top:10px">${s.latestMisses.map((x) => `<li>${fmt.hm(x.t)} ${esc(modeLabel(x.slot))} — ${esc(x.reason)}</li>`).join('')}</ul>`
    : '';
  el.innerHTML = `${modeTable}<div style="margin-top:10px">${ledgerCostLine('today', s.today, fmt)}${ledgerCostLine('yesterday', s.yesterday, fmt)}</div>${missesHtml}`;
}

// build markArea bands for consecutive same-slot blocks (for mode shading)
// `t` is the block START while the plan's predicted temp_c / soc_kwh are END-of-block values —
// chart or label a forecast value at its block END, or the whole curve reads 15 min early. Assumes
// a UNIFORM 15-min grid (reads the gap between entries 0 and 1) — always call this with an
// `expandTimeline`-expanded array, never the raw plan timeline.
function blockEnd(tl, t) {
  const width = tl.length > 1 ? new Date(tl[1].t) - new Date(tl[0].t) : 15 * 60000;
  return new Date(new Date(t).getTime() + width).toISOString();
}

// `t` is the block START, so a run must end at the START OF THE NEXT block — i.e. the end of its
// own last block. Ending at `tl[i-1].t` under-covered every run by one 15-min block and collapsed
// a SINGLE-block run to zero width, which ECharts draws as nothing at all: precisely the short
// charge_from_grid / discharge_to_grid / inverter_off windows the shading exists to make visible.
// Assumes a uniform 15-min grid like `blockEnd` — call with an `expandTimeline`-expanded array.
function modeBands(tl) {
  const width = tl.length > 1 ? new Date(tl[1].t) - new Date(tl[0].t) : 15 * 60000;
  const endOf = (i) => (tl[i] ? tl[i].t : new Date(new Date(tl[i - 1].t).getTime() + width).toISOString());
  const bands = []; let start = 0;
  for (let i = 1; i <= tl.length; i++) {
    if (i === tl.length || tl[i].slot !== tl[start].slot) {
      const c = modeOf(tl[start].slot).color;
      bands.push([{ xAxis: tl[start].t, itemStyle: { color: c + '14' } }, { xAxis: endOf(i) }]);
      start = i;
    }
  }
  return bands;
}

// item M: the real [start, end) instant covered by a timeline array, for the day-divider series.
// `uniformStepMs` overrides the last block's own `dt_minutes` — pass it for an `expandTimeline`d
// array, whose entries all carry a 15-min-uniform grid even when copied from a wider raw block.
function timelineSpanMs(tl, uniformStepMs) {
  if (!tl || !tl.length) return null;
  const start = new Date(tl[0].t).getTime();
  const last = tl[tl.length - 1];
  const step = uniformStepMs ?? (last.dt_minutes ?? 15) * 60000;
  return { start, end: new Date(last.t).getTime() + step };
}

// item M: an invisible series carrying local-midnight day dividers (+ weekday/date label, a
// subtle 22:00-06:00 night band) and, unless `opts.now === false`, the "now" line — for any
// time-axis chart. Appended LAST to `series` (like the 'mode' ribbon above) so it never shifts an
// existing `color:[...]` palette, since it never renders a visible line/legend entry itself
// (`data: []`, no `name`). `opts.now: 'labeled'` prints a small "now" tag on that line, matching
// the original `nowMark(true)`; `opts.uniformStepMs` is forwarded to `timelineSpanMs`.
function dayDividerSeries(tl, opts = {}) {
  const span = timelineSpanMs(tl, opts.uniformStepMs);
  const mids = span ? localMidnights(span.start, span.end, siteOffsetMinutes()) : [];
  const data = mids.map((m) => ({
    xAxis: m.ms,
    label: { show: true, formatter: m.label, color: css('--faint'), fontSize: 10, position: 'insideEndTop' },
    lineStyle: { color: css('--border'), type: 'solid', width: 1 },
  }));
  if (opts.now !== false) {
    data.push({
      xAxis: Date.now(),
      label: opts.now === 'labeled' ? { show: true, formatter: 'now', color: css('--faint'), fontSize: 10, position: 'insideEndTop' } : { show: false },
      lineStyle: { color: css('--faint'), type: 'dashed', width: 1 },
    });
  }
  const bands = (span ? mids : []).map((m) => [
    { xAxis: Math.max(span.start, m.ms - 2 * 3600000), itemStyle: { color: css('--surface-3') + '30' } },
    { xAxis: Math.min(span.end, m.ms + 6 * 3600000) },
  ]);
  return {
    type: 'line', data: [], silent: true, symbol: 'none', tooltip: { show: false },
    markLine: { silent: true, symbol: 'none', data },
    markArea: { silent: true, data: bands },
  };
}

// Tooltip for the plan/energy charts: rounded values with units, the block's battery mode, and
// confine:true so it can't slide off-screen on mobile. `tl` supplies the per-block mode by time.
// Series the charts draw MIRRORED below the axis (a stacked +/- pair); the tooltip shows their
// magnitude. Any other series keeps its sign — notably prices, which go genuinely negative here.
const INVERTED_SERIES = new Set(['Discharge', 'Export']);

function planTooltip(tl) {
  const blocks = tl.map((b) => [new Date(b.t).getTime(), b.slot]);
  const modeAt = (t) => {
    let slot = null, best = 9e5; // only label a mode within ~15 min (one block) of the hovered point
    for (const [bt, s] of blocks) { const d = Math.abs(bt - t); if (d < best) { best = d; slot = s; } }
    return slot;
  };
  const unit = (n) => /price/i.test(n) ? ' Kč/kWh' : /%/.test(n) ? ' %' : /soc/i.test(n) ? ' kWh' : ' kW';
  return {
    trigger: 'axis', confine: true,
    backgroundColor: css('--surface-2'), borderColor: css('--border'), textStyle: { color: css('--text') },
    axisPointer: { lineStyle: { color: css('--border') } },
    formatter: (ps) => {
      if (!ps || !ps.length) return '';
      const seen = new Set();
      const rows = ps
        .filter((p) => p.seriesName !== 'mode') // the mode ribbon explains itself via the header
        .filter((p) => Array.isArray(p.value) && p.value[1] != null && isFinite(p.value[1]))
        .filter((p) => !seen.has(p.seriesName) && seen.add(p.seriesName)) // measured + forecast share a name
        // `Discharge` / `Export` are plotted NEGATIVE on purpose (mirrored below the axis), so the
        // tooltip un-flips just those. Everything else keeps its sign — blanket Math.abs() showed a
        // negative spot price as positive, exactly when the sign is the whole story.
        .map((p) => {
          const v = INVERTED_SERIES.has(p.seriesName) ? Math.abs(p.value[1]) : p.value[1];
          return `${p.marker}${esc(p.seriesName)} <b>${v.toFixed(2)}${unit(p.seriesName)}</b>`;
        });
      const t = ps[0].axisValue;
      const when = Number.isFinite(t) ? fmt.hm(new Date(t).toISOString()) : (ps[0].axisValueLabel || '');
      const m = Number.isFinite(t) ? modeAt(t) : null;
      const head = `${when}${m ? ` · <b>${esc(modeLabel(m))}</b>` : ''}`;
      return `<div style="margin-bottom:3px">${head}</div>${rows.join('<br>')}`;
    },
  };
}

// ---------- price helpers ----------
// Split a per-block series into (real, placeholder) halves so charts can draw the synthetic
// pre-auction tail DASHED instead of presenting fiction as a published price. One overlap point
// keeps the line visually continuous at the boundary.
function splitByPlaceholder(tl, val) {
  const real = [], ph = [];
  let prevReal = false, lastReal = null;
  tl.forEach((b) => {
    const point = [b.t, val(b)];
    if (b.price_is_placeholder) {
      // Bridge: repeat the last real point at the boundary so the dotted tail connects to the
      // solid line. (Keyed on the previous block's KIND — an always-false `!ph.length` guard here
      // once made this dead code and the tail floated disconnected.)
      if (prevReal && lastReal) ph[ph.length - 1] = lastReal.slice();
      ph.push(point);
      real.push([b.t, null]);
      prevReal = false;
    } else {
      lastReal = point;
      real.push(point);
      ph.push([b.t, null]);
      prevReal = true;
    }
  });
  return { real, ph };
}

function priceStats(tl) {
  // Real published prices only: the unpublished tail of the horizon carries a synthetic placeholder
  // curve (`price_is_placeholder`), and letting its invented 0.04/0.10/0.18 shape into the terciles
  // skewed the Cheap/Normal/Expensive chip against fiction.
  const ps = tl.filter((b) => !b.price_is_placeholder).map((b) => b.import_price).filter((x) => isFinite(x));
  if (!ps.length) return null;
  const sorted = [...ps].sort((a, b) => a - b);
  return { min: sorted[0], max: sorted[sorted.length - 1], lo: sorted[Math.floor(sorted.length * 0.33)], hi: sorted[Math.floor(sorted.length * 0.66)] };
}
function priceLevel(price, st) {
  if (!st || price == null) return { label: '—', cls: 'chip' };
  if (price <= st.lo) return { label: 'Cheap', cls: 'chip green' };
  if (price >= st.hi) return { label: 'Expensive', cls: 'chip red' };
  return { label: 'Normal', cls: 'chip amber' };
}
// The CZK/EUR rate the PLAN was priced with. Preferred source: the plan's own `eur_czk_rate`
// (the exact tariff figure the optimizer used). Older brains without that field fall back to the
// ratio of the two cost totals, and — only when that division is meaningless (near-zero horizon
// cost) — to a last-resort constant 25, which can mis-state CZK figures on such a day; the field
// exists precisely to retire that path.
function czkRate(plan) {
  if (plan?.eur_czk_rate > 0) return plan.eur_czk_rate; // the rate the optimizer actually priced with
  const e = plan?.total_cost_eur, c = plan?.total_cost_czk;
  return (e && c && Math.abs(e) > 0.01) ? c / e : 25; // last resort, older brain only
}

// The comfort band in force RIGHT NOW. /api/zones resolves the daily override windows (night
// setback etc.) server-side with the optimizer's own `band_at`, so a zone gliding to its setback
// floor reads as "comfortable" instead of being flagged cold. Falls back to the static band on an
// older brain that doesn't send the resolved pair yet.
function bandNow(z) {
  if (!z) return { lo: null, hi: null };
  return {
    lo: z.t_min_now != null ? z.t_min_now : z.t_min,
    hi: z.t_max_now != null ? z.t_max_now : z.t_max,
  };
}

// A zone's overheat allowance (K above t_max_now it may bank slab heat into), or null when the
// zone has none configured — the single gate every overheat-aware render checks, so a zone
// without `overheat_c` never touches the new code paths below (identical output guaranteed by
// construction, not by coincidence).
function overheatCeiling(z) {
  return z && z.overheat_c > 0 ? z.t_max_boost_now : null;
}

function comfort(temp, z) {
  if (temp == null || !z) return { label: '', cls: '' };
  const { lo, hi } = bandNow(z);
  if (temp < lo - 0.1) return { label: 'cold', cls: 'red' };
  const boost = overheatCeiling(z);
  if (boost != null && temp > hi + 0.1 && temp <= boost + 0.1) return { label: 'banking heat', cls: 'gold' };
  if (temp > hi + 0.1) return { label: 'warm', cls: 'amber' };
  return { label: 'comfortable', cls: 'green' };
}

// Tiny inline-SVG sparkline of a measured [[iso, °C]] series — trend line + end dot only (the
// row's gauge beside it carries the comfort-band context). Returns '' when there's too little
// data to draw a line.
function sparkline(series, w = 144, h = 34) {
  // Keep only finite samples so a stray NaN/Infinity can never produce NaN SVG coordinates.
  const data = (series || []).filter((p) => Array.isArray(p) && Number.isFinite(p[1]));
  if (data.length < 2) return '';
  // Scale to the DATA alone. Forcing the comfort band into the y-domain flattened the line into
  // a fraction of the height and painted the rest as a solid band fill — the tiles read as green
  // blobs with no visible trend. The gauge beside the sparkline owns the band context now.
  const vals = data.map((p) => p[1]);
  let lo = Math.min(...vals), hi = Math.max(...vals);
  if (hi - lo < 0.5) { hi += 0.25; lo -= 0.25; } // keep a near-flat series from squashing to a bar
  const pad = 2;
  const px = (i) => pad + (i / (data.length - 1)) * (w - 2 * pad);
  const py = (v) => pad + (1 - (v - lo) / (hi - lo)) * (h - 2 * pad);
  const pts = data.map((p, i) => `${px(i).toFixed(1)},${py(p[1]).toFixed(1)}`).join(' ');
  const lx = px(data.length - 1), ly = py(data[data.length - 1][1]);
  // NB: --accent is not defined in style.css — a bare var(--accent) makes the stroke invalid and
  // the line silently invisible (only the dot rendered). Always keep the --blue fallback.
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true"><polyline points="${pts}" fill="none" stroke="var(--accent, var(--blue))" stroke-width="2" vector-effect="non-scaling-stroke"/><circle cx="${lx.toFixed(1)}" cy="${ly.toFixed(1)}" r="2.5" fill="var(--accent, var(--blue))"/></svg>`;
}
const nowBlock = (tl) => { const now = Date.now(); let i = 0; for (let k = 0; k < tl.length; k++) if (new Date(tl[k].t).getTime() <= now) i = k; return i; };

// ---------- insight engine: human "why" ----------
function insights(store) {
  const plan = store['/api/plan/latest']?.data;
  // Bail on an empty timeline too (a warming-up / 503 plan): nowBlock would return 0 and `tl[0]` be
  // undefined, so the "what & why" reasons would read from an all-default block and mislead.
  if (!plan || !plan.timeline?.length) return { headline: 'Warming up…', reasons: [] };
  const tl = plan.timeline;
  const i = nowBlock(tl), b = tl[i] || {};
  const fs = plan.first_step || {};
  const st = priceStats(tl);
  const rate = czkRate(plan);
  const lvl = priceLevel(b.import_price, st);
  const m = modeOf(fs.mode?.slot || b.slot);
  const reasons = [];

  // battery / grid rationale
  if (!fs.mode?.inverter_on) reasons.push('Inverter is paused — grid prices are deeply negative right now.');
  else if (b.charge_kw > 0.05 && b.grid_import_kw > 0.05) reasons.push(`Charging the battery from the grid while power is cheap (${fmt.czk(b.import_price * rate, 2)}/kWh).`);
  else if (b.charge_kw > 0.05) reasons.push('Storing surplus solar in the battery.');
  else if (b.grid_export_kw > 0.05) reasons.push(`Selling surplus solar to the grid (spot above the sell floor).`);
  else if (b.discharge_kw > 0.05) reasons.push('Discharging the battery to cover the house and avoid buying expensive grid power.');
  else reasons.push('Running on solar / battery — nothing to buy or sell right now.');

  // Export is an orthogonal toggle (settable in any mode), so call it out separately.
  if (fs.mode?.inverter_on && b.export_enabled === false) reasons.push('Grid export is held off this block (spot below the sell floor) — independent of the battery mode.');

  // pre-heating / comfort rationale
  const heatingZones = Object.entries(fs.heat_kw || {}).filter(([, kw]) => isRelayOn(kw)).map(([z]) => z);
  if (heatingZones.length) {
    const cheap = st && b.import_price <= st.lo;
    reasons.push(`Heating ${heatingZones.map((z) => z.replace(/_/g, ' ')).join(', ')}${cheap ? ' on cheap power — pre-warming the slab before prices rise.' : ' to hold the comfort band.'}`);
  } else {
    reasons.push('No heating needed — all rooms are coasting within their comfort band.');
  }

  // upcoming cheapest / most expensive window
  if (st) {
    const future = tl.slice(i);
    // Placeholder blocks are FICTION — pointing the user at "cheap power at 03:00" from the
    // synthetic pre-auction curve recommended a price the market never published.
    const real = future.filter((x) => !x.price_is_placeholder);
    const cheapest = real.reduce((a, x) => (x.import_price < a.import_price ? x : a), real[0]);
    if (cheapest && cheapest.t !== b.t) reasons.push(`Cheapest power coming up around ${fmt.hm(cheapest.t)} (${fmt.czk(cheapest.import_price * rate, 2)}/kWh).`);
  }

  // Escape the interpolated values; keep the <strong> scaffold literal.
  const headline = `${esc(m.label)} — ${esc(m.desc)}. Power is <strong>${esc(lvl.label.toLowerCase())}</strong> right now.`;
  return { headline, reasons, level: lvl, rate };
}

// ---------- routes ----------
const ROUTES = [
  { id: 'home',    name: 'Home',     ep: [FP_ROOMS_EP, '/api/live', '/api/plan/latest', '/api/state', '/api/zones', FP_SERIES_EP, FP_HISTORY_EP, '/api/ledger?days=2'] },
  { id: 'energy',  name: 'Energy',   ep: ['/api/plan/latest', '/api/live', '/api/history'] },
  { id: 'ev',      name: 'EV',       ep: ['/api/ev', '/api/plan/timeline'], cap: 'has_ev' },
  { id: 'heating', name: 'Heating',  ep: ['/api/plan/latest', '/api/state', '/api/zones'] },
  { id: 'house',   name: 'House',    ep: ['/api/model/topology', '/api/model/solar', '/api/state', '/api/zones', '/api/live'] },
  { id: 'model',   name: 'Model',    ep: ['/api/calibration/gains', '/api/forecast/validation'] },
  { id: 'features', name: 'New features', ep: ['/api/features'] },
  { id: 'system',  name: 'System',   ep: ['/api/version', '/api/plan/latest'] },
];
const routeById = (id) => ROUTES.find((r) => r.id === id) || ROUTES[0];

// ---------- New features screen: pure helpers ----------
const FEATURE_STATUS_CLASS = { live: 'green', staged: 'amber' };
const FEATURE_KIND_CLASS = { ab_live: 'green', ab_replay: 'amber' };
const FEATURE_VERDICT_CLASS = { helped: 'green', neutral: 'blue', worse: 'red', info: 'blue', insufficient: 'amber', off: '' };
const featureStatusClass = (status) => FEATURE_STATUS_CLASS[status] ?? '';
const featureKindClass = (kind) => FEATURE_KIND_CLASS[kind] ?? '';
const featureVerdictClass = (state) => FEATURE_VERDICT_CLASS[state] ?? '';
// "3 Oct 2026" (UTC, so the date matches the backend's UTC days); '—' for null/invalid.
function featureDate(iso) {
  const d = iso ? new Date(iso) : null;
  if (!d || isNaN(d)) return '—';
  return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`;
}
// One "3 Oct 2026 · 78f9c2d" label per release; a release without an instant shows the sha only.
function featureReleaseLabels(f) {
  const rel = Array.isArray(f.releases) && f.releases.length ? f.releases : (f.shas || []).map((sha) => ({ sha, at: null }));
  return rel.map((r) => (r.at ? `${featureDate(r.at)} · ${r.sha}` : String(r.sha)));
}
// "1 Oct 2026 · 112ea92" for a graduated feature; the sha alone when it has no date.
const featureGraduatedLabel = (g) => (g.date ? `${featureDate(g.date)} · ${g.sha}` : String(g.sha));
const featureHasValue = (v) => typeof v === 'number' && isFinite(v);
// A chart is drawable only if some series carries at least one number.
function featureChartEmpty(c) {
  return !c || !Array.isArray(c.x) || c.x.length === 0 || !Array.isArray(c.series) ||
    !c.series.some((s) => Array.isArray(s.values) && s.values.some(featureHasValue));
}
function featureTableEmpty(t) { return !t || !Array.isArray(t.rows) || t.rows.length === 0 || !Array.isArray(t.columns); }
// ECharts series data: numbers stay, everything else (null gap, NaN) becomes null so the line breaks.
function featureSeriesData(values) { return (values || []).map((v) => (featureHasValue(v) ? v : null)); }
// Table cell text: integers as is, other numbers to 3 significant digits (so 0.043 vs 0.251 and a
// -0.004 K delta stay distinguishable; 549.3 -> 549), null -> an em dash.
function featureCell(v) {
  if (v == null) return '—';
  if (typeof v === 'number') {
    if (!isFinite(v)) return '—';
    if (Number.isInteger(v)) return String(v);
    return Math.abs(v) >= 1000 ? v.toFixed(0) : String(Number(v.toPrecision(3)));
  }
  return String(v);
}
// A feature draws something iff it has a non-empty chart or table.
function featureHasData(f) { return (f.charts || []).some((c) => !featureChartEmpty(c)) || (f.tables || []).some((t) => !featureTableEmpty(t)); }

// ============================================================ FLOOR PLAN (Home screen card)
// The house as an architectural maquette at dusk: both floors drawn from /static/floorplan.json in one
// shared orientation (screen-up faces azimuth 50), each a slab with capped walls and glass, the upper
// floor lifted above the lower one. Rooms are tinted on one continuous temperature scale against their
// comfort band (cold blue below the floor, a calm teal-sage-sand inside the band, amber to red above the
// ceiling); the attic readings sit beside the plan as plain numbers. /api/rooms feeds every card. The
// projection is a pure affine map per floor, so stacked <-> flat is a matrix interpolation. The third view,
// 3D, is a real WebGL model in fp3d.js, fetched with three.js only when picked. Pure helpers first
// (unit-tested in dashboard_test.js), DOM code after.

const FP_MODE_KEY = 'mpc-floor-mode';
const FP_STALE_S = 21600;
const FP_THETA = -20 * Math.PI / 180;
const FP_SY = 0.52;
const FP_LIFT = 8.7;
const FP_GAP = 3.6;
const FP_SLAB = 0.42;
const FP_X_MIN = -3;
const FP_X_MAX = 4;

// ---- colour: OKLab / OKLCH, one continuous scale from cold through the band to hot ----
function fpToLab(hex) {
  const n = parseInt(hex.slice(1), 16);
  const lin = (c) => { c /= 255; return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); };
  const r = lin((n >> 16) & 255), g = lin((n >> 8) & 255), b = lin(n & 255);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
  ];
}
function fpFromLab(lab) {
  const [L, a, b] = lab;
  const l = Math.pow(L + 0.3963377774 * a + 0.2158037573 * b, 3);
  const m = Math.pow(L - 0.1055613458 * a - 0.0638541728 * b, 3);
  const s = Math.pow(L - 0.0894841775 * a - 1.291485548 * b, 3);
  const rgb = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  const enc = (c) => {
    c = Math.max(0, Math.min(1, c));
    return Math.round((c <= 0.0031308 ? 12.92 * c : 1.055 * Math.pow(c, 1 / 2.4) - 0.055) * 255);
  };
  return '#' + rgb.map((c) => enc(c).toString(16).padStart(2, '0')).join('');
}
function fpFromLch(L, C, h) { const r = h * Math.PI / 180; return fpFromLab([L, C * Math.cos(r), C * Math.sin(r)]); }
// Scale stops per theme, OKLCH [x, L, C, hue] along the scale position x (see fpScaleX): x < 0 is kelvin
// below the floor, 0..1 the position inside the band (floor .. ceiling), above 1 the warm excess (the
// overheat tier sits at 2.5). Hue falls steadily from blue through teal, sage and sand to
// amber and red, so a warmer room always reads warmer. Chroma is low inside the band (calm) and grows
// outward on each arm. Dark theme: the arms brighten outward (glow on a dark table); light theme: they
// darken.
function fpAnchors(theme) {
  if (theme === 'light') {
    return {
      stops: [[-3, 0.53, 0.155, 262], [-1.5, 0.73, 0.1, 249], [0, 0.885, 0.04, 215], [0.5, 0.9, 0.03, 160], [1, 0.895, 0.048, 92], [2.5, 0.79, 0.125, 70], [4, 0.6, 0.175, 33]],
      none: '#eceff4',
    };
  }
  return {
    stops: [[-3, 0.74, 0.13, 258], [-1.5, 0.58, 0.12, 250], [0, 0.45, 0.05, 218], [0.5, 0.455, 0.045, 160], [1, 0.47, 0.055, 92], [2.5, 0.62, 0.125, 68], [4, 0.69, 0.165, 35]],
    none: '#262d3c',
  };
}
// [L, C, hue] at scale position x (clamped to the scale), interpolated linearly in OKLCH.
function fpScaleLch(x, theme) {
  const S = fpAnchors(theme).stops;
  x = Math.min(FP_X_MAX, Math.max(FP_X_MIN, x));
  let i = 0;
  while (i < S.length - 2 && x > S[i + 1][0]) i++;
  const a = S[i], b = S[i + 1], f = (x - a[0]) / (b[0] - a[0]);
  return [1, 2, 3].map((k) => a[k] + (b[k] - a[k]) * f);
}
function fpScaleColor(x, theme) { const [L, C, h] = fpScaleLch(x, theme); return fpFromLch(L, C, h); }
// A stale reading keeps its lightness but loses most of its chroma.
function fpDesaturate(hex, keep) {
  const [L, a, b] = fpToLab(hex);
  return fpFromLab([L, a * keep, b * keep]);
}

// ---- room state: where the measured temperature sits against the comfort band ----
// kind: 'nosensor' | 'noband' | 'cold' | 'in' | 'boost' (above the ceiling but inside the overheat
// allowance: banking heat on purpose) | 'warm'. `d` is K outside the band (0 inside).
function fpRoomState(r) {
  const temp = r && r.measured && r.measured.temp_c != null ? r.measured.temp_c : null;
  const b = r && r.band ? r.band : null;
  const lo = b ? (b.t_min_now != null ? b.t_min_now : b.t_min) : null;
  const hi = b ? (b.t_max_now != null ? b.t_max_now : b.t_max) : null;
  const boost = b && b.overheat_c > 0 && b.t_max_boost_now != null ? b.t_max_boost_now : null;
  const age = r && r.measured ? r.measured.age_s : null;
  const stale = temp != null && age != null && age > FP_STALE_S;
  const base = { temp, lo, hi, boost, stale, failed: !!(r && r.sensor_failed) };
  if (temp == null) return { ...base, kind: 'nosensor', d: null };
  if (lo == null || hi == null) return { ...base, kind: 'noband', d: null };
  if (temp < lo - 0.1) return { ...base, kind: 'cold', d: temp - lo };
  if (temp > hi + 0.1) {
    if (boost != null && temp <= boost + 0.1) return { ...base, kind: 'boost', d: temp - hi };
    return { ...base, kind: 'warm', d: temp - hi };
  }
  return { ...base, kind: 'in', d: 0 };
}
// The room's position on the colour scale, null when it has no sensor or no band. Inside the band it is
// the fraction from floor to ceiling; the overheat tier [ceiling, boost ceiling] sits on one step (2.5).
// No warm room lands within 0.5 of that step, so banking heat never shares a colour with a real
// overheat: without an allowance, k kelvin over the ceiling is 1 + k up to 2, then 3 + (k - 1); above
// a boost ceiling it is 3 + the excess. The colour never gets milder as the temperature rises.
function fpScaleX(st) {
  if (st.kind === 'cold') return Math.max(FP_X_MIN, st.d);
  if (st.kind === 'in') return st.hi > st.lo ? Math.min(1, Math.max(0, (st.temp - st.lo) / (st.hi - st.lo))) : 0.5;
  if (st.kind === 'boost') return 2.5;
  if (st.kind === 'warm') return Math.min(FP_X_MAX, st.boost != null ? 3 + Math.max(0, st.temp - st.boost) : st.d < 1 ? 1 + st.d : 2 + st.d);
  return null;
}
function fpFill(st, theme) {
  const x = fpScaleX(st);
  if (x == null) return fpAnchors(theme).none;
  const hex = fpScaleColor(x, theme);
  return st.stale ? fpDesaturate(hex, 0.15) : hex;
}
// The legend bar: x mapped to a percentage, the band in the middle 40 %.
function fpScalePct(x) {
  if (x < 0) return (x - FP_X_MIN) / -FP_X_MIN * 30;
  if (x <= 1) return 30 + x * 40;
  return 70 + (x - 1) / (FP_X_MAX - 1) * 30;
}
function fpStatusPhrase(st) {
  const k1 = (x) => Math.abs(x).toFixed(1);
  let s;
  if (st.kind === 'nosensor') return st.failed ? 'sensor read failed' : 'no sensor';
  if (st.kind === 'noband') s = 'no comfort band';
  else if (st.kind === 'cold') s = `${k1(st.d)} K below floor`;
  else if (st.kind === 'warm') s = `${k1(st.d)} K above ceiling`;
  else if (st.kind === 'boost') s = `banking heat, ${k1(st.d)} K above ceiling`;
  else s = 'in comfort band';
  return st.stale ? `${s}, stale reading` : s;
}
function fpAgo(s) {
  if (s == null || !isFinite(s)) return '';
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 172800) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}
// Where the rooms' heating comes from: 'now' (the committed plan), 'plan' (a later block) or 'past'
// (measured), as fpRoomAt stamped them.
function fpHeatSource(rooms) {
  const r = (rooms || []).find((x) => x && x.at);
  return r ? r.at.kind : 'now';
}
function fpAria(r) {
  const st = fpRoomState(r);
  const t = st.temp == null ? '' : `, ${st.temp.toFixed(1)} °C`;
  const heat = fpHeatSource([r]) === 'past' ? ', heating (measured)' : ', heating per plan';
  return `${r.name}${t}, ${fpStatusPhrase(st)}${r.heating && r.heating.on ? heat : ''}`;
}
// One plain-words line: "3 rooms heating, 1 below its floor".
function fpHouseSummary(rooms) {
  let heating = 0, cold = 0, warm = 0, stale = 0, ok = 0;
  for (const r of rooms || []) {
    const st = fpRoomState(r);
    if (r.heating && r.heating.on) heating++;
    if (st.stale) stale++;
    if (st.kind === 'cold') cold++;
    else if (st.kind === 'warm') warm++;
    else if (st.kind === 'in' || st.kind === 'boost') ok++;
  }
  const past = fpHeatSource(rooms) === 'past';
  // null heating is unknown (no plan block, or nothing recorded), never "off"
  const none = !(rooms || []).some((r) => r.heating) ? (past ? 'Heating not recorded' : 'No plan block, heating unknown')
    : past ? 'No room heating measured' : 'No room heating planned';
  const parts = [heating ? `${heating} ${heating === 1 ? 'room' : 'rooms'} heating ${past ? '(measured)' : 'per plan'}` : none];
  if (cold) parts.push(`${cold} below ${cold === 1 ? 'its' : 'their'} floor`);
  if (warm) parts.push(`${warm} above ${warm === 1 ? 'its' : 'their'} ceiling`);
  if (stale) parts.push(`${stale} stale ${stale === 1 ? 'sensor' : 'sensors'}`);
  if (!cold && !warm && ok) parts.push('all in comfort band');
  return parts.join(', ');
}
// The banner under the summary when /api/rooms reports sensor_error. A failed read marks only the rooms
// whose temperature it holds (sensor_failed; a humidity or presence bucket marks none), and the roof
// rooms are not on the plan, so their failure carries no "read failed" label.
function fpSensorErrText(rooms) {
  const rs = rooms || [];
  if (rs.some((r) => r.sensor_failed && r.floor !== 'roof')) {
    return rs.some((r) => r.measured)
      ? 'Some sensor reads failed; the rooms marked "read failed" show no reading.'
      : 'The sensor read failed, so the rooms show no readings.';
  }
  return rs.some((r) => r.sensor_failed)
    ? 'A roof sensor read failed; the roof rooms show no measured reading.'
    : 'A sensor read failed; no room temperature is affected.';
}
// The header strip: rooms in band, the coldest room, heating (planned, or measured in the past), the
// area-weighted mean. Only rooms with a sensor and a band count (the garage and the attic have neither
// a band nor a say).
function fpStats(rooms) {
  let banded = 0, inBand = 0, heating = 0, kw = 0, wSum = 0, tSum = 0, coldest = null, heatKnown = false;
  for (const r of rooms || []) {
    if (r.heating) heatKnown = true;
    if (r.heating && r.heating.on) { heating++; kw += r.heating.kw || 0; }
    const st = fpRoomState(r);
    if (r.floor === 'roof' || st.kind === 'nosensor' || st.kind === 'noband') continue;
    banded++;
    if (st.kind === 'in' || st.kind === 'boost') inBand++;
    if (!coldest || st.temp < coldest.temp) coldest = { zone: r.zone, name: r.name, temp: st.temp };
    const w = r.area_m2 > 0 ? r.area_m2 : 1;
    wSum += w; tSum += w * st.temp;
  }
  return { banded, inBand, heating, kw, coldest, meanC: wSum > 0 ? tSum / wSum : null, source: fpHeatSource(rooms), heatKnown };
}
function fpStatsHtml(s) {
  const item = (value, label, title) => `<div class="fp-stat"${title ? ` title="${esc(title)}"` : ''}><b>${value}</b><span>${label}</span></div>`;
  const deg = (v) => (v == null ? '—' : `${v.toFixed(1)}<small>°C</small>`);
  const [on, off, title] = s.source === 'past' ? ['heating then', 'no heating measured then', 'Measured, that block']
    : s.source === 'plan' ? ['heating then', 'no heating planned then', 'Planned, that block'] : ['heating now', 'no heating planned now', 'Committed plan, this block'];
  const kw = s.heatKnown ? `${s.kw.toFixed(2)}<small>kW</small>` : '—';
  const unknown = s.source === 'past' ? 'heating not recorded' : 'no plan block';
  return [
    item(s.banded ? `${s.inBand}<small>/${s.banded}</small>` : '—', 'rooms in comfort band'),
    item(deg(s.coldest && s.coldest.temp), s.coldest ? `coldest, ${esc(s.coldest.name)}` : 'coldest room'),
    item(kw, s.heating ? `${on}, ${s.heating} ${s.heating === 1 ? 'room' : 'rooms'}` : s.heatKnown ? off : unknown, title),
    item(deg(s.meanC), 'mean indoor', 'Area-weighted over rooms with a comfort band'),
  ].join('');
}
// The attic and garage attic as plain readings: measured, else the model estimate marked "est.".
function fpAtticRows(rooms) {
  return (rooms || []).filter((r) => r.floor === 'roof').map((r) => {
    const st = fpRoomState(r);
    if (st.temp != null) return { zone: r.zone, name: r.name, value: `${st.temp.toFixed(1)}°`, est: false, stale: st.stale };
    if (r.estimate_c != null) return { zone: r.zone, name: r.name, value: `${r.estimate_c.toFixed(1)}°`, est: true, stale: false };
    return { zone: r.zone, name: r.name, value: '—', est: false, stale: false };
  });
}

// ---- projection: one affine matrix per floor, stacked <-> flat by interpolation ----
// Layers: 0 ground, 1 first floor. Matrices are SVG [a,b,c,d,e,f].
const fpFloors = (geom) => geom.floors.filter((f) => f.id !== 'roof');
function fpLayout(geom, narrow) {
  const c = Math.cos(FP_THETA), s = Math.sin(FP_THETA);
  const lin = [c, s * FP_SY, -s, c * FP_SY];
  const stacked = [0, FP_LIFT].map((l) => [...lin, 0, -l]);
  const ext = (f) => {
    const pts = (geom.floors.find((x) => x.id === f) || { outline: [[0, 0]] }).outline;
    return { w: Math.max(...pts.map((p) => p[0])), h: Math.max(...pts.map((p) => p[1])) };
  };
  const g = ext('ground'), f1 = ext('first');
  const at = (x, y) => [1, 0, 0, 1, x, y];
  const flat = narrow ? [at(0, f1.h + FP_GAP), at(0, 0)] : [at(0, 0), at(g.w + FP_GAP, 0)];
  return { narrow, stacked, flat };
}
function planMatrix(mode, idx, t, layout) {
  const to = layout[mode][idx];
  const from = layout[mode === 'stacked' ? 'flat' : 'stacked'][idx];
  return from.map((v, i) => v + (to[i] - v) * t);
}
function project(pt, m) {
  return [m[0] * pt[0] + m[2] * pt[1] + m[4], m[1] * pt[0] + m[3] * pt[1] + m[5]];
}
// The plan-space delta that moves a point by (dx, dy) on screen under matrix m (the hover lift).
function fpScreenToPlan(m, dx, dy) {
  const det = m[0] * m[3] - m[1] * m[2] || 1;
  return [(m[3] * dx - m[2] * dy) / det, (-m[1] * dx + m[0] * dy) / det];
}
// Screen bounds of both floors at stackedness s (0 flat .. 1 stacked), with room for the captions.
function fpBounds(geom, layout, s, k) {
  k = k > 0 ? k : 30;
  let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
  fpFloors(geom).forEach((f, i) => {
    const m = planMatrix('stacked', i, s, layout);
    for (const p of f.outline) {
      const q = project(p, m);
      x0 = Math.min(x0, q[0]); x1 = Math.max(x1, q[0]);
      y0 = Math.min(y0, q[1]); y1 = Math.max(y1, q[1]);
    }
  });
  const padX = Math.max(0.5, 12 / k), padTop = Math.max(0.5, 16 / k), padBot = FP_SLAB + Math.max(1, 34 / k);
  return { x: x0 - padX, y: y0 - padTop, w: x1 - x0 + 2 * padX, h: y1 - y0 + padTop + padBot };
}
// Screen rotation (degrees clockwise from up) of true north under matrix m. Plan screen-up faces
// azimuth 50, so north is 50 degrees counter-clockwise from plan up.
function fpNorthDeg(m) {
  const a = 50 * Math.PI / 180, n = [-Math.sin(a), -Math.cos(a)];
  const v = [m[0] * n[0] + m[2] * n[1], m[1] * n[0] + m[3] * n[1]];
  return Math.atan2(v[0], -v[1]) * 180 / Math.PI;
}
// Label size in px: a small room name over a large temperature, or the temperature alone.
function fpLabelBox(name, hasTemp, showName, extraW) {
  const tempW = (hasTemp ? 42 : 54) + (extraW || 0);
  const nameW = showName ? name.length * 5.6 + 6 : 0;
  return { w: Math.max(nameW, tempW), h: showName ? 32 : 20 };
}
// Labels shrink a little on small scenes (never below 80 %, so the 16 px temperature stays near 13 px).
function fpLabelScale(k) { return Math.max(0.8, Math.min(1, k / 26)); }
function fpShowName(areaM2, det, k, nameLen) {
  const ls = fpLabelScale(k);
  const px = Math.sqrt(Math.max(areaM2, 0) * Math.abs(det)) * k;
  return px >= Math.max(40 * ls, nameLen * 5.6 * 0.82 * ls);
}
// Greedy label collision pass in screen space. items: [{id, x, y, prio, levels: [{name, w, h}, ...]}]
// (levels from richest to leanest). Lower prio is placed first; each label takes the richest level that
// clears everything already placed, else it is 'hidden' (the room stays tappable and in the room list).
function fpResolveLabels(items, gap) {
  const overlap = (a, b) => Math.abs(a.x - b.x) < (a.w + b.w) / 2 + gap && Math.abs(a.y - b.y) < (a.h + b.h) / 2 + gap;
  const order = items.map((it, i) => [it, i]).sort((p, q) => p[0].prio - q[0].prio || p[1] - q[1]);
  const placed = [], out = {};
  for (const [it] of order) {
    out[it.id] = 'hidden';
    for (const lv of it.levels) {
      const rect = { x: it.x, y: it.y, w: lv.w, h: lv.h };
      if (!placed.some((p) => overlap(p, rect))) { placed.push(rect); out[it.id] = lv.name; break; }
    }
  }
  return out;
}
// Heated or out-of-band rooms keep their label first, then the larger rooms.
function fpLabelPrio(r, areaM2) {
  const st = fpRoomState(r);
  const notable = (r && r.heating && r.heating.on) || st.kind === 'cold' || st.kind === 'warm' || st.kind === 'boost';
  return (notable ? 0 : 1000) - Math.min(areaM2 || 0, 999);
}
// The small temperature-only label used when the fuller ones collide.
function fpCompactBox(hasTemp, heating) { return { w: (hasTemp ? 32 : 44) + (heating ? 9 : 0), h: 14 }; }
// Where the floating card goes: beside the room's projected bbox (right/left first, on the side with
// more free space), above/below only when neither side fits, always clamped to the viewport. `r` is a
// {left, top, right, bottom} client rect.
function fpPlaceCard(r, w, h, vw, vh, gap, pad) {
  const cx = (r.left + r.right) / 2, cy = (r.top + r.bottom) / 2;
  const free = { right: vw - r.right, left: r.left, below: vh - r.bottom, above: r.top };
  const raw = [
    { side: 'right', left: r.right + gap, top: cy - h / 2 },
    { side: 'left', left: r.left - gap - w, top: cy - h / 2 },
    { side: 'below', left: cx - w / 2, top: r.bottom + gap },
    { side: 'above', left: cx - w / 2, top: r.top - gap - h },
  ].map((c) => ({
    side: c.side,
    left: Math.min(Math.max(c.left, pad), Math.max(pad, vw - w - pad)),
    top: Math.min(Math.max(c.top, pad), Math.max(pad, vh - h - pad)),
  }));
  const covers = (c) => c.left < r.right && c.left + w > r.left && c.top < r.bottom && c.top + h > r.top;
  const horiz = (c) => (c.side === 'right' || c.side === 'left' ? 0 : 1);
  const rank = (a, b) => horiz(a) - horiz(b) || free[b.side] - free[a.side];
  const ok = raw.filter((c) => !covers(c)).sort(rank);
  return ok.length ? ok[0] : raw.sort((a, b) => free[b.side] - free[a.side])[0];
}
// Scroll adjustment (px, positive = down) after a keyboard focus so the card title stays out from
// under the sticky topbar when the whole stretch from title to room fits, else just the room.
function fpScrollDelta(roomTop, roomBottom, cardTop, topH, vh) {
  const lim = topH + 12;
  if (cardTop < lim && roomBottom - cardTop <= vh - lim - 12) return cardTop - lim;
  if (roomTop < lim) return roomTop - lim;
  if (roomBottom > vh - 12) return roomBottom - vh + 12;
  return 0;
}
// Rooms worth a glance: outside their band (worst first), then the ones heating. One row each.
function fpAttention(rooms, limit) {
  const out = [];
  for (const r of rooms || []) {
    const st = fpRoomState(r);
    if (st.kind === 'cold' || st.kind === 'warm') out.push({ zone: r.zone, name: r.name, text: `${st.d < 0 ? '−' : '+'}${Math.abs(st.d).toFixed(1)} K`, kind: st.kind, w: Math.abs(st.d), r });
  }
  out.sort((a, b) => b.w - a.w);
  const heatWord = fpHeatSource(rooms) === 'past' ? 'measured' : 'planned';
  for (const r of rooms || []) {
    if (r.heating && r.heating.on && !out.some((o) => o.zone === r.zone)) out.push({ zone: r.zone, name: r.name, text: `${r.heating.kw.toFixed(1)} kW ${heatWord}`, kind: 'heat', w: 0, r });
  }
  return out.slice(0, limit || 6);
}

// ---- detail card: band gauge, 12 h plan chart, facts ----
function fpNum(x, d) { return x == null || !isFinite(x) ? null : x.toFixed(d); }
function fpPresenceText(p) {
  if (!p || p.motion == null) return null;
  const ago = fpAgo(p.age_s);
  if (ago === 'just now') return p.motion ? 'motion now' : 'quiet now';
  return p.motion ? `motion ${ago}` : `quiet ${ago.replace(' ago', '')}`;
}
function fpGauge(st, est) {
  if (st.lo == null || st.hi == null) return '';
  const hiEdge = st.boost != null ? Math.max(st.hi, st.boost) : st.hi;
  const vals = [st.lo, hiEdge, st.temp, est].filter((v) => v != null);
  const lo = Math.min(...vals) - 1.5, hi = Math.max(...vals) + 1.5;
  const W = 296, X0 = 4, X1 = W - 4;
  const x = (v) => (X0 + (v - lo) / (hi - lo) * (X1 - X0)).toFixed(1);
  const boost = st.boost != null && st.boost > st.hi
    ? `<rect class="fpg-boost" x="${x(st.hi)}" y="11" width="${(x(st.boost) - x(st.hi)).toFixed(1)}" height="8" rx="2"/>` : '';
  const estM = est != null ? `<path class="fpg-est" d="M${x(est)} 8v14"/>` : '';
  const tM = st.temp != null ? `<circle class="fpg-dot" cx="${x(st.temp)}" cy="15" r="5.5"/>` : '';
  const lab = (v, anchor, txt) => `<text class="fpg-t" x="${x(v)}" y="31" text-anchor="${anchor}">${txt}</text>`;
  return `<svg class="fp-gauge" viewBox="0 0 ${W} 34" role="img" aria-label="Comfort band ${st.lo} to ${st.hi} degrees">
    <rect class="fpg-track" x="${X0}" y="11" width="${X1 - X0}" height="8" rx="4"/>
    <rect class="fpg-band" x="${x(st.lo)}" y="9" width="${(x(st.hi) - x(st.lo)).toFixed(1)}" height="12" rx="3"/>${boost}${estM}${tM}
    ${lab(st.lo, 'middle', `floor ${st.lo}°`)}${lab(st.hi, 'middle', `ceiling ${st.hi}°`)}
  </svg>`;
}
// Next-12 h chart: predicted temperature over the band (one temperature axis), planned heat as thin
// bars on their own strip underneath.
function fpPlanChart(r, nowMs) {
  const full = r.plan;
  if (!full || !full.t || !full.t.length) return '';
  // the card's chart covers the next 12 h even when the room carries the whole plan
  const lim = (nowMs || Date.parse(full.t[0])) + 12 * 3600000;
  const keep = full.t.map((t, i) => i).filter((i) => Date.parse(full.t[i]) < lim);
  if (!keep.length) return '';
  const pick = (a) => keep.map((i) => a[i]);
  const p = { t: pick(full.t), dt_minutes: pick(full.dt_minutes), heat_kw: pick(full.heat_kw), temp_c: pick(full.temp_c) };
  const st = fpRoomState(r);
  const t0 = Date.parse(p.t[0]);
  const ends = p.t.map((t, i) => Date.parse(t) + (p.dt_minutes[i] || 15) * 60000);
  const t1 = ends[ends.length - 1];
  if (!isFinite(t0) || !isFinite(t1) || t1 <= t0) return '';
  const W = 296, H = 72, L = 26, R = 8, T = 4, B = 14, STRIP = 12, GAP = 3;
  const plotH = H - T - B - STRIP - GAP;
  const x = (ms) => L + (ms - t0) / (t1 - t0) * (W - L - R);
  const pts = [];
  if (r.estimate_c != null) pts.push([Math.max(t0, nowMs || t0), r.estimate_c]);
  p.temp_c.forEach((v, i) => { if (v != null) pts.push([ends[i], v]); });
  const temps = pts.map((q) => q[1]);
  if (st.lo != null) temps.push(st.lo, st.hi);
  if (!temps.length) return '';
  let lo = Math.min(...temps), hi = Math.max(...temps);
  const pad = Math.max(0.4, (hi - lo) * 0.12);
  lo -= pad; hi += pad;
  const y = (v) => T + (hi - v) / (hi - lo) * plotH;
  const band = st.lo != null
    ? `<rect class="fpc-band" x="${L}" y="${y(st.hi).toFixed(1)}" width="${W - L - R}" height="${(y(st.lo) - y(st.hi)).toFixed(1)}"/>` : '';
  const boost = st.boost != null && st.boost > st.hi
    ? `<rect class="fpc-boost" x="${L}" y="${y(st.boost).toFixed(1)}" width="${W - L - R}" height="${(y(st.hi) - y(st.boost)).toFixed(1)}"/>` : '';
  const line = pts.length > 1 ? `<path class="fpc-line" d="${pts.map((q, i) => `${i ? 'L' : 'M'}${x(q[0]).toFixed(1)} ${y(q[1]).toFixed(1)}`).join('')}"/>` : '';
  const end = pts.length ? pts[pts.length - 1] : null;
  const endDot = end ? `<circle class="fpc-dot" cx="${x(end[0]).toFixed(1)}" cy="${y(end[1]).toFixed(1)}" r="3"/>` : '';
  const maxKw = Math.max(r.max_heat_kw || 0, ...p.heat_kw.map((v) => v || 0), 0.1);
  const sy0 = T + plotH + GAP;
  const bars = p.heat_kw.map((kw, i) => {
    if (!isRelayOn(kw)) return '';
    const bx = x(Date.parse(p.t[i])), bw = Math.max(1.2, x(ends[i]) - bx - 0.8);
    const bh = Math.max(1.5, kw / maxKw * STRIP);
    return `<rect class="fpc-bar" x="${bx.toFixed(1)}" y="${(sy0 + STRIP - bh).toFixed(1)}" width="${bw.toFixed(1)}" height="${bh.toFixed(1)}" rx="1"/>`;
  }).join('');
  const nowX = nowMs && nowMs > t0 && nowMs < t1 ? `<path class="fpc-now" d="M${x(nowMs).toFixed(1)} ${T}V${sy0 + STRIP}"/>` : '';
  const hm = (ms) => { const d = new Date(ms); return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`; };
  const xt = (ms, a) => `<text class="fpc-t" x="${x(ms).toFixed(1)}" y="${H - 5}" text-anchor="${a}">${hm(ms)}</text>`;
  const yt = (v) => `<text class="fpc-t" x="${L - 4}" y="${(y(v) + 3).toFixed(1)}" text-anchor="end">${v.toFixed(0)}°</text>`;
  const ticks = [Math.ceil(lo + pad), Math.floor(hi - pad)].filter((v, i, a) => a.indexOf(v) === i && v >= lo && v <= hi);
  return `<svg class="fp-chart" viewBox="0 0 ${W} ${H}" role="img" aria-label="Predicted temperature and planned heating for the next 12 hours">
    ${band}${boost}${nowX}${line}${endDot}${bars}
    <path class="fpc-axis" d="M${L} ${sy0 + STRIP}H${W - R}"/>
    ${ticks.map(yt).join('')}${xt(t0, 'start')}${xt(t0 + (t1 - t0) / 2, 'middle')}${xt(t1, 'end')}
  </svg>`;
}
function fpFacts(r, ctx) {
  const rows = [];
  const add = (k, v, note) => { if (v != null && v !== '' && v !== false) rows.push([k, v, note || '']); };
  // the drawn area, flagged when the model's own (volume / floor height) disagrees by more than 15 %
  const modelArea = ctx && ctx.floorHeight > 0 && r.volume_m3 > 0 ? r.volume_m3 / ctx.floorHeight : null;
  const odd = modelArea != null && r.area_m2 > 0 && Math.abs(r.area_m2 - modelArea) / modelArea > 0.15;
  add('Area', fpNum(r.area_m2, 1) && `${fpNum(r.area_m2, 1)} m²`, odd ? '(as drawn)' : null);
  add('Volume', fpNum(r.volume_m3, 0) && `${fpNum(r.volume_m3, 0)} m³`);
  add('Heater', r.max_heat_kw ? `${fpNum(r.max_heat_kw, 2)} kW max` : null);
  add('Air changes', fpNum(r.ach, 2) && `${fpNum(r.ach, 2)} /h`);
  add('Humidity', r.humidity && fpNum(r.humidity.pct, 0) && `${fpNum(r.humidity.pct, 0)} %`);
  add('Presence', fpPresenceText(r.presence));
  add('Solar gain', r.solar_scale != null && `× ${fpNum(r.solar_scale, 2)}`, 'model estimate');
  add('Disturbance', r.disturbance_w != null && `${r.disturbance_w >= 0 ? '+' : '−'}${Math.abs(r.disturbance_w).toFixed(0)} W`, 'model estimate');
  if (r.warmth) {
    const wv = r.warmth.value_eur_per_kh;
    add('Warmth value', wv != null && (ctx && ctx.rate ? `${(wv * ctx.rate).toFixed(3)} Kč/K·h` : `€${wv.toFixed(4)}/K·h`));
    const be = r.warmth.break_even_eur_per_kwh;
    add('Break-even', be != null && (ctx && ctx.rate ? `${(be * ctx.rate).toFixed(2)} Kč/kWh` : `€${be.toFixed(3)}/kWh`));
  }
  const hc = r.heat_credit_eur_per_kwh;
  add('Heat credit', hc != null && hc > 0 && (ctx && ctx.rate ? `${(hc * ctx.rate).toFixed(2)} Kč/kWh` : `€${hc.toFixed(4)}/kWh`));
  return rows;
}
function fpCardHtml(r, ctx) {
  ctx = ctx || {};
  const st = fpRoomState(r);
  const theme = ctx.theme || 'dark';
  const temp = st.temp != null ? `${st.temp.toFixed(1)}<small>°C</small>` : (r.estimate_c != null ? `${r.estimate_c.toFixed(1)}<small>°C est.</small>` : '—');
  const age = r.measured ? fpAgo(r.measured.age_s) : '';
  const sub = st.temp == null && r.estimate_c != null ? `model estimate, ${fpStatusPhrase(st)}` : age;
  const swatch = `<i class="fpk-sw" style="background:${fpFill(st, theme)}"></i>`;
  const h = r.heating;
  let heat;
  const today = (r.heating_today_kwh != null ? `<span class="fpk-sub">${r.heating_today_kwh.toFixed(2)} kWh today</span>` : '');
  if (r.heated === false) heat = '<span class="fpk-off">no heater</span>';
  else if (!h) heat = `<span class="fpk-off">${r.at && r.at.kind === 'past' ? 'not recorded' : 'no plan block'}</span>` + today;
  else {
    heat = (h.on
      ? `<span class="fpk-on"><i class="fpk-flame"></i>on, ${h.kw.toFixed(2)} kW</span>`
      : '<span class="fpk-off">off</span>') + today;
  }
  const planKwh = r.plan_heat_kwh_12h != null ? `<span>${r.plan_heat_kwh_12h.toFixed(2)} kWh heat planned</span>` : '';
  const chart = fpPlanChart(r, ctx.nowMs);
  const chartHead = chart ? `<div class="fpk-chead"><span>Next 12 h</span>${planKwh}</div>` : '';
  const facts = fpFacts(r, ctx).map(([k, v, n]) => `<div class="fpk-f"><dt>${esc(k)}</dt><dd>${esc(v)}${n ? `<small>${esc(n)}</small>` : ''}</dd></div>`).join('');
  const est = r.estimate_c != null && st.temp != null ? `model ${r.estimate_c.toFixed(1)}°` : '';
  // the HA panel only: this room's blinds (or the garage's doors) at the time bar's instant
  const cv = ctx.covers && ctx.covers.rows.length ? ctx.covers : null;
  const covers = cv ? `<div class="fpk-chead fpk-covhead"><span>${esc(cv.title)}</span><span>${esc(cv.note)}</span></div>${cv.rows.map((c) => `<div class="fpk-row fpk-cov${c.unknown ? ' is-unknown' : ''}"><span class="fpk-k">${esc(c.name)}${c.unsure ? '<span class="fpk-sub">placed by a guess</span>' : ''}</span><span class="fpk-v">${esc(c.text)}</span></div>`).join('')}` : '';
  return `<div class="fpk-head"><div class="fpk-name">${esc(r.name)}</div><div class="fpk-floor">${esc(ctx.floorName || '')}</div></div>
    ${ctx.at ? `<div class="fpk-at">${esc(ctx.at)}</div>` : ''}
    <div class="fpk-temp"><span class="fpk-big">${temp}</span><span class="fpk-age">${esc([sub, est].filter(Boolean).join(', '))}</span></div>
    <div class="fpk-status">${swatch}<span>${esc(fpStatusPhrase(st))}${r.at && st.temp != null && st.lo != null ? ', against the current band' : ''}</span></div>
    ${ctx.fire ? `<div class="fpk-fire"><i class="fpk-flame"></i>${esc(ctx.fire.text)}</div>` : ''}
    ${fpGauge(st, r.estimate_c)}
    <div class="fpk-row"><span class="fpk-k">${r.at ? 'Heating then' : 'Heating now'}<span class="fpk-sub">${r.at ? (r.at.kind === 'past' ? 'measured, that block' : 'planned, that block') : 'committed plan, this block'}</span></span><span class="fpk-v">${heat}</span></div>
    ${covers}${chartHead}${chart}
    <dl class="fpk-facts">${facts}</dl>`;
}

// ---- doors and windows (the walls in floorplan.json already have the openings cut out) ----
// Plan-space drawing of one opening: `rect` is its footprint through the wall, `jambs` the two frame
// lines across the wall, `floor` what lies on the floor (a swing door's leaf and swing arc, a sliding
// door's two leaves, a garage sectional door's leaf just inside the inner face).
function fpOpeningShape(o) {
  const [ax, ay] = o.a, [bx, by] = o.b, [nx, ny] = o.n, h = o.depth_m / 2;
  const P = (x, y) => `${+x.toFixed(3)} ${+y.toFixed(3)}`;
  const at = (t, k) => P(ax + (bx - ax) * t + nx * k, ay + (by - ay) * t + ny * k);
  const rect = `M${at(0, -h)}L${at(1, -h)}L${at(1, h)}L${at(0, h)}Z`;
  const jambs = `M${at(0, -h)}L${at(0, h)}M${at(1, -h)}L${at(1, h)}`;
  let floor = '';
  if (o.type === 'swing' && o.swing) {
    const { hinge: H, r, leaf: F, closed: C } = o.swing;
    const cross = (C[0] - H[0]) * (F[1] - H[1]) - (C[1] - H[1]) * (F[0] - H[0]);
    floor = `M${P(H[0], H[1])}L${P(F[0], F[1])}M${P(C[0], C[1])}A${r} ${r} 0 0 ${cross > 0 ? 1 : 0} ${P(F[0], F[1])}`;
  } else if (o.type === 'sliding') {
    const k = Math.min(0.04, h * 0.4);
    floor = `M${at(0, k)}L${at(0.55, k)}M${at(0.45, -k)}L${at(1, -k)}`;
  } else if (o.type === 'sectional') {
    floor = `M${at(0, 0.06 - h)}L${at(1, 0.06 - h)}`;
  }
  return { rect, jambs, floor };
}
// The glass standing in a window or glazed door: vs / vh are the plan-space offsets that lift the
// sill and the head on screen. `sill` / `head` are the bottom and top edges (the glowing line sits on
// the sill, or on the head for a clerestory), `frame` the sides and head.
function fpPaneShape(o, vs, vh) {
  const P = (p, v) => `${+(p[0] + v[0]).toFixed(3)} ${+(p[1] + v[1]).toFixed(3)}`;
  return {
    pane: `M${P(o.a, vs)}L${P(o.b, vs)}L${P(o.b, vh)}L${P(o.a, vh)}Z`,
    sill: `M${P(o.a, vs)}L${P(o.b, vs)}`,
    head: `M${P(o.a, vh)}L${P(o.b, vh)}`,
    frame: `M${P(o.a, vs)}L${P(o.a, vh)}M${P(o.b, vs)}L${P(o.b, vh)}M${P(o.a, vh)}L${P(o.b, vh)}`,
  };
}

// ---- fixtures: the fireplace and the stairwell ----
// The house has no fireplace sensor. The state estimator's unexplained heat in the fireplace room is
// the only hint, so above FP_FIRE_W the fire is called likely lit, worded as the estimate it is.
const FP_FIRE_W = 400;
function fpFireLikely(r) {
  const w = r && r.disturbance_w;
  if (w == null || !isFinite(w) || w <= FP_FIRE_W) return null;
  return { w: Math.round(w), text: `fire likely lit (model estimate, +${Math.round(w)} W unexplained heat)` };
}
// Plan path along a firebox's glass faces, a hair outside them.
function fpFireboxPath(fb) {
  const P = (p, n) => `${+(p[0] + n[0] * 0.03).toFixed(3)} ${+(p[1] + n[1] * 0.03).toFixed(3)}`;
  return fb.faces.map((f) => `M${P(f.a, f.n)}L${P(f.b, f.n)}`).join('');
}
// The room a stairwell opens from: the one sharing the most of the void's outline (within 12 cm), or
// null when none touches it.
function fpVoidRoom(poly, rooms) {
  const segDist = (p, a, b) => {
    const dx = b[0] - a[0], dy = b[1] - a[1];
    const t = Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy || 1)));
    return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
  };
  const samples = [];
  poly.forEach((a, i) => { const b = poly[(i + 1) % poly.length]; for (let k = 0; k < 20; k++) samples.push([a[0] + (b[0] - a[0]) * (k + 0.5) / 20, a[1] + (b[1] - a[1]) * (k + 0.5) / 20]); });
  let best = null, bestN = 0;
  for (const r of rooms || []) {
    const pg = r.polygon;
    const n = samples.filter((p) => pg.some((a, i) => segDist(p, a, pg[(i + 1) % pg.length]) <= 0.12)).length;
    if (n > bestN) { best = r.zone; bestN = n; }
  }
  return best;
}

// ---- the sun: where it stands, which outside faces and windows it reaches ----
// Sun position (NOAA's solar calculator, without refraction): azimuth in degrees clockwise from true
// north, elevation in degrees above the horizon, for UTC instant `ms` at latitude/longitude in degrees.
function fpSunPosition(ms, lat, lon) {
  const rad = Math.PI / 180;
  const T = (ms / 86400000 + 2440587.5 - 2451545) / 36525;
  const L0 = (((280.46646 + T * (36000.76983 + T * 0.0003032)) % 360) + 360) % 360;
  const M = 357.52911 + T * (35999.05029 - 0.0001537 * T);
  const e = 0.016708634 - T * (0.000042037 + 0.0000001267 * T);
  const C = Math.sin(M * rad) * (1.914602 - T * (0.004817 + 0.000014 * T)) + Math.sin(2 * M * rad) * (0.019993 - 0.000101 * T) + Math.sin(3 * M * rad) * 0.000289;
  const omega = 125.04 - 1934.136 * T;
  const lambda = L0 + C - 0.00569 - 0.00478 * Math.sin(omega * rad);
  const eps = 23 + (26 + (21.448 - T * (46.815 + T * (0.00059 - T * 0.001813))) / 60) / 60 + 0.00256 * Math.cos(omega * rad);
  const decl = Math.asin(Math.sin(eps * rad) * Math.sin(lambda * rad));
  const y = Math.pow(Math.tan(eps * rad / 2), 2);
  const eqMin = 4 / rad * (y * Math.sin(2 * L0 * rad) - 2 * e * Math.sin(M * rad) + 4 * e * y * Math.sin(M * rad) * Math.cos(2 * L0 * rad)
    - 0.5 * y * y * Math.sin(4 * L0 * rad) - 1.25 * e * e * Math.sin(2 * M * rad));
  const dayMin = (((ms / 60000) % 1440) + 1440) % 1440;
  const tst = (((dayMin + eqMin + 4 * lon) % 1440) + 1440) % 1440;
  const ha = tst / 4 - 180;
  const phi = lat * rad;
  const cosZ = Math.max(-1, Math.min(1, Math.sin(phi) * Math.sin(decl) + Math.cos(phi) * Math.cos(decl) * Math.cos(ha * rad)));
  const zen = Math.acos(cosZ);
  const den = Math.cos(phi) * Math.sin(zen);
  let az = 180;
  if (Math.abs(den) > 1e-9) {
    const a = Math.acos(Math.max(-1, Math.min(1, (Math.sin(phi) * Math.cos(zen) - Math.sin(decl)) / den))) / rad;
    az = ha > 0 ? (a + 180) % 360 : (540 - a) % 360;
  }
  return { azimuth: az, elevation: 90 - zen / rad };
}
// The instant the sun is drawn for: ?fpSunTime=<ISO> (or window.__fpSunTime) pins it, else now.
function fpSunClock() {
  let v = window.__fpSunTime;
  try { v = v || new URLSearchParams(location.search).get('fpSunTime'); } catch (e) { /* no URL */ }
  const ms = v ? Date.parse(v) : NaN;
  return isFinite(ms) ? { ms, pinned: true } : { ms: Date.now(), pinned: false };
}
// Local midnight (the browser's zone: the dashboard runs in the house) of the day holding `ms`.
function fpLocalDayStart(ms) { const d = new Date(ms); d.setHours(0, 0, 0, 0); return d.getTime(); }
// Sunrise and sunset (upper limb on the horizon with standard refraction, elevation -0.833 deg) on the
// 24 h from `dayStart`, found on the sun position itself: a minute scan, then linear interpolation.
// null for either when the sun does not cross that day.
function fpSunTimes(dayStart, lat, lon) {
  const H = -0.833, el = (m) => fpSunPosition(dayStart + m * 60000, lat, lon).elevation - H;
  let rise = null, set = null, prev = el(0);
  for (let m = 1; m <= 1440; m++) {
    const cur = el(m);
    if (prev < 0 && cur >= 0 && rise == null) rise = dayStart + (m - 1 + prev / (prev - cur)) * 60000;
    if (prev >= 0 && cur < 0) set = dayStart + (m - 1 + prev / (prev - cur)) * 60000;
    prev = cur;
  }
  return { sunrise: rise, sunset: set };
}
// Compass bearing (degrees from north, clockwise) of a plan direction; the plan's screen-up faces upAz.
function fpPlanAzimuth(n, upAz) {
  return (((upAz + Math.atan2(n[0], -n[1]) * 180 / Math.PI) % 360) + 360) % 360;
}
// Direct sun on a vertical outside face with outward plan normal n: cos of the angle between the face
// normal and the sun (cos elevation x cos of the bearing difference), 0 when it faces away or the sun is down.
function fpWallIncidence(n, sun, upAz) {
  if (!sun || sun.elevation <= 0) return 0;
  const r = Math.PI / 180;
  return Math.max(0, Math.cos(sun.elevation * r) * Math.cos((sun.azimuth - fpPlanAzimuth(n, upAz)) * r));
}
// The outline's edges with their outward unit normals (plan metres), whatever the ring's winding.
function fpOutlineFacades(outline) {
  const inside = (p) => {
    let c = false;
    for (let i = 0, j = outline.length - 1; i < outline.length; j = i++) {
      const [xi, yi] = outline[i], [xj, yj] = outline[j];
      if ((yi > p[1]) !== (yj > p[1]) && p[0] < (xj - xi) * (p[1] - yi) / (yj - yi) + xi) c = !c;
    }
    return c;
  };
  return outline.map((a, i) => {
    const b = outline[(i + 1) % outline.length], L = Math.hypot(b[0] - a[0], b[1] - a[1]) || 1;
    let n = [(b[1] - a[1]) / L, -(b[0] - a[0]) / L];
    if (inside([(a[0] + b[0]) / 2 + n[0] * 0.05, (a[1] + b[1]) / 2 + n[1] * 0.05])) n = [-n[0], -n[1]];
    return { a, b, n };
  });
}
// The outside glazing (windows and glazed doors) the sun shines on now: incidence over 0.1 with the sun
// over 3 degrees. Neighbours and trees are not modelled, so this is the geometry alone.
const FP_SUN_MIN_INC = 0.1;
const FP_SUN_MIN_EL = 3;
function fpSunlitWindows(floors, sun, upAz) {
  const out = [];
  if (!sun || sun.elevation <= FP_SUN_MIN_EL) return out;
  for (const f of floors || []) {
    for (const o of f.openings || []) {
      if (!(o.rooms || []).includes('outside') || !(o.kind === 'window' || o.type === 'glazed')) continue;
      const inc = fpWallIncidence(o.n, sun, upAz);
      if (inc > FP_SUN_MIN_INC) out.push({ floor: f.id, o, inc, zone: o.rooms.find((z) => z !== 'outside') });
    }
  }
  return out;
}
const FP_COMPASS_WORDS = ['north', 'north-east', 'east', 'south-east', 'south', 'south-west', 'west', 'north-west'];
function fpCompassWord(az) { return FP_COMPASS_WORDS[Math.round((((az % 360) + 360) % 360) / 45) % 8]; }
// "☀ 34° high, south (187°) · sunset 18:31", or "Night · sunrise 07:02". `next` holds the times of the
// following day (the sunrise after an evening). `hm` formats an instant.
function fpSunCaption(sun, ms, today, next, hm) {
  if (!sun) return '';
  const fmt = (t) => (t == null ? '—' : hm(t));
  if (sun.elevation > 0) {
    return `☀ ${Math.round(sun.elevation)}° high, ${fpCompassWord(sun.azimuth)} (${Math.round(sun.azimuth)}°) · sunset ${fmt(today.sunset)}`;
  }
  const morning = today.sunrise != null && ms < today.sunrise;
  const rise = morning ? today.sunrise : next && next.sunrise;
  return `${sun.elevation > -6 ? 'Twilight' : 'Night'} · sunrise ${fmt(rise)}`;
}
function fpHm(t) { const d = new Date(t); return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`; }

// ---- the house timeline: the floor plan at any time from a day ago to the end of the plan ----
// The time bar moves in 15-minute steps relative to now; 0 is now (the live view, unchanged). The past
// is measured (sensor series, the decision ledger, the history feed), the future is the plan.
const FP_STEP_MS = 15 * 60000;
const FP_PAST_STEPS = 96; // 24 h back
// The block of a multi-rate plan (start instants `starts` in ms, lengths `dts` in minutes) that covers
// instant T, or -1.
function fpBlockIndex(starts, dts, T) {
  for (let i = 0; i < starts.length; i++) if (starts[i] <= T && T < starts[i] + (dts[i] || 15) * 60000) return i;
  return -1;
}
// A measured series ([[iso, value]], ascending) at instant T: linear between the neighbouring points,
// null outside the series or across a gap longer than maxGapMs.
function fpSeriesAt(series, T, maxGapMs) {
  if (!series || !series.length) return null;
  let prev = null;
  for (const [iso, v] of series) {
    const t = Date.parse(iso);
    if (v == null || !isFinite(t)) continue;
    if (t === T) return v;
    if (t > T) {
      if (!prev || t - prev[0] > maxGapMs) return null;
      return prev[1] + (v - prev[1]) * (T - prev[0]) / (t - prev[0]);
    }
    prev = [t, v];
  }
  return null;
}
// /api/zones/series (30 m) and /api/history (15 m) are window means stamped at the window END (Flux
// timeSrc "_stop", src/app.rs zone_temp_history and src/web.rs measured_series): the value at T is
// the window centred on T. The newest window is still open and stamped at the server's read time
// (cached, so before the client's now): a T it covers reads that newest mean. A window with no
// samples has no point (createEmpty: false; Loxone logs on change), so past the newest point its mean
// holds for up to maxGapMs, as the backend's resample_ffill holds it.
const FP_SERIES_WINDOW_MS = 30 * 60000;
const FP_HISTORY_WINDOW_MS = 15 * 60000;
function fpWindowMeanAt(series, T, windowMs, maxGapMs) {
  let q = T + windowMs / 2;
  for (let i = (series ? series.length : 0) - 1; i >= 0; i--) {
    const t = Date.parse(series[i][0]);
    if (series[i][1] == null || !isFinite(t)) continue;
    if (T <= t && t < q) q = t;
    else if (t < T) return q - t <= maxGapMs ? series[i][1] : null;
    break;
  }
  return fpSeriesAt(series, q, maxGapMs);
}
// Which side of now the selected step is on: 'now' (offset 0), 'past' (measured) or 'plan'.
function fpTimeKind(offsetSteps) { return offsetSteps === 0 ? 'now' : offsetSteps < 0 ? 'past' : 'plan'; }
// The bar's range in steps: a day back to the end of the plan's last block (at least now).
function fpTimelineRange(nowMs, timeline) {
  const last = timeline && timeline.length ? timeline[timeline.length - 1] : null;
  const end = last ? Date.parse(last.t) + (last.dt_minutes || 15) * 60000 : nowMs;
  return { min: -FP_PAST_STEPS, max: Math.max(0, Math.floor((end - nowMs) / FP_STEP_MS) - 1) };
}
// "Sat 14:30 · plan" / "Fri 22:00 · measured" / "Now".
function fpTimeLabel(T, kind) {
  if (kind === 'now') return 'Now';
  const d = new Date(T);
  const day = d.toLocaleDateString('en-GB', { weekday: 'short' });
  return `${day} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')} · ${kind === 'plan' ? 'plan' : 'measured'}`;
}
// A plan quantity given at block ENDS (temp_c, soc_kwh) at instant T: linear between the known end
// values around T, starting from the live value v0 at nowMs (when both are given); the nearest known
// value outside them. `starts` in ms, `dts` in minutes; null when nothing is known.
function fpPlanValueAt(starts, dts, vals, T, nowMs, v0) {
  const anchored = v0 != null && nowMs != null;
  const pts = anchored ? [[nowMs, v0]] : [];
  starts.forEach((s, i) => {
    const end = s + (dts[i] || 15) * 60000;
    if (vals[i] != null && (!anchored || end > nowMs)) pts.push([end, vals[i]]);
  });
  if (!pts.length) return null;
  const k = pts.findIndex((p) => p[0] >= T);
  if (k < 0) return pts[pts.length - 1][1];
  if (k === 0) return pts[0][1];
  const [ta, a] = pts[k - 1], [tb, b] = pts[k];
  return a + (b - a) * (T - ta) / (tb - ta);
}
// A room as it stands at instant T: the same shape /api/rooms gives, with the temperature and heating of
// that time, so every view and the card paint it unchanged. Plan: the predicted temperature at T
// (fpPlanValueAt from the live estimate) and the planned heat of the block covering T. Past: the
// measured series and the ledger's measured heating energy for that block. Unknown stays null, and so
// do the live-only readings (the fire's disturbance, the solar scale). The band stays the one in force
// now: /api/rooms carries no band schedule, and the card says "against the current band".
// src: { now (ms), series: {zone: [[iso, °C]]}, ledger: [rows] }.
function fpRoomAt(r, T, kind, src) {
  if (kind === 'now' || !r) return r;
  let temp = null, heating = null;
  if (kind === 'plan') {
    const p = r.plan;
    if (p && p.t && p.t.length) {
      const starts = p.t.map((x) => Date.parse(x));
      const i = fpBlockIndex(starts, p.dt_minutes, T);
      if (i >= 0) {
        temp = fpPlanValueAt(starts, p.dt_minutes, p.temp_c, T, src && src.now, r.estimate_c);
        const kw = p.heat_kw[i];
        if (kw != null) heating = { on: isRelayOn(kw), kw };
      }
    }
  } else {
    temp = fpWindowMeanAt(src && src.series ? src.series[r.zone] : null, T, FP_SERIES_WINDOW_MS, 75 * 60000);
    const rows = (src && src.ledger) || [];
    const i = fpBlockIndex(rows.map((x) => Date.parse(x.t)), rows.map((x) => x.dt_minutes), T);
    const kwh = i >= 0 && rows[i].measured && rows[i].measured.heat_kwh ? rows[i].measured.heat_kwh[r.zone] : null;
    if (kwh != null) { const kw = kwh / ((rows[i].dt_minutes || 15) / 60); heating = { on: isRelayOn(kw), kw }; }
  }
  const round = (v, d) => (v == null ? null : Math.round(v * 10 ** d) / 10 ** d);
  return {
    ...r,
    measured: temp != null ? { temp_c: round(temp, 2), at: null, age_s: null } : null,
    heating: r.heated === false ? r.heating : heating,
    estimate_c: null, humidity: null, presence: null, heating_today_kwh: null,
    disturbance_w: null, solar_scale: null, sensor_failed: false,
    at: { T, kind },
  };
}
const FP_FULL_OLDER_MS = 15 * 60000;
const FP_FULL_MAX_LAG_MS = 60 * 60000;
const FP_FULL_REFETCH_MS = 60000;
// Whether the last fetched whole plan (plan_at fullAt) may stand in beyond the live rooms' 12 h, against
// the live plan liveAt: the loop replans every minute, so the previous one is shown while the newer one
// loads. 'current'; 'older' when it trails by over 15 min (shown, marked); null when there is none or it
// trails by over an hour.
function fpFullPlanUse(fullAt, liveAt) {
  if (!fullAt || !liveAt) return null;
  const lag = Date.parse(liveAt) - Date.parse(fullAt);
  if (!isFinite(lag) || lag > FP_FULL_MAX_LAG_MS) return null;
  return lag > FP_FULL_OLDER_MS ? 'older' : 'current';
}
// The house at instant T for the readout row: price (EUR/kWh import) and the EUR/CZK rate a past row
// was recorded with, planned or measured heating total (kW), battery (kWh), PV (kW), outside (°C, now
// only: no feed carries a past or planned outside temperature) and the battery mode. src: { now (ms),
// live, timeline, ledger, history }. Missing values are null. At now, where planned and measured values
// share the row, `from` says which each one is ('plan' / 'measured').
function fpReadoutAt(T, kind, src) {
  const o = { price: null, rate: null, heatKw: null, socKwh: null, pvKw: null, outsideC: null, slot: null, from: null };
  const sum = (m) => (m ? Object.values(m).reduce((a, v) => a + (v || 0), 0) : null);
  const tl = (src && src.timeline) || [];
  const starts = tl.map((b) => Date.parse(b.t)), dts = tl.map((b) => b.dt_minutes);
  const plan = tl[fpBlockIndex(starts, dts, T)];
  if (kind === 'plan' || kind === 'now') {
    if (plan) Object.assign(o, { price: plan.import_price, heatKw: sum(plan.heat_kw), socKwh: plan.soc_kwh, pvKw: plan.pv_kw, slot: plan.slot });
    if (plan && kind === 'plan') o.socKwh = fpPlanValueAt(starts, dts, tl.map((b) => b.soc_kwh), T, src.now, src.live ? src.live.soc_kwh : null);
    if (kind === 'now') {
      o.from = plan ? { heatKw: 'plan', socKwh: 'plan', pvKw: 'plan' } : {};
      const l = (src && src.live) || {};
      if (l.solar_kw != null) { o.pvKw = l.solar_kw; o.from.pvKw = 'measured'; }
      if (l.soc_kwh != null) { o.socKwh = l.soc_kwh; o.from.socKwh = 'measured'; }
      if (l.outside_temp_c != null) { o.outsideC = l.outside_temp_c; o.from.outsideC = 'measured'; }
    }
    return o;
  }
  const rows = (src && src.ledger) || [];
  const row = rows[fpBlockIndex(rows.map((x) => Date.parse(x.t)), rows.map((x) => x.dt_minutes), T)];
  if (row) {
    const h = (row.dt_minutes || 15) / 60, m = row.measured;
    o.price = row.import_price;
    o.rate = row.eur_czk_rate > 0 ? row.eur_czk_rate : null;
    o.slot = row.slot;
    if (m) {
      o.pvKw = m.pv_kwh != null ? m.pv_kwh / h : null;
      // the ledger lists only the zones it could measure: none listed is unknown, not zero
      const heat = Object.values(m.heat_kwh || {}).filter((v) => v != null);
      o.heatKw = heat.length ? heat.reduce((a, v) => a + v, 0) / h : null;
    }
  }
  const hist = src && src.history;
  if (hist) {
    o.socKwh = fpWindowMeanAt(hist.soc_kwh, T, FP_HISTORY_WINDOW_MS, 45 * 60000);
    if (o.pvKw == null) o.pvKw = fpWindowMeanAt(hist.pv_kw, T, FP_HISTORY_WINDOW_MS, 45 * 60000);
  }
  return o;
}
// The readout as [label, text] pairs. CZK at the row's own rate, else `rate` (the plan's); EUR when
// neither is known. Outside only when known.
function fpReadoutItems(o, rate) {
  const tag = (k) => (o.from && o.from[k] ? ` (${o.from[k]})` : '');
  const n = (v, d, unit, k) => (v == null || !isFinite(v) ? '—' : `${v.toFixed(d)} ${unit}${tag(k)}`);
  const czk = o.rate || rate;
  const items = [
    ['Price', o.price == null ? '—' : czk ? `${(o.price * czk).toFixed(2)} Kč/kWh` : `€${o.price.toFixed(3)}/kWh`],
    ['Heating', n(o.heatKw, 1, 'kW', 'heatKw')],
    ['Battery', n(o.socKwh, 1, 'kWh', 'socKwh')],
    ['PV', n(o.pvKw, 1, 'kW', 'pvKw')],
  ];
  if (o.outsideC != null) items.push(['Outside', n(o.outsideC, 1, '°C', 'outsideC')]);
  items.push(['Mode', o.slot ? modeLabel(o.slot) : '—']);
  return items;
}

// ---- shading and doors from Home Assistant (display-only; only inside the HA panel) ----
// The HA panel passes its cover mapping as MPC_HOST.covers (this repo holds no entity ids):
//   {entity, kind: 'blind' | 'garage', zone, azimuth}  a window jalousie / a garage sectional door, matched
//                                                      to a floorplan.json opening by zone + facade azimuth
//   {entity, kind: 'pergola_roof'} / {entity, kind: 'pergola_screen', side: 'se' | 'sw' | 'nw'} /
//   {entity, kind: 'pergola_light'}
// plus MPC_HOST.pergola = {polygon: [[x, y], ...] (floorplan metres), height_m} or null, and optionally
// MPC_HOST.history(entityIds, startIso, endIso) -> {entity: [[iso, state, attrs], ...]}. HA's cover
// convention: position 100 = fully open (blind raised, door open), 0 = closed; tilt 100 = slats open.
// Nothing here runs on the brain-served page (no MPC_HOST.covers, no hass there).
const FP_COVER_PITCH = 0.1; // drawn slat pitch of a window jalousie, m
const FP_BLIND_CLOSED_DEG = 80; // a closed jalousie's slats, degrees off horizontal
const FP_SIDE_AZ = { n: 0, ne: 45, e: 90, se: 135, s: 180, sw: 225, w: 270, nw: 315 };
const fpAngDiff = (a, b) => Math.abs((((a - b) % 360) + 540) % 360 - 180);
function fpCoversOn(host) {
  return !!(host && Array.isArray(host.covers) && host.covers.length && host.hass && host.hass.states && typeof host.hass.states === 'object');
}
// The floorplan opening a blind or garage cover belongs to: an outside opening of its zone, the right
// kind first (a blind: a window before a glazed door; a garage: a sectional door before any other door),
// then the facade azimuth. 'exact' within 2 degrees, else the 'nearest' facade within 90 degrees, else
// 'none'. `ambiguous` when another opening matches equally well.
function fpCoverOpening(geom, cover) {
  const garage = cover.kind === 'garage', az = +cover.azimuth, hasAz = cover.azimuth != null && isFinite(az);
  const cands = [];
  for (const f of fpFloors(geom)) {
    for (const o of f.openings || []) {
      const rooms = o.rooms || [];
      if (!rooms.includes('outside') || !rooms.includes(cover.zone)) continue;
      const rank = garage ? (o.type === 'sectional' ? 0 : o.kind === 'door' ? 1 : -1)
        : o.kind === 'window' && o.type === 'window' ? 0 : o.kind === 'window' || o.type === 'glazed' ? 1 : -1;
      if (rank < 0) continue;
      const d = hasAz && o.azimuth_deg != null ? fpAngDiff(o.azimuth_deg, az) : hasAz ? 180 : 0;
      cands.push({ floor: f.id, o, rank, d, exact: hasAz && d <= 2 });
    }
  }
  const usable = cands.filter((c) => c.d <= 90);
  if (!usable.length) return { floor: null, o: null, match: 'none', ambiguous: false };
  usable.sort((p, q) => (q.exact - p.exact) || (p.rank - q.rank) || (p.d - q.d));
  const best = usable[0];
  const ambiguous = usable.some((c, i) => i > 0 && c.exact === best.exact && c.rank === best.rank && Math.abs(c.d - best.d) < 1);
  return { floor: best.floor, o: best.o, match: best.exact ? 'exact' : 'nearest', ambiguous };
}
// ASSUMPTION, for the owner to correct through HA YAML (`mpc_brain: pergola:`): the pergola is not in the
// DXF. By default it stands on the terrace in front of the living room's south-west glazed portal, as
// wide as that portal and the kitchen's terrace door together (jamb to jamb along the wall), 4.0 m deep
// from the outside face of the wall. Null when the plan has no such portal.
const FP_PERGOLA_DEPTH = 4.0;
function fpDefaultPergola(geom) {
  const floors = fpFloors(geom);
  const g = floors.find((f) => f.id === 'ground') || floors[0];
  const ext = ((g && g.openings) || []).filter((o) => o.type === 'glazed' && (o.rooms || []).includes('outside'));
  const portal = ext.filter((o) => o.rooms.includes('livingroom')).sort((p, q) => q.width_m - p.width_m)[0];
  if (!portal) return null;
  const span = ext.filter((o) => (o.rooms.includes('livingroom') || o.rooms.includes('kitchen')) && fpAngDiff(o.azimuth_deg, portal.azimuth_deg) < 2);
  const L = Math.hypot(portal.b[0] - portal.a[0], portal.b[1] - portal.a[1]) || 1;
  const u = [(portal.b[0] - portal.a[0]) / L, (portal.b[1] - portal.a[1]) / L], n = portal.n;
  const ts = span.flatMap((o) => [o.a, o.b]).map((p) => (p[0] - portal.a[0]) * u[0] + (p[1] - portal.a[1]) * u[1]);
  const t0 = Math.min(...ts), t1 = Math.max(...ts), k = portal.depth_m / 2;
  const at = (t, d) => [+(portal.a[0] + u[0] * t + n[0] * (k + d)).toFixed(3), +(portal.a[1] + u[1] * t + n[1] * (k + d)).toFixed(3)];
  return [at(t0, 0), at(t1, 0), at(t1, FP_PERGOLA_DEPTH), at(t0, FP_PERGOLA_DEPTH)];
}
// The pergola as drawn: the host's polygon (else the default above), its height and its edges with
// outward normals and facade azimuths (a screen hangs on the edge nearest its compass side).
function fpPergolaGeom(geom, spec) {
  const given = spec && Array.isArray(spec.polygon) && spec.polygon.length >= 3 && spec.polygon.every((p) => Array.isArray(p) && isFinite(p[0]) && isFinite(p[1]));
  const polygon = given ? spec.polygon.map((p) => [+p[0], +p[1]]) : fpDefaultPergola(geom);
  if (!polygon) return null;
  const upAz = geom.frame ? geom.frame.up_azimuth_deg : 0;
  const edges = fpOutlineFacades(polygon).map((e) => ({ ...e, az: fpPlanAzimuth(e.n, upAz) }));
  return { polygon, height: spec && spec.height_m > 0 ? +spec.height_m : 2.5, edges, assumed: !given };
}
// Index of the pergola edge facing compass side `side` ('se', ...), within 50 degrees, else -1.
function fpPergolaSide(edges, side) {
  const az = FP_SIDE_AZ[String(side || '').toLowerCase()];
  if (az == null) return -1;
  let best = -1, bd = 50.0001;
  edges.forEach((e, i) => { const d = fpAngDiff(e.az, az); if (d < bd) { bd = d; best = i; } });
  return best;
}
// Everything the views draw, from the host's covers: the matched openings, the pergola with its roof,
// screens and light, and what could not be placed. Two covers claiming one opening are both ambiguous.
function fpCoverMap(geom, covers, pergolaSpec) {
  const out = { openings: [], pergola: null, unmatched: [] };
  const list = (Array.isArray(covers) ? covers : []).filter((c) => c && typeof c.entity === 'string');
  for (const c of list.filter((x) => x.kind === 'blind' || x.kind === 'garage')) {
    const m = fpCoverOpening(geom, c);
    if (!m.o) { out.unmatched.push(c.entity); continue; }
    out.openings.push({ entity: c.entity, kind: c.kind, cover: c, floor: m.floor, o: m.o, match: m.match, ambiguous: m.ambiguous });
  }
  for (const e of out.openings) if (out.openings.some((x) => x !== e && x.o === e.o)) e.ambiguous = true;
  const roof = list.find((c) => c.kind === 'pergola_roof');
  const light = list.find((c) => c.kind === 'pergola_light');
  const screens = list.filter((c) => c.kind === 'pergola_screen');
  if (roof || light || screens.length) {
    const p = fpPergolaGeom(geom, pergolaSpec);
    if (p) {
      p.roof = roof ? roof.entity : null;
      p.light = light ? light.entity : null;
      p.screens = screens.map((c) => ({ entity: c.entity, side: String(c.side || '').toLowerCase(), cover: c, edge: fpPergolaSide(p.edges, c.side) }));
      p.screens.filter((s) => s.edge < 0).forEach((s) => out.unmatched.push(s.entity));
      p.screens = p.screens.filter((s) => s.edge >= 0);
      out.pergola = p;
    } else out.unmatched.push(...[roof, light, ...screens].filter(Boolean).map((c) => c.entity));
  }
  return out;
}
// One HA entity state as the views need it. Covers: position / tilt 0..100 (a missing position falls
// back to open = 100 / closed = 0), `known` false when HA says unavailable / unknown or nothing usable
// is reported. The light: on / off.
function fpCoverState(raw, kind) {
  const s = raw && typeof raw.state === 'string' ? raw.state : 'unknown';
  if (kind === 'pergola_light') return { known: s === 'on' || s === 'off', state: s, on: s === 'on' };
  const none = { known: false, state: s, pos: null, tilt: null, moving: null };
  if (!raw || s === 'unavailable' || s === 'unknown') return none;
  const a = raw.attributes || {};
  const num = (v) => (v != null && v !== '' && isFinite(+v) ? Math.max(0, Math.min(100, +v)) : null);
  let pos = num(a.current_position);
  if (pos == null) pos = s === 'open' ? 100 : s === 'closed' ? 0 : null;
  const tilt = num(a.current_tilt_position);
  const known = kind === 'pergola_roof' ? tilt != null || pos != null : pos != null;
  return known ? { known, state: s, pos, tilt, moving: s === 'opening' || s === 'closing' ? s : null } : none;
}
// Geometry parameters from a state: `drop` 0..1 is how much of the opening is covered from the top (a
// blind lowered, a door or screen closed); a jalousie's `slatDeg` is degrees off horizontal (0 = open,
// FP_BLIND_CLOSED_DEG = closed; 45 when the tilt is not reported); the louvre roof's `bladeDeg` is
// degrees off vertical (0 = open to the sky, 90 = flat and closed). Unknown: a blind or screen is
// drawn raised, a door closed, the roof half open, all grey.
function fpCoverGeom(kind, st) {
  const known = !!(st && st.known);
  const drop = known && st.pos != null ? (100 - st.pos) / 100 : null;
  if (kind === 'blind') {
    const d = drop == null ? 0 : drop;
    const slatDeg = known && st.tilt != null ? (100 - st.tilt) / 100 * FP_BLIND_CLOSED_DEG : 45;
    return { drop: d, slatDeg, raised: d < 0.02, known };
  }
  if (kind === 'garage') return { drop: drop == null ? 1 : drop, known };
  if (kind === 'pergola_screen') return { drop: drop == null ? 0 : drop, known };
  if (kind === 'pergola_roof') {
    const t = known ? (st.tilt != null ? st.tilt : st.pos) : null;
    return { bladeDeg: t == null ? 45 : (100 - t) / 100 * 90, known };
  }
  if (kind === 'pergola_light') return { on: known && st.on, known };
  return { known };
}
// How a state reads, in words: "raised", "60 % down, slats open", "35 % open, closing", "unavailable".
function fpCoverText(kind, st) {
  if (!st || !st.known) return st && st.state === 'unavailable' ? 'unavailable' : 'unknown';
  if (kind === 'pergola_light') return st.on ? 'on' : 'off';
  const pct = (v) => `${Math.round(v)} %`;
  const slats = (t) => (t == null ? null : t >= 99.5 ? 'slats open' : t <= 0.5 ? 'slats closed' : `slats ${pct(t)} open`);
  const moving = st.moving === 'opening' ? (kind === 'blind' || kind === 'pergola_screen' ? 'raising' : 'opening')
    : st.moving === 'closing' ? (kind === 'blind' || kind === 'pergola_screen' ? 'lowering' : 'closing') : null;
  let parts;
  if (kind === 'pergola_roof') {
    const t = st.tilt != null ? st.tilt : st.pos;
    parts = [t >= 99.5 ? 'open' : t <= 0.5 ? 'closed' : `${pct(t)} open`];
  } else if (kind === 'garage') {
    parts = [st.pos >= 99.5 ? 'open' : st.pos <= 0.5 ? 'closed' : `${pct(st.pos)} open`];
  } else {
    const down = 100 - st.pos;
    parts = [down <= 0.5 ? 'raised' : down >= 99.5 ? 'down' : `${pct(down)} down`];
    if (kind === 'blind' && down > 0.5) parts.push(slats(st.tilt));
  }
  if (moving) parts.push(moving);
  return parts.filter(Boolean).join(', ');
}
// What a cover is called in the views: the host's `name`, else from the plan. A garage door on the
// same facade as the house's front door (the entrance's outside door) is the street door, the one
// opposite the garden door.
function fpCoverName(e, geom) {
  if (e.cover && typeof e.cover.name === 'string' && e.cover.name) return e.cover.name;
  const word = (az) => { const w = fpCompassWord(az); return w[0].toUpperCase() + w.slice(1); };
  if (e.kind === 'garage') {
    const front = fpFloors(geom).flatMap((f) => f.openings || []).find((o) => o.kind === 'door' && (o.rooms || []).includes('entrance') && o.rooms.includes('outside') && o.azimuth_deg != null);
    if (front && fpAngDiff(front.azimuth_deg, e.o.azimuth_deg) <= 2) return 'Street door';
    if (front && fpAngDiff(front.azimuth_deg, e.o.azimuth_deg) >= 135) return 'Garden door';
    return `${word(e.o.azimuth_deg)} door`;
  }
  return `${word(e.o.azimuth_deg)} window`;
}
// A recorded entity at instant T: the last sample at or before T ([[iso, state, attrs]]), as an HA
// state object, or null when the record starts after T (or is empty).
function fpHistoryAt(rows, T) {
  let best = null, bt = -Infinity;
  for (const r of rows || []) {
    const t = Array.isArray(r) ? Date.parse(r[0]) : NaN;
    if (isFinite(t) && t <= T && t >= bt) { bt = t; best = r; }
  }
  return best ? { state: best[1], attributes: best[2] || {} } : null;
}
// Every mapped entity at the time bar's instant: live states now and in the plan (nothing plans covers,
// so the future shows the present, `note: 'now'`); in the past the HA history at T (`note: 'history'`),
// or unknown while it loads ('loading') or when the host has none ('nohistory').
// hist: {data, loading} or null; returns {states: {entity: fpCoverState}, note}.
function fpCoverStatesAt(map, kind, T, hassStates, hist) {
  const kinds = {};
  for (const e of map.openings) kinds[e.entity] = e.kind;
  const p = map.pergola;
  if (p) {
    if (p.roof) kinds[p.roof] = 'pergola_roof';
    if (p.light) kinds[p.light] = 'pergola_light';
    for (const s of p.screens) kinds[s.entity] = 'pergola_screen';
  }
  const past = kind === 'past';
  const note = !past ? (kind === 'plan' ? 'now' : null) : hist && hist.data ? 'history' : hist && hist.loading ? 'loading' : 'nohistory';
  const states = {};
  for (const ent in kinds) {
    const raw = !past ? (hassStates || {})[ent] : hist && hist.data ? fpHistoryAt(hist.data[ent], T) : null;
    states[ent] = fpCoverState(raw, kinds[ent]);
  }
  return { states, kinds, note };
}
// The strip under the readout: [label, text] for the pergola, each screen, the light, the garage doors
// and a count of the window blinds down. `names`: {entity: display name}.
function fpShadeItems(map, cs, names) {
  const items = [];
  const st = (ent) => cs.states[ent];
  const p = map.pergola;
  if (p && p.roof) items.push(['Pergola roof', fpCoverText('pergola_roof', st(p.roof))]);
  if (p) for (const s of p.screens) items.push([`Screen ${s.side.toUpperCase()}`, fpCoverText('pergola_screen', st(s.entity))]);
  if (p && p.light) items.push(['Pergola light', fpCoverText('pergola_light', st(p.light))]);
  for (const e of map.openings.filter((x) => x.kind === 'garage')) items.push([names[e.entity] || e.entity, fpCoverText('garage', st(e.entity))]);
  const blinds = map.openings.filter((x) => x.kind === 'blind');
  if (blinds.length) {
    const known = blinds.filter((e) => st(e.entity) && st(e.entity).known);
    const down = known.filter((e) => st(e.entity).pos < 99.5).length;
    const unk = blinds.length - known.length;
    items.push(['Blinds', `${down} of ${blinds.length} down${unk ? `, ${unk} unknown` : ''}`]);
  }
  return items;
}
const FP_COVER_NOTES = { now: 'Shown as they are now: nothing plans the covers', loading: 'Loading the covers’ history…', nohistory: 'No history of the covers for this time' };

// ---- DOM: build once per mount, repaint in place on every poll ----
const fpState = {
  geom: null, geomPromise: null, rooms: [], byZone: {}, rate: null,
  mode: 'stacked', s: 1, narrow: false, layout: null, k: 1, els: null,
  hot: null, pinned: null, anim: 0, animating: false, introDone: false, lastPointer: null, openedAt: 0,
  tlOffset: 0, tlShown: 0, tlPlay: null, tlSrc: {}, roomsLive: [], roomsPlanAt: null, sensorError: null,
  fullPlan: null, fullPlanAt: null, fullTry: { retryAt: 0, busy: false }, onScreen: true, cardRect: null,
};
const FP_WALL_H = 0.26;
const FP_EXTRUDE_N = 6;
const fpMat = (m) => `matrix(${m.map((v) => +v.toFixed(4)).join(' ')})`;
const fpPoly = (poly) => poly.map((p, i) => `${i ? 'L' : 'M'}${p[0]} ${p[1]}`).join('') + 'Z';
const fpTheme = () => (document.documentElement.getAttribute('data-theme') === 'light' ? 'light' : 'dark');
const fpReduced = () => window.matchMedia && matchMedia('(prefers-reduced-motion: reduce)').matches;
const fpIsSheet = () => !(window.matchMedia && matchMedia('(hover: hover) and (pointer: fine)').matches) || innerWidth < 720;
const fpSvgEl = (tag, attrs, parent) => {
  const e = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (parent) parent.appendChild(e);
  return e;
};
// The user's explicit choice (only ever written by the toggle), or null.
function fpSavedChoice() {
  try { const v = localStorage.getItem(FP_MODE_KEY); return v === 'flat' || v === 'stacked' || v === '3d' ? v : null; } catch (e) { return null; }
}
// With no saved choice the view follows the screen: phones flat (the stacked model is too small to read), the rest stacked.
function fpDefaultMode() { return innerWidth < 560 ? 'flat' : 'stacked'; }
function fpLoadGeom() {
  if (!fpState.geomPromise) {
    fpState.geomPromise = mpcHost().fetch(mpcAsset('floorplan.json')).then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .catch((e) => { console.error('floorplan.json', e); fpState.geomPromise = null; return null; });
  }
  return fpState.geomPromise;
}

const fpMountHtml = () => `
  <section class="card span-full fp-card" id="fp-card" aria-labelledby="fp-title" data-mode="${fpState.mode}">
    <div class="fp-head">
      <div class="fp-titles">
        <div class="card-title" id="fp-title"><span class="ico">🏠</span> House</div>
        <p class="fp-summary" id="fp-summary" aria-live="polite">Loading rooms…</p>
        <p class="fp-sensor-err" id="fp-sensor-err" hidden></p>
      </div>
      <div class="fp-seg" role="group" aria-label="Floor plan view">
        <button type="button" data-mode="stacked" aria-pressed="true">Stacked</button>
        <button type="button" data-mode="flat" aria-pressed="false">Flat</button>
        <button type="button" data-mode="3d" aria-pressed="false">3D</button>
      </div>
    </div>
    <div class="fp-stats" id="fp-stats"></div>
    <div class="fp-watch" id="fp-chips" hidden></div>
    <div class="fp-stage" id="fp-stage">
      <svg id="fp-svg" xmlns="http://www.w3.org/2000/svg" tabindex="-1" role="group" aria-label="House floor plan, both floors"></svg>
      <div class="fp-empty" id="fp-empty">Loading the floor plan…</div>
      <div class="fp-attic" id="fp-attic" hidden></div>
      <div class="fp-note" id="fp-note" role="status" hidden></div>
    </div>
    <div class="fp-time" id="fp-time" role="group" aria-label="House timeline: a day back to the end of the plan" hidden>
      <button type="button" class="fp-play" id="fp-play" aria-label="Play the timeline" title="Play the timeline"><svg viewBox="0 0 12 12" aria-hidden="true"><path class="fp-i-play" d="M3 1.8v8.4l7-4.2z"/><path class="fp-i-pause" d="M2.8 2h2.2v8H2.8zM7 2h2.2v8H7z"/></svg></button>
      <div class="fp-track" id="fp-track"><span class="fp-past"></span><span class="fp-future"></span><input type="range" id="fp-slider" min="-96" max="0" step="1" value="0" aria-label="Time on the house timeline"><span class="fp-nowmark" aria-hidden="true">now</span></div>
      <span class="fp-when" id="fp-when">Now</span>
      <button type="button" class="fp-nowbtn" id="fp-nowbtn" aria-pressed="true">Now</button>
    </div>
    <div class="fp-readout" id="fp-readout"></div>
    <div class="fp-legend">
      <div class="fp-scale">
        <div class="fp-key-title">Temperature against the comfort band</div>
        <div class="fp-scale-bar" id="fp-scale-bar"></div>
        <div class="fp-scale-ticks"><span style="left:0">−3 K</span><span style="left:30%">floor</span><span style="left:70%">ceiling</span><span style="left:100%">+2 K</span></div>
      </div>
      <ul class="fp-keys">
        <li><i class="fk-heat"></i><span id="fp-key-heat">Heating per plan</span></li>
        <li><i class="fk-boost"></i>Banking heat above the ceiling</li>
        <li><i class="fk-hatch"></i>No sensor or no band</li>
        <li><i class="fk-stale"></i>Stale, over ${FP_STALE_S / 3600} h old</li>
      </ul>
    </div>
    <details class="fp-list"><summary>Room list</summary><div class="tbl-wrap"><table id="fp-table"></table></div></details>
    <div class="fp-scrim" id="fp-scrim" hidden></div>
    <div class="fp-pop" id="fp-pop" role="dialog" aria-label="Room details" hidden></div>
  </section>`;

// ---- build the SVG scene (geometry only; colours and text come from fpPaint) ----
function fpBuild() {
  const g = fpState.geom, svg = $('#fp-svg');
  const els = { layers: [], fill: {}, label: {}, hit: {} };
  const floors = fpFloors(g);
  const clips = floors.flatMap((f) => f.rooms.map((r) => `<clipPath id="fp-clip-${r.zone}"><path d="${fpPoly(r.polygon)}"/></clipPath>`)).join('');
  svg.innerHTML = `<defs>
    <pattern id="fp-hatch" width="0.5" height="0.5" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><path d="M0 0V0.5" class="fp-hatch-line"/></pattern>
    <pattern id="fp-pipes" width="0.32" height="0.32" patternUnits="userSpaceOnUse"><path d="M0 0.16H0.32" class="fp-pipe"/></pattern>
    <radialGradient id="fp-heatg" cx="0.5" cy="0.55" r="0.72"><stop offset="0" style="stop-color:var(--fp-heat);stop-opacity:.95"/><stop offset="0.55" style="stop-color:var(--fp-heat);stop-opacity:.45"/><stop offset="1" style="stop-color:var(--fp-heat);stop-opacity:.12"/></radialGradient>
    <linearGradient id="fp-sheen" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#fff" stop-opacity=".1"/><stop offset="0.5" stop-color="#fff" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity=".1"/></linearGradient>
    <radialGradient id="fp-gridfade" gradientUnits="userSpaceOnUse" cx="11" cy="7.6" r="19"><stop offset="0.2" stop-color="#fff"/><stop offset="1" stop-color="#000"/></radialGradient>
    <mask id="fp-gridmask" maskUnits="userSpaceOnUse" x="-12" y="-12" width="50" height="45"><rect x="-12" y="-12" width="50" height="45" fill="url(#fp-gridfade)"/></mask>
    <filter id="fp-blur" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation="0.7"/></filter>
    <filter id="fp-blur-s" x="-30%" y="-30%" width="160%" height="160%"><feGaussianBlur stdDeviation="0.13"/></filter>
    ${clips}
  </defs>`;
  let grid = '';
  for (let x = -8; x <= 31; x++) grid += `M${x} -9V24`;
  for (let y = -9; y <= 24; y++) grid += `M-8 ${y}H31`;
  const gridG = fpSvgEl('g', { class: 'fp-gridwrap', mask: 'url(#fp-gridmask)' }, svg);
  els.grid = fpSvgEl('g', {}, gridG);
  fpSvgEl('path', { class: 'fp-grid', d: grid }, els.grid);
  els.struts = fpSvgEl('g', { class: 'fp-struts' }, svg);
  svg.removeChild(els.struts);
  els.sit = fpSvgEl('g', { class: 'fp-sit' }, svg); // the ground floor's contact shadow on the table
  fpSvgEl('path', { d: fpPoly(floors[0].outline), filter: 'url(#fp-blur)' }, els.sit);

  floors.forEach((f, i) => {
    const layer = fpSvgEl('g', { class: 'fp-layer', 'data-floor': f.id }, svg);
    if (i === 1) { // the upper floor's soft shadow on the lower one, drawn between the two layers
      els.cast = fpSvgEl('g', { class: 'fp-cast' }, svg);
      fpSvgEl('path', { d: fpPoly(f.outline), filter: 'url(#fp-blur)' }, els.cast);
      svg.insertBefore(els.cast, layer);
    }
    const L = { f, layer, extrude: [] };
    const ex = fpSvgEl('g', { class: 'fp-extrude' }, layer);
    for (let j = 0; j < FP_EXTRUDE_N; j++) L.extrude.push(fpSvgEl('path', { class: 'fp-edge', d: fpPoly(f.outline) }, ex));
    L.body = fpSvgEl('g', { class: 'fp-body' }, layer);
    fpSvgEl('path', { class: 'fp-slab', d: fpPoly(f.outline) }, L.body);
    f.rooms.forEach((r) => {
      const fg = fpSvgEl('g', { class: 'fp-fill', 'data-zone': r.zone }, L.body);
      const d = fpPoly(r.polygon), clip = `url(#fp-clip-${r.zone})`;
      fpSvgEl('path', { class: 'fp-drop', d, filter: 'url(#fp-blur-s)' }, fg);
      const base = fpSvgEl('path', { class: 'fp-base', d }, fg);
      fpSvgEl('path', { class: 'fp-sheen', d }, fg);
      const hatch = fpSvgEl('path', { class: 'fp-hatch', d }, fg);
      const heat = fpSvgEl('g', { class: 'fp-heat' }, fg);
      fpSvgEl('path', { class: 'fp-glow', d }, heat);
      fpSvgEl('path', { class: 'fp-pipes', d }, heat);
      fpSvgEl('path', { class: 'fp-ao fp-ao-1', d, 'clip-path': clip }, fg);
      fpSvgEl('path', { class: 'fp-ao fp-ao-2', d, 'clip-path': clip }, fg);
      fpSvgEl('path', { class: 'fp-ao fp-ao-3', d, 'clip-path': clip }, fg);
      const edge = fpSvgEl('path', { class: 'fp-redge', d }, fg);
      els.fill[r.zone] = { g: fg, base, hatch, heat, edge, room: r, layer: i };
    });
    const det = fpSvgEl('g', { class: 'fp-details' }, L.body);
    // a stairwell reads as part of the room it opens from (its fill follows that room), not a dark hole
    (f.voids || []).forEach((v) => {
      const zone = fpVoidRoom(v.polygon, f.rooms);
      const el = fpSvgEl('path', { class: 'fp-void', d: fpPoly(v.polygon) }, det);
      if (zone) (els.voids = els.voids || []).push({ el, zone });
    });
    const stairs = (f.details.stairs || []).map((s) => `M${s[0][0]} ${s[0][1]}L${s[1][0]} ${s[1][1]}`).join('');
    if (stairs) fpSvgEl('path', { class: 'fp-line', d: stairs }, det);
    // doors and windows: the walls arrive with the openings already cut out of them
    const ops = (f.openings || []).map((o) => ({ o, shape: fpOpeningShape(o) }));
    const sills0 = ops.filter((x) => x.o.kind === 'door').map((x) => x.shape.rect).join('');
    if (sills0) fpSvgEl('path', { class: 'fp-threshold', d: sills0 }, det);
    const leaves = ops.map((x) => x.shape.floor).join('');
    if (leaves) fpSvgEl('path', { class: 'fp-line fp-door', d: leaves }, det);
    // a chimney block stands like a wall; the firebox is a dark edge along its glass at the floor
    const feats = f.features || [];
    const wd = (f.walls || []).map(fpPoly).join('') + feats.map((x) => fpPoly(x.polygon)).join('');
    feats.filter((x) => x.firebox).forEach((x) => {
      const fb = fpSvgEl('path', { class: 'fp-firebox', d: fpFireboxPath(x.firebox) }, det);
      (els.fireboxes = els.fireboxes || []).push({ el: fb, zone: x.zone });
    });
    L.wallSide = [];
    for (let j = 0; j < 3; j++) L.wallSide.push(fpSvgEl('path', { class: 'fp-wall-side', d: wd }, L.body));
    // a window's sill block stands lower than the wall caps; the glass rises from it to the head
    L.glass = [];
    const sills = fpSvgEl('g', { class: 'fp-raised' }, L.body);
    ops.filter((x) => x.o.head_m > 0).forEach(({ o, shape }) => {
      const h = f.height_m > 0 ? f.height_m : 2.5;
      // a clerestory (a 20 cm strip under the ceiling) keeps the wall at full height below its glass
      const gl = { o, high: o.type === 'clerestory', sill: Math.min(1, (o.sill_m || 0) / h), head: Math.min(1, o.head_m / h), sides: [], top: null };
      if (gl.sill > 0) {
        for (let j = 0; j < 2; j++) gl.sides.push(fpSvgEl('path', { class: 'fp-wall-side', d: shape.rect }, sills));
        gl.top = fpSvgEl('g', {}, sills);
        fpSvgEl('path', { class: 'fp-sill', d: shape.rect }, gl.top);
        fpSvgEl('path', { class: 'fp-frame', d: shape.jambs }, gl.top);
      }
      L.glass.push(gl);
    });
    L.raised = fpSvgEl('g', { class: 'fp-raised' }, L.body);
    L.wallTop = fpSvgEl('path', { class: 'fp-wall-top', d: wd }, L.raised);
    // the outside faces, glowing warm in proportion to the sun on them (fpPaint)
    fpOutlineFacades(f.outline).forEach((x) => {
      const el = fpSvgEl('path', { class: 'fp-sunwall', d: `M${x.a[0]} ${x.a[1]}L${x.b[0]} ${x.b[1]}` }, L.raised);
      (els.sunWalls = els.sunWalls || []).push({ el, n: x.n });
    });
    feats.forEach((x) => { const fl = x.flue; if (fl && fl.center) fpSvgEl('circle', { class: 'fp-flue', cx: fl.center[0], cy: fl.center[1], r: fl.r || 0.09 }, L.raised); });
    const panes = fpSvgEl('g', { class: 'fp-raised' }, L.body);
    L.glass.forEach((gl) => {
      const hi = gl.high ? ' is-high' : '';
      gl.pane = fpSvgEl('path', { class: 'fp-pane' + hi }, panes);
      gl.glow = fpSvgEl('path', { class: 'fp-win-glow' + hi }, panes);
      gl.line = fpSvgEl('path', { class: 'fp-win' + hi }, panes);
      gl.frame = fpSvgEl('path', { class: 'fp-frame' + hi }, panes);
      (els.glints = els.glints || []).push({ el: gl.line, glow: gl.glow, o: gl.o });
    });
    // interaction: transparent room polygons in plan space (the visible fills lift; these stay put)
    const hits = fpSvgEl('g', { class: 'fp-hits' }, L.body);
    f.rooms.forEach((r) => { els.hit[r.zone] = fpSvgEl('path', { class: 'fp-hit', d: fpPoly(r.polygon), 'data-zone': r.zone }, hits); });
    L.labels = fpSvgEl('g', { class: 'fp-labels' }, layer);
    f.rooms.forEach((r) => { els.label[r.zone] = fpLabelEl(L.labels, r.zone, r.name); });
    els.layers.push(L);
  });
  svg.insertBefore(els.struts, els.layers[1].layer);
  // floor captions; the ground one carries the compass and, in the flat view, a 5 m scale bar
  els.captions = floors.map((f, i) => {
    const cg = fpSvgEl('g', { class: 'fp-capg' }, svg);
    const text = fpSvgEl('text', { class: 'fp-caption', y: 17 }, cg);
    text.textContent = f.name;
    const c = { g: cg, text, tw: 0 };
    if (i === 0) {
      c.compass = fpSvgEl('g', { class: 'fp-compass', role: 'img', 'aria-label': 'Compass: north' }, cg);
      fpSvgEl('circle', { r: 8 }, c.compass);
      c.needle = fpSvgEl('g', {}, c.compass);
      fpSvgEl('path', { class: 'fp-needle-n', d: 'M0 -6.6L2.1 0H-2.1Z' }, c.needle);
      fpSvgEl('path', { class: 'fp-needle-s', d: 'M0 6.6L2.1 0H-2.1Z' }, c.needle);
      c.sun = fpSvgEl('g', { class: 'fp-compass-sun' }, c.compass);
      fpSvgEl('path', { d: 'M0 -2.2V-8' }, c.sun);
      fpSvgEl('circle', { cy: -8, r: 2.4 }, c.sun);
      c.north = fpSvgEl('text', { class: 'fp-north', 'text-anchor': 'middle' }, c.compass);
      c.north.textContent = 'N';
      c.scale = fpSvgEl('g', { class: 'fp-scalebar' }, cg);
      c.scaleLine = fpSvgEl('path', {}, c.scale);
      c.scaleTxt = fpSvgEl('text', { class: 'fp-scaletxt', y: 16 }, c.scale);
      c.scaleTxt.textContent = '5 m';
    }
    return c;
  });
  const top = fpSvgEl('g', { class: 'fp-toplabels' }, svg);
  els.layers.forEach((L) => top.appendChild(L.labels));
  fpState.els = els;
  if (fpCovInit()) fpBuildCovers2d();
}
function fpLabelEl(parent, zone, name) {
  const room = fpSvgEl('g', { class: 'fp-room', 'data-zone': zone, role: 'button', tabindex: '0', 'aria-label': name }, parent);
  const pos = fpSvgEl('g', { class: 'fp-lab' }, room);
  const inner = fpSvgEl('g', { class: 'fp-lab-in' }, pos);
  const e = { room, pos, inner };
  e.ring = fpSvgEl('rect', { class: 'fp-ring', rx: 8 }, inner);
  e.name = fpSvgEl('text', { class: 'fp-name', 'text-anchor': 'middle' }, inner);
  e.temp = fpSvgEl('text', { class: 'fp-temp', 'text-anchor': 'middle' }, inner);
  e.stale = fpSvgEl('path', { class: 'fp-stale', d: 'M0 -4A4 4 0 1 1 -.01 -4ZM0 -2.2V0L1.6 1' }, inner);
  e.stale.appendChild(document.createElementNS('http://www.w3.org/2000/svg', 'title')).textContent = 'stale reading';
  e.flame = fpSvgEl('path', { class: 'fp-flame', d: 'M0 -5.6C.35 -3.1 3.4 -2.4 3.4 1A3.4 3.4 0 0 1 -3.4 1C-3.4 -.5 -2.6 -1.5 -1.9 -2.4C-1.7 -1 -1 -.8 -.8 -1.4C-.5 -2.6 -.4 -3.9 0 -5.6Z' }, inner);
  e.pres = fpSvgEl('circle', { class: 'fp-pres', r: 2.4 }, inner);
  return e;
}

// ---- per-frame geometry: matrices, walls, captions, labels, viewBox ----
function fpApplyScene() {
  const st = fpState, els = st.els, g = st.geom;
  if (!els) return;
  const s = st.s;
  const M = (i) => planMatrix('stacked', i, s, st.layout);
  const svg = $('#fp-svg');
  const vb = fpBounds(g, st.layout, s, st.k);
  svg.setAttribute('viewBox', `${vb.x.toFixed(3)} ${vb.y.toFixed(3)} ${vb.w.toFixed(3)} ${vb.h.toFixed(3)}`);
  const r = svg.getBoundingClientRect();
  st.k = r.width > 0 && r.height > 0 ? Math.min(r.width / vb.w, r.height / vb.h) : 30;
  const k = st.k;
  els.grid.setAttribute('transform', fpMat(M(0)));
  // walls stand up in the stacked model and lie almost flat in the plan view
  const wallH = FP_WALL_H * (0.15 + 0.85 * s);
  els.layers.forEach((L, i) => {
    const m = M(i);
    L.body.setAttribute('transform', fpMat(m));
    L.extrude.forEach((p, j) => p.setAttribute('transform', fpMat([m[0], m[1], m[2], m[3], m[4], m[5] + FP_SLAB * (j + 1) / FP_EXTRUDE_N])));
    const rv = fpScreenToPlan(m, 0, -wallH);
    L.raised.setAttribute('transform', `translate(${rv[0].toFixed(4)} ${rv[1].toFixed(4)})`);
    L.wallSide.forEach((p, j) => {
      const q = fpScreenToPlan(m, 0, -wallH * (j + 1) / 4);
      p.setAttribute('transform', `translate(${q[0].toFixed(4)} ${q[1].toFixed(4)})`);
    });
    const tr = (v) => `translate(${v[0].toFixed(4)} ${v[1].toFixed(4)})`;
    L.glass.forEach((gl) => {
      const vs = fpScreenToPlan(m, 0, -wallH * gl.sill), vh = fpScreenToPlan(m, 0, -wallH * gl.head);
      gl.sides.forEach((p, j) => p.setAttribute('transform', tr(fpScreenToPlan(m, 0, -wallH * gl.sill * (j + 1) / 3))));
      if (gl.top) gl.top.setAttribute('transform', tr(vs));
      const g = fpPaneShape(gl.o, vs, vh);
      gl.pane.setAttribute('d', g.pane);
      gl.glow.setAttribute('d', gl.high ? g.head : g.sill);
      gl.line.setAttribute('d', gl.high ? g.head : g.sill);
      gl.frame.setAttribute('d', g.frame);
    });
    const lv = fpScreenToPlan(m, 0, -4 / k);
    L.layer.style.setProperty('--lx', `${lv[0].toFixed(4)}px`);
    L.layer.style.setProperty('--ly', `${lv[1].toFixed(4)}px`);
    const cap = els.captions[i];
    const x0 = Math.min(...L.f.outline.map((p) => p[0]));
    let y0 = Math.max(...L.f.outline.filter((p) => p[0] === x0).map((p) => p[1]));
    // the HA panel's pergola stands on the terrace under this corner: the caption goes below it
    if (i === 0 && !st.narrow && els.cov && els.cov.perg && st.cov && st.cov.map.pergola) y0 = Math.max(y0, ...st.cov.map.pergola.polygon.map((p) => p[1]));
    const cp = project([x0, y0], m);
    cap.g.setAttribute('transform', `translate(${cp[0].toFixed(3)} ${(cp[1] + FP_SLAB).toFixed(3)}) scale(${(1 / k).toFixed(5)})`);
    if (cap.compass) {
      if (!cap.tw && cap.text.getComputedTextLength) cap.tw = cap.text.getComputedTextLength();
      const cx = (cap.tw || 74) + 20;
      const deg = fpNorthDeg(m), a = deg * Math.PI / 180;
      cap.compass.setAttribute('transform', `translate(${cx.toFixed(1)} 12)`);
      cap.needle.setAttribute('transform', `rotate(${deg.toFixed(2)})`);
      const sunUp = st.sun && st.sun.elevation > 0;
      cap.sun.style.display = sunUp ? '' : 'none';
      if (sunUp) cap.sun.setAttribute('transform', `rotate(${(deg + st.sun.azimuth).toFixed(2)})`);
      cap.north.setAttribute('x', (Math.sin(a) * 13.5).toFixed(2));
      cap.north.setAttribute('y', (-Math.cos(a) * 13.5 + 3).toFixed(2));
      const sx = cx + 24, len = 5 * k;
      cap.scaleLine.setAttribute('d', `M${sx.toFixed(1)} 12H${(sx + len).toFixed(1)}M${sx.toFixed(1)} 9V15M${(sx + len).toFixed(1)} 9V15`);
      cap.scaleTxt.setAttribute('x', (sx + len + 6).toFixed(1));
      cap.scale.style.opacity = Math.max(0, 1 - 2 * s).toFixed(3);
    }
  });
  const m0 = M(0), m1 = M(1);
  els.sit.setAttribute('transform', fpMat([m0[0], m0[1], m0[2], m0[3], m0[4], m0[5] + FP_SLAB + 0.45]));
  els.cast.setAttribute('transform', fpMat([m0[0], m0[1], m0[2], m0[3], m0[4], m0[5] + 0.35 * s]));
  els.cast.style.opacity = (0.1 * s).toFixed(3);
  // exploded-view guides: dashed verticals from each footprint corner of the upper floor to the ground
  const outline = els.layers[1] ? els.layers[1].f.outline : [];
  let sd = '';
  outline.forEach((p) => {
    const a = project(p, m0), b = project(p, m1);
    sd += `M${a[0].toFixed(3)} ${a[1].toFixed(3)}L${b[0].toFixed(3)} ${(b[1] + FP_SLAB).toFixed(3)}`;
  });
  if (!els.strutPath) els.strutPath = fpSvgEl('path', {}, els.struts);
  els.strutPath.setAttribute('d', sd);
  els.struts.style.opacity = s.toFixed(3);
  fpPlaceLabels();
}
// The label anchor on screen for a zone, under the current matrices.
function fpAnchor(zone) {
  const st = fpState, fl = st.els.fill[zone];
  return fl ? project(fl.room.label, planMatrix('stacked', fl.layer, st.s, st.layout)) : [0, 0];
}
function fpPlaceLabels() {
  const st = fpState, e = st.els;
  for (const zone in e.label) {
    const a = fpAnchor(zone);
    e.label[zone].pos.setAttribute('transform', `translate(${a[0].toFixed(3)} ${a[1].toFixed(3)}) scale(${(fpLabelScale(st.k) / st.k).toFixed(5)})`);
  }
  if (e.cov) {
    // the cover marks sit on the raised (wall-top) layer: wallH up on screen, as fpApplyScene lifts it
    const wallH = FP_WALL_H * (0.15 + 0.85 * st.s), sc = (fpLabelScale(st.k) / st.k).toFixed(5);
    for (const it of [...e.cov.items, e.cov.perg && e.cov.perg.it].filter((x) => x && x.lab)) {
      const a = project(it.at, planMatrix('stacked', it.layer, st.s, st.layout));
      it.lab.setAttribute('transform', `translate(${a[0].toFixed(3)} ${(a[1] - wallH).toFixed(3)}) scale(${sc})`);
    }
  }
}

// ---- repaint from data (fills, labels, header strip, attic, legend, list, open card) ----
function fpPaint() {
  const st = fpState, e = st.els;
  if (!e) return;
  fpCovRefresh();
  const theme = fpTheme();
  const card = $('#fp-card');
  card.style.setProperty('--fp-boost', fpScaleColor(2.5, theme));
  card.style.setProperty('--fp-stalec', fpDesaturate(fpScaleColor(2.2, theme), 0.15));
  card.style.setProperty('--fp-none', fpAnchors(theme).none);
  const bar = $('#fp-scale-bar');
  if (bar) {
    const stops = [];
    for (let x = FP_X_MIN; x <= FP_X_MAX + 1e-9; x += 0.25) stops.push(`${fpScaleColor(x, theme)} ${fpScalePct(x).toFixed(1)}%`);
    bar.style.background = `linear-gradient(90deg, ${stops.join(',')})`;
  }
  for (const zone in e.label) {
    const r = st.byZone[zone];
    const stt = fpRoomState(r);
    const fl = e.fill[zone];
    fl.base.style.fill = fpFill(stt, theme);
    fl.hatch.style.display = stt.kind === 'nosensor' || stt.kind === 'noband' ? '' : 'none';
    const heating = !!(r && r.heating && r.heating.on);
    fl.heat.style.display = heating ? '' : 'none';
    fl.g.classList.toggle('is-heating', heating);
    fl.g.classList.toggle('is-stale', !!stt.stale);
    e.label[zone].room.setAttribute('aria-label', r ? fpAria(r) : `${fl.room.name || zone}, no data`);
    e.label[zone].state = { r, stt };
  }
  for (const v of e.voids || []) v.el.style.fill = fpFill(fpRoomState(st.byZone[v.zone]), theme);
  // the sun on the outside walls and windows (none at night)
  const site = st.site && isFinite(st.site.lat) ? st.site : null;
  st.sun = site ? fpSunPosition(fpSunMs().ms, site.lat, site.lon) : null;
  const upAz = st.geom.frame ? st.geom.frame.up_azimuth_deg : 0;
  for (const w of e.sunWalls || []) w.el.style.opacity = fpWallIncidence(w.n, st.sun, upAz).toFixed(3);
  const lit = new Set(fpSunlitWindows(fpFloors(st.geom), st.sun, upAz).map((x) => x.o));
  for (const g of e.glints || []) { g.el.classList.toggle('is-sunlit', lit.has(g.o)); g.glow.classList.toggle('is-sunlit', lit.has(g.o)); }
  for (const fb of e.fireboxes || []) fb.el.classList.toggle('is-lit', !!fpFireLikely(st.byZone[fb.zone]));
  fpRenderLabels();
  const rooms = st.rooms;
  const summary = rooms.length ? fpHouseSummary(rooms) : 'Room data unavailable';
  if ($('#fp-summary').textContent !== summary) $('#fp-summary').textContent = summary; // a live region: write only a change
  $('#fp-stats').innerHTML = rooms.length ? fpStatsHtml(fpStats(rooms)) : '';
  const se = $('#fp-sensor-err');
  se.hidden = !(st.sensorError && st.tlKind === 'now');
  se.title = st.sensorError || '';
  if (st.sensorError) se.textContent = fpSensorErrText(st.roomsLive);
  const att = fpAttention(rooms, 6);
  const watch = $('#fp-chips');
  const hot = (zone) => (zone === st.hot ? ' hot' : '');
  watch.hidden = !att.length;
  fpSetHtml(watch, att.length ? `<span class="fp-watch-lead">Worth a look</span>${att.map((a) => `<button type="button" class="fp-chip${hot(a.zone)}" data-zone="${esc(a.zone)}" aria-label="${esc(fpAria(a.r))}"><i class="fp-dot" style="background:${fpFill(fpRoomState(a.r), theme)}"></i><span>${esc(a.name)}</span><b>${esc(a.text)}</b></button>`).join('')}` : '');
  const attic = fpAtticRows(rooms);
  const ab = $('#fp-attic');
  ab.hidden = !attic.length;
  fpSetHtml(ab, attic.map((a) => `<button type="button" class="fp-chip fp-attic-row${a.stale ? ' is-stale' : ''}${hot(a.zone)}" data-zone="${esc(a.zone)}" aria-label="${esc(`${a.name}, ${a.value === '—' ? 'no reading' : `${a.value.replace('°', ' °C')}${a.est ? ', model estimate' : ''}`}`)}"><svg class="fp-roof-ico" viewBox="0 0 12 9" aria-hidden="true"><path d="M1 8V4.6L6 1l5 3.6V8"/></svg><span>${esc(a.name)}</span><b>${esc(a.value)}${a.est ? '<small>est.</small>' : ''}</b></button>`).join(''));
  const past = st.tlKind === 'past', heatSrc = past ? 'measured' : 'plan';
  $('#fp-table').innerHTML = `<thead><tr><th>Room</th><th>Floor</th><th>Temperature</th><th>Status</th><th>Heating (${heatSrc})</th></tr></thead><tbody>${
    rooms.map((r) => {
      const stt = fpRoomState(r);
      return `<tr><td>${esc(r.name)}</td><td>${esc(r.floor)}</td><td>${stt.temp != null ? stt.temp.toFixed(1) + ' °C' : '—'}</td><td>${esc(fpStatusPhrase(stt))}</td><td>${r.heating && r.heating.on ? `on (${heatSrc}), ${r.heating.kw.toFixed(2)} kW` : r.heated === false ? 'no heater' : !r.heating ? '—' : 'off'}</td></tr>`;
    }).join('')}</tbody>`;
  fpPaintCovers();
  if (st.pinned || st.hot) {
    fpRenderCard(st.pinned || st.hot);
    const pop = $('#fp-pop');
    if (st.shown && pop.classList.contains('is-open') && pop.classList.contains('is-float')) fpPlaceFloat(st.cardRect || fpAnchorRect(st.shown));
  }
}
// Set a container's markup (rows and chips re-rendered on every poll and timeline step). The same
// zones in the same order are patched in place: Play repaints every ~60 ms, and a press whose mousedown
// and mouseup land on different nodes clicks neither. Otherwise keyboard focus moves to the new control
// for the same zone.
function fpSetHtml(box, html) {
  if (box.fpHtml === html) return;
  box.fpHtml = html;
  const a = document.activeElement;
  const zone = a && a !== box && box.contains(a) ? a.getAttribute('data-zone') : null;
  const tpl = document.createElement('template');
  tpl.innerHTML = html;
  const next = [...tpl.content.childNodes], cur = [...box.childNodes];
  const same = next.length === cur.length && next.every((n, i) => n.nodeType === 1 && cur[i].nodeType === 1
    && n.tagName === cur[i].tagName && n.getAttribute('data-zone') === cur[i].getAttribute('data-zone'));
  if (same) next.forEach((n, i) => fpPatchEl(cur[i], n));
  else box.innerHTML = html;
  const n = zone && !box.contains(document.activeElement) && box.querySelector(`[data-zone="${zone}"]`);
  if (n) n.focus({ preventScroll: true });
}
// Down the tree while the node structure matches, then the differing subtree is replaced.
function fpPatchEl(el, src) {
  for (const { name } of [...el.attributes]) if (!src.hasAttribute(name)) el.removeAttribute(name);
  for (const { name, value } of src.attributes) if (el.getAttribute(name) !== value) el.setAttribute(name, value);
  const cur = el.childNodes, next = src.childNodes;
  const same = cur.length === next.length && [...next].every((n, i) => n.nodeName === cur[i].nodeName
    && (n.nodeType !== 1 || n.getAttribute('data-zone') === cur[i].getAttribute('data-zone')));
  if (!same) { if (el.innerHTML !== src.innerHTML) el.innerHTML = src.innerHTML; return; }
  next.forEach((n, i) => {
    if (n.nodeType === 1) fpPatchEl(cur[i], n);
    else if (cur[i].nodeValue !== n.nodeValue) cur[i].nodeValue = n.nodeValue;
  });
}
function fpRenderLabels() {
  const st = fpState, e = st.els;
  const m = (i) => planMatrix('stacked', i, st.s, st.layout);
  const ls = fpLabelScale(st.k);
  const prep = {}, items = [];
  for (const zone in e.label) {
    const L = e.label[zone];
    const fl = e.fill[zone];
    const r = L.state && L.state.r;
    const stt = L.state ? L.state.stt : { temp: null, stale: false, kind: 'nosensor' };
    const name = fl.room.name || (r ? r.name : zone);
    const mm = m(fl.layer);
    const canName = fpShowName(fl.room.area_m2, mm[0] * mm[3] - mm[1] * mm[2], st.k, name.length);
    const hasTemp = stt.temp != null;
    const heating = !!(r && r.heating && r.heating.on);
    const motion = !!(r && r.presence && r.presence.motion && r.presence.age_s != null && r.presence.age_s < 900);
    const staleW = stt.stale ? 11 : 0;
    // away from now a heated room's label carries its kW (planned, or measured in the past)
    const kwText = st.tlKind && st.tlKind !== 'now' && heating ? `${r.heating.kw.toFixed(1)} kW` : '';
    const extra = (heating ? 11 : 0) + (motion ? 6 : 0) + staleW + (kwText ? 34 : 0);
    prep[zone] = { L, name, stt, hasTemp, heating, motion, staleW, extra, kwText };
    const px = (b) => ({ w: b.w * ls, h: b.h * ls });
    const levels = [];
    if (canName) levels.push({ name: 'full', ...px(fpLabelBox(name, hasTemp, true, extra)) });
    levels.push({ name: 'bare', ...px(fpLabelBox(name, hasTemp, false, extra)) });
    levels.push({ name: 'compact', ...px(fpCompactBox(hasTemp, heating)) });
    const a = fpAnchor(zone);
    items.push({ id: zone, x: a[0] * st.k, y: a[1] * st.k, prio: fpLabelPrio(r, fl.room.area_m2), levels });
  }
  const picked = fpResolveLabels(items, 3);
  for (const zone in prep) {
    const p = prep[zone], L = p.L;
    const lv = picked[zone] || 'full';
    const hidden = lv === 'hidden';
    const compact = lv === 'compact' || hidden;
    const showName = lv === 'full';
    const box = compact ? fpCompactBox(p.hasTemp, p.heating) : fpLabelBox(p.name, p.hasTemp, showName, p.extra);
    L.room.classList.toggle('is-collapsed', hidden);
    L.room.classList.toggle('is-compact', compact);
    L.ring.setAttribute('x', -box.w / 2 - 4); L.ring.setAttribute('y', -box.h / 2 - 3);
    L.ring.setAttribute('width', box.w + 8); L.ring.setAttribute('height', box.h + 6);
    L.name.textContent = p.name;
    L.name.style.display = showName ? '' : 'none';
    L.name.setAttribute('y', -box.h / 2 + 9.5);
    L.temp.textContent = '';
    if (p.hasTemp) {
      L.temp.appendChild(document.createTextNode(p.stt.temp.toFixed(1)));
      fpSvgEl('tspan', { class: 'fp-deg' }, L.temp).textContent = '°';
      if (p.kwText && !compact) fpSvgEl('tspan', { class: 'fp-kw', dx: '4' }, L.temp).textContent = p.kwText;
    } else L.temp.textContent = st.tlKind === 'plan' || st.tlKind === 'past' ? 'no data' : p.stt.failed ? 'read failed' : 'no sensor';
    L.temp.classList.toggle('is-none', !p.hasTemp);
    const ty = compact ? 4.5 : showName ? box.h / 2 - 3.5 : 6;
    const tw = compact ? (p.hasTemp ? 28 : 40) : p.hasTemp ? 38 : 50;
    const staleW = compact ? 0 : p.staleW, motion = p.motion && !compact;
    const shift = ((p.heating ? 11 : 0) - (motion ? 6 : 0) - staleW) / 2;
    L.temp.setAttribute('x', shift + (p.kwText && !compact ? 17 : 0)); L.temp.setAttribute('y', ty);
    L.stale.style.display = p.stt.stale && !compact ? '' : 'none';
    L.stale.setAttribute('transform', `translate(${shift + tw / 2 + 7} ${ty - 5})`);
    L.flame.style.display = p.heating ? '' : 'none';
    L.flame.setAttribute('transform', `translate(${shift - tw / 2 - 4} ${ty - (compact ? 3.5 : 5)})${compact ? ' scale(.8)' : ''}`);
    L.pres.style.display = motion ? '' : 'none';
    L.pres.setAttribute('cx', shift + tw / 2 + 4); L.pres.setAttribute('cy', ty - 10);
    L.room.classList.toggle('is-stale', !!p.stt.stale);
  }
}

// ---- shading and doors on the plan (HA panel only; see fpCoverMap) ----
// The pergola's own frame: u along its longest edge (the louvres run this way), v across it into the
// polygon, the polygon's extent in both and its centroid (plan metres).
function fpPergolaFrame(poly) {
  let bi = 0, bl = 0;
  poly.forEach((a, i) => { const b = poly[(i + 1) % poly.length], l = Math.hypot(b[0] - a[0], b[1] - a[1]); if (l > bl) { bl = l; bi = i; } });
  const a = poly[bi], b = poly[(bi + 1) % poly.length];
  const u = [(b[0] - a[0]) / (bl || 1), (b[1] - a[1]) / (bl || 1)];
  const c = [poly.reduce((s, p) => s + p[0], 0) / poly.length, poly.reduce((s, p) => s + p[1], 0) / poly.length];
  let v = [-u[1], u[0]];
  if ((c[0] - a[0]) * v[0] + (c[1] - a[1]) * v[1] < 0) v = [-v[0], -v[1]];
  const us = poly.map((p) => (p[0] - a[0]) * u[0] + (p[1] - a[1]) * u[1]), vs = poly.map((p) => (p[0] - a[0]) * v[0] + (p[1] - a[1]) * v[1]);
  return { o: a, u, v, u0: Math.min(...us), u1: Math.max(...us), v0: Math.min(...vs), v1: Math.max(...vs), c };
}
// Read the host's mapping once per mount (or when the first hass arrives); false on the brain page.
function fpCovInit() {
  const st = fpState;
  st.cov = null;
  if (!st.geom || !fpCoversOn(mpcHost())) return false;
  const map = fpCoverMap(st.geom, mpcHost().covers, mpcHost().pergola);
  const names = {};
  for (const e of map.openings) names[e.entity] = fpCoverName(e, st.geom);
  const p = map.pergola;
  if (p) {
    if (p.roof) names[p.roof] = 'Pergola roof';
    if (p.light) names[p.light] = 'Pergola light';
    for (const s of p.screens) names[s.entity] = (s.cover && s.cover.name) || `Pergola screen ${s.side.toUpperCase()}`;
  }
  if (map.unmatched.length) console.warn('Home Assistant covers with no matching opening on the plan:', map.unmatched.join(', '));
  const unsure = map.openings.filter((e) => e.ambiguous || e.match !== 'exact').map((e) => `${e.entity} (${e.ambiguous ? 'ambiguous' : 'nearest facade'})`);
  if (unsure.length) console.warn('Home Assistant covers placed by a guess:', unsure.join(', '));
  st.cov = { map, names, cs: null, view: null, sig: '', hist: null, session: 0, wasPast: false, histT: 0 };
  return true;
}
// The mapped entities' live states, as one string (an 'mpc-hass' event fires on every state change in
// the house; only a change to one of ours repaints).
function fpCovSig() {
  const c = fpState.cov, s = (mpcHass() || {}).states || {};
  return c && c.cs ? Object.keys(c.cs.kinds).map((k) => { const x = s[k], a = (x && x.attributes) || {}; return `${x ? x.state : '-'}/${a.current_position}/${a.current_tilt_position}`; }).join('|') : '';
}
// States, geometry parameters and words for the time bar's instant (fp3d reads fpState.cov.view).
function fpCovRefresh() {
  const st = fpState, c = st.cov;
  if (!c) return;
  const kind = st.tlKind || 'now';
  if (kind === 'past') fpCovWantHistory(); else c.wasPast = false;
  c.cs = fpCoverStatesAt(c.map, kind, st.tlT || Date.now(), (mpcHass() || {}).states, c.hist);
  c.view = {};
  for (const ent in c.cs.kinds) {
    const k = c.cs.kinds[ent], s = c.cs.states[ent];
    c.view[ent] = { kind: k, st: s, g: fpCoverGeom(k, s), text: fpCoverText(k, s), name: c.names[ent] || ent };
  }
}
// The HA history for the bar's past (a day back), fetched once per scrub session (leaving now starts
// one), reused for two minutes across sessions, debounced so a quick pass through the past is free.
const FP_COV_HIST_REUSE_MS = 120000;
function fpCovWantHistory() {
  const c = fpState.cov, host = mpcHost();
  if (typeof host.history !== 'function') return;
  if (!c.wasPast) { c.wasPast = true; c.session++; }
  const h = c.hist;
  if (h && (h.loading || h.session === c.session || Date.now() - h.at < FP_COV_HIST_REUSE_MS)) return;
  c.hist = { data: h ? h.data : null, at: h ? h.at : 0, session: h ? h.session : 0, loading: true };
  clearTimeout(c.histT);
  c.histT = setTimeout(() => fpCovLoadHistory(c), 300);
}
function fpCovLoadHistory(c) {
  if (fpState.cov !== c) return;
  const session = c.session, end = Date.now(), start = end - (FP_PAST_STEPS + 1) * FP_STEP_MS;
  const ids = Object.keys((c.cs && c.cs.kinds) || {});
  Promise.resolve().then(() => mpcHost().history(ids, new Date(start).toISOString(), new Date(end).toISOString()))
    .then((d) => (d && typeof d === 'object' ? d : null), (e) => { console.warn('Home Assistant cover history', e); return null; })
    .then((d) => {
      if (fpState.cov !== c) return;
      c.hist = { data: d || (c.hist && c.hist.data) || null, at: Date.now(), session, loading: false };
      if (fpState.tlKind === 'past') fpCovRepaint();
    });
}
// Repaint every view's covers in place (no geometry is rebuilt).
function fpCovRepaint() {
  const st = fpState;
  if (!st.cov || !st.els || !$('#fp-stage')) return;
  fpCovRefresh();
  fpPaintCovers();
  if (fp3dActive()) window.fp3d.updateCovers();
  if (st.pinned || st.hot) fpRenderCard(st.pinned || st.hot);
}
// HA pushed new states: repaint at now and in the plan (both show the live states), not in the past.
function fpOnHass() {
  const st = fpState;
  if (!st.els || !$('#fp-stage')) return;
  if (!st.cov) {
    if (!fpCovInit()) return;
    fpBuildCovers2d();
    if (fp3dActive()) window.fp3d.buildCovers();
    fpApplyScene();
    fpCovRepaint();
  }
  if (st.tlKind === 'past') return;
  const sig = fpCovSig();
  if (sig === st.cov.sig) return;
  st.cov.sig = sig;
  fpCovRepaint();
}
window.addEventListener('mpc-hass', fpOnHass);
const fpP = (p) => `${+p[0].toFixed(3)} ${+p[1].toFixed(3)}`;
// The 2D marks, in each floor's raised layer (they lift with the wall tops): a bar outside each
// mapped window or garage door whose filled part is the covered part, a jalousie's slats as ticks
// (their top view: long when open, short when closed), the pergola as a dashed outline with its louvres,
// screens and light. Text (a door's %, a "?" for unknown) floats in screen space like the room labels.
function fpBuildCovers2d() {
  const st = fpState, c = st.cov, e = st.els;
  if (!c || !e || e.cov) return;
  const top = $('#fp-svg .fp-toplabels');
  const txtG = fpSvgEl('g', { class: 'fp-cov-labels' }, top);
  const items = [];
  const label = (it, at) => {
    it.at = at;
    it.lab = fpSvgEl('g', { class: 'fp-cov-lab' }, txtG);
    it.txt = fpSvgEl('text', { class: 'fp-cov-txt', 'text-anchor': 'middle', y: 3 }, it.lab);
  };
  for (const en of c.map.openings) {
    const i = e.layers.findIndex((L) => L.f.id === en.floor);
    if (i < 0) continue;
    const o = en.o, k = o.depth_m / 2 + 0.16;
    const A = [o.a[0] + o.n[0] * k, o.a[1] + o.n[1] * k], B = [o.b[0] + o.n[0] * k, o.b[1] + o.n[1] * k];
    const g = fpSvgEl('g', { class: `fp-cov is-${en.kind}` }, e.layers[i].raised);
    const title = fpSvgEl('title', {}, g);
    fpSvgEl('path', { class: 'fp-cov-track', d: `M${fpP(A)}L${fpP(B)}` }, g);
    const it = { entity: en.entity, kind: en.kind, layer: i, g, title, A, B, n: o.n, fill: fpSvgEl('path', { class: 'fp-cov-fill' }, g) };
    if (en.kind === 'blind') it.ticks = fpSvgEl('path', { class: 'fp-cov-ticks' }, g);
    // the words go just inside the room (outside, an edge opening's text would leave the plan's bounds)
    label(it, [(o.a[0] + o.b[0]) / 2 - o.n[0] * 0.6, (o.a[1] + o.b[1]) / 2 - o.n[1] * 0.6]);
    items.push(it);
  }
  const p = c.map.pergola;
  let perg = null;
  if (p) {
    const gi = Math.max(0, e.layers.findIndex((L) => L.f.id === 'ground'));
    const g = fpSvgEl('g', { class: 'fp-perg' }, e.layers[gi].raised);
    perg = { g, title: fpSvgEl('title', {}, g) };
    fpSvgEl('path', { class: 'fp-perg-line', d: fpPoly(p.polygon) }, g);
    const F = fpPergolaFrame(p.polygon);
    let blades = '';
    for (let t = F.v0 + 0.35; t < F.v1 - 0.2; t += 0.4) {
      const at = (s) => [F.o[0] + F.u[0] * s + F.v[0] * t, F.o[1] + F.u[1] * s + F.v[1] * t];
      blades += `M${fpP(at(F.u0 + 0.12))}L${fpP(at(F.u1 - 0.12))}`;
    }
    perg.blades = fpSvgEl('path', { class: 'fp-perg-blades', d: blades }, g);
    for (const s of p.screens) {
      const ed = p.edges[s.edge];
      const sg = fpSvgEl('g', { class: 'fp-cov is-pergola_screen' }, g);
      const title = fpSvgEl('title', {}, sg);
      const A = [ed.a[0] + ed.n[0] * 0.1, ed.a[1] + ed.n[1] * 0.1], B = [ed.b[0] + ed.n[0] * 0.1, ed.b[1] + ed.n[1] * 0.1];
      fpSvgEl('path', { class: 'fp-cov-track', d: `M${fpP(A)}L${fpP(B)}` }, sg);
      items.push({ entity: s.entity, kind: 'pergola_screen', layer: gi, g: sg, title, A, B, n: ed.n, fill: fpSvgEl('path', { class: 'fp-cov-fill' }, sg) });
    }
    perg.light = fpSvgEl('circle', { class: 'fp-perg-light', cx: F.c[0], cy: F.c[1], r: 0.24 }, g);
    perg.it = { layer: gi };
    label(perg.it, [F.c[0], F.c[1] + 0.001]);
  }
  e.cov = { items, perg, txtG };
  const shade = document.createElement('div');
  shade.className = 'fp-shade';
  shade.id = 'fp-shade';
  shade.setAttribute('aria-label', 'Shading and garage doors, from Home Assistant');
  $('#fp-readout').after(shade);
}
// Repaint the 2D marks and the strip from fpState.cov.view.
function fpPaintCovers() {
  const c = fpState.cov, e = fpState.els;
  if (!c || !c.view || !e || !e.cov) return;
  for (const it of e.cov.items) {
    const v = c.view[it.entity];
    if (!v) continue;
    const g = v.g, len = Math.hypot(it.B[0] - it.A[0], it.B[1] - it.A[1]) || 1;
    const u = [(it.B[0] - it.A[0]) / len, (it.B[1] - it.A[1]) / len];
    const at = (t, k) => [it.A[0] + u[0] * t + it.n[0] * k, it.A[1] + u[1] * t + it.n[1] * k];
    it.g.classList.toggle('is-unknown', !g.known);
    it.fill.setAttribute('d', g.known && g.drop > 0.005 ? `M${fpP(at(0, 0))}L${fpP(at(len * g.drop, 0))}` : '');
    if (it.ticks) {
      let d = '';
      const tl = 0.05 + 0.25 * Math.cos(g.slatDeg * Math.PI / 180);
      if (g.known) for (let t = 0.1; t <= len * g.drop - 0.04; t += 0.2) d += `M${fpP(at(t, 0))}L${fpP(at(t, tl))}`;
      it.ticks.setAttribute('d', d);
    }
    it.title.textContent = `${v.name}: ${v.text}`;
    if (it.txt) {
      const s = v.st;
      const t = !g.known ? '?' : it.kind === 'garage' ? (s.pos >= 99.5 ? 'open' : s.pos <= 0.5 ? 'closed' : `${Math.round(s.pos)} %`) : '';
      it.txt.textContent = t;
      it.lab.style.display = t ? '' : 'none';
      it.lab.classList.toggle('is-unknown', !g.known);
    }
  }
  const p = c.map.pergola, P = e.cov.perg;
  if (p && P) {
    const roof = p.roof ? c.view[p.roof] : null, light = p.light ? c.view[p.light] : null;
    // louvres seen from above: thin lines when open to the sky, near-solid when flat and closed
    P.blades.style.strokeWidth = roof ? (0.7 + 2.6 * Math.sin(roof.g.bladeDeg * Math.PI / 180)).toFixed(2) : '';
    P.blades.classList.toggle('is-unknown', !!roof && !roof.g.known);
    P.blades.style.display = roof ? '' : 'none';
    P.light.style.display = light ? '' : 'none';
    P.light.classList.toggle('is-on', !!light && light.g.on);
    P.light.classList.toggle('is-unknown', !!light && !light.g.known);
    P.title.textContent = [roof, light].filter(Boolean).map((v) => `${v.name}: ${v.text}`).join('\n');
    const unk = [roof, light].some((v) => v && !v.g.known);
    P.it.txt.textContent = unk ? '?' : '';
    P.it.lab.style.display = unk ? '' : 'none';
    P.it.lab.classList.toggle('is-unknown', unk);
  }
  const box = $('#fp-shade');
  if (box) {
    const note = c.cs.note && FP_COVER_NOTES[c.cs.note];
    const html = fpShadeItems(c.map, c.cs, c.names).map(([k, v]) => `<span class="fp-ro${/unknown|unavailable/.test(v) ? ' is-unknown' : ''}"><span>${esc(k)}</span><b>${esc(v)}</b></span>`).join('')
      + (note ? `<span class="fp-shade-note">${esc(note)}</span>` : '');
    if (box.fpHtml !== html) { box.fpHtml = html; box.innerHTML = html; }
  }
}
// The cover rows of a room's card, or null: its windows' blinds, the garage's doors.
const FP_COVER_CARD_NOTES = { now: 'as now, not planned', loading: 'loading history…', nohistory: 'no history', history: 'recorded' };
function fpCovCard(zone) {
  const c = fpState.cov;
  if (!c || !c.view) return null;
  const ents = c.map.openings.filter((e) => e.cover.zone === zone && c.view[e.entity]);
  if (!ents.length) return null;
  return {
    title: ents.every((e) => e.kind === 'garage') ? 'Doors' : 'Blinds',
    note: (c.cs.note && FP_COVER_CARD_NOTES[c.cs.note]) || '',
    rows: ents.map((e) => ({ name: c.names[e.entity] || e.entity, text: c.view[e.entity].text, unknown: !c.view[e.entity].st.known, unsure: e.ambiguous || e.match !== 'exact' })),
  };
}

// ---- stacked <-> flat ----
function fpSetMode(mode, animate, persist) {
  const st = fpState;
  st.mode = mode;
  if (persist) { try { localStorage.setItem(FP_MODE_KEY, mode); } catch (e) { /* storage may be blocked */ } }
  document.querySelectorAll('.fp-seg button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.mode === mode)));
  const card = $('#fp-card');
  if (card) card.dataset.mode = mode;
  if (mode === '3d') { fpEnter3d(); return; }
  fpExit3d();
  const to = mode === 'stacked' ? 1 : 0;
  cancelAnimationFrame(st.anim);
  st.animating = false;
  const settle = () => { st.animating = false; if (card) card.classList.remove('is-morphing'); fpRenderLabels(); };
  if (!animate || fpReduced() || !st.els) { st.s = to; fpApplyScene(); settle(); return; }
  st.animating = true;
  if (card) card.classList.add('is-morphing');
  const from = st.s, t0 = performance.now(), dur = 520;
  const ease = (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);
  const step = (now) => {
    const x = Math.min(1, (now - t0) / dur);
    st.s = from + (to - from) * ease(x);
    fpApplyScene();
    if (x < 1) st.anim = requestAnimationFrame(step); else settle();
  };
  st.anim = requestAnimationFrame(step);
}
function fpResize() {
  const st = fpState;
  if (!st.els || !st.geom) return;
  const stage = $('#fp-stage');
  if (!stage) return;
  const narrow = stage.clientWidth < 700;
  if (!st.layout || narrow !== st.narrow) { st.narrow = narrow; st.layout = fpLayout(st.geom, narrow); }
  $('#fp-card').classList.toggle('is-narrow', narrow);
  const tb = document.querySelector('.topbar');
  if (tb) document.documentElement.style.setProperty('--topbar-h', `${tb.offsetHeight}px`);
  // nothing saved: follow the screen (rotate a phone, resize a window)
  if (!st.userMode && !st.animating && fpDefaultMode() !== st.mode) { fpSetMode(fpDefaultMode(), true, false); return; }
  fpApplyScene();
  fpRenderLabels();
}

// ---- detail card: floating beside the pointer on desktop, a bottom sheet on touch / narrow ----
function fpRenderCard(zone) {
  const r = fpState.byZone[zone];
  const pop = $('#fp-pop');
  if (!pop) return;
  const f = fpState.geom.floors.find((x) => x.rooms.some((q) => q.zone === zone));
  const hasFire = !!f && (f.features || []).some((x) => x.kind === 'fireplace' && x.zone === zone);
  const at = fpState.tlKind && fpState.tlKind !== 'now' ? `at ${fpTimeLabel(fpState.tlT, fpState.tlKind).replace(' · ', ' (')})` : null;
  const covers = fpCovCard(zone);
  const body = r ? fpCardHtml(r, { floorName: f ? f.name : '', floorHeight: f ? f.height_m : 0, rate: fpState.rate, theme: fpTheme(), nowMs: Date.now(), fire: hasFire && !at ? fpFireLikely(r) : null, at, covers })
    : `<div class="fpk-head"><div class="fpk-name">${esc(zone)}</div></div><div class="fpk-cap">No data for this room yet.</div>`;
  // the close button and handle are built once, so a repaint never takes focus off them
  let nb = pop.querySelector('.fp-pop-body');
  if (!nb) {
    pop.innerHTML = '<button type="button" class="fp-close" aria-label="Close room details">×</button><div class="fp-handle"></div><div class="fp-pop-body"></div>';
    nb = pop.querySelector('.fp-pop-body');
  }
  const scroll = nb.scrollTop;
  nb.innerHTML = body;
  nb.scrollTop = scroll;
}
// The chip (attention row or attic reading) for a zone, if one is shown.
const fpChipFor = (zone) => document.querySelector(`#fp-attic .fp-chip[data-zone="${zone}"]`) || document.querySelector(`.fp-chip[data-zone="${zone}"]`);
// Client rect of what the card should sit beside: the room's projected polygon, else its chip.
function fpAnchorRect(zone) {
  const e = fpState.els, in3d = fpState.mode === '3d' && fp3dActive();
  const r3 = in3d ? window.fp3d.rect(zone) : null;
  if (r3) return r3;
  const el = (in3d ? null : e.hit[zone]) || fpChipFor(zone) || (in3d ? $('#fp-stage') : null);
  const r = el ? el.getBoundingClientRect() : { left: innerWidth / 2, top: innerHeight / 2, right: innerWidth / 2, bottom: innerHeight / 2 };
  return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
}
// `rect` is kept so a repaint that changes the card's height re-places it beside the same thing.
function fpPlaceFloat(rect) {
  const pop = $('#fp-pop');
  fpState.cardRect = rect;
  const c = fpPlaceCard(rect, pop.offsetWidth, pop.offsetHeight, innerWidth, innerHeight, 14, 10);
  pop.style.left = `${c.left}px`; pop.style.top = `${c.top}px`;
}
function fpOpenCard(zone, opts) {
  const st = fpState;
  const pop = $('#fp-pop'), scrim = $('#fp-scrim');
  if (!pop) return;
  const sheet = fpIsSheet();
  st.shown = zone;
  st.openedAt = Date.now();
  st.from = opts.from || null;
  fpRenderCard(zone);
  pop.hidden = false;
  pop.classList.toggle('is-sheet', sheet);
  pop.classList.toggle('is-float', !sheet);
  pop.classList.toggle('is-pinned', !!opts.pin);
  if (sheet) {
    pop.setAttribute('aria-modal', 'true');
    scrim.hidden = false;
    pop.style.left = ''; pop.style.top = '';
    // style the closed sheet once (it was display:none), or it appears without sliding in
    if (!fpReduced()) void pop.offsetHeight;
    pop.classList.add('is-open'); scrim.classList.add('is-open');
    pop.querySelector('.fp-close').focus({ preventScroll: true });
  } else {
    pop.removeAttribute('aria-modal');
    pop.classList.add('is-open');
    scrim.hidden = true;
    fpPlaceFloat(opts.rect || fpAnchorRect(zone));
  }
}
function fpCloseCard(restoreFocus) {
  const st = fpState;
  const pop = $('#fp-pop'), scrim = $('#fp-scrim');
  if (!pop) return;
  const was = st.pinned;
  st.pinned = null; st.shown = null;
  fpSetHot(null);
  pop.classList.remove('is-open', 'is-pinned'); scrim.classList.remove('is-open');
  const done = () => { if (!pop.classList.contains('is-open')) { pop.hidden = true; scrim.hidden = true; } };
  if (fpReduced() || !pop.classList.contains('is-sheet')) done(); else setTimeout(done, 240);
  if (restoreFocus && was && st.els) {
    // an Indoor comfort row or a chip may have been rebuilt since: return to its replacement
    const gone = st.from && !st.from.isConnected ? st.from : null;
    const again = gone && gone.classList.contains('cz-row') ? document.querySelector(`.cz-row[data-zone="${was}"]`)
      : gone && gone.classList.contains('fp-chip') ? fpChipFor(was) : null;
    const target = st.from && st.from.isConnected ? st.from : again || (st.els.label[was] ? st.els.label[was].room : fpChipFor(was));
    st.restoring = true;
    if (target) target.focus({ preventScroll: true });
    st.restoring = false;
  }
}
function fpSetHot(zone) {
  const st = fpState, e = st.els;
  if (!e || st.hot === zone) return;
  if (st.hot) fpToggleHot(st.hot, false);
  st.hot = zone;
  if (zone) fpToggleHot(zone, true);
}
function fpToggleHot(zone, on) {
  const e = fpState.els;
  const fl = e.fill[zone];
  if (fl) fl.g.classList.toggle('hot', on);
  document.querySelectorAll(`#fp-card .fp-chip[data-zone="${zone}"], .cz-row[data-zone="${zone}"]`).forEach((c) => c.classList.toggle('hot', on));
  if (e.label[zone]) e.label[zone].room.classList.toggle('hot', on);
  if (fp3dActive()) window.fp3d.setHot(zone, on);
  // raise the hovered room's label group to the top of its layer so its label is never covered
  // (never re-parent the focused node: moving it in the DOM drops keyboard focus)
  if (on && e.label[zone] && document.activeElement !== e.label[zone].room) { const p = e.label[zone].room.parentNode; p.appendChild(e.label[zone].room); }
}
function fpZoneFrom(target) {
  const el = target && target.closest ? target.closest('[data-zone]') : null;
  return el ? el.getAttribute('data-zone') : null;
}
function fpWire() {
  const st = fpState;
  const svg = $('#fp-svg'), card = $('#fp-card');
  svg.addEventListener('pointermove', (ev) => {
    if (ev.pointerType === 'touch' || st.pinned || fpIsSheet()) return;
    const zone = fpZoneFrom(ev.target);
    if (zone !== st.hot) {
      fpSetHot(zone);
      if (zone) fpOpenCard(zone, { pin: false }); else fpCloseSoft();
    }
  });
  svg.addEventListener('pointerleave', () => { if (!st.pinned && !fpIsSheet()) { fpSetHot(null); fpCloseSoft(); } });
  svg.addEventListener('click', (ev) => {
    const zone = fpZoneFrom(ev.target);
    if (!zone) { if (st.pinned) fpCloseCard(false); return; }
    if (fpIsSheet()) { st.pinned = zone; fpSetHot(zone); fpOpenCard(zone, { pin: true }); return; }
    if (st.pinned === zone) { fpCloseCard(false); return; }
    st.pinned = zone; fpSetHot(zone); fpOpenCard(zone, { pin: true });
  });
  svg.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Enter' && ev.key !== ' ') return;
    const zone = fpZoneFrom(ev.target);
    if (!zone) return;
    ev.preventDefault();
    st.pinned = zone; fpSetHot(zone);
    fpOpenCard(zone, { pin: true });
  });
  svg.addEventListener('focusin', (ev) => {
    const zone = fpZoneFrom(ev.target);
    if (!zone || st.pinned || st.restoring || fpIsSheet()) return;
    fpSetHot(zone);
    fpKeepVisible(st.els.label[zone].room);
    fpOpenCard(zone, { pin: false });
  });
  svg.addEventListener('focusout', () => { if (!st.pinned && !fpIsSheet()) { fpSetHot(null); fpCloseSoft(); } });
  card.addEventListener('click', (ev) => {
    const chip = ev.target.closest && ev.target.closest('.fp-chip');
    if (chip) {
      const rect = chip.getBoundingClientRect();
      st.pinned = chip.dataset.zone; fpSetHot(st.pinned);
      fpOpenCard(st.pinned, { rect, pin: true, from: chip });
      return;
    }
    if (ev.target.closest && ev.target.closest('.fp-close')) fpCloseCard(true);
    if (ev.target.id === 'fp-scrim') fpCloseCard(true);
    const seg = ev.target.closest && ev.target.closest('.fp-seg button');
    if (seg) { st.userMode = seg.dataset.mode; fpSetMode(seg.dataset.mode, true, true); }
  });
  document.addEventListener('keydown', fpOnKey);
  window.addEventListener('scroll', fpOnScroll, { passive: true });
  $('#fp-slider').addEventListener('input', (ev) => { fpTimeStop(); fpTimeSet(+ev.target.value); });
  $('#fp-nowbtn').addEventListener('click', () => { fpTimeStop(); fpTimeSet(0); });
  $('#fp-play').addEventListener('click', fpTimePlay);
  if (window.ResizeObserver) { st.ro = new ResizeObserver(() => fpResize()); st.ro.observe($('#fp-stage')); }
  // the heating glow breathes only while the plan can be seen
  if (window.IntersectionObserver) {
    st.io = new IntersectionObserver((es) => { st.onScreen = es[es.length - 1].isIntersecting; fpOnVisibility(); });
    st.io.observe($('#fp-stage'));
  }
  document.addEventListener('visibilitychange', fpOnVisibility);
}
function fpOnVisibility() {
  const card = $('#fp-card');
  if (card) card.classList.toggle('is-paused', document.hidden || !fpState.onScreen);
}
// After a keyboard focus the browser scrolls the room into view: nudge it so the card title and the
// toggle are not left under the sticky topbar.
function fpKeepVisible(el) {
  const tb = document.querySelector('.topbar'), card = $('#fp-card');
  if (!tb || !card) return;
  requestAnimationFrame(() => {
    const r = el.getBoundingClientRect();
    const d = fpScrollDelta(r.top, r.bottom, card.getBoundingClientRect().top, tb.offsetHeight, innerHeight);
    if (d) window.scrollBy(0, d);
    if (fpState.shown) fpPlaceFloat(fpAnchorRect(fpState.shown));
  });
}
function fpCloseSoft() {
  const pop = $('#fp-pop');
  if (!pop || fpState.pinned) return;
  pop.classList.remove('is-open');
  fpState.shown = null;
  setTimeout(() => { if (!pop.classList.contains('is-open')) pop.hidden = true; }, fpReduced() ? 0 : 120);
}
function fpOnKey(ev) {
  const pop = document.getElementById('fp-pop');
  if (!fpState.shown || !pop) return;
  if (ev.key === 'Escape') { fpCloseCard(true); return; }
  // the sheet is modal: Tab cycles through its own controls
  if (ev.key !== 'Tab' || !pop.classList.contains('is-sheet')) return;
  const f = [...pop.querySelectorAll('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')]
    .filter((e) => !e.disabled && e.getClientRects().length);
  if (!f.length) return;
  const a = document.activeElement, first = f[0], last = f[f.length - 1];
  const wrap = !pop.contains(a) ? (ev.shiftKey ? last : first) : ev.shiftKey && a === first ? last : !ev.shiftKey && a === last ? first : null;
  if (wrap) { ev.preventDefault(); wrap.focus({ preventScroll: true }); }
}
function fpOnScroll() {
  // focusing a room scrolls it into view: don't treat that scroll as the user moving on
  if (!fpState.pinned && fpState.shown && !fpIsSheet() && Date.now() - fpState.openedAt > 700) { fpSetHot(null); fpCloseSoft(); }
}

// ---- entry points used by the Home screen ----
function fpMount() {
  const st = fpState;
  cancelAnimationFrame(st.anim);
  st.animating = false;
  if (st.ro) st.ro.disconnect();
  if (st.io) st.io.disconnect();
  document.removeEventListener('keydown', fpOnKey);
  window.removeEventListener('scroll', fpOnScroll);
  document.removeEventListener('visibilitychange', fpOnVisibility);
  fpExit3d();
  st.userMode = fpSavedChoice();
  fpTimeStop();
  if (st.cov) clearTimeout(st.cov.histT);
  Object.assign(st, { els: null, hot: null, pinned: null, shown: null, layout: null, mode: st.userMode || fpDefaultMode(), tlOffset: 0, tlShown: 0, tlKind: null, onScreen: true, cov: null });
  st.s = st.mode === 'stacked' ? 1 : 0;
  if (!st.geom) fpLoadGeom(); // alongside the first poll, not after it
}
async function fpUpdate(store) {
  const st = fpState;
  const stage = $('#fp-stage');
  if (!stage) return;
  const res = store[FP_ROOMS_EP];
  const data = res && res.data && Array.isArray(res.data.rooms) ? res.data : null;
  if (data) {
    st.roomsLive = data.rooms;
    st.sensorError = data.sensor_error || null;
    st.roomsPlanAt = data.plan_at || null;
    st.site = data.site || null;
    const plan = store['/api/plan/latest']?.data;
    st.rate = plan ? czkRate(plan) : null;
  }
  // what the time bar reads besides the rooms (the views keep the selected offset across refreshes)
  st.tlSrc = {
    live: store['/api/live']?.data || null,
    timeline: store['/api/plan/latest']?.data?.timeline || [],
    ledger: store['/api/ledger?days=2']?.data?.rows || [],
    history: store[FP_HISTORY_EP]?.data || null,
    series: Object.fromEntries(arrData(store, FP_SERIES_EP).map((x) => [x.zone, x.series])),
  };
  fpLoadFullPlan();
  fpTimeApply();
  if (!st.geom) st.geom = await fpLoadGeom();
  if (!$('#fp-stage')) return; // navigated away while the geometry loaded
  if (!st.geom) { $('#fp-empty').textContent = 'The floor plan geometry could not be loaded.'; return; }
  if (!st.els) {
    fpBuild();
    $('#fp-empty').hidden = true;
    st.narrow = stage.clientWidth < 700;
    st.layout = fpLayout(st.geom, st.narrow);
    $('#fp-card').classList.toggle('is-narrow', st.narrow);
    fpWire();
    document.querySelectorAll('.fp-seg button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.mode === st.mode)));
    fpApplyScene();
    // once per page load the model assembles itself: the upper floor lifts away from the lower one
    if (st.mode === 'stacked' && !fpState.introDone && !fpReduced()) { st.s = 0; fpApplyScene(); fpPaint(); fpSetMode('stacked', true); }
    fpState.introDone = true;
    if (st.mode === '3d') fpEnter3d();
  }
  $('#fp-time').hidden = false;
  fpPaint();
  if (!st.animating) fpApplyScene();
  if (fp3dActive()) window.fp3d.update();
}

// The selected instant and the rooms as they stood (or are planned) then; 0 offset = the live rooms.
// tlOffset is the step the user picked and survives a poll that shrinks the bar (a failed plan fetch);
// tlShown is that step clamped into the bar, what every view shows.
function fpTimeApply() {
  const st = fpState, now = Date.now();
  const rg = fpTimelineRange(now, st.tlSrc.timeline);
  st.tlShown = Math.max(rg.min, Math.min(rg.max, st.tlOffset));
  const kind = fpTimeKind(st.tlShown), T = now + st.tlShown * FP_STEP_MS;
  st.tlKind = kind;
  st.tlT = T;
  const src = { ...st.tlSrc, now };
  // the live rooms' 12 h plan where it reaches T, the last fetched whole plan beyond it
  const use = kind === 'plan' && st.fullPlan ? fpFullPlanUse(st.fullPlanAt, st.roomsPlanAt) : null;
  let older = false;
  st.rooms = (st.roomsLive || []).map((r) => {
    const at = fpRoomAt(r, T, kind, src);
    if (!use || !st.fullPlan[r.zone] || at.measured || at.heating) return at;
    older = older || use === 'older';
    return fpRoomAt({ ...r, plan: st.fullPlan[r.zone] }, T, kind, src);
  });
  st.byZone = Object.fromEntries(st.rooms.map((r) => [r.zone, r]));
  const card = $('#fp-card');
  if (!card) return;
  card.classList.toggle('tl-plan', kind === 'plan');
  card.classList.toggle('tl-past', kind === 'past');
  $('#fp-key-heat').textContent = kind === 'past' ? 'Heating (measured)' : 'Heating per plan';
  // the summary is announced at now only, not on every step of a scrub or a Play
  $('#fp-summary').setAttribute('aria-live', kind === 'now' ? 'polite' : 'off');
  const sl = $('#fp-slider');
  sl.min = String(rg.min); sl.max = String(rg.max);
  if (+sl.value !== st.tlShown) sl.value = String(st.tlShown);
  const label = fpTimeLabel(T, kind);
  sl.setAttribute('aria-valuetext', older ? `${label}, from an earlier plan` : label);
  const when = $('#fp-when');
  when.textContent = label;
  when.classList.toggle('is-older', older);
  when.title = older ? `From the plan of ${fmt.hm(st.fullPlanAt)}; the newest is not loaded yet` : '';
  $('#fp-nowbtn').setAttribute('aria-pressed', String(kind === 'now'));
  $('#fp-track').style.setProperty('--now', `${((0 - rg.min) / Math.max(1, rg.max - rg.min) * 100).toFixed(2)}%`);
  $('#fp-play').disabled = fpReduced();
  const items = fpReadoutItems(fpReadoutAt(T, kind, src), st.rate);
  $('#fp-readout').innerHTML = items.map(([k, v]) => `<span class="fp-ro"><span>${esc(k)}</span><b>${esc(v)}</b></span>`).join('');
}
// Move the time bar (a step offset from now) and repaint every view in place.
function fpTimeSet(offset) {
  const st = fpState;
  if (offset === st.tlOffset && st.tlKind) return;
  st.tlOffset = offset;
  fpLoadFullPlan();
  fpTimeApply();
  fpPaint();
  if (fp3dActive()) window.fp3d.update();
}
// The rooms' whole plan, for the bar beyond the 12 h that /api/rooms carries by default: fetched only
// while the bar is in the plan or playing and the live plan differs, at most once a minute (the loop
// replans every minute). The server caches each horizon on its own, so the reply can still carry an
// earlier plan; fpFullPlanUse decides whether it is shown.
function fpLoadFullPlan() {
  const st = fpState, f = st.fullTry;
  if (f.busy || !(st.tlOffset > 0 || st.tlPlay)) return;
  if (!st.roomsPlanAt || st.fullPlanAt === st.roomsPlanAt || Date.now() < f.retryAt) return;
  f.busy = true;
  f.retryAt = Date.now() + FP_FULL_REFETCH_MS;
  api(FP_ROOMS_FULL_EP).then((res) => {
    f.busy = false;
    const d = res.ok && res.data && Array.isArray(res.data.rooms) ? res.data : null;
    if (d) {
      st.fullPlan = Object.fromEntries(d.rooms.map((r) => [r.zone, r.plan]));
      st.fullPlanAt = d.plan_at || null;
    }
    if (d && st.els && $('#fp-stage') && st.tlKind === 'plan') { fpTimeApply(); fpPaint(); if (fp3dActive()) window.fp3d.update(); }
  });
}
function fpTimeStop() {
  const st = fpState;
  if (!st.tlPlay) return;
  cancelAnimationFrame(st.tlPlay.raf);
  st.tlPlay = null;
  const t = $('#fp-time');
  if (t) { t.classList.remove('is-playing'); $('#fp-play').setAttribute('aria-label', 'Play the timeline'); }
}
// Play: from the bar's position (or its start, from the end) to the end of the plan in about 15 s.
function fpTimePlay() {
  const st = fpState;
  if (st.tlPlay) { fpTimeStop(); return; }
  if (fpReduced()) return;
  const sl = $('#fp-slider'), min = +sl.min, max = +sl.max;
  const from = st.tlShown >= max ? min : st.tlShown;
  const perMs = (max - min) / 15000, t0 = performance.now();
  st.tlPlay = { raf: 0 };
  $('#fp-time').classList.add('is-playing');
  $('#fp-play').setAttribute('aria-label', 'Pause');
  const step = (now) => {
    if (!st.tlPlay || !$('#fp-time')) return;
    const v = Math.min(max, Math.round(from + (now - t0) * perMs));
    fpTimeSet(v);
    if (v >= max) { fpTimeStop(); return; }
    st.tlPlay.raf = requestAnimationFrame(step);
  };
  st.tlPlay.raf = requestAnimationFrame(step);
}
// The instant the sun is drawn for: now (or ?fpSunTime=), moved by the time bar.
function fpSunMs() {
  const c = fpSunClock();
  return fpState.tlShown ? { ms: c.ms + fpState.tlShown * FP_STEP_MS, pinned: true } : c;
}

// ---- the 3D view (fp3d.js + a vendored three.js, fetched only when someone picks 3D) ----
const fp3dActive = () => !!(window.fp3d && window.fp3d.active);
let fp3dScripts = null;
function fp3dLoad() {
  const add = (src) => new Promise((ok, fail) => {
    const s = document.createElement('script');
    s.src = src;
    s.onload = ok;
    s.onerror = () => fail(new Error(`${src} did not load`));
    document.head.appendChild(s);
  });
  if (!fp3dScripts) {
    fp3dScripts = (window.THREE ? Promise.resolve() : add(mpcAsset('three.min.js')))
      .then(() => (window.fp3d ? null : add(mpcAsset('fp3d.js'))))
      .catch((e) => { fp3dScripts = null; throw e; });
  }
  return fp3dScripts;
}
function fpNote(text) {
  const n = $('#fp-note');
  if (!n) return;
  n.textContent = text;
  n.hidden = !text;
  clearTimeout(fpState.noteT);
  if (text) fpState.noteT = setTimeout(() => { n.hidden = true; }, 9000);
}
// 3D could not start (no WebGL 2, a lost context, the scripts did not load): back to the screen's 2D view
function fpFallback2d(why) {
  fpSetMode(fpDefaultMode(), true, false);
  fpNote(why);
}
// Probed once per page load, before three.js (600 KB) is fetched; the throwaway context is released.
let fpGl2 = null;
function fpHasWebGl2() {
  if (fpGl2 == null) {
    const gl = window.WebGL2RenderingContext ? document.createElement('canvas').getContext('webgl2') : null;
    fpGl2 = !!gl;
    const lose = gl && gl.getExtension('WEBGL_lose_context');
    if (lose) lose.loseContext();
  }
  return fpGl2;
}
const FP_NO_WEBGL2 = 'This browser cannot draw the 3D model (it needs WebGL 2), so the 2D plan is shown instead.';
function fpEnter3d() {
  const st = fpState, stage = $('#fp-stage');
  if (!stage || !st.els || fp3dActive()) return; // before the geometry is in, fpUpdate enters once it is
  if (!fpHasWebGl2()) { fpFallback2d(FP_NO_WEBGL2); return; }
  fpCloseCard(false);
  const seq = st.enter3d = (st.enter3d || 0) + 1;
  $('#fp-card').classList.add('is-loading3d');
  fp3dLoad().then(() => {
    if (seq !== st.enter3d || st.mode !== '3d' || !$('#fp-stage') || fp3dActive()) return;
    try { window.fp3d.mount($('#fp-stage')); } catch (e) {
      console.warn('3D view unavailable:', e);
      fpFallback2d(/webgl/i.test(e && e.message) ? FP_NO_WEBGL2 : 'The 3D model could not be built, so the 2D plan is shown instead.');
    }
  }, () => {
    if (seq === st.enter3d && st.mode === '3d') fpFallback2d('The 3D model could not be loaded, so the 2D plan is shown instead.');
  }).finally(() => { if (seq === st.enter3d) fpLoaded3d(); });
}
function fpLoaded3d() {
  const c = $('#fp-card');
  if (c) c.classList.remove('is-loading3d');
}
function fpExit3d() {
  fpState.enter3d = (fpState.enter3d || 0) + 1;
  fpLoaded3d();
  if (fp3dActive()) { fpCloseCard(false); window.fp3d.dispose(); }
}

// ============================================================ INDOOR COMFORT (Home screen card)
// Every room as one compact row under the floor plan, grouped like the plan reads: the ground and first
// floor left to right as drawn, then "Other" for the rooms without a comfort band and the roof spaces.
// The status dot uses the plan's colour scale; hovering a row lights the room on the plan and back.
// Pure helpers first (unit-tested in dashboard_test.js), DOM after.

// Rows in groups. `zoneIds`: the zones to show; `geom`: floorplan.json (or null before it loads);
// `banded`: the set of zones that have a comfort band. Unknown zones fall back to their id.
function czGroups(zoneIds, geom, banded) {
  const where = {};
  const floorName = { ground: 'Ground floor', first: 'First floor' };
  for (const f of (geom && geom.floors) || []) {
    if (floorName[f.id] && f.name) floorName[f.id] = f.name;
    for (const r of f.rooms) where[r.zone] = { floor: f.id, name: r.name, x: r.label[0], y: r.label[1] };
  }
  const groups = ['ground', 'first', 'other'].map((id) => ({ id, name: floorName[id] || 'Other', rows: [] }));
  for (const z of zoneIds || []) {
    const w = where[z];
    const gid = w && banded.has(z) && floorName[w.floor] ? w.floor : 'other';
    groups.find((g) => g.id === gid).rows.push({ zone: z, name: w ? w.name : z.replace(/_/g, ' '), x: w ? w.x : Infinity, y: w ? w.y : 0 });
  }
  for (const g of groups) g.rows.sort((a, b) => a.x - b.x || a.y - b.y || (a.zone < b.zone ? -1 : a.zone > b.zone ? 1 : 0));
  return groups.filter((g) => g.rows.length);
}
// Temperature trend (K/h) over the last ~hour of the 30-min measured series, null below 0.1 K/h.
function czTrend(ser, t) {
  if (!ser || ser.length < 3 || t == null) return null;
  const [ta, va] = ser[ser.length - 3], [tb, vb] = ser[ser.length - 1];
  const dh = (new Date(tb) - new Date(ta)) / 3.6e6;
  const rate = dh > 0 ? (vb - va) / dh : 0;
  return Math.abs(rate) >= 0.1 ? rate : null;
}
// The row's secondary facts, terse: the model's coming extreme (relative to the band edge it nears,
// the overheat ceiling where one exists), the next heating window with the planned energy over the
// horizon, and the disturbance observer's unexplained flux. `hm` formats a time. Each fact carries a
// full-sentence `title` for hover and screen readers.
function czFacts(zone, zc, future, fs, dW, hm) {
  const facts = [];
  const endOfBlock = (b) => new Date(new Date(b.t).getTime() + (b.dt_minutes ?? 15) * 60000).toISOString();
  if (future.length && zc) {
    let mn = Infinity, mx = -Infinity, mnT, mxT;
    for (const b of future) {
      const v = b.temp_c?.[zone];
      if (v == null) continue;
      if (v < mn) { mn = v; mnT = endOfBlock(b); }
      if (v > mx) { mx = v; mxT = endOfBlock(b); }
    }
    if (isFinite(mn)) {
      const zb = bandNow(zc);
      const hiEdge = overheatCeiling(zc) ?? zb.hi;
      const coldRisk = mn - zb.lo, warmRisk = hiEdge - mx; // negative = leaves the band
      const low = coldRisk <= warmRisk;
      const [val, at, margin] = low ? [mn, mnT, coldRisk] : [mx, mxT, warmRisk];
      const cls = margin < -0.1 ? 'zwarn' : margin < 0.2 ? 'zclose' : '';
      facts.push({ kind: 'forecast', cls, text: `${low ? '↓' : '↑'} ${val.toFixed(1)}° ${hm(at)}`, title: `Model forecast ${low ? 'low' : 'high'}: ${val.toFixed(1)} °C at ${hm(at)}` });
    }
  }
  if (future.length) {
    const nowKw = fs.heat_kw?.[zone] || 0;
    let first = null, kwh = isRelayOn(nowKw) ? nowKw * 0.25 : 0;
    for (const b of future) { const kw = b.heat_kw?.[zone] || 0; if (isRelayOn(kw) && !first) first = b.t; kwh += kw * ((b.dt_minutes ?? 15) / 60); }
    if (isRelayOn(nowKw)) facts.push({ kind: 'heat', cls: 'zheat', text: `now ${kwh.toFixed(1)} kWh`, title: `Heating now; ${kwh.toFixed(1)} kWh planned over the horizon` });
    else if (first) facts.push({ kind: 'heat', cls: 'zheat', text: `${hm(first)} ${kwh.toFixed(1)} kWh`, title: `Heating from ${hm(first)}; ${kwh.toFixed(1)} kWh planned over the horizon` });
  }
  if (dW != null && Math.abs(dW) >= 150) {
    const w = `${dW > 0 ? '+' : '−'}${Math.round(Math.abs(dW))} W`;
    facts.push({ kind: 'alarm', cls: 'zwarn', text: `⚠ ${w}`, title: `${w} unexplained by the model (open window or an extra load?)` });
  }
  return facts;
}
// The status dot / gauge marker colour: the plan's fill pushed away from the card surface so a small
// mark reads (same hue, more chroma for the calm in-band tints): darker in the light theme, lighter
// in the dark one.
function czMarkColour(fill, theme) {
  const [L, a, b] = fpToLab(fill);
  const k = Math.hypot(a, b) < 0.06 ? 1.9 : 1.15;
  return fpFromLab([theme === 'light' ? Math.min(L, 0.74) : Math.max(L, 0.62), a * k, b * k]);
}
// Slim band gauge: the comfort band, the overheat allowance, the reading as a marker in the room's
// scale colour; the band edges printed under it. '' without a band or a reading.
function czGauge(t, zc, colour) {
  if (!zc || t == null) return '';
  const zb = bandNow(zc);
  if (zb.lo == null || zb.hi == null) return '';
  const boost = overheatCeiling(zc);
  const hiEdge = boost != null ? Math.max(zb.hi, boost) : zb.hi;
  const lo = Math.min(zb.lo, t) - 1, hi = Math.max(hiEdge, t) + 1;
  const pct = (v) => clamp((v - lo) / (hi - lo) * 100, 0, 100).toFixed(1);
  const hasBoost = boost != null && boost > zb.hi;
  const strip = hasBoost ? `<i class="cz-boost" style="left:${pct(zb.hi)}%;width:${(pct(boost) - pct(zb.hi)).toFixed(1)}%"></i>` : '';
  // the boost edge gets its own number only where it clears the ceiling's (else it lives in the title)
  const boostLbl = hasBoost && pct(boost) - pct(zb.hi) >= 22 ? `<span class="cz-bedge" style="left:${pct(boost)}%">${boost}</span>` : '';
  const edges = `<span style="left:${pct(zb.lo)}%">${zb.lo}</span><span style="left:${pct(zb.hi)}%">${zb.hi}</span>${boostLbl}`;
  const title = `Comfort band ${zb.lo}–${zb.hi} °C${hasBoost ? `, overheat allowance to ${boost} °C` : ''}; now ${t.toFixed(1)} °C`;
  return `<span class="cz-track" title="${title}"><i class="cz-band" style="left:${pct(zb.lo)}%;width:${(pct(zb.hi) - pct(zb.lo)).toFixed(1)}%"></i>${strip}<i class="cz-mark" style="left:${pct(t)}%;background:${colour}"></i></span><span class="cz-edges">${edges}</span>`;
}

const czMountHtml = () => `
  <section class="card span-full cz-card" id="cz-card" style="margin-top:18px" aria-labelledby="cz-title">
    <div class="card-head"><div class="card-title" id="cz-title"><span class="ico">🌡️</span> Indoor comfort</div><div class="card-sub" id="cz-sub"></div></div>
    <div class="cz-groups" id="cz-groups"></div>
  </section>`;

function czRender(store) {
  const box = $('#cz-groups');
  if (!box) return;
  const theme = fpTheme();
  const zones = arrData(store, '/api/zones');
  const sdata = store['/api/state']?.data || {};
  const smap = Object.fromEntries((sdata.zones || []).map((s) => [s.zone, s.temp_c]));
  const dmap = sdata.disturbance_w || {};
  const zmap = Object.fromEntries(zones.map((z) => [z.zone, z]));
  // the sparkline keeps to its 24 h (the series reaches 25 h back for the time bar)
  const since = Date.now() - 24 * 3600000;
  const sermap = Object.fromEntries(arrData(store, FP_SERIES_EP).map((s) => [s.zone, (s.series || []).filter(([t]) => Date.parse(t) >= since)]));
  const plan = store['/api/plan/latest']?.data;
  const tl = plan?.timeline || [];
  // "heating now" only from the block that covers now: an old plan's first block is not now
  const fs = tl[fpBlockIndex(tl.map((b) => Date.parse(b.t)), tl.map((b) => b.dt_minutes), Date.now())] || {};
  const future = tl.slice(nowBlock(tl) + 1);
  const ids = [...new Set([...zones.map((z) => z.zone), ...Object.keys(smap)])];
  const groups = czGroups(ids, fpState.geom, new Set(zones.map((z) => z.zone)));
  const row = (g) => {
    const z = g.zone, zc = zmap[z], t = smap[z];
    const st = fpRoomState({ measured: t != null ? { temp_c: t, age_s: 0 } : null, band: zc || null });
    const colour = czMarkColour(fpFill(st, theme), theme);
    const c = comfort(t, zc);
    const heating = isRelayOn(fs.heat_kw?.[z]);
    const trend = czTrend(sermap[z], t);
    const facts = czFacts(z, zc, future, fs, dmap[z], fmt.hm);
    const status = c.label && c.label !== 'comfortable' ? [{ cls: c.cls === 'red' ? 'zwarn' : c.cls === 'amber' ? 'zclose' : '', text: c.label, title: c.label }] : [];
    const all = status.concat(facts);
    const factHtml = all.map((f) => `<span class="${f.cls}"${f.kind === 'heat' ? ' data-heat' : ''} title="${esc(f.title)}">${f.kind === 'heat' ? '<i class="cz-flame" aria-hidden="true"></i>' : ''}${esc(f.text)}</span>`).join('');
    const spark = sparkline(sermap[z], 56, 22);
    const gauge = czGauge(t, zc, colour);
    const aria = [g.name, t != null ? `${t.toFixed(1)} °C (model estimate)` : 'no reading', c.label, heating ? 'heating now' : '', ...facts.map((f) => f.title)].filter(Boolean).join(', ');
    return `<button type="button" class="cz-row${heating ? ' is-heating' : ''}" data-zone="${esc(z)}" aria-label="${esc(aria)}">
      <span class="cz-id"><i class="cz-dot" style="background:${colour}"></i><span class="cz-name">${esc(g.name)}</span></span>
      <span class="cz-facts">${factHtml}</span>
      <span class="cz-spark">${spark}</span>
      <span class="cz-gauge">${gauge}</span>
      <span class="cz-t"><b>${t != null ? `${t.toFixed(1)}<span class="cz-deg">°</span>` : '—'}</b>${trend != null ? `<small title="trend over the last hour">${trend > 0 ? '↗' : '↘'} ${Math.abs(trend).toFixed(1)}°/h</small>` : ''}</span>
    </button>`;
  };
  fpSetHtml(box, groups.map((g) => {
    const banded = g.rows.filter((r) => zmap[r.zone]);
    const ok = banded.filter((r) => ['green', 'gold'].includes(comfort(smap[r.zone], zmap[r.zone]).cls)).length;
    return `<div class="cz-group"><div class="cz-floor"><span>${esc(g.name)}</span>${banded.length ? `<small>${ok}/${banded.length} comfortable</small>` : ''}</div>${g.rows.map(row).join('')}</div>`;
  }).join(''));
  $('#cz-sub').textContent = 'model estimate · 24 h trend';
  if (fpState.hot) fpToggleHot(fpState.hot, true);
}
function czWire() {
  const card = $('#cz-card');
  if (!card || card.dataset.wired) return;
  card.dataset.wired = '1';
  const zoneOf = (ev) => { const r = ev.target.closest && ev.target.closest('.cz-row'); return r ? r.dataset.zone : null; };
  card.addEventListener('pointerover', (ev) => { const z = zoneOf(ev); if (z && !fpState.pinned && ev.pointerType !== 'touch') fpSetHot(z); });
  card.addEventListener('pointerout', (ev) => { if (!fpState.pinned && zoneOf(ev) && !(ev.relatedTarget && ev.relatedTarget.closest && ev.relatedTarget.closest('.cz-row'))) fpSetHot(null); });
  card.addEventListener('focusin', (ev) => { const z = zoneOf(ev); if (z && !fpState.pinned) fpSetHot(z); });
  card.addEventListener('focusout', () => { if (!fpState.pinned) fpSetHot(null); });
  card.addEventListener('click', (ev) => {
    const r = ev.target.closest && ev.target.closest('.cz-row');
    if (!r || !fpState.els) return;
    fpState.pinned = r.dataset.zone; fpSetHot(fpState.pinned);
    fpOpenCard(fpState.pinned, { rect: r.getBoundingClientRect(), pin: true, from: r });
  });
}

// ============================================================ SCREENS
const screens = {};

// ---- HOME ----
screens.home = {
  mount() {
    fpMount();
    return `
    ${fpMountHtml()}
    ${czMountHtml()}
    <div class="grid cols-3" style="margin-top:18px">
      <section class="card span-2">
        <div class="card-head"><div class="card-title"><span class="ico">⚡</span> Live energy flow</div><div class="card-sub" id="live-age"></div></div>
        <div class="flow">
          <div class="flow-node" id="n-solar"><div class="flow-ico">☀️</div><div class="flow-name">Solar</div><div class="flow-val" id="v-solar">—</div></div>
          <div class="flow-node" id="n-house"><div class="flow-ico">🏠</div><div class="flow-name">House</div><div class="flow-val" id="v-house">—</div></div>
          <div class="flow-node" id="n-batt"><div class="flow-ico">🔋</div><div class="flow-name">Battery</div><div class="flow-val" id="v-batt">—</div></div>
          <div class="flow-node" id="n-grid"><div class="flow-ico">🔌</div><div class="flow-name">Grid</div><div class="flow-val" id="v-grid">—</div></div>
        </div>
      </section>
      <section class="card">
        <div class="card-head"><div class="card-title"><span class="ico">🔋</span> Battery</div></div>
        <div class="soc-ring">
          <svg width="120" height="120" viewBox="0 0 120 120">
            <circle class="track" cx="60" cy="60" r="52"></circle>
            <circle class="fill" id="soc-arc" cx="60" cy="60" r="52" stroke-dasharray="327" stroke-dashoffset="327"></circle>
          </svg>
          <div class="label"><div class="pct" id="soc-pct">—</div><div class="kwh" id="soc-kwh"></div></div>
        </div>
      </section>
    </div>

    <div class="grid cols-3" style="margin-top:18px">
      <section class="card"><div class="kpi"><div class="kpi-label">Electricity price now</div><div class="kpi-value" id="price-now">—</div><div class="kpi-sub"><span id="price-level" class="chip">—</span></div></div></section>
      <section class="card"><div class="kpi"><div class="kpi-label">Outside</div><div class="kpi-value" id="outside">—</div><div class="kpi-sub" id="comfort-sub">indoor comfort</div></div></section>
      <section class="card"><div class="kpi"><div class="kpi-label">Planned cost (36 h)</div><div class="kpi-value" id="cost-today">—</div><div class="kpi-sub" id="cost-sub"></div></div></section>
    </div>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🧠</span> What the system is doing &amp; why</div></div>
      <div class="insight" id="headline">…</div>
      <ul class="reasons" id="reasons" style="margin-top:12px"></ul>
    </section>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">📒</span> Decisions — planned vs done</div></div>
      <div id="ledger-card"><div class="insight">no scored blocks yet</div></div>
    </section>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">📈</span> The day — price, PV &amp; SoC (history → forecast)</div>
        <div class="legend" id="day-legend"></div></div>
      <div class="chart" id="home-chart"></div>
    </section>
`;
  },
  update(store) {
    fpUpdate(store).then(() => { czRender(store); czWire(); }).catch((e) => console.error('floor plan', e));
    const live = store['/api/live']?.data;
    const plan = store['/api/plan/latest']?.data;
    const zones = arrData(store, '/api/zones');
    const state = store['/api/state']?.data?.zones || [];

    // live flow
    if (live) {
      $('#live-age').textContent = fmt.ago(store['/api/live']?.age);
      $('#v-solar').textContent = live.solar_kw == null ? '—' : `${fmt.kw(live.solar_kw)} kW`;
      $('#n-solar').classList.toggle('active', (live.solar_kw || 0) > 0.05);
      $('#v-house').textContent = live.house_kw == null ? '—' : `${fmt.kw(live.house_kw)} kW`;
      $('#n-house').classList.toggle('active', (live.house_kw || 0) > 0.05);
      const bv = $('#v-batt'); bv.textContent = live.battery_kw == null ? '—' : `${fmt.signedkw(live.battery_kw)} kW`;
      bv.className = 'flow-val'; $('#n-batt').classList.toggle('active', Math.abs(live.battery_kw || 0) > 0.05);
      const gv = $('#v-grid'); gv.textContent = live.grid_kw == null ? '—' : `${fmt.signedkw(live.grid_kw)} kW`;
      gv.className = 'flow-val ' + ((live.grid_kw || 0) > 0.05 ? 'imp' : (live.grid_kw || 0) < -0.05 ? 'exp' : '');
      $('#n-grid').classList.toggle('active', Math.abs(live.grid_kw || 0) > 0.05);
      // soc ring
      const soc = live.soc_pct == null ? null : clamp(live.soc_pct, 0, 100);
      if (soc != null) {
        const C = 2 * Math.PI * 52;
        const arc = $('#soc-arc'); arc.setAttribute('stroke-dasharray', C.toFixed(0));
        arc.setAttribute('stroke-dashoffset', (C * (1 - soc / 100)).toFixed(1));
        arc.style.stroke = soc < 25 ? css('--red') : soc < 50 ? css('--amber') : css('--green');
        $('#soc-pct').textContent = fmt.pct(soc);
        $('#soc-kwh').textContent = live.soc_kwh != null ? `${fmt.kw(live.soc_kwh, 1)} kWh` : '';
      } else {
        // `soc_pct` is deliberately null when the Growatt feed is stale or out of range. Every other
        // live field clears to '—'; leaving the ring alone froze a dead reading on screen while the
        // age counter kept ticking — the opposite of what the freshness envelope is for.
        const C = 2 * Math.PI * 52;
        const arc = $('#soc-arc');
        arc.setAttribute('stroke-dasharray', C.toFixed(0));
        arc.setAttribute('stroke-dashoffset', C.toFixed(0));
        arc.style.stroke = css('--muted');
        $('#soc-pct').textContent = '—';
        $('#soc-kwh').textContent = '';
      }
      $('#outside').innerHTML = live.outside_temp_c != null ? `${fmt.temp(live.outside_temp_c)}<span class="unit">C</span>` : '—';
    } else {
      // The endpoint itself failed or timed out. Without this the whole panel — values, active-node
      // highlights, SoC ring — kept its last render while the age label stopped advancing: a
      // plausible, motionless energy flow that looks current. The per-field null paths above
      // already refuse to do that; the whole-endpoint failure needs the same honesty.
      $('#live-age').textContent = 'unavailable';
      for (const [v, nd] of [['#v-solar', '#n-solar'], ['#v-house', '#n-house'], ['#v-batt', '#n-batt'], ['#v-grid', '#n-grid']]) {
        $(v).textContent = '—'; $(v).className = 'flow-val'; $(nd).classList.remove('active');
      }
      const C = 2 * Math.PI * 52;
      const arc = $('#soc-arc');
      arc.setAttribute('stroke-dasharray', C.toFixed(0));
      arc.setAttribute('stroke-dashoffset', C.toFixed(0));
      arc.style.stroke = css('--muted');
      $('#soc-pct').textContent = '—';
      $('#soc-kwh').textContent = '';
      $('#outside').innerHTML = '—';
    }

    // price + cost
    if (plan) {
      const tl = plan.timeline || [];
      const i = nowBlock(tl), b = tl[i] || {};
      const st = priceStats(tl), rate = czkRate(plan);
      $('#price-now').innerHTML = b.import_price != null ? `${fmt.czk(b.import_price * rate, 2)}<span class="unit">/kWh</span>` : '—';
      const lvl = priceLevel(b.import_price, st);
      const pl = $('#price-level'); pl.className = lvl.cls; pl.textContent = lvl.label;
      $('#cost-today').innerHTML = `${fmt.czk(plan.total_cost_czk)}`;
      $('#cost-sub').textContent = `${fmt.eur(plan.total_cost_eur)} · ${fmt.kw(plan.pv_calibrated_kwh, 0)} kWh PV · grid ${fmt.kw(plan.grid_import_kwh, 0)}/${fmt.kw(plan.grid_export_kwh, 0)} kWh`;

      // insight
      const ins = insights(store);
      $('#headline').innerHTML = ins.headline;
      $('#reasons').innerHTML = ins.reasons.map((r) => `<li><span class="dot"></span><span>${esc(r)}</span></li>`).join('');

      // day chart — expanded so an hourly (far-horizon) block draws as four 15-min points, like a
      // uniform grid (item F).
      this.dayChart(expandTimeline(tl), rate, store);
    }

    // comfort counts for the Outside tile (the room rows themselves render in czRender, after the plan)
    const zmap = Object.fromEntries(zones.map((z) => [z.zone, z]));
    const smap = Object.fromEntries(state.map((s) => [s.zone, s.temp_c]));
    const heated = zones.length ? zones : Object.keys(smap).map((z) => ({ zone: z }));

    // Banking heat (gold) is a positive state — inside the zone's granted allowance, not a
    // comfort violation — so it counts as comfortable here.
    const okZones = heated.filter((z) => ['green', 'gold'].includes(comfort(smap[z.zone], zmap[z.zone]).cls)).length;
    $('#comfort-sub').textContent = `${okZones}/${heated.length} rooms comfortable`;

    renderLedgerCard(store);
  },
  dayChart(tl, rate, store) {
    const c = chart('home-chart'); if (!c) return;
    const pv = css('--yellow'), soc = css('--amber'), price = css('--blue'), house = css('--red');
    const base = '#f472b6'; // base-load pink — deliberately NOT the house red (different quantity)
    const priceSplit = splitByPlaceholder(tl, (b) => b.import_price * rate);
    $('#day-legend').innerHTML = modeLegend();
    // SoC in % (its own hidden 0–100 scale, matching the battery ring) — derived from the live
    // pct/kWh pair; without it the kWh fallback keeps the old behaviour on the kW axis.
    const live = store['/api/live']?.data;
    const cap = live?.soc_kwh != null && live?.soc_pct > 1 ? live.soc_kwh / (live.soc_pct / 100) : null;
    const socName = cap ? 'SoC %' : 'SoC';
    const socAxis = cap ? 3 : 1;
    const toSoc = (rows) => cap ? rows.map(([t, v]) => [t, v == null ? null : v / cap * 100]) : rows;
    // today so far, from the time bar's day-long history (one feed for both)
    const midnight = new Date().setHours(0, 0, 0, 0);
    const today = (key) => histData(store, key, FP_HISTORY_EP).filter(([t]) => Date.parse(t) >= midnight);
    // Weekday-aware x labels: midnight ticks show "Fri 25", the rest plain hours.
    const xfmt = (v) => { const d = new Date(v); return d.getHours() === 0 && d.getMinutes() === 0 ? `${d.toLocaleDateString([], { weekday: 'short' })} ${d.getDate()}` : d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false }); };
    const opt = Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [price, price, pv, house, base, soc], // ONE entry per UNIQUE series name (first-appearance order: Price, Price (est.), PV, House, Base load, SoC) — ECharts colours legend items by unique name, so a missing entry shifts every later swatch off its line
      legend: { show: true, data: ['PV', 'House', 'Base load (excl. heat/EV)', socName, 'Price'], top: 0, textStyle: { color: css('--muted') }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
      grid: { left: 50, right: 56, top: 30, bottom: 30, containLabel: true },
      yAxis: [
        yAxis('Kč/kWh', { position: 'right' }),
        yAxis(cap ? 'kW' : 'kW · kWh', { position: 'left' }),
        // the mode ribbon's scale: bars of height 0.05 → a strip hugging the bottom edge
        { type: 'value', min: 0, max: 1, show: false },
        // SoC %: hidden 0–105 so the full-battery plateau doesn't stretch the kW axis
        { type: 'value', min: 0, max: 105, show: false },
      ],
      series: [
        { name: 'Price', type: 'line', step: 'end', yAxisIndex: 0, data: priceSplit.real, smooth: false, symbol: 'none', lineStyle: { color: price, width: 2 }, areaStyle: { color: price + '14' },
          markLine: nowMark(true) },
        { name: 'Price (est.)', type: 'line', step: 'end', yAxisIndex: 0, data: priceSplit.ph, smooth: false, symbol: 'none', lineStyle: { color: price, width: 2, type: 'dotted', opacity: 0.55 } },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: today('pv_kw'), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 2 }, areaStyle: { color: grad(pv) } },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.pv_kw]), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 1.5, type: 'dashed' } },
        // House consumption: the measured TOTAL (solid red) vs the plan's BASE-load forecast
        // (dashed pink). Not the same quantity: the forecast excludes heating + EV electricity
        // (the LP carries those as decision variables), so in heating/charging periods the pink
        // line sits below the red one by design — distinct colour + label say so.
        { name: 'House', type: 'line', yAxisIndex: 1, data: today('house_kw'), smooth: true, symbol: 'none', lineStyle: { color: house, width: 2 } },
        { name: 'Base load (excl. heat/EV)', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.load_kw]), smooth: true, symbol: 'none', lineStyle: { color: base, width: 1.5, type: 'dashed' } },
        { name: socName, type: 'line', yAxisIndex: socAxis, data: toSoc(today('soc_kwh')), smooth: true, symbol: 'none', lineStyle: { color: soc, width: 2 } },
        { name: socName, type: 'line', yAxisIndex: socAxis, data: toSoc(tl.map((b) => [blockEnd(tl, b.t), b.soc_kwh])), smooth: true, symbol: 'none', lineStyle: { color: soc, width: 1.5, type: 'dashed' } },
        // Battery-mode ribbon along the bottom — one cell per 15-min block, the legend chips
        // above the chart give the colour key (replaces the old full-height washes). LAST in the
        // list: an earlier position would consume a palette slot and shift every legend swatch.
        { name: 'mode', type: 'bar', yAxisIndex: 2, silent: true, barWidth: '99%', z: 1, data: tl.map((b) => ({ value: [b.t, 0.05], itemStyle: { color: modeOf(b.slot).color + 'd9' } })) },
        // item M: day dividers — the Price series above already carries the "now" line.
        dayDividerSeries(tl, { now: false, uniformStepMs: 15 * 60000 }),
      ],
    });
    opt.xAxis = Object.assign(opt.xAxis, { axisLabel: { color: css('--muted'), formatter: xfmt } });
    c?.setOption(opt, true);
  },
};

// ---- ENERGY ----
screens.energy = {
  mount() {
    return `
    <div class="grid cols-4">
      ${['Horizon cost', 'Grid import', 'Grid export', 'PV curtailed'].map((l, i) => `<section class="card"><div class="kpi"><div class="kpi-label">${l}</div><div class="kpi-value" id="ek-${i}">—</div><div class="kpi-sub" id="eks-${i}"></div></div></section>`).join('')}
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">💰</span> Prices &amp; PV — today &amp; forecast</div><div class="legend" id="e-legend"></div></div>
      <div class="chart tall" id="e-price"></div>
    </section>
    <div class="grid cols-2" style="margin-top:18px">
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">🔋</span> Battery plan</div></div><div class="chart" id="e-batt"></div></section>
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">🔌</span> Grid &amp; curtailment</div></div><div class="chart" id="e-grid"></div></section>
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">📋</span> Per-block plan</div><div class="card-sub">Growatt mode applied per 15-min block</div></div>
      <div style="overflow-x:auto"><table class="tbl" id="e-table"></table></div>
    </section>`;
  },
  update(store) {
    const plan = store['/api/plan/latest']?.data; if (!plan) return;
    const tl = plan.timeline || []; const rate = czkRate(plan);
    // Charts get the expanded (uniform 15-min) view; the table below shows the REAL blocks with
    // their own width (item F).
    const tlx = expandTimeline(tl);
    const k = [`${fmt.czk(plan.total_cost_czk)}`, `${fmt.kw(plan.grid_import_kwh, 1)} kWh`, `${fmt.kw(plan.grid_export_kwh, 1)} kWh`, `${fmt.kw(plan.pv_curtailed_kwh, 1)} kWh`];
    const ks = [`${fmt.eur(plan.total_cost_eur)} · wear ${fmt.czk(plan.battery_wear_czk)}`, '', '', `final SoC ${fmt.kw(plan.final_soc_kwh, 1)} kWh`];
    k.forEach((v, i) => { $(`#ek-${i}`).textContent = v; $(`#eks-${i}`).textContent = ks[i]; });

    $('#e-legend').innerHTML = modeLegend();

    chart('e-price')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tlx),
      // One entry per UNIQUE series name (PV, Import, Import est., Export, Export est.) — ECharts
      // colours legend items by unique name, so a missing entry shifts every later swatch.
      color: [css('--yellow'), css('--blue'), css('--blue'), css('--blue'), css('--blue')],
      yAxis: [yAxis('kW'), yAxis('Kč/kWh', { position: 'right', splitLine: { show: false } })],
      series: [
        { name: 'PV', type: 'line', data: histData(store, 'pv_kw'), smooth: true, symbol: 'none', lineStyle: { color: css('--yellow'), width: 2 }, areaStyle: { color: grad(css('--yellow')) }, markArea: { silent: true, data: modeBands(tlx) }, markLine: nowMark() },
        { name: 'PV', type: 'line', data: tlx.map((b) => [b.t, b.pv_kw]), smooth: true, symbol: 'none', lineStyle: { color: css('--yellow'), width: 1.5, type: 'dashed' } },
        { name: 'Import price', type: 'line', step: 'end', yAxisIndex: 1, data: splitByPlaceholder(tlx, (b) => b.import_price * rate).real, symbol: 'none', lineStyle: { color: css('--blue'), width: 2 } },
        { name: 'Import price (est.)', type: 'line', step: 'end', yAxisIndex: 1, data: splitByPlaceholder(tlx, (b) => b.import_price * rate).ph, symbol: 'none', lineStyle: { color: css('--blue'), width: 2, type: 'dotted', opacity: 0.55 } },
        { name: 'Export price', type: 'line', step: 'end', yAxisIndex: 1, data: splitByPlaceholder(tlx, (b) => b.export_price * rate).real, symbol: 'none', lineStyle: { color: css('--blue'), width: 1, type: 'dashed' } },
        { name: 'Export price (est.)', type: 'line', step: 'end', yAxisIndex: 1, data: splitByPlaceholder(tlx, (b) => b.export_price * rate).ph, symbol: 'none', lineStyle: { color: css('--blue'), width: 1, type: 'dotted', opacity: 0.55 } },
        // item M: day dividers — the PV series above already carries the "now" line.
        dayDividerSeries(tlx, { now: false, uniformStepMs: 15 * 60000 }),
      ],
    }), true);

    chart('e-batt')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tlx),
      color: [css('--purple'), css('--gold'), css('--amber'), css('--amber')], // legend swatches match the series
      yAxis: [yAxis('kW'), yAxis('SoC kWh', { position: 'right', splitLine: { show: false } })],
      series: [
        { name: 'Charge', type: 'bar', stack: 'b', data: tlx.map((b) => [b.t, b.charge_kw]), itemStyle: { color: css('--purple') } },
        { name: 'Discharge', type: 'bar', stack: 'b', data: tlx.map((b) => [b.t, -b.discharge_kw]), itemStyle: { color: css('--gold') } },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: histData(store, 'soc_kwh'), smooth: true, symbol: 'none', lineStyle: { color: css('--amber'), width: 2 }, markLine: nowMark() },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: tlx.map((b) => [blockEnd(tlx, b.t), b.soc_kwh]), smooth: true, symbol: 'none', lineStyle: { color: css('--amber'), width: 1.5, type: 'dashed' } },
        // item M: day dividers — the SoC series above already carries the "now" line.
        dayDividerSeries(tlx, { now: false, uniformStepMs: 15 * 60000 }),
      ],
    }), true);

    chart('e-grid')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tlx),
      color: [css('--red'), css('--green'), css('--faint')], // legend swatches match the series
      yAxis: [yAxis('kW')],
      series: [
        { name: 'Import', type: 'bar', stack: 'g', data: tlx.map((b) => [b.t, b.grid_import_kw]), itemStyle: { color: css('--red') } },
        { name: 'Export', type: 'bar', stack: 'g', data: tlx.map((b) => [b.t, -b.grid_export_kw]), itemStyle: { color: css('--green') } },
        { name: 'Curtailed', type: 'line', data: tlx.map((b) => [b.t, b.curtail_kw]), symbol: 'none', lineStyle: { color: css('--faint'), type: 'dotted' }, areaStyle: { color: css('--surface-3') } },
        // item M: no other series here carries a "now" line yet — this one does, labeled.
        dayDividerSeries(tlx, { now: 'labeled', uniformStepMs: 15 * 60000 }),
      ],
    }), true);

    const i = nowBlock(tl);
    $('#e-table').innerHTML = `<thead><tr><th>Time</th><th class="num">Width</th><th class="num">Import</th><th class="num">Export</th><th class="num">PV</th><th class="num">SoC</th><th>Battery mode</th><th>Export on/off</th></tr></thead><tbody>`
      + tl.map((b, fi) => {
        const m = modeOf(b.slot); const isNow = fi === i; // the block containing "now"
        return `<tr class="${isNow ? 'now' : ''}"><td>${fmt.hm(b.t)}</td><td class="num">${b.dt_minutes ?? 15}m</td><td class="num">${fmt.n(b.import_price * rate, 2)}</td><td class="num">${fmt.n(b.export_price * rate, 2)}</td><td class="num">${fmt.n(b.pv_kw, 1)}</td><td class="num">${fmt.n(b.soc_kwh, 1)}</td><td><span class="badge" style="background:${m.color}22;color:${m.color}">${m.label}</span></td><td>${b.export_enabled ? '<span class="chip green" style="padding:1px 8px">on</span>' : '<span class="chip" style="padding:1px 8px">off</span>'}</td></tr>`;
      }).join('') + '</tbody>';
  },
};

// ---- HEATING ----
screens.heating = {
  mount() {
    return `
    <section class="card span-full">
      <div class="card-head"><div class="card-title"><span class="ico">🌡️</span> Predicted room temperatures (36 h)</div><div class="card-sub">model forecast · comfort band shaded</div></div>
      <div class="chart tall" id="ht-temp"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔥</span> Heating schedule</div><div class="card-sub">one row per relay zone — solid bar = exact on period, light bar = expected duty (quarters not yet decided)</div></div>
      <div class="chart tall" id="ht-sched"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🏠</span> Rooms now</div></div>
      <div class="zone-grid" id="ht-zones"></div>
    </section>`;
  },
  update(store) {
    const plan = store['/api/plan/latest']?.data;
    const zones = arrData(store, '/api/zones');
    const state = store['/api/state']?.data?.zones || [];
    if (!plan) return;
    const tl = plan.timeline || [];
    // Charts get the expanded (uniform 15-min) view (item F); the "rooms now" min/max text below
    // reads the raw blocks.
    const tlx = expandTimeline(tl);
    const znames = zones.map((z) => z.zone);
    const palette = ['#4f9cff', '#34d399', '#fbbf24', '#fb7185', '#a78bfa', '#22d3ee', '#f472b6', '#84cc16', '#fb923c', '#60a5fa'];

    // temperature prediction lines + a soft global comfort band
    const tmin = Math.min(...zones.map((z) => bandNow(z).lo));
    const tmax = Math.max(...zones.map((z) => bandNow(z).hi));
    const tempSeries = znames.map((z, k) => ({ name: z.replace(/_/g, ' '), type: 'line', smooth: true, symbol: 'none', lineStyle: { width: 1.6, color: palette[k % palette.length] }, itemStyle: { color: palette[k % palette.length] }, data: tlx.map((b) => [blockEnd(tlx, b.t), b.temp_c?.[z]]) }));
    if (isFinite(tmin) && isFinite(tmax)) {
      tempSeries.unshift({ name: 'comfort', type: 'line', data: tlx.map((b) => [b.t, tmax]), symbol: 'none', lineStyle: { opacity: 0 }, areaStyle: { color: css('--green') + '12', origin: tmin }, silent: true, tooltip: { show: false } });
    }
    chart('ht-temp')?.setOption(Object.assign(baseOption(), {
      legend: { type: 'scroll', textStyle: { color: css('--muted') }, top: 0 },
      yAxis: [yAxis('°C', { scale: true })],
      series: [...tempSeries, dayDividerSeries(tlx, { now: 'labeled', uniformStepMs: 15 * 60000 })],
    }), true);

    // item M: heating schedule as ON-PERIOD bars, one row per relay zone (every zone with
    // underfloor heating, i.e. `z.heated` — present even with an empty horizon, so the table reads
    // at a glance). `mergeOnPeriods` (pure, unit-tested — dashboard_test.js) does the actual
    // merge/quarters logic; this just lays the result out as a custom (Gantt-style) series: a
    // solid bar for a merged run of EXACT on blocks, a lighter dashed-border bar for an
    // undecided-quarters hourly block. HVAC (`hvac_heat_kw`/`cool_kw`) never enters `heat_kw` and
    // has no row here.
    const heatZones = zones.filter((z) => z.heated);
    const planStart = tl[0]?.t;
    // Scale the chart to the number of rows so a house with many relay zones doesn't crush them
    // into an illegibly short strip, and a house with few doesn't leave a mostly-empty tall panel.
    const schedDom = document.getElementById('ht-sched');
    if (schedDom) schedDom.style.height = `${Math.max(220, 64 + heatZones.length * 44)}px`;
    const schedRows = [];
    heatZones.forEach((z, zi) => {
      const color = palette[zi % palette.length];
      mergeOnPeriods(tl, z.zone, z.max_heat_kw).forEach((p) => {
        const label = p.exact
          ? `${fmt.hm(p.start)}–${fmt.hm(p.end)} · ${p.minutes} min`
          : `${p.quarters} × 15 min in this hour`;
        schedRows.push({
          value: [zi, new Date(p.start).getTime(), new Date(p.end).getTime()],
          zone: z.zone, period: p, color, label,
        });
      });
    });
    const renderHeatingBar = (params, api) => {
      const row = schedRows[params.dataIndex];
      const start = api.coord([api.value(1), api.value(0)]);
      const end = api.coord([api.value(2), api.value(0)]);
      const rowH = api.size([0, 1])[1];
      const barH = Math.max(10, Math.min(26, rowH * 0.55));
      const rect = echarts.graphic.clipRectByRect(
        { x: start[0], y: start[1] - barH / 2, width: Math.max(2, end[0] - start[0]), height: barH },
        { x: params.coordSys.x, y: params.coordSys.y, width: params.coordSys.width, height: params.coordSys.height }
      );
      if (!rect) return null;
      const exact = row.period.exact;
      return {
        type: 'group',
        children: [
          { type: 'rect', shape: { x: rect.x, y: rect.y, width: rect.width, height: rect.height, r: 3 },
            style: { fill: exact ? row.color : row.color + '55', stroke: row.color, lineWidth: exact ? 0 : 1, lineDash: exact ? [] : [3, 2] } },
          { type: 'text', style: { text: row.label, x: rect.x + 4, y: rect.y - 3, fill: css('--muted'), fontSize: 10, textVerticalAlign: 'bottom', textAlign: 'left' }, silent: true },
        ],
      };
    };
    chart('ht-sched')?.setOption(Object.assign(baseOption(), {
      grid: { left: 110, right: 24, top: 20, bottom: 30, containLabel: true },
      legend: { show: false },
      yAxis: { type: 'category', data: heatZones.map((z) => z.zone.replace(/_/g, ' ')), inverse: true, axisLabel: { color: css('--muted') }, axisLine: { lineStyle: { color: css('--border') } }, splitLine: { show: false } },
      tooltip: {
        trigger: 'item', confine: true, backgroundColor: css('--surface-2'), borderColor: css('--border'), textStyle: { color: css('--text') },
        formatter: (p) => {
          const period = p.data?.period; if (!period) return '';
          const now = Date.now();
          const active = now >= new Date(period.start).getTime() && now < new Date(period.end).getTime();
          // Only a block inside the solver's actual pin window (isNearTermBlock, 30 min) is a
          // GUARANTEED integral relay decision; a later fine (15-min) block reads solid too (it's
          // near-binary in practice) but is still, strictly, the relaxed LP's own value.
          const pinned = period.exact && isNearTermBlock(period.start, planStart);
          const head = period.exact
            ? `${fmt.hm(period.start)}–${fmt.hm(period.end)} · ${period.minutes} min${pinned ? ' · pinned' : ''}`
            : `${fmt.hm(period.start)}–${fmt.hm(period.end)} · ${period.quarters}/4 quarters — exactly which not yet decided`;
          return `<div style="margin-bottom:3px">${esc(p.data.zone.replace(/_/g, ' '))}</div>${head}<br>${fmt.kw(period.kwh, 2)} kWh${active ? ' · <b>actuated now</b>' : ''}`;
        },
      },
      series: [
        { type: 'custom', renderItem: renderHeatingBar, encode: { x: [1, 2], y: 0 }, data: schedRows, z: 2 },
        dayDividerSeries(tl, { now: 'labeled' }),
      ],
    }), true);
    chart('ht-sched')?.resize();

    // rooms now
    const smap = Object.fromEntries(state.map((s) => [s.zone, s.temp_c]));
    const fs = plan.first_step || {};
    $('#ht-zones').innerHTML = zones.map((z) => {
      const t = smap[z.zone]; const c = comfort(t, z); const heating = isRelayOn(fs.heat_kw?.[z.zone]);
      const preds = tl.map((b) => b.temp_c?.[z.zone]).filter((x) => x != null);
      const lo = preds.length ? Math.min(...preds) : null, hi = preds.length ? Math.max(...preds) : null;
      return `<div class="zone ${heating ? 'heating' : ''}">
        <div class="zname"><span>${esc(z.zone.replace(/_/g, ' '))}</span>${heating ? `<span class="heat-dot">🔥 ${fmt.kw(fs.heat_kw?.[z.zone], 1)}kW</span>` : (c.cls ? `<span class="chip ${c.cls}" style="padding:1px 7px">${c.label}</span>` : '')}</div>
        <div class="ztemp">${fmt.temp(t)}</div>
        <div class="faint" style="font-size:0.72rem">band ${bandNow(z).lo}–${bandNow(z).hi}° · forecast ${fmt.n(lo, 1)}–${fmt.n(hi, 1)}°</div>
      </div>`;
    }).join('');
  },
};

// ---- MODEL ----
screens.model = {
  mount() {
    return `
    <div class="grid cols-2">
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">🎯</span> Forward prediction vs measured — worst zone</div><div class="card-sub" id="vmeta"></div></div><div class="chart" id="m-valid"></div></section>
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">📐</span> Per-zone prediction error (RMSE)</div></div><div class="chart" id="m-rmse"></div></section>
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔧</span> Internal-gain calibration</div><div class="card-sub" id="gmeta">occupants · appliances · fireplace — self-fitted</div></div>
      <div class="chart" id="m-gains"></div>
    </section>`;
  },
  update(store) {
    const val = store['/api/forecast/validation']?.data;
    const cal = store['/api/calibration/gains']?.data;

    if (val?.zones?.length) {
      const worst = val.zones[0];
      $('#vmeta').textContent = val.mean_rmse_k != null ? `worst: ${worst.zone.replace(/_/g, ' ')} · mean RMSE ${fmt.n(val.mean_rmse_k, 2)} K · ${val.zones.length} zones · since ${fmt.hm(val.anchored_at)}` : '—';
      if (worst?.points?.length) {
        chart('m-valid')?.setOption(Object.assign(baseOption(), {
          legend: { top: 0, textStyle: { color: css('--muted') } },
          yAxis: [yAxis('°C', { scale: true })],
          series: [
            { name: `${worst.zone} predicted`, type: 'line', smooth: true, symbol: 'none', data: worst.points.map((p) => [p.t, p.predicted_c]), lineStyle: { color: css('--blue') } },
            { name: 'measured', type: 'line', smooth: true, symbol: 'circle', symbolSize: 4, data: worst.points.map((p) => [p.t, p.measured_c]), lineStyle: { color: css('--amber') } },
          ],
        }), true);
      }
      // ECharts category axis renders index 0 at the bottom, so reverse to put the worst zone on top.
      const zrev = [...val.zones].reverse();
      chart('m-rmse')?.setOption({
        textStyle: { color: css('--muted') }, grid: { left: 90, right: 30, top: 10, bottom: 24 },
        tooltip: { trigger: 'axis', confine: true, axisPointer: { type: 'shadow' },
          formatter: (ps) => { const z = zrev[ps[0].dataIndex]; if (!z) return ''; const b = z.mean_bias_k; return `${esc(z.zone.replace(/_/g, ' '))}<br/>RMSE ${z.rmse_k.toFixed(2)} K · bias ${b >= 0 ? '+' : ''}${b.toFixed(2)} K · n=${z.n}`; } },
        xAxis: { type: 'value', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        yAxis: { type: 'category', data: zrev.map((z) => z.zone.replace(/_/g, ' ')), axisLabel: { color: css('--muted') } },
        series: [{ type: 'bar', data: zrev.map((z) => z.rmse_k), itemStyle: { color: css('--blue'), borderRadius: [0, 4, 4, 0] } }],
      }, true);
    } else if (val?.zones_unavailable?.length) {
      // A FAILED measurement read is not "warming up" — say so, or a broken DB looks like patience.
      $('#vmeta').textContent = `measurement read failed for ${val.zones_unavailable.length} zone(s) — check the InfluxDB connection`;
    } else {
      $('#vmeta').textContent = 'warming up — scoring needs ≥3 h of measured data after a snapshot';
    }

    if (cal) {
      $('#gmeta').textContent = cal.live?.fitted_at ? `fitted ${fmt.hm(cal.live.fitted_at)} · ${cal.window_days}-day window · re-fits every ${cal.recalibrate_hours}h · no bar = not fitted / not configured` : 'config baseline';
      const live = cal.live?.gains_w || {}; const base = cal.config_baseline_w || {};
      const znames = [...new Set([...Object.keys(live), ...Object.keys(base)])].sort();
      // Gains are per-daypart profiles {night, day, evening} (fitted separately — occupancy heat is
      // daypart-shaped). `?? null` (not `|| 0`): an absent zone draws NO bar, so "not fitted" /
      // "not configured" can't be misread as a real 0 W gain.
      const lbl = { show: true, position: 'top', color: css('--muted'), fontSize: 9, formatter: (p) => p.value != null ? Math.round(p.value) : '' };
      const part = (m, z, k) => (m[z] ? (typeof m[z] === 'number' ? m[z] : m[z][k]) ?? null : null);
      chart('m-gains')?.setOption({
        textStyle: { color: css('--muted') }, grid: { left: 50, right: 20, top: 28, bottom: 60, containLabel: true },
        tooltip: { trigger: 'axis', confine: true, axisPointer: { type: 'shadow' }, valueFormatter: (v) => v == null ? 'n/a' : v.toFixed(0) + ' W' },
        legend: { top: 0, textStyle: { color: css('--muted') } },
        xAxis: { type: 'category', data: znames.map((z) => z.replace(/_/g, ' ')), axisLabel: { color: css('--muted'), rotate: 35 } },
        yAxis: { type: 'value', name: 'W', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        series: [
          { name: 'night', type: 'bar', data: znames.map((z) => part(live, z, 'night')), label: lbl, itemStyle: { color: css('--blue'), borderRadius: [4, 4, 0, 0] } },
          { name: 'day', type: 'bar', data: znames.map((z) => part(live, z, 'day')), label: lbl, itemStyle: { color: css('--green'), borderRadius: [4, 4, 0, 0] } },
          { name: 'evening', type: 'bar', data: znames.map((z) => part(live, z, 'evening')), label: lbl, itemStyle: { color: css('--amber'), borderRadius: [4, 4, 0, 0] } },
          { name: 'config baseline', type: 'bar', data: znames.map((z) => part(base, z, 'evening')), label: lbl, itemStyle: { color: css('--faint'), borderRadius: [4, 4, 0, 0] } },
        ],
      }, true);
    }
  },
};

// ---- SYSTEM ----
// ---- NEW FEATURES ----
let featuresRendered = null; // generated_at of the payload currently drawn (charts are rebuilt only when it changes)
function featureCard(f, i) {
  const rel = featureReleaseLabels(f).map((l) => `<span class="mono">${esc(l)}</span>`).join(' · ');
  const charts = (f.charts || []).map((c, j) => featureChartEmpty(c) ? ''
    : `<div class="feat-chart-title">${esc(c.title)}${c.y_unit ? ` <span class="faint">(${esc(c.y_unit)})</span>` : ''}</div><div class="feat-chart" id="feat-chart-${i}-${j}"></div>`).join('');
  const tables = (f.tables || []).map((t) => featureTableEmpty(t) ? ''
    : `<div class="feat-chart-title">${esc(t.title)}</div><div class="tbl-wrap"><table class="tbl"><thead><tr>${t.columns.map((c) => `<th>${esc(c)}</th>`).join('')}</tr></thead><tbody>${t.rows.map((r) => `<tr>${r.map((v) => `<td class="${typeof v === 'number' ? 'num' : ''}">${esc(featureCell(v))}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`).join('');
  const v = f.verdict || {};
  const notes = (f.notes || []).length ? `<ul class="feat-notes">${f.notes.map((n) => `<li>${esc(n)}</li>`).join('')}</ul>` : '';
  const proof = f.baseline?.text ? `<div class="feat-baseline"><b>Release proof</b> ${esc(f.baseline.text)}</div>` : '';
  return `<section class="card feat" id="feat-${esc(f.id)}">
    <div class="card-head"><div class="card-title">${esc(f.name)}</div>
      <div class="feat-badges"><span class="chip ${featureStatusClass(f.status)}">${esc(f.status)}</span><span class="chip ${featureKindClass(f.kind)}">${esc(f.kind_label || f.kind)}</span></div></div>
    <div class="card-sub feat-rel">${rel}</div>
    <div class="feat-what">${esc(f.what)}</div>
    <div class="feat-verdict ${featureVerdictClass(v.state)}">${esc(v.text)}${f.n != null ? ` <span class="feat-n">n=${esc(f.n)}</span>` : ''}</div>
    ${charts}${tables}${proof}${notes}
  </section>`;
}
// The "Graduated" list: features that proved themselves and are now the only behaviour (static text).
function featureGraduatedHtml(list) {
  if (!Array.isArray(list) || !list.length) return '';
  const rows = list.map((g) => `<li><b>${esc(g.name)}</b> <span class="mono">${esc(featureGraduatedLabel(g))}</span> — ${esc(g.result)}</li>`).join('');
  return `<section class="card feat-graduated"><div class="card-head"><div class="card-title">Graduated</div><div class="card-sub">proven, now the only behaviour</div></div><ul class="feat-notes">${rows}</ul></section>`;
}
screens.features = {
  mount() { featuresRendered = null; return '<div id="features-body"><div class="loading">Loading…</div></div>'; },
  async update(store) {
    const d = store['/api/features']?.data;
    const body = $('#features-body');
    if (!body || !d || !Array.isArray(d.features)) return;
    if (featuresRendered === d.generated_at) return;
    featuresRendered = d.generated_at;
    disposeCharts();
    body.innerHTML = `<div class="card-sub feat-gen">Generated ${esc(d.generated_at)} · UTC days · each comparison is labelled with how strong its evidence is</div>
      <div class="grid cols-2 feat-grid">${d.features.map(featureCard).join('')}</div>
      ${featureGraduatedHtml(d.graduated)}`;
    const palette = [css('--blue'), css('--amber'), css('--green'), css('--purple'), css('--red')];
    d.features.forEach((f, i) => (f.charts || []).forEach((c, j) => {
      if (featureChartEmpty(c)) return;
      const ch = chart(`feat-chart-${i}-${j}`); if (!ch) return;
      ch.setOption({
        ...baseOption(),
        grid: { left: 8, right: 12, top: 34, bottom: 8, containLabel: true },
        xAxis: { type: 'category', data: c.x, axisLabel: { color: css('--muted'), hideOverlap: true }, axisLine: { lineStyle: { color: css('--border') } } },
        yAxis: yAxis(c.y_unit || ''),
        color: palette,
        series: c.series.map((s) => ({ name: s.name, type: c.x.length <= 12 ? 'bar' : 'line', data: featureSeriesData(s.values), barMaxWidth: 28 })),
      }, true);
    }));
  },
};

screens.system = {
  mount() {
    return `
    <div class="grid cols-2">
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">🩺</span> Status</div></div><div id="sys-status"></div></section>
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">📡</span> Data feed health</div></div><div id="sys-feeds"></div></section>
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🧾</span> Decision now (raw)</div><div class="card-sub">the raw per-controller decision (battery, heating &amp; EV)</div></div>
      <div id="sys-decision"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔗</span> JSON API</div></div>
      <div class="muted" style="font-size:0.85rem">All data is served read-only at <a class="link" href="${mpcUrl('/api')}">/api</a>. Endpoints:
        <a class="link" href="${mpcUrl('/api/plan/latest')}">/api/plan/latest</a>, <a class="link" href="${mpcUrl('/api/live')}">/api/live</a>, <a class="link" href="${mpcUrl('/api/history')}">/api/history</a>, <a class="link" href="${mpcUrl('/api/calibration/gains')}">/api/calibration/gains</a>, <a class="link" href="${mpcUrl('/api/forecast/validation')}">/api/forecast/validation</a>, <a class="link" href="${mpcUrl('/api/version')}">/api/version</a>.</div>
    </section>`;
  },
  async update(store) {
    const v = store['/api/version']?.data; const plan = store['/api/plan/latest']?.data;
    const ready = window.__ready || {};
    const rows = [
      ['Version', v ? `<span class="mono">${esc(v.git_sha)}</span>` : '—'],
      ['Built', esc(v?.built_at) || '—'],
      ['Config / model', v ? `<span class="mono">${esc(v.config_fingerprint?.slice(0, 8))} / ${esc(v.model_fingerprint?.slice(0, 8))}</span>` : '—'],
      ['Ready', ready.ready === true ? '<span class="badge green">yes</span>' : '<span class="badge amber">warming up</span>'],
      ['Last plan tick', ready.last_tick_age_seconds != null ? fmt.ago(ready.last_tick_age_seconds) : '—'],
    ];
    $('#sys-status').innerHTML = rows.map(([k, val]) => `<div class="stat-row"><span class="k">${k}</span><span class="v">${val}</span></div>`).join('');

    const feeds = plan?.placeholder_inputs || [];
    $('#sys-feeds').innerHTML = feeds.length
      ? feeds.map((f) => `<div class="stat-row"><span class="k">⚠️ fallback</span><span class="v" style="font-weight:500;color:var(--amber)">${esc(f)}</span></div>`).join('')
      : '<div class="warn-box" style="background:var(--green-soft);color:var(--green)">✓ all data feeds are live</div>';

    const fs = plan?.first_step;
    if (fs) {
      const heating = Object.entries(fs.heat_kw || {}).filter(([, kw]) => isRelayOn(kw));
      $('#sys-decision').innerHTML = [
        ['Block start', fmt.hm(fs.hour_start)],
        ['Mode', `${modeOf(fs.mode?.slot).label} (${esc(fs.mode?.slot)})`],
        ['Export / inverter', `${fs.mode?.export_enabled ? 'enabled' : 'disabled'} / ${fs.mode?.inverter_on ? 'on' : 'off'}`],
        ['Battery', `charge ${fmt.kw(fs.battery_charge_kw)} / discharge ${fmt.kw(fs.battery_discharge_kw)} kW`],
        ['Grid', `import ${fmt.kw(fs.grid_import_kw)} / export ${fmt.kw(fs.grid_export_kw)} kW`],
        ['Heating', heating.length ? heating.map(([z, kw]) => `${esc(z.replace(/_/g, ' '))} ${fmt.kw(kw, 1)}kW`).join(', ') : 'none'],
      ].map(([k, val]) => `<div class="stat-row"><span class="k">${k}</span><span class="v">${val}</span></div>`).join('');
    }
  },
};

// ---- EV ----
const EV_BADGE = {
  charging: ['green', '⚡ Charging on our wallbox'],
  connected: ['blue', '🔌 On our wallbox (idle)'],
  charging_away: ['amber', '🚗 Charging elsewhere'],
  away: ['', '— Away / driving'],
};
// Friendly labels for the API's strategy vocabulary — shown as one-tap segments.
const EV_STRATEGIES = [
  { id: 'cost_optimized',  label: '💰 Cheapest',     desc: 'meet the target by the deadline at the lowest cost' },
  { id: 'solar_preferred', label: '🌤️ Solar first',  desc: 'solar first, top up from cheap grid to make the deadline' },
  { id: 'solar_only',      label: '☀️ Solar only',   desc: 'only surplus solar — never grid; may miss the target' },
  { id: 'charge_now',      label: '⚡ Now',           desc: 'full rate immediately, price-blind' },
];
const EV_TARGETS = [60, 70, 80, 90, 100];
const DEADLINE_SRC = { pref: 'your override', learned: 'learned departure', config: 'default' };

// Optimistic per-charger preference overlay. /api/ev is served from a 60 s cache, so the polls
// right after a save still carry the OLD values and would snap the tapped control back — which
// reads as "the click did nothing". Saved fields override the server view until it catches up.
const evPending = {};
function evEffective(e) {
  const p = evPending[e.name];
  if (!p) return e;
  // `e.target_pct` is the EFFECTIVE target — the stored preference capped at the car's own charge
  // limit. Requiring an exact match meant posting 100 % to a car limited to 80 % never reconciled
  // (the server can only ever answer 80), so the overlay hung for its full timeout. So: a server
  // value at-or-below what we asked for counts as applied — but ONLY once it has actually moved off
  // the pre-save value. Without that second clause, raising the target always reconciled on the very
  // first poll (60 <= 80 holds while the cache still says 60), the overlay was dropped and the chip
  // snapped back for up to the full cache TTL — the exact "the click did nothing" symptom it exists
  // to prevent. Only decreases were protected.
  // Third clause: `target_capped` is the server saying "the stored preference exceeds the car's
  // own limit and I capped it" — that answer is FINAL, not cache lag, so accept it even when it
  // equals the pre-save value (a target already at the cap can never "move off" `was`; requiring
  // that hung the overlay for its full timeout and then snapped the chip back).
  const tgt = (p) => p.target_pct == null
    || (e.target_pct != null
        && Math.round(e.target_pct) <= Math.round(p.target_pct)
        && ((p.was == null || Math.round(e.target_pct) !== Math.round(p.was))
            || (e.target_capped && Math.round(p.target_pct) > Math.round(e.target_pct))));
  const caughtUp = (p.strategy == null || e.strategy === p.strategy)
    && tgt(p)
    && (p.deadline == null || e.deadline_hm === p.deadline);
  if (caughtUp || Date.now() - p.ts > 90000) { delete evPending[e.name]; return e; }
  const m = { ...e };
  if (p.strategy != null) m.strategy = p.strategy;
  if (p.target_pct != null) m.target_pct = p.target_pct;
  if (p.deadline != null) { m.deadline_hm = p.deadline; m.deadline_source = 'pref'; }
  return m;
}

// The planned charging window read off the schedule array against the plan timeline (or null when
// no block charges).
function evWindow(e, tl) {
  const kw = e.charge_kw || [];
  let first = -1, last = -1;
  for (let i = 0; i < Math.min(kw.length, tl.length); i++) {
    if (kw[i] > 0.05) { if (first < 0) first = i; last = i; }
  }
  if (first < 0) return null;
  // Past the last block, fall back to ITS OWN duration (item F: may be 60 min, not always 15).
  const end = tl[last + 1]?.t || new Date(new Date(tl[last].t).getTime() + (tl[last].dt_minutes ?? 15) * 60000).toISOString();
  return { from: tl[first].t, to: end, now: kw[nowBlock(tl)] > 0.05 };
}

// One honest sentence: what the plan will actually do with this car — the answer to "I plugged
// it in, what happens now?".
function evSummary(e, tl) {
  const by = e.deadline_hm ? ` · ready by <b>${esc(e.deadline_hm)}</b>` : '';
  if (e.status === 'away') return `Away — plug in and the next plan (a few minutes) schedules the charge${by}.`;
  if (e.status === 'charging_away') return 'Charging elsewhere — observed only, never scheduled by the MPC.';
  if (e.soc_pct == null) return 'On our wallbox but the car’s SoC is unknown — the MPC can’t schedule a target charge; any measured draw is still accounted for in the plan.';
  const win = evWindow(e, tl);
  if (win) {
    const kwh = e.charged_kwh > 0.05 ? ` · <b>${fmt.kw(e.charged_kwh, 1)} kWh</b> planned` : '';
    return win.now
      ? `Charging now, until ~<b>${fmt.hm(win.to)}</b>${kwh}${by}.`
      : `Charge scheduled <b>${fmt.hm(win.from)}–${fmt.hm(win.to)}</b> (cheapest / sunniest blocks)${kwh}${by}.`;
  }
  if (e.soc_pct >= (e.target_pct ?? 100) - 0.5) return `At target — nothing to schedule${by}.`;
  if (e.strategy === 'solar_only') return 'Solar-only — waiting for surplus solar; no charge in the current plan.';
  return `No charge in the current plan yet — it re-plans within a few minutes${by}.`;
}

function evCard(e, tl) {
  const [cls, label] = EV_BADGE[e.status] || ['', '—'];
  const soc = e.soc_pct, tgt = e.target_pct;
  const toAdd = (soc != null && tgt != null && e.capacity_kwh != null && tgt > soc)
    ? ` <span class="faint">(+${fmt.kw((tgt - soc) / 100 * e.capacity_kwh, 1)} kWh)</span>` : '';
  const src = DEADLINE_SRC[e.deadline_source] || e.deadline_source || 'default';
  const seg = EV_STRATEGIES.map((s) =>
    `<button class="ev-opt ${e.strategy === s.id ? 'on' : ''}" data-k="strategy" data-v="${s.id}" title="${s.desc}">${s.label}</button>`).join('');
  const tchips = EV_TARGETS.map((t) =>
    `<button class="ev-opt ${tgt != null && Math.round(tgt) === t ? 'on' : ''}" data-k="target_pct" data-v="${t}">${t}%</button>`).join('');
  // Full 24 h picker at half-hour steps (a native select wheels nicely on mobile); an odd stored
  // time (e.g. a legacy 08:29 override) stays visible as its own entry.
  const dtimes = [];
  for (let hh = 0; hh < 24; hh++) for (const mm of ['00', '30']) dtimes.push(`${String(hh).padStart(2, '0')}:${mm}`);
  if (e.deadline_hm && !dtimes.includes(e.deadline_hm)) dtimes.push(e.deadline_hm);
  const dsel = `<select class="ev-deadline" aria-label="ready-by time">${dtimes.map((d) =>
    `<option value="${esc(d)}" ${e.deadline_hm === d ? 'selected' : ''}>${esc(d)}</option>`).join('')}</select>`;
  return `<section class="card">
    <div class="card-head"><div class="card-title"><span class="ico">🚗</span> ${esc(e.name)}</div>
      <span class="badge ${cls}">${label}</span></div>
    <div class="ev-body">
    <div class="ev-info">
      <div class="ev-soc-nums">
        <span class="big">${soc != null ? Math.round(soc) + '%' : '—'}</span>
        <span class="faint">→</span>
        <span class="tgt">${tgt != null ? Math.round(tgt) + '%' : '—'}</span>${toAdd}
        <span class="ev-live">${e.charger_power_kw > 0.05 ? '⚡ ' + fmt.kw(e.charger_power_kw, 1) + ' kW' : ''}</span>
      </div>
      <div class="ev-bar">
        <div class="ev-bar-fill" style="width:${clamp(soc ?? 0, 0, 100)}%"></div>
        ${tgt != null ? `<div class="ev-bar-tgt" style="left:${clamp(tgt, 0, 100)}%"></div>` : ''}
      </div>
      <div class="ev-plan">${evSummary(e, tl)}</div>
    </div>
    <div class="ev-controls" data-charger="${esc(e.name)}">
      <div class="ev-lbl">Strategy</div>
      <div class="ev-seg">${seg}</div>
      <div class="ev-lbl">Charge to</div>
      <div class="ev-seg">${tchips}</div>
      <div class="ev-lbl">Ready by — time of day, next occurrence <span class="ev-src">(${esc(src)})</span></div>
      <div class="ev-seg">${dsel}</div>
      <div class="ev-foot"><span class="ev-flash"></span><button class="ev-clear link-btn">↩ Reset to defaults</button></div>
    </div>
    </div>
  </section>`;
}

// While a save's optimistic state (and its "✓ saved" flash) is on screen, hold off the poll's
// card rebuild so an in-flight pre-merge fetch can't snap the tapped chip back (it reconciles
// right after the hold).
let evHoldUntil = 0;

// Every control saves itself on tap/change (the POST merges per field server-side) — no Save
// button to forget on a phone.
// What a readOnly host shows instead of the EV controls: its own {label, href}, else a plain note.
function evControlPointer(controlLink) {
  const label = controlLink && typeof controlLink.label === 'string' ? controlLink.label.trim() : '';
  const href = controlLink && typeof controlLink.href === 'string' ? controlLink.href.trim() : '';
  if (label && href) return { label, href };
  return { label: label || 'Changing settings is only available on the brain dashboard', href: null };
}
function wireEv(e) {
  // Match by the decoded `data-charger` value rather than a CSS selector built from the name: the
  // attribute is HTML-escaped (esc) but CSS.escape doesn't escape quotes, so a name with a `"` would
  // make the selector a syntax error and the controls silently dead. A direct compare is name-safe.
  const root = [...document.querySelectorAll('.ev-controls')].find((el) => el.dataset.charger === e.name);
  if (!root) return;
  const flash = (msg, ok = true) => {
    // Resolve from the LIVE DOM at call time, not the captured `root`: a poll that rebuilt the
    // cards mid-request detaches `root`, and a flash written into the orphan is invisible —
    // failures went silent exactly when the server was slow.
    const ctrl = [...document.querySelectorAll('.ev-controls')].find((el) => el.dataset.charger === e.name);
    const f = ctrl && ctrl.querySelector('.ev-flash');
    if (!f) return;
    f.textContent = msg; f.style.color = ok ? 'var(--green)' : 'var(--red)';
    setTimeout(() => { if (f.textContent === msg) f.textContent = ''; }, 2500);
  };
  const post = async (body) => {
    evHoldUntil = Date.now() + 2600;
    // Overlay immediately: /api/ev is 60 s-cached, so polls keep returning pre-save values for a
    // while — evEffective() keeps the saved fields on screen until the server view catches up.
    // `was` is the server's target BEFORE this save, so evEffective() can tell "the server capped
    // my request" apart from "the server hasn't seen it yet" — see the `tgt` rule there. Keep the
    // ORIGINAL `was` across a second save while an overlay is still pending: that first value is
    // the last one the server actually confirmed.
    const prev = evPending[e.name];
    const was = prev && 'was' in prev ? prev.was : e.target_pct;
    evPending[e.name] = { ...prev, ...body, was, ts: Date.now() };
    const ok = await apiPost(`/api/ev/${encodeURIComponent(e.name)}/preference`, body);
    // Re-arm the hold from the RESPONSE: it was armed at request start, so any write slower than
    // ~2.6 s (no timeout on apiSend; the 401 branch blocks on a token prompt) expired it before
    // flash() ran — the next poll rebuilt the cards and the failure feedback landed in a
    // detached DOM node, i.e. failures went silent exactly when the server was struggling.
    evHoldUntil = Date.now() + 2600;
    flash(ok ? '✓ saved' : '✗ save failed', ok);
    if (!ok) {
      // Roll back ONLY the fields this request tried to set. Dropping the whole overlay also
      // discarded earlier saves that DID succeed but whose values the 60 s-cached /api/ev has not
      // surfaced yet — so one failed tap visually reverted unrelated, already-persisted choices.
      const pend = evPending[e.name];
      if (pend) {
        for (const k of Object.keys(body)) delete pend[k];
        // `ts` and `was` are bookkeeping, not saved fields — an entry holding only those is empty.
        if (Object.keys(pend).filter((k) => k !== 'ts' && k !== 'was').length === 0) delete evPending[e.name];
      }
      // KEEP the render hold on failure: dropping it let the 400 ms refresh rebuild the cards
      // and destroy the "save failed" flash ~2 s early. The optimistic fields are already
      // rolled back above — but the CHIP HIGHLIGHT was applied straight to the DOM, so also
      // schedule a rebuild for just after the hold expires, or the failed tap stays visually
      // selected until the next 10 s poll.
      setTimeout(refresh, 2700);
    }
    setTimeout(refresh, 400);
  };
  if (mpcHost().readOnly) { // the host has its own EV control: no second path here, just a pointer to it
    const link = evControlPointer(mpcHost().controlLink);
    const note = document.createElement('div');
    note.className = 'ev-readonly';
    if (link.href) {
      const a = document.createElement('a');
      a.className = 'link'; a.href = link.href; a.target = '_top'; a.rel = 'noopener'; a.textContent = link.label;
      note.appendChild(a);
    } else note.textContent = link.label;
    root.replaceChildren(note);
    return;
  }
  root.querySelectorAll('.ev-opt').forEach((b) => b.onclick = () => {
    // Optimistic highlight so the tap feels instant; refresh() reconciles with the server.
    root.querySelectorAll(`.ev-opt[data-k="${b.dataset.k}"]`).forEach((x) => x.classList.toggle('on', x === b));
    post({ [b.dataset.k]: b.dataset.k === 'target_pct' ? parseFloat(b.dataset.v) : b.dataset.v });
  });
  const dl = root.querySelector('.ev-deadline');
  dl.onchange = () => { if (dl.value) { post({ deadline: dl.value }); dl.blur(); } }; // blur so the poll may re-render
  root.querySelector('.ev-clear').onclick = async () => {
    // Hold the render like `post()` does. Without it the next poll (every 10 s) re-rendered the card
    // mid-request, destroying the `.ev-flash` node this handler is about to write to — so the
    // confirmation vanished and the card appeared to snap back while the DELETE was still in flight.
    evHoldUntil = Date.now() + 2600;
    // Discard the overlay optimistically, but RESTORE it if the DELETE fails: the server still
    // holds the previously-saved values then, and dropping the overlay snapped the card back to
    // the stale 60 s-cached view instead.
    const prevPending = evPending[e.name];
    delete evPending[e.name];
    const ok = await apiDelete(`/api/ev/${encodeURIComponent(e.name)}/preference`);
    // Restore only if no NEWER overlay landed while the DELETE was in flight (the controls stay
    // live, so a concurrent tap's post() may have saved successfully); refresh ts so a slow
    // DELETE doesn't restore an overlay that evEffective's 90 s expiry kills on the next poll.
    if (!ok && prevPending && !evPending[e.name]) evPending[e.name] = { ...prevPending, ts: Date.now() };
    evHoldUntil = Date.now() + 2600; // re-armed from the response — see the save handler
    flash(ok ? '✓ back to defaults' : '✗ clear failed', ok);
    // Hold kept on failure too — see the save handler: the flash must outlive the 400 ms
    // refresh, and a rebuild after the hold reconciles the highlight.
    if (!ok) setTimeout(refresh, 2700);
    setTimeout(refresh, 400);
  };
}

screens.ev = {
  mount() {
    return `
    <div id="ev-cards" class="grid ev-grid"></div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔌</span> Charge schedule — by source</div>
        <div class="card-sub">solar / grid / battery → car, per 15-min block</div></div>
      <div class="chart tall" id="ev-chart"></div>
    </section>`;
  },
  update(store) {
    // Defensive at the boundary too: a non-array here must degrade to the placeholder, never throw.
  const evs = arrData(store, '/api/ev').map(evEffective);
    const tl = arrData(store, '/api/plan/timeline');
    // Don't rebuild the cards while the native time picker is open (the 10 s poll would destroy
    // the input mid-interaction) or during a save's optimistic hold — the chart still updates.
    const active = document.activeElement;
    const pickerOpen = active && active.classList && active.classList.contains('ev-deadline');
    if (!pickerOpen && Date.now() >= evHoldUntil) {
      $('#ev-cards').innerHTML = evs.length
        ? evs.map((e) => evCard(e, tl)).join('')
        : '<section class="card"><div class="faint">No EV charger configured, or the plan is warming up.</div></section>';
      evs.forEach(wireEv);
    }
    // The chart gets the expanded (uniform 15-min) view (item F); `evCard`/`evWindow` above keep
    // the raw blocks, matching `e.charge_kw`'s own per-BLOCK (not per-fine-step) indexing.
    this.chart(evs, expandTimeline(tl));
  },
  chart(evs, tl) {
    const c = chart('ev-chart');
    if (!c) return;
    if (!evs.length) {
      // Returning early left the PREVIOUS poll's schedule on screen — a chart that looks current
      // while the feed is empty (warming up, or the charger dropped out). Clear it instead.
      c.clear();
      return;
    }
    const e = evs[0]; // the schedule chart shows the first charger
    // `tl` here is the EXPANDED view — `b._i` is its original block index, matching `e[key]`'s own
    // per-block (not per-expanded-point) indexing.
    const leg = (key, color, name) => ({
      name, type: 'line', stack: 'ev', symbol: 'none', smooth: false, step: 'end',
      areaStyle: { color: color + '88' }, lineStyle: { width: 0 },
      data: tl.map((b) => [b.t, (e[key] || [])[b._i] || 0]),
    });
    const series = [leg('solar_kw', css('--amber'), 'Solar'), leg('grid_kw', css('--blue'), 'Grid'), leg('batt_kw', css('--purple'), 'Battery')];
    // "now" divider + the resolved ready-by deadline (next local occurrence, if inside the plan).
    const marks = [{ xAxis: Date.now(), lineStyle: { color: css('--faint'), type: 'dashed' }, label: { show: false } }];
    // Use the SERVER-resolved instant (`deadline_at`): resolving `deadline_hm` here would use the
    // browser's timezone, marking the wrong moment for anyone viewing from away from the site.
    if (e.deadline_at && tl.length) {
      const at = new Date(e.deadline_at).getTime();
      if (isFinite(at) && at <= new Date(tl[tl.length - 1].t).getTime()) {
        marks.push({ xAxis: at, lineStyle: { color: css('--red'), type: 'dashed' }, label: { formatter: 'ready by', color: css('--red'), position: 'insideEndTop' } });
      }
    }
    // A dataless series carries the reference lines so toggling Solar in the legend can't hide
    // them (a markLine follows its host series' legend visibility).
    series.push({ name: 'marks', type: 'line', data: [], silent: true, tooltip: { show: false }, markLine: { silent: true, symbol: 'none', data: marks } });
    c.setOption(Object.assign(baseOption(), {
      tooltip: { trigger: 'axis', confine: true, valueFormatter: (v) => typeof v === 'number' ? `${v.toFixed(2)} kW` : v },
      color: [css('--amber'), css('--blue'), css('--purple')], // legend swatches match the source areas
      legend: { show: true, data: ['Solar', 'Grid', 'Battery'], top: 0, textStyle: { color: css('--muted') }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
      yAxis: [yAxis('kW')],
      series,
    }), true);
  },
};

// ---- HOUSE (thermal envelope) ----
// Static topology + the current selection/sort, kept across the 10s re-render so the graph isn't
// rebuilt and the inspected boundary survives a refresh.
const house = { topo: null, temps: {}, outside: null, ground: null, solar: {}, sun: null, comfort: {}, sel: null, sort: { key: 'ua', dir: -1 }, tableBuilt: false };

// Colour a zone by live air temperature (cold blue → warm red).
function tempColor(t) {
  if (t == null || !isFinite(t)) return css('--faint');
  const f = clamp((t - 16) / 10, 0, 1);
  const stops = [[59, 130, 246], [52, 211, 153], [251, 191, 36], [244, 99, 99]];
  const seg = f * (stops.length - 1), i = Math.min(Math.floor(seg), stops.length - 2), k = seg - i;
  const c = stops[i].map((a, j) => Math.round(a + (stops[i + 1][j] - a) * k));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}
const KIND = {
  exterior: { color: '#f59e0b', label: 'Exterior wall', dash: false },
  roof:     { color: '#60a5fa', label: 'Roof',          dash: true },
  ground:   { color: '#b07d3b', label: 'Ground / floor', dash: true },
  interior: { color: '#5b6b86', label: 'Interior',       dash: false },
};
const kindOf = (k) => KIND[k] || KIND.interior;
const COMPASS = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
const compassDir = (deg) => deg == null ? null : COMPASS[Math.round(((deg % 360) + 360) % 360 / 45) % 8];
const nice = (s) => esc(String(s).replace(/_/g, ' '));
// Layer fill by conductivity λ: insulation (low λ) bright, dense masonry (high λ) dark.
function layerColor(lambda) {
  const f = clamp(Math.log10(clamp(lambda, 0.02, 3) / 0.02) / Math.log10(150), 0, 1);
  const l = Math.round(70 - 42 * f); // lightness 70%→28%
  return `hsl(${Math.round(28 + 10 * (1 - f))} 45% ${l}%)`;
}
// Colour by insulation quality (U-value): well-insulated green → leaky red.
function uColor(u) {
  if (u == null || !isFinite(u)) return css('--faint');
  const f = clamp((u - 0.15) / (1.35), 0, 1); // 0.15 (passive-house) → 1.5 (poor); opens clamp to red
  const stops = [[52, 211, 153], [251, 191, 36], [244, 99, 99]]; // green → amber → red
  const seg = f * 2, i = Math.min(Math.floor(seg), 1), k = seg - i;
  const c = stops[i].map((a, j) => Math.round(a + (stops[i + 1][j] - a) * k));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}
const heatGrade = (u) => u == null ? '—' : u < 0.25 ? 'A' : u < 0.4 ? 'B' : u < 0.7 ? 'C' : u < 1.1 ? 'D' : 'E';

screens.house = {
  mount() {
    // Fresh DOM + disposed charts on every navigation — rebuild the table, keep no stale selection.
    house.tableBuilt = false; house.sel = null;
    return `
    <div class="grid cols-4" id="env-kpis"></div>
    <div class="grid cols-3" style="margin-top:18px">
      <section class="card span-2">
        <div class="card-head"><div class="card-title"><span class="ico">🕸️</span> Zone adjacency — the thermal model</div>
          <div class="legend" id="env-legend"></div></div>
        <div class="chart tall" id="env-graph"></div>
        <div class="card-sub">node size = air volume · colour = live temperature · edge = boundary (width ∝ area). Click a zone or edge to inspect.</div>
      </section>
      <section class="card">
        <div class="card-head"><div class="card-title"><span class="ico">🧱</span> Boundary detail</div></div>
        <div id="env-detail" class="faint">Pick a boundary in the graph or the table to see its construction, U-value and orientation.</div>
      </section>
    </div>
    <div class="grid cols-2" style="margin-top:18px">
      <section class="card">
        <div class="card-head"><div class="card-title"><span class="ico">📉</span> Where the house loses heat — now</div>
          <div class="card-sub">conductive loss = area × U × ΔT (exterior + ground)</div></div>
        <div class="chart" id="env-loss"></div>
      </section>
      <section class="card">
        <div class="card-head"><div class="card-title"><span class="ico">📋</span> Boundaries</div><div class="card-sub" id="env-count"></div></div>
        <div class="env-table-wrap"><table class="env-table" id="env-table"></table></div>
      </section>
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🌡️</span> Per-zone heat balance — now</div><div class="card-sub" id="env-sun"></div></div>
      <div class="env-zone-grid" id="env-zones"></div>
    </section>`;
  },
  update(store) {
    const topo = store['/api/model/topology']?.data;
    if (!topo) { $('#env-detail').textContent = 'Model topology unavailable.'; return; }
    const temps = {}; (store['/api/state']?.data?.zones || []).forEach((z) => { temps[z.zone] = z.temp_c; });
    house.topo = topo; house.temps = temps;
    house.outside = store['/api/live']?.data?.outside_temp_c ?? null;
    house.ground = topo.ground_temperature_c ?? null; // configured slab/ground boundary temperature
    // Keep each surface's mode too: opaque surfaces ABSORB at the outer face, glazing TRANSMITS
    // into the room — labelling both "absorbed" mislabelled every window (usually the bigger gain).
    house.solar = {}; house.solarBeam = {}; house.solarDiffuse = {}; house.solarMode = {};
    (store['/api/model/solar']?.data?.boundaries || []).forEach((b) => {
      house.solar[b.id] = b.solar_w; house.solarBeam[b.id] = b.beam_w; house.solarDiffuse[b.id] = b.diffuse_w; house.solarMode[b.id] = b.mode;
    });
    house.sun = store['/api/model/solar']?.data?.sun || null;
    house.comfort = {}; arrData(store, '/api/zones').forEach((z) => { house.comfort[z.zone] = z; });

    this.kpis();
    this.graph();
    this.loss();
    this.zoneBalance();
    if (!house.tableBuilt) { this.table(); house.tableBuilt = true; }
    $('#env-legend').innerHTML = Object.values(KIND).map((k) => `<span><i style="background:${k.color}33;border:1px solid ${k.color}"></i>${k.label}</span>`).join('');
    this.renderDetail();
  },
  // --- envelope = boundaries that touch outside/ground (the real heat-loss surfaces) ---
  envBoundaries() { return house.topo.boundaries.filter((b) => b.kind !== 'interior'); },
  zoneTemp(name) { return name === 'outside' ? house.outside : name === 'ground' ? house.ground : (house.temps[name] ?? null); },
  // ΔT for an envelope loss: the interior zone vs the outside/ground it faces (null for interior).
  lossDeltaT(b) {
    if (b.kind === 'interior') return null;
    const z = b.zone_a === 'outside' || b.zone_a === 'ground' ? b.zone_b : b.zone_a;
    const ti = this.zoneTemp(z), to = b.kind === 'ground' ? house.ground : house.outside;
    return (ti == null || to == null) ? null : ti - to;
  },
  lossW(b) { const dt = this.lossDeltaT(b); return dt == null ? null : Math.max(0, b.ua * dt); },
  // Surface-absorbed solar load now (W) — opaque exterior surfaces only; not direct room heat.
  solarW(b) { return house.solar[b.id] || 0; },
  solarBeamW(b) { return house.solarBeam[b.id] || 0; },
  solarDiffuseW(b) { return house.solarDiffuse[b.id] || 0; },
  // Signed conductive flow across an INTERIOR boundary now (W): + = zone_a → zone_b.
  interFlow(b) {
    if (b.kind !== 'interior') return null;
    const ta = this.zoneTemp(b.zone_a), tb = this.zoneTemp(b.zone_b);
    return (ta == null || tb == null) ? null : b.ua * (ta - tb);
  },
  kpis() {
    const env = this.envBoundaries();
    const area = env.reduce((s, b) => s + b.area_m2, 0);
    const ua = env.reduce((s, b) => s + b.ua, 0);
    const meanU = area > 0 ? ua / area : null;
    const worst = env.reduce((m, b) => (b.u_value > (m?.u_value ?? -1) ? b : m), null);
    // The dominant losses are to outside (walls/roof), so a figure is only complete with the live
    // outdoor temperature — otherwise we'd show a misleading ground-only partial as the whole.
    const haveLoss = house.outside != null && env.some((b) => this.lossW(b) != null);
    const liveLoss = env.map((b) => this.lossW(b)).filter((x) => x != null).reduce((s, x) => s + x, 0);
    const liveSolar = env.reduce((s, b) => s + this.solarW(b), 0);
    const kpi = (label, val, sub) => `<section class="card"><div class="kpi"><div class="kpi-label">${label}</div><div class="kpi-value">${val}</div><div class="kpi-sub">${sub}</div></div></section>`;
    $('#env-kpis').innerHTML = [
      kpi('Insulation grade', `<span style="color:${uColor(meanU)}">${heatGrade(meanU)}</span><span class="unit"> U ${fmt.n(meanU, 2)}</span>`, 'area-weighted envelope U'),
      kpi('Heat-loss coefficient', `${fmt.n(ua, 0)}<span class="unit">W/K</span>`, `${fmt.n(area, 0)} m² · ${env.length} surfaces`),
      kpi('Heat loss now', haveLoss ? `${fmt.n(liveLoss / 1000, 2)}<span class="unit">kW</span>` : '—', haveLoss ? (liveSolar > 1 ? `☀ ${fmt.n(liveSolar / 1000, 1)} kW sun on surfaces` : 'conductive, through the envelope') : 'waiting for outdoor temperature'),
      kpi('Leakiest surface', worst ? `<span style="color:${uColor(worst.u_value)}">${fmt.n(worst.u_value, 2)}</span><span class="unit">U</span>` : '—', worst ? nice(worst.type_name) : '—'),
    ].join('');
  },
  zoneBalance() {
    const el = $('#env-zones'); if (!el) return;
    if (house.sun) $('#env-sun').textContent = house.sun.up ? `☀ sun ${Math.round(house.sun.elevation_deg)}° up, ${compassDir(house.sun.azimuth_deg)}` : '🌙 sun below horizon';
    const interior = house.topo.zones.filter((z) => z.role === 'interior' && z.volume_m3);
    const cards = interior.map((z) => {
      const bs = house.topo.boundaries.filter((b) => (b.zone_a === z.name || b.zone_b === z.name) && b.kind !== 'interior');
      const ua = bs.reduce((s, b) => s + b.ua, 0);
      const loss = bs.map((b) => this.lossW(b)).filter((x) => x != null).reduce((s, x) => s + x, 0);
      const solarBeam = bs.reduce((s, b) => s + this.solarBeamW(b), 0);
      const solarDiffuse = bs.reduce((s, b) => s + this.solarDiffuseW(b), 0);
      const ti = house.temps[z.name];
      const cf = house.comfort[z.name];
      const cb = bandNow(cf);
      const boostHi = overheatCeiling(cf);
      // Same ±0.1 tolerances as comfort() so both screens flip state at the same temperature.
      const band = cf && ti != null
        ? (ti < cb.lo - 0.1 ? ['blue', 'cool']
          : ti <= cb.hi + 0.1 ? ['green', 'comfort']
          : boostHi != null && ti <= boostHi + 0.1 ? ['gold', 'banking']
          : ['red', 'warm'])
        : null;
      const dom = bs.slice().sort((a, b) => (this.lossW(b) || 0) - (this.lossW(a) || 0))[0];
      return `<div class="env-zone" data-z="${esc(z.name)}">
        <div class="env-zone-head"><span class="env-zone-name">${nice(z.name)}</span>${band ? `<span class="badge ${band[0]}">${band[1]}</span>` : ''}</div>
        <div class="env-zone-temp" style="color:${tempColor(ti)}">${fmt.temp(ti)}<span class="env-zone-band">${cf ? ` / ${fmt.n(cb.lo, 0)}–${fmt.n(cb.hi, 0)}°` : ''}</span></div>
        <div class="env-zone-row"><span>UA to outside</span><span>${fmt.n(ua, 1)} W/K</span></div>
        <div class="env-zone-row"><span>loss now</span><span style="color:${css('--red')}">${(ti == null || house.outside == null) ? '—' : `${fmt.n(loss, 0)} W`}</span></div>
        ${(solarBeam > 1 || solarDiffuse > 1) ? `<div class="env-zone-row"><span${solarBeam > 0.5 ? '' : ' class="sun-off"'}>☀ on surfaces</span><span>${fmt.n(solarBeam, 0)} W${solarDiffuse > 1 ? `<span class="muted"> · ${fmt.n(solarDiffuse, 0)} W diffuse</span>` : ''}</span></div>` : ''}
        ${dom ? `<div class="env-zone-dom faint">biggest path: ${nice(dom.zone_a === z.name ? dom.zone_b : dom.zone_a)} · ${esc(dom.kind)} · ${fmt.n(dom.ua, 1)} W/K</div>` : ''}
      </div>`;
    });
    el.innerHTML = cards.join('') || '<div class="faint">No heated zones with boundaries.</div>';
    el.querySelectorAll('.env-zone').forEach((c) => { c.onclick = () => { house.sel = { zone: c.dataset.z }; screens.house.renderDetail(); }; });
  },
  graph() {
    const c = chart('env-graph'); if (!c) return;
    const t = house.topo;
    const vols = t.zones.map((z) => z.volume_m3 || 0);
    const vmax = Math.max(1, ...vols);
    const nodes = t.zones.map((z) => ({
      name: z.name, value: z.volume_m3,
      symbolSize: z.role === 'interior' ? 12 + 34 * Math.sqrt((z.volume_m3 || 0) / vmax) : 40,
      symbol: z.role === 'interior' ? 'circle' : 'roundRect',
      itemStyle: { color: z.role === 'interior' ? tempColor(house.temps[z.name]) : (z.role === 'ground' ? '#6b5436' : '#27406a'), borderColor: css('--border'), borderWidth: 1 },
      label: { show: true, color: css('--text'), fontSize: 10, formatter: nice(z.name) },
    }));
    // Aggregate boundaries into one edge per zone-pair (sum area; the most-exterior kind dominates).
    const rank = { exterior: 3, roof: 3, ground: 2, interior: 1 };
    const agg = new Map();
    for (const b of t.boundaries) {
      const key = [b.zone_a, b.zone_b].sort().join(' ');
      const e = agg.get(key) || { source: b.zone_a, target: b.zone_b, area: 0, kind: 'interior', ids: [] };
      e.area += b.area_m2; e.ids.push(b.id);
      if (rank[b.kind] > rank[e.kind]) e.kind = b.kind;
      agg.set(key, e);
    }
    const amax = Math.max(1, ...[...agg.values()].map((e) => e.area));
    const links = [...agg.values()].map((e) => {
      const k = kindOf(e.kind);
      return {
        source: e.source, target: e.target, ids: e.ids,
        lineStyle: { color: k.color, width: 1 + 6 * Math.sqrt(e.area / amax), opacity: e.kind === 'interior' ? 0.35 : 0.85, type: k.dash ? 'dashed' : 'solid', curveness: 0.12 },
      };
    });
    // "Built" is tied to the live ECharts instance (a theme toggle disposes it without re-mounting),
    // so check the instance for the graph series rather than a module flag that would outlive it.
    const opt = c.getOption();
    const built = opt && Array.isArray(opt.series) && opt.series.some((s) => s.type === 'graph');
    if (!built) {
      c.setOption({
        tooltip: {
          trigger: 'item', confine: true, backgroundColor: css('--surface-2'), borderColor: css('--border'), textStyle: { color: css('--text') },
          formatter: (p) => p.dataType === 'edge'
            ? `${nice(p.data.source)} ↔ ${nice(p.data.target)}<br>${p.data.ids.length} boundary(ies)`
            : `${nice(p.name)}${p.value ? `<br>${fmt.n(p.value, 0)} m³ · ${fmt.temp(house.temps[p.name])}` : ''}`,
        },
        series: [{
          type: 'graph', layout: 'force', roam: true, draggable: true,
          force: { repulsion: 220, edgeLength: [40, 140], gravity: 0.12 },
          emphasis: { focus: 'adjacency', lineStyle: { width: 6 } },
          data: nodes, links, lineStyle: { color: 'source' },
        }],
      }, true);
      c.off('click');
      c.on('click', (p) => {
        if (p.dataType === 'edge') { const id = p.data.ids.slice().sort((a, b) => house.topo.boundaries[b].area_m2 - house.topo.boundaries[a].area_m2)[0]; house.sel = id; screens.house.renderDetail(); }
        else if (p.dataType === 'node') { house.sel = { zone: p.name }; screens.house.renderDetail(); }
      });
    } else {
      // recolour nodes live (positions kept; merge, not replace)
      c.setOption({ series: [{ data: nodes }] });
    }
  },
  loss() {
    const c = chart('env-loss'); if (!c) return;
    // Need the live outdoor temperature for a complete picture (walls/roof dominate); without it,
    // show the waiting state rather than a misleading ground-only ranking.
    const rows = house.outside == null ? []
      : this.envBoundaries().map((b) => ({ b, w: this.lossW(b) })).filter((r) => r.w != null)
        .sort((a, z) => z.w - a.w).slice(0, 10).reverse();
    const labels = rows.map((r) => `${nice(r.b.zone_a === 'outside' || r.b.zone_a === 'ground' ? r.b.zone_b : r.b.zone_a)} · ${esc(r.b.kind)}`);
    c.setOption(Object.assign(baseOption(), {
      grid: { left: 8, right: 40, top: 10, bottom: 20, containLabel: true },
      tooltip: { trigger: 'axis', confine: true, valueFormatter: (v) => `${Math.round(v)} W` },
      xAxis: { type: 'value', name: 'W', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
      yAxis: { type: 'category', data: labels, axisLabel: { color: css('--muted'), fontSize: 10 } },
      series: [{ type: 'bar', data: rows.map((r) => Math.round(r.w)), itemStyle: { color: css('--red'), borderRadius: [0, 4, 4, 0] }, barWidth: '62%', label: { show: true, position: 'right', color: css('--muted'), formatter: (p) => `${p.value} W` } }],
    }), true);
    if (!rows.length) c.setOption({ graphic: { type: 'text', left: 'center', top: 'middle', style: { text: 'waiting for live indoor/outdoor temperatures…', fill: css('--faint') } } });
  },
  table() {
    const head = [['type_name', 'Boundary'], ['kind', 'Kind'], ['area_m2', 'Area'], ['u_value', 'U'], ['azimuth_deg', 'Facing'], ['ua', 'UA']];
    const render = () => {
      const { key, dir } = house.sort;
      const rows = house.topo.boundaries.slice().sort((a, b) => {
        const va = a[key], vb = b[key];
        if (va == null) return 1; if (vb == null) return -1;
        return (va > vb ? 1 : va < vb ? -1 : 0) * dir;
      });
      const th = head.map(([k, lbl]) => `<th data-k="${k}" class="${house.sort.key === k ? 'sorted ' + (dir < 0 ? 'desc' : 'asc') : ''}">${lbl}</th>`).join('');
      const tr = rows.map((b) => `<tr data-id="${b.id}" class="${house.sel === b.id ? 'sel' : ''}">
        <td>${nice(b.zone_a)} ↔ ${nice(b.zone_b)}<div class="faint" style="font-size:.72rem">${nice(b.type_name)}</div></td>
        <td><span class="env-pill" style="background:${kindOf(b.kind).color}22;color:${kindOf(b.kind).color}">${esc(b.kind)}</span></td>
        <td class="num">${fmt.n(b.area_m2, 1)}</td>
        <td class="num" style="color:${uColor(b.u_value)};font-weight:700">${fmt.n(b.u_value, 2)}</td>
        <td class="num">${b.azimuth_deg == null ? '—' : compassDir(b.azimuth_deg)}</td>
        <td class="num">${fmt.n(b.ua, 1)}</td></tr>`).join('');
      $('#env-table').innerHTML = `<thead><tr>${th}</tr></thead><tbody>${tr}</tbody>`;
      $('#env-count').textContent = `${rows.length} boundaries`;
      $('#env-table').querySelectorAll('th').forEach((el) => el.onclick = () => {
        const k = el.dataset.k; house.sort = { key: k, dir: house.sort.key === k ? -house.sort.dir : -1 }; render();
      });
      $('#env-table').querySelectorAll('tbody tr').forEach((el) => el.onclick = () => { house.sel = +el.dataset.id; render(); screens.house.renderDetail(); });
    };
    house._renderTable = render; render();
  },
  renderDetail() {
    const el = $('#env-detail'); if (!el) return;
    const sel = house.sel;
    if (sel == null) return;
    if (typeof sel === 'object' && sel.zone) { if (house._renderTable) house._renderTable(); el.innerHTML = this.zoneDetail(sel.zone); return; }
    const b = house.topo.boundaries[sel]; if (!b) return;
    if (house._renderTable) house._renderTable(); // keep the table's selected-row highlight in sync
    el.innerHTML = this.boundaryDetail(b);
  },
  zoneDetail(name) {
    const z = house.topo.zones.find((x) => x.name === name) || {};
    const bs = house.topo.boundaries.filter((b) => b.zone_a === name || b.zone_b === name).sort((a, b) => b.area_m2 - a.area_m2);
    const t = house.temps[name];
    const rows = bs.map((b) => `<div class="stat-row"><span class="k">${nice(b.zone_a === name ? b.zone_b : b.zone_a)} <span class="env-pill" style="background:${kindOf(b.kind).color}22;color:${kindOf(b.kind).color}">${esc(b.kind)}</span></span><span class="v">${fmt.n(b.area_m2, 1)} m² · U ${fmt.n(b.u_value, 2)}</span></div>`).join('');
    return `<div class="env-d-head"><span class="env-d-ico" style="color:${tempColor(t)}">${z.role === 'interior' ? '🏠' : z.role === 'ground' ? '⛰️' : '🌳'}</span><div><div class="env-d-title">${nice(name)}</div><div class="faint">${z.volume_m3 ? fmt.n(z.volume_m3, 0) + ' m³' : 'infinite reservoir'} · now ${fmt.temp(t)}</div></div></div>
      <div class="env-d-sub">${bs.length} boundaries</div>${rows}`;
  },
  boundaryDetail(b) {
    const compass = b.azimuth_deg == null ? '' : (() => {
      const a = (b.azimuth_deg % 360) * Math.PI / 180, x = 19 + 15 * Math.sin(a), y = 19 - 15 * Math.cos(a);
      return `<svg width="40" height="40" viewBox="0 0 38 38" class="env-compass"><circle cx="19" cy="19" r="17" fill="none" stroke="${css('--border')}"/><line x1="19" y1="19" x2="${x.toFixed(1)}" y2="${y.toFixed(1)}" stroke="${css('--amber')}" stroke-width="2"/><circle cx="19" cy="19" r="2" fill="${css('--amber')}"/></svg>`;
    })();
    const interior = b.kind === 'interior';
    const flow = this.interFlow(b);
    const facts = [
      ['Between', `${nice(b.zone_a)} ↔ ${nice(b.zone_b)}`],
      ['Area', `${fmt.n(b.area_m2, 2)} m²`],
      ['U-value', `<span style="color:${uColor(b.u_value)};font-weight:700">${fmt.n(b.u_value, 3)}</span> W/m²K · grade ${heatGrade(b.u_value)}`],
      ['R-value', `${fmt.n(b.r_value, 2)} m²K/W`],
      !interior && this.lossW(b) != null ? ['Heat loss now', `${Math.round(this.lossW(b))} W (ΔT ${fmt.n(this.lossDeltaT(b), 1)} K)`] : null,
      !interior && this.solarW(b) > 0.5 ? ['Solar load now', `${solarSplitText(this.solarBeamW(b), this.solarDiffuseW(b))} ${house.solarMode[b.id] === 'transmitted' ? 'transmitted into the room' : 'absorbed on the surface'}`] : null,
      interior && flow != null ? ['Flow between zones', `<span style="color:${css('--amber')}">${nice(flow >= 0 ? b.zone_a : b.zone_b)} → ${nice(flow >= 0 ? b.zone_b : b.zone_a)} · ${Math.round(Math.abs(flow))} W</span>`] : null,
      b.azimuth_deg != null ? ['Facing', `${Math.round(b.azimuth_deg)}° ${compassDir(b.azimuth_deg)}${b.tilt_deg != null ? ` · tilt ${Math.round(b.tilt_deg)}°` : ''}`] : null,
      b.solar_absorptance != null ? ['Solar absorptance', fmt.n(b.solar_absorptance, 2)] : null,
    ].filter(Boolean);
    return `<div class="env-d-head"><span class="env-d-ico" style="color:${kindOf(b.kind).color}">🧱</span>
        <div><div class="env-d-title">${nice(b.type_name)}</div><div class="faint">${esc(b.kind)} boundary</div></div>${compass}</div>
      ${this.crossSection(b)}
      <div class="env-d-facts">${facts.map(([k, v]) => `<div class="stat-row"><span class="k">${k}</span><span class="v">${v}</span></div>`).join('')}</div>`;
  },
  crossSection(b) {
    if (!b.layers || !b.layers.length) {
      return `<div class="env-xsec-simple faint">Simple boundary — modelled as a single U/g pane (no layered construction)${b.u_value > 10 ? '; the high U marks an open/virtual connection between zones.' : '.'}</div>`;
    }
    // Layers are stored zones[0]→zones[1]. Orient them so the OUTER (outside/ground) side is on the
    // left and the inner (room) side on the right — reversing when the exterior zone is zones[1] — so
    // the end labels and the underfloor-heating 🔥 marker land on the physically correct end (floors,
    // roofs and inter-floor slabs are authored room-side-first, the opposite of exterior walls).
    const isExt = (z) => z === 'outside' || z === 'ground';
    const reverse = isExt(b.zone_b) && !isExt(b.zone_a);
    const layers = reverse ? b.layers.slice().reverse() : b.layers;
    const outerName = reverse ? b.zone_b : b.zone_a;
    const innerName = reverse ? b.zone_a : b.zone_b;
    const tot = layers.reduce((s, l) => s + Math.max(l.thickness_mm, 4), 0);
    const segs = layers.map((l) => {
      const w = (Math.max(l.thickness_mm, 4) / tot * 100).toFixed(2);
      const mk = l.marker ? ` env-marker` : '';
      const fire = l.marker === 'heating' ? '<span class="env-fire">🔥</span>' : '';
      return `<div class="env-layer${mk}" style="width:${w}%;background:${layerColor(l.conductivity)}" title="${nice(l.material)} — ${fmt.n(l.thickness_mm, 0)} mm, λ ${fmt.n(l.conductivity, 3)}">${fire}<span class="env-layer-lbl">${fmt.n(l.thickness_mm, 0)}</span></div>`;
    }).join('');
    const legend = layers.map((l) => `<div class="env-leg-row"><i style="background:${layerColor(l.conductivity)}"></i>${nice(l.material)}<span class="faint"> · ${fmt.n(l.thickness_mm, 0)} mm · λ ${fmt.n(l.conductivity, 3)}${l.marker ? ` · ${nice(l.marker)}` : ''}</span></div>`).join('');
    return `<div class="env-xsec"><div class="env-xsec-bars">${segs}</div><div class="env-xsec-ends"><span>${nice(outerName)}</span><span>${nice(innerName)}</span></div></div><div class="env-xsec-legend">${legend}</div>`;
  },
};

// ============================================================ shell + loop
let current = null;
let timer = null;
const POLL_MS = 10000;
const store = {};
let refreshSeq = 0; // overlap guard: only the newest in-flight refresh may commit its result

async function refresh() {
  const r = current; if (!r) return;
  if (document.hidden) return; // a hidden tab/panel polls nothing; syncPolling() refreshes on return
  const seq = ++refreshSeq;
  // Re-fetch the screen's own endpoints, plus /readyz for the status dot.
  const paths = [...new Set([...r.ep, '/readyz'])];
  const res = await loadAll(paths);
  if (r !== current) return; // navigated away mid-fetch — don't render against the new screen's DOM
  // A newer refresh already started (10 s poll overlapping a slow fetch, or the EV handlers'
  // 400 ms nudge): committing this older snapshot would move the UI backwards in time.
  if (seq !== refreshSeq) return;
  Object.assign(store, res);
  // Overwrite whenever the probe ANSWERED (200 or 503) — keeping the previous value on a not-ready
  // answer is what froze the dot green. `null` marks an unreachable server, which updateStatus()
  // renders as "connecting…" rather than as the last known good state.
  // The probe ANSWERED → use its answer. It failed to answer (fetch threw, or a body we could not
  // read) → `null`, which updateStatus() renders as "connecting…". Only a completely absent entry
  // keeps the previous value.
  const rz = res['/readyz'];
  window.__ready = rz ? (rz.data !== undefined ? rz.data : null) : window.__ready;
  updateStatus();
  // Screens early-return when their endpoint fails, which leaves the LAST render on screen — right
  // for a blip, but with nothing marking it the page silently showed an old plan as current for as
  // long as the outage lasted. Mark the whole view stale whenever any of this screen's endpoints
  // answered without data; the CSS shows a banner and dims the content.
  const anyStale = r.ep.some((p2) => res[p2] && res[p2].data === undefined);
  document.getElementById('view').classList.toggle('stale-data', anyStale);
  try { await screens[r.id].update(store); } catch (e) { console.error('render', e); }
}

function updateStatus() {
  const ready = window.__ready;
  const s = $('#status');
  if (!ready) { s.className = 'status'; $('#status-text').textContent = 'connecting…'; return; }
  if (ready.ready) { s.className = 'status ok'; $('#status-text').textContent = 'live'; }
  else { s.className = 'status bad'; $('#status-text').textContent = ready.plan_available ? 'stale' : 'warming up'; }
}

function go(id) {
  const r = routeById(id); current = r;
  updateNav();
  disposeCharts();
  if (window.fp3d) window.fp3d.dispose();
  const view = $('#view'); view.innerHTML = ''; view.appendChild(h(`<div>${screens[r.id].mount()}</div>`));
  refresh();
}
function updateNav() {
  // config-driven sections (e.g. EV) appear only when their capability is present.
  const visible = ROUTES.filter((r) => !r.cap || window.__caps?.[r.cap]);
  $('#nav').innerHTML = visible.map((r) => `<a href="#/${r.id}" class="${current && current.id === r.id ? 'active' : ''}">${r.name}</a>`).join('');
}

function tickClock() { $('#clock').textContent = new Date().toLocaleTimeString([], { hour12: false }); }

// Poll every POLL_MS while the page is visible; stop while hidden, and on return refresh at once and
// resume. The stale banner is derived from each refresh's own results, so resuming cannot show a
// false "stale" (nothing ages while paused).
function syncPolling(refreshNow = true) {
  if (document.hidden) { clearInterval(timer); timer = null; return; }
  if (timer !== null) return;
  if (refreshNow) refresh();
  timer = setInterval(refresh, POLL_MS);
}

async function init() {
  // theme toggle (persisted)
  const saved = localStorage.getItem('mpc-theme');
  if (saved) document.documentElement.setAttribute('data-theme', saved);
  $('#theme-toggle').onclick = () => {
    const cur = document.documentElement.getAttribute('data-theme') === 'light' ? 'dark' : 'light';
    document.documentElement.setAttribute('data-theme', cur); localStorage.setItem('mpc-theme', cur);
    disposeCharts(); refresh();
  };
  // capabilities decide which config-driven nav entries (EV) are shown.
  window.__caps = (await api('/api/capabilities')).data || {};
  window.addEventListener('hashchange', () => go((location.hash.slice(2) || 'home')));
  updateNav();
  go(location.hash.slice(2) || 'home');
  tickClock(); setInterval(tickClock, 1000);
  document.addEventListener('visibilitychange', () => syncPolling());
  syncPolling(false); // go() above already refreshed
  $('#footer').innerHTML = `MPC Home Control — read-only dashboard · the controllers actuate the house · data via the <a class="link" href="${mpcUrl('/api')}">JSON API</a>`;
}
// A host that injects this script after the page has loaded (HA builds the document at runtime) would
// miss DOMContentLoaded, so start at once when the DOM is already parsed.
if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
else init();
