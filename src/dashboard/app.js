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
const modeLabel = (slot) => MODE[slot]?.label || slot || '—'; // raw string for an unmapped loxone mode
const modeLegend = () => Object.values(MODE).map((m) => `<span><i style="background:${m.color}33;border:1px solid ${m.color}"></i>${m.label}</span>`).join('');

// ---------- API ----------
async function api(path) {
  try {
    const r = await fetch(path, { cache: 'no-store' });
    if (r.status === 503) return { ok: false, warming: true };
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
// The dashboard's one writable call: set an EV preference (the MPC persists it to its own file).
async function apiPost(path, body) {
  try {
    const r = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    return r.ok;
  } catch (e) { console.error('post', e); return false; }
}

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

// A vertical "now" divider separating measured history (solid) from forecast (dashed) on a time axis.
const nowMark = () => ({ silent: true, symbol: 'none', label: { show: false }, lineStyle: { color: css('--faint'), type: 'dashed', width: 1 }, data: [{ xAxis: Date.now() }] });
// Measured history series ([[iso, value]]) from /api/history; [] when the endpoint has no data yet.
const histData = (store, key) => store['/api/history']?.data?.[key] || [];

// build markArea bands for consecutive same-slot blocks (for mode shading)
function modeBands(tl) {
  const bands = []; let start = 0;
  for (let i = 1; i <= tl.length; i++) {
    if (i === tl.length || tl[i].slot !== tl[start].slot) {
      const c = modeOf(tl[start].slot).color;
      bands.push([{ xAxis: tl[start].t, itemStyle: { color: c + '14' } }, { xAxis: tl[i - 1].t }]);
      start = i;
    }
  }
  return bands;
}

// Tooltip for the plan/energy charts: rounded values with units, the block's battery mode, and
// confine:true so it can't slide off-screen on mobile. `tl` supplies the per-block mode by time.
function planTooltip(tl) {
  const blocks = tl.map((b) => [new Date(b.t).getTime(), b.slot]);
  const modeAt = (t) => {
    let slot = null, best = 9e5; // only label a mode within ~15 min (one block) of the hovered point
    for (const [bt, s] of blocks) { const d = Math.abs(bt - t); if (d < best) { best = d; slot = s; } }
    return slot;
  };
  const unit = (n) => /price/i.test(n) ? ' Kč/kWh' : /soc/i.test(n) ? ' kWh' : ' kW';
  return {
    trigger: 'axis', confine: true,
    backgroundColor: css('--surface-2'), borderColor: css('--border'), textStyle: { color: css('--text') },
    axisPointer: { lineStyle: { color: css('--border') } },
    formatter: (ps) => {
      if (!ps || !ps.length) return '';
      const seen = new Set();
      const rows = ps
        .filter((p) => Array.isArray(p.value) && p.value[1] != null && isFinite(p.value[1]))
        .filter((p) => !seen.has(p.seriesName) && seen.add(p.seriesName)) // measured + forecast share a name
        .map((p) => `${p.marker}${esc(p.seriesName)} <b>${Math.abs(p.value[1]).toFixed(2)}${unit(p.seriesName)}</b>`);
      const t = ps[0].axisValue;
      const when = Number.isFinite(t) ? fmt.hm(new Date(t).toISOString()) : (ps[0].axisValueLabel || '');
      const m = Number.isFinite(t) ? modeAt(t) : null;
      const head = `${when}${m ? ` · <b>${esc(modeLabel(m))}</b>` : ''}`;
      return `<div style="margin-bottom:3px">${head}</div>${rows.join('<br>')}`;
    },
  };
}

// ---------- price helpers ----------
function priceStats(tl) {
  const ps = tl.map((b) => b.import_price).filter((x) => isFinite(x));
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
// derived CZK rate (plan cost ratio) with a sane fallback
function czkRate(plan) {
  const e = plan?.total_cost_eur, c = plan?.total_cost_czk;
  return (e && c && Math.abs(e) > 0.01) ? c / e : 25;
}

function comfort(temp, z) {
  if (temp == null || !z) return { label: '', cls: '' };
  if (temp < z.t_min - 0.1) return { label: 'cold', cls: 'red' };
  if (temp > z.t_max + 0.1) return { label: 'warm', cls: 'amber' };
  return { label: 'comfortable', cls: 'green' };
}

// Tiny inline-SVG sparkline of a measured [[iso, °C]] series with the comfort band shaded. Returns
// '' when there's too little data to draw a line.
function sparkline(series, tmin, tmax, w = 144, h = 34) {
  // Keep only finite samples so a stray NaN/Infinity can never produce NaN SVG coordinates.
  const data = (series || []).filter((p) => Array.isArray(p) && Number.isFinite(p[1]));
  if (data.length < 2) return '';
  const vals = data.map((p) => p[1]);
  let lo = Math.min(...vals), hi = Math.max(...vals);
  if (tmin != null) lo = Math.min(lo, tmin);
  if (tmax != null) hi = Math.max(hi, tmax);
  if (hi - lo < 0.5) { hi += 0.5; lo -= 0.5; } // keep a near-flat series from squashing to a bar
  const pad = 2;
  const px = (i) => pad + (i / (data.length - 1)) * (w - 2 * pad);
  const py = (v) => pad + (1 - (v - lo) / (hi - lo)) * (h - 2 * pad);
  const pts = data.map((p, i) => `${px(i).toFixed(1)},${py(p[1]).toFixed(1)}`).join(' ');
  let band = '';
  if (tmin != null && tmax != null) {
    const yTop = py(tmax), bandH = py(tmin) - py(tmax);
    band = `<rect x="0" y="${yTop.toFixed(1)}" width="${w}" height="${Math.max(0, bandH).toFixed(1)}" fill="var(--green)" opacity="0.13"/>`;
  }
  const lx = px(data.length - 1), ly = py(data[data.length - 1][1]);
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true">${band}<polyline points="${pts}" fill="none" stroke="var(--accent)" stroke-width="1.5" vector-effect="non-scaling-stroke"/><circle cx="${lx.toFixed(1)}" cy="${ly.toFixed(1)}" r="2" fill="var(--accent)"/></svg>`;
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
  const heatingZones = Object.entries(fs.heat_kw || {}).filter(([, kw]) => kw > 0.05).map(([z]) => z);
  if (heatingZones.length) {
    const cheap = st && b.import_price <= st.lo;
    reasons.push(`Heating ${heatingZones.map((z) => z.replace(/_/g, ' ')).join(', ')}${cheap ? ' on cheap power — pre-warming the slab before prices rise.' : ' to hold the comfort band.'}`);
  } else {
    reasons.push('No heating needed — all rooms are coasting within their comfort band.');
  }

  // upcoming cheapest / most expensive window
  if (st) {
    const future = tl.slice(i);
    const cheapest = future.reduce((a, x) => (x.import_price < a.import_price ? x : a), future[0] || b);
    if (cheapest && cheapest.t !== b.t) reasons.push(`Cheapest power coming up around ${fmt.hm(cheapest.t)} (${fmt.czk(cheapest.import_price * rate, 2)}/kWh).`);
  }

  // Escape the interpolated values; keep the <strong> scaffold literal.
  const headline = `${esc(m.label)} — ${esc(m.desc)}. Power is <strong>${esc(lvl.label.toLowerCase())}</strong> right now.`;
  return { headline, reasons, level: lvl, rate };
}

// ---------- routes ----------
const ROUTES = [
  { id: 'home',    name: 'Home',     ep: ['/api/live', '/api/plan/latest', '/api/state', '/api/zones', '/api/zones/series', '/api/history'] },
  { id: 'energy',  name: 'Energy',   ep: ['/api/plan/latest', '/api/live', '/api/history'] },
  { id: 'ev',      name: 'EV',       ep: ['/api/ev', '/api/plan/timeline'], cap: 'has_ev' },
  { id: 'heating', name: 'Heating',  ep: ['/api/plan/latest', '/api/state', '/api/zones'] },
  { id: 'house',   name: 'House',    ep: ['/api/model/topology', '/api/model/solar', '/api/state', '/api/zones', '/api/live'] },
  { id: 'model',   name: 'Model',    ep: ['/api/calibration/gains', '/api/forecast/validation'] },
  { id: 'system',  name: 'System',   ep: ['/api/version', '/api/plan/latest'] },
];
const routeById = (id) => ROUTES.find((r) => r.id === id) || ROUTES[0];

// ============================================================ SCREENS
const screens = {};

// ---- HOME ----
screens.home = {
  mount() {
    return `
    <div class="grid cols-3">
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
      <section class="card"><div class="kpi"><div class="kpi-label">Today's projected cost</div><div class="kpi-value" id="cost-today">—</div><div class="kpi-sub" id="cost-sub"></div></div></section>
    </div>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🧠</span> What the system is doing &amp; why</div></div>
      <div class="insight" id="headline">…</div>
      <ul class="reasons" id="reasons" style="margin-top:12px"></ul>
    </section>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">📈</span> The day — price, PV &amp; SoC (history → forecast)</div>
        <div class="legend" id="day-legend"></div></div>
      <div class="chart" id="home-chart"></div>
    </section>

    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🌡️</span> Indoor comfort</div></div>
      <div class="zone-grid" id="zone-grid"></div>
    </section>`;
  },
  update(store) {
    const live = store['/api/live']?.data;
    const plan = store['/api/plan/latest']?.data;
    const zones = store['/api/zones']?.data || [];
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
      }
      $('#outside').innerHTML = live.outside_temp_c != null ? `${fmt.temp(live.outside_temp_c)}<span class="unit">C</span>` : '—';
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

      // day chart
      this.dayChart(tl, rate, store);
    }

    // comfort grid
    const zmap = Object.fromEntries(zones.map((z) => [z.zone, z]));
    const smap = Object.fromEntries(state.map((s) => [s.zone, s.temp_c]));
    const sermap = Object.fromEntries((store['/api/zones/series']?.data || []).map((s) => [s.zone, s.series]));
    const fs = plan?.first_step || {};
    const heated = zones.length ? zones : Object.keys(smap).map((z) => ({ zone: z }));
    $('#zone-grid').innerHTML = heated.map((z) => {
      const t = smap[z.zone]; const c = comfort(t, zmap[z.zone]);
      const heating = (fs.heat_kw?.[z.zone] || 0) > 0.05;
      const band = zmap[z.zone] ? `${zmap[z.zone].t_min}–${zmap[z.zone].t_max}°` : '';
      const spark = sparkline(sermap[z.zone], zmap[z.zone]?.t_min, zmap[z.zone]?.t_max);
      return `<div class="zone ${heating ? 'heating' : ''}">
        <div class="zname"><span>${esc(z.zone.replace(/_/g, ' '))}</span>${heating ? '<span class="heat-dot">🔥</span>' : (c.cls ? `<span class="chip ${c.cls}" style="padding:1px 7px">${c.label}</span>` : '')}</div>
        <div class="ztemp">${fmt.temp(t)}</div>
        ${spark}
        <div class="faint" style="font-size:0.72rem">comfort ${band}${spark ? ' · 24 h' : ''}</div>
      </div>`;
    }).join('');

    const okZones = heated.filter((z) => comfort(smap[z.zone], zmap[z.zone]).cls === 'green').length;
    $('#comfort-sub').textContent = `${okZones}/${heated.length} rooms comfortable`;
  },
  dayChart(tl, rate, store) {
    const c = chart('home-chart'); if (!c) return;
    const pv = css('--yellow'), soc = css('--amber'), price = css('--blue'), house = css('--red');
    const priceData = tl.map((b) => [b.t, b.import_price * rate]);
    $('#day-legend').innerHTML = modeLegend();
    c?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [price, pv, house, soc], // ONE entry per UNIQUE series name (first-appearance order) — ECharts colours legend items by unique name, so duplicate hist/forecast entries here would shift the swatches off the lines
      legend: { show: true, data: ['PV', 'House', 'SoC', 'Price'], top: 0, textStyle: { color: css('--muted') }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
      grid: { left: 50, right: 56, top: 30, bottom: 30, containLabel: true },
      yAxis: [yAxis('Kč/kWh', { position: 'right' }), yAxis('kW · kWh', { position: 'left' })],
      series: [
        { name: 'Price', type: 'line', step: 'end', yAxisIndex: 0, data: priceData, smooth: false, symbol: 'none', lineStyle: { color: price, width: 2 }, areaStyle: { color: grad(price) },
          markArea: { silent: true, data: modeBands(tl) }, markLine: nowMark() },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: histData(store, 'pv_kw'), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 2 }, areaStyle: { color: grad(pv) } },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.pv_kw]), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 1.5, type: 'dashed' } },
        // House consumption: measured (solid) vs the model's forecast (dashed) — same INVPowerToLocalLoad quantity.
        { name: 'House', type: 'line', yAxisIndex: 1, data: histData(store, 'house_kw'), smooth: true, symbol: 'none', lineStyle: { color: house, width: 2 } },
        { name: 'House', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.load_kw]), smooth: true, symbol: 'none', lineStyle: { color: house, width: 1.5, type: 'dashed' } },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: histData(store, 'soc_kwh'), smooth: true, symbol: 'none', lineStyle: { color: soc, width: 2 } },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.soc_kwh]), smooth: true, symbol: 'none', lineStyle: { color: soc, width: 1.5, type: 'dashed' } },
      ],
    }), true);
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
    const k = [`${fmt.czk(plan.total_cost_czk)}`, `${fmt.kw(plan.grid_import_kwh, 1)} kWh`, `${fmt.kw(plan.grid_export_kwh, 1)} kWh`, `${fmt.kw(plan.pv_curtailed_kwh, 1)} kWh`];
    const ks = [`${fmt.eur(plan.total_cost_eur)} · wear ${fmt.czk(plan.battery_wear_czk)}`, '', '', `final SoC ${fmt.kw(plan.final_soc_kwh, 1)} kWh`];
    k.forEach((v, i) => { $(`#ek-${i}`).textContent = v; $(`#eks-${i}`).textContent = ks[i]; });

    $('#e-legend').innerHTML = modeLegend();

    chart('e-price')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [css('--yellow'), css('--yellow'), css('--blue'), css('--blue')], // legend swatches match the lines
      yAxis: [yAxis('kW'), yAxis('Kč/kWh', { position: 'right', splitLine: { show: false } })],
      series: [
        { name: 'PV', type: 'line', data: histData(store, 'pv_kw'), smooth: true, symbol: 'none', lineStyle: { color: css('--yellow'), width: 2 }, areaStyle: { color: grad(css('--yellow')) }, markArea: { silent: true, data: modeBands(tl) }, markLine: nowMark() },
        { name: 'PV', type: 'line', data: tl.map((b) => [b.t, b.pv_kw]), smooth: true, symbol: 'none', lineStyle: { color: css('--yellow'), width: 1.5, type: 'dashed' } },
        { name: 'Import price', type: 'line', step: 'end', yAxisIndex: 1, data: tl.map((b) => [b.t, b.import_price * rate]), symbol: 'none', lineStyle: { color: css('--blue'), width: 2 } },
        { name: 'Export price', type: 'line', step: 'end', yAxisIndex: 1, data: tl.map((b) => [b.t, b.export_price * rate]), symbol: 'none', lineStyle: { color: css('--blue'), width: 1, type: 'dashed' } },
      ],
    }), true);

    chart('e-batt')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [css('--purple'), css('--gold'), css('--amber'), css('--amber')], // legend swatches match the series
      yAxis: [yAxis('kW'), yAxis('SoC kWh', { position: 'right', splitLine: { show: false } })],
      series: [
        { name: 'Charge', type: 'bar', stack: 'b', data: tl.map((b) => [b.t, b.charge_kw]), itemStyle: { color: css('--purple') } },
        { name: 'Discharge', type: 'bar', stack: 'b', data: tl.map((b) => [b.t, -b.discharge_kw]), itemStyle: { color: css('--gold') } },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: histData(store, 'soc_kwh'), smooth: true, symbol: 'none', lineStyle: { color: css('--amber'), width: 2 }, markLine: nowMark() },
        { name: 'SoC', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.soc_kwh]), smooth: true, symbol: 'none', lineStyle: { color: css('--amber'), width: 1.5, type: 'dashed' } },
      ],
    }), true);

    chart('e-grid')?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [css('--red'), css('--green'), css('--faint')], // legend swatches match the series
      yAxis: [yAxis('kW')],
      series: [
        { name: 'Import', type: 'bar', stack: 'g', data: tl.map((b) => [b.t, b.grid_import_kw]), itemStyle: { color: css('--red') } },
        { name: 'Export', type: 'bar', stack: 'g', data: tl.map((b) => [b.t, -b.grid_export_kw]), itemStyle: { color: css('--green') } },
        { name: 'Curtailed', type: 'line', data: tl.map((b) => [b.t, b.curtail_kw]), symbol: 'none', lineStyle: { color: css('--faint'), type: 'dotted' }, areaStyle: { color: css('--surface-3') } },
      ],
    }), true);

    const i = nowBlock(tl);
    $('#e-table').innerHTML = `<thead><tr><th>Time</th><th class="num">Import</th><th class="num">Export</th><th class="num">PV</th><th class="num">SoC</th><th>Battery mode</th><th>Export on/off</th></tr></thead><tbody>`
      + tl.map((b, fi) => {
        const m = modeOf(b.slot); const isNow = fi === i; // the 15-min block containing "now"
        return `<tr class="${isNow ? 'now' : ''}"><td>${fmt.hm(b.t)}</td><td class="num">${fmt.n(b.import_price * rate, 2)}</td><td class="num">${fmt.n(b.export_price * rate, 2)}</td><td class="num">${fmt.n(b.pv_kw, 1)}</td><td class="num">${fmt.n(b.soc_kwh, 1)}</td><td><span class="badge" style="background:${m.color}22;color:${m.color}">${m.label}</span></td><td>${b.export_enabled ? '<span class="chip green" style="padding:1px 8px">on</span>' : '<span class="chip" style="padding:1px 8px">off</span>'}</td></tr>`;
      }).join('') + '</tbody>';
  },
};

// ---- HEATING ----
screens.heating = {
  mount() {
    return `
    <section class="card span-full">
      <div class="card-head"><div class="card-title"><span class="ico">🌡️</span> Predicted room temperatures (24 h)</div><div class="card-sub">model forecast · comfort band shaded</div></div>
      <div class="chart tall" id="ht-temp"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔥</span> Heating schedule</div><div class="card-sub">per-zone underfloor power (kW)</div></div>
      <div class="chart" id="ht-sched"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🏠</span> Rooms now</div></div>
      <div class="zone-grid" id="ht-zones"></div>
    </section>`;
  },
  update(store) {
    const plan = store['/api/plan/latest']?.data;
    const zones = store['/api/zones']?.data || [];
    const state = store['/api/state']?.data?.zones || [];
    if (!plan) return;
    const tl = plan.timeline || [];
    const znames = zones.map((z) => z.zone);
    const palette = ['#4f9cff', '#34d399', '#fbbf24', '#fb7185', '#a78bfa', '#22d3ee', '#f472b6', '#84cc16', '#fb923c', '#60a5fa'];

    // temperature prediction lines + a soft global comfort band
    const tmin = Math.min(...zones.map((z) => z.t_min));
    const tmax = Math.max(...zones.map((z) => z.t_max));
    const tempSeries = znames.map((z, k) => ({ name: z.replace(/_/g, ' '), type: 'line', smooth: true, symbol: 'none', lineStyle: { width: 1.6, color: palette[k % palette.length] }, itemStyle: { color: palette[k % palette.length] }, data: tl.map((b) => [b.t, b.temp_c?.[z]]) }));
    if (isFinite(tmin) && isFinite(tmax)) {
      tempSeries.unshift({ name: 'comfort', type: 'line', data: tl.map((b) => [b.t, tmax]), symbol: 'none', lineStyle: { opacity: 0 }, areaStyle: { color: css('--green') + '12', origin: tmin }, silent: true, tooltip: { show: false } });
    }
    chart('ht-temp')?.setOption(Object.assign(baseOption(), {
      legend: { type: 'scroll', textStyle: { color: css('--muted') }, top: 0 },
      yAxis: [yAxis('°C', { scale: true })], series: tempSeries,
    }), true);

    // heating schedule stacked area
    chart('ht-sched')?.setOption(Object.assign(baseOption(), {
      legend: { type: 'scroll', textStyle: { color: css('--muted') }, top: 0 },
      yAxis: [yAxis('kW')],
      series: znames.map((z, k) => ({ name: z.replace(/_/g, ' '), type: 'line', stack: 'h', smooth: false, step: 'end', symbol: 'none', areaStyle: { color: palette[k % palette.length] + '99' }, lineStyle: { width: 0 }, itemStyle: { color: palette[k % palette.length] }, data: tl.map((b) => [b.t, b.heat_kw?.[z] || 0]) })),
    }), true);

    // rooms now
    const smap = Object.fromEntries(state.map((s) => [s.zone, s.temp_c]));
    const fs = plan.first_step || {};
    $('#ht-zones').innerHTML = zones.map((z) => {
      const t = smap[z.zone]; const c = comfort(t, z); const heating = (fs.heat_kw?.[z.zone] || 0) > 0.05;
      const preds = tl.map((b) => b.temp_c?.[z.zone]).filter((x) => x != null);
      const lo = preds.length ? Math.min(...preds) : null, hi = preds.length ? Math.max(...preds) : null;
      return `<div class="zone ${heating ? 'heating' : ''}">
        <div class="zname"><span>${esc(z.zone.replace(/_/g, ' '))}</span>${heating ? `<span class="heat-dot">🔥 ${fmt.kw(fs.heat_kw?.[z.zone], 1)}kW</span>` : (c.cls ? `<span class="chip ${c.cls}" style="padding:1px 7px">${c.label}</span>` : '')}</div>
        <div class="ztemp">${fmt.temp(t)}</div>
        <div class="faint" style="font-size:0.72rem">band ${z.t_min}–${z.t_max}° · forecast ${fmt.n(lo, 1)}–${fmt.n(hi, 1)}°</div>
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
          formatter: (ps) => { const z = zrev[ps[0].dataIndex]; if (!z) return ''; const b = z.mean_bias_k; return `${z.zone.replace(/_/g, ' ')}<br/>RMSE ${z.rmse_k.toFixed(2)} K · bias ${b >= 0 ? '+' : ''}${b.toFixed(2)} K · n=${z.n}`; } },
        xAxis: { type: 'value', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        yAxis: { type: 'category', data: zrev.map((z) => z.zone.replace(/_/g, ' ')), axisLabel: { color: css('--muted') } },
        series: [{ type: 'bar', data: zrev.map((z) => z.rmse_k), itemStyle: { color: css('--blue'), borderRadius: [0, 4, 4, 0] } }],
      }, true);
    } else {
      $('#vmeta').textContent = 'warming up — scoring needs ≥3 h of measured data after a snapshot';
    }

    if (cal) {
      $('#gmeta').textContent = cal.live?.fitted_at ? `fitted ${fmt.hm(cal.live.fitted_at)} · ${cal.window_days}-day window · re-fits every ${cal.recalibrate_hours}h · no bar = not fitted / not configured` : 'config baseline';
      const live = cal.live?.gains_w || {}; const base = cal.config_baseline_w || {};
      const znames = [...new Set([...Object.keys(live), ...Object.keys(base)])].sort();
      // `?? null` (not `|| 0`): an absent zone draws NO bar, so "not fitted" / "not configured" can't be
      // misread as a real 0 W gain. Value labels keep the small live bars legible next to big baselines.
      const lbl = { show: true, position: 'top', color: css('--muted'), fontSize: 9, formatter: (p) => p.value != null ? Math.round(p.value) : '' };
      chart('m-gains')?.setOption({
        textStyle: { color: css('--muted') }, grid: { left: 50, right: 20, top: 28, bottom: 60, containLabel: true },
        tooltip: { trigger: 'axis', confine: true, axisPointer: { type: 'shadow' }, valueFormatter: (v) => v == null ? 'n/a' : v.toFixed(0) + ' W' },
        legend: { top: 0, textStyle: { color: css('--muted') } },
        xAxis: { type: 'category', data: znames.map((z) => z.replace(/_/g, ' ')), axisLabel: { color: css('--muted'), rotate: 35 } },
        yAxis: { type: 'value', name: 'W', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        series: [
          { name: 'live fit', type: 'bar', data: znames.map((z) => live[z] ?? null), label: lbl, itemStyle: { color: css('--green'), borderRadius: [4, 4, 0, 0] } },
          { name: 'config baseline', type: 'bar', data: znames.map((z) => base[z] ?? null), label: lbl, itemStyle: { color: css('--faint'), borderRadius: [4, 4, 0, 0] } },
        ],
      }, true);
    }
  },
};

// ---- SYSTEM ----
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
      <div class="muted" style="font-size:0.85rem">All data is served read-only at <a class="link" href="/api">/api</a>. Endpoints:
        <a class="link" href="/api/plan/latest">/api/plan/latest</a>, <a class="link" href="/api/live">/api/live</a>, <a class="link" href="/api/history">/api/history</a>, <a class="link" href="/api/calibration/gains">/api/calibration/gains</a>, <a class="link" href="/api/forecast/validation">/api/forecast/validation</a>, <a class="link" href="/api/version">/api/version</a>.</div>
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
      const heating = Object.entries(fs.heat_kw || {}).filter(([, kw]) => kw > 0.05);
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
const EV_STRATEGIES = ['cost_optimized', 'solar_preferred', 'solar_only', 'charge_now'];

function evCard(e) {
  const [cls, label] = EV_BADGE[e.status] || ['', '—'];
  const soc = e.soc_pct;
  const opts = EV_STRATEGIES.map((sname) => `<option value="${sname}" ${e.strategy === sname ? 'selected' : ''}>${sname.replace(/_/g, ' ')}</option>`).join('');
  return `<section class="card">
    <div class="card-head"><div class="card-title"><span class="ico">🚗</span> ${esc(e.name)}</div>
      <span class="badge ${cls}">${label}</span></div>
    <div class="stat-row"><span class="k">Car battery</span><span class="v">${soc != null ? fmt.pct(soc) : '—'} → ${fmt.pct(e.target_pct)}</span></div>
    <div class="stat-row"><span class="k">Charging now</span><span class="v">${fmt.kw(e.charger_power_kw, 1)} kW</span></div>
    <div class="stat-row"><span class="k">Planned this session</span><span class="v">${fmt.kw(e.charged_kwh, 1)} kWh</span></div>
    <div class="ev-controls" data-charger="${esc(e.name)}" style="margin-top:10px;display:flex;flex-wrap:wrap;gap:8px;align-items:end">
      <label class="faint" style="font-size:.8rem">Strategy<br><select class="ev-strategy">${opts}</select></label>
      <label class="faint" style="font-size:.8rem">Target %<br><input class="ev-target" type="number" min="0" max="100" step="5" value="${Math.round(e.target_pct ?? 80)}" style="width:64px"></label>
      <label class="faint" style="font-size:.8rem">By<br><input class="ev-deadline" type="time" value=""></label>
      <button class="ev-save icon-btn" style="width:auto;padding:0 12px">Save</button>
    </div>
  </section>`;
}

function wireEv(e) {
  // Match by the decoded `data-charger` value rather than a CSS selector built from the name: the
  // attribute is HTML-escaped (esc) but CSS.escape doesn't escape quotes, so a name with a `"` would
  // make the selector a syntax error and the Save button silently dead. A direct compare is name-safe.
  const root = [...document.querySelectorAll('.ev-controls')].find((el) => el.dataset.charger === e.name);
  if (!root) return;
  const save = root.querySelector('.ev-save');
  if (!save) return;
  save.onclick = async () => {
    const body = { strategy: root.querySelector('.ev-strategy').value };
    // Only send a finite target — an empty/invalid field would JSON-encode as null and silently
    // reset to the config default; omitting it leaves the existing target untouched (cf. deadline).
    const target = parseFloat(root.querySelector('.ev-target').value);
    if (Number.isFinite(target)) body.target_pct = target;
    const dl = root.querySelector('.ev-deadline').value;
    if (dl) body.deadline = dl;
    const ok = await apiPost(`/api/ev/${encodeURIComponent(e.name)}/preference`, body);
    if (ok) setTimeout(refresh, 400); // give the next plan tick a moment to pick it up
  };
}

screens.ev = {
  mount() {
    return `
    <div id="ev-cards" class="grid cols-2"></div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔌</span> Charge schedule — by source</div>
        <div class="card-sub">solar / grid / battery → car, per 15-min block</div></div>
      <div class="chart tall" id="ev-chart"></div>
    </section>`;
  },
  update(store) {
    const evs = store['/api/ev']?.data || [];
    const tl = store['/api/plan/timeline']?.data || [];
    $('#ev-cards').innerHTML = evs.length
      ? evs.map(evCard).join('')
      : '<section class="card"><div class="faint">No EV charger configured, or the plan is warming up.</div></section>';
    evs.forEach(wireEv);
    this.chart(evs, tl);
  },
  chart(evs, tl) {
    const c = chart('ev-chart');
    if (!c || !evs.length) return;
    const e = evs[0]; // the schedule chart shows the first charger
    const leg = (key, color, name) => ({
      name, type: 'line', stack: 'ev', symbol: 'none', smooth: false, step: 'end',
      areaStyle: { color: color + '88' }, lineStyle: { width: 0 },
      data: tl.map((b, i) => [b.t, (e[key] || [])[i] || 0]),
    });
    c.setOption(Object.assign(baseOption(), {
      tooltip: { trigger: 'axis', confine: true, valueFormatter: (v) => typeof v === 'number' ? `${v.toFixed(2)} kW` : v },
      color: [css('--amber'), css('--blue'), css('--purple')], // legend swatches match the source areas
      legend: { show: true, data: ['Solar', 'Grid', 'Battery'], top: 0, textStyle: { color: css('--muted') }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
      yAxis: [yAxis('kW')],
      series: [leg('solar_kw', css('--amber'), 'Solar'), leg('grid_kw', css('--blue'), 'Grid'), leg('batt_kw', css('--purple'), 'Battery')],
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
    house.solar = {}; (store['/api/model/solar']?.data?.boundaries || []).forEach((b) => { house.solar[b.id] = b.solar_w; });
    house.sun = store['/api/model/solar']?.data?.sun || null;
    house.comfort = {}; (store['/api/zones']?.data || []).forEach((z) => { house.comfort[z.zone] = z; });

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
      const solar = bs.reduce((s, b) => s + this.solarW(b), 0);
      const ti = house.temps[z.name];
      const cf = house.comfort[z.name];
      const band = cf && ti != null ? (ti < cf.t_min ? ['blue', 'cool'] : ti > cf.t_max ? ['red', 'warm'] : ['green', 'comfort']) : null;
      const dom = bs.slice().sort((a, b) => (this.lossW(b) || 0) - (this.lossW(a) || 0))[0];
      return `<div class="env-zone" data-z="${esc(z.name)}">
        <div class="env-zone-head"><span class="env-zone-name">${nice(z.name)}</span>${band ? `<span class="badge ${band[0]}">${band[1]}</span>` : ''}</div>
        <div class="env-zone-temp" style="color:${tempColor(ti)}">${fmt.temp(ti)}<span class="env-zone-band">${cf ? ` / ${fmt.n(cf.t_min, 0)}–${fmt.n(cf.t_max, 0)}°` : ''}</span></div>
        <div class="env-zone-row"><span>UA to outside</span><span>${fmt.n(ua, 1)} W/K</span></div>
        <div class="env-zone-row"><span>loss now</span><span style="color:${css('--red')}">${(ti == null || house.outside == null) ? '—' : `${fmt.n(loss, 0)} W`}</span></div>
        ${solar > 1 ? `<div class="env-zone-row"><span>☀ on surfaces</span><span>${fmt.n(solar, 0)} W</span></div>` : ''}
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
      !interior && this.solarW(b) > 0.5 ? ['Solar load now', `${Math.round(this.solarW(b))} W absorbed on the surface`] : null,
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
const store = {};

async function refresh() {
  const r = current; if (!r) return;
  // Re-fetch the screen's own endpoints, plus /readyz for the status dot.
  const paths = [...new Set([...r.ep, '/readyz'])];
  const res = await loadAll(paths);
  if (r !== current) return; // navigated away mid-fetch — don't render against the new screen's DOM
  Object.assign(store, res);
  window.__ready = res['/readyz']?.data || window.__ready;
  updateStatus();
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
  const view = $('#view'); view.innerHTML = ''; view.appendChild(h(`<div>${screens[r.id].mount()}</div>`));
  refresh();
}
function updateNav() {
  // config-driven sections (e.g. EV) appear only when their capability is present.
  const visible = ROUTES.filter((r) => !r.cap || window.__caps?.[r.cap]);
  $('#nav').innerHTML = visible.map((r) => `<a href="#/${r.id}" class="${current && current.id === r.id ? 'active' : ''}">${r.name}</a>`).join('');
}

function tickClock() { $('#clock').textContent = new Date().toLocaleTimeString([], { hour12: false }); }

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
  timer = setInterval(refresh, 10000); // poll every 10s
  $('#footer').innerHTML = `MPC Home Control — read-only dashboard · the controllers actuate the house · data via the <a class="link" href="/api">JSON API</a>`;
}
document.addEventListener('DOMContentLoaded', init);
