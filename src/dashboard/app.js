/* MPC Home Control — dashboard app. Vanilla JS, hash routing, polling, ECharts. Read-only. */
'use strict';

// ---------- tiny helpers ----------
const $ = (sel, root = document) => root.querySelector(sel);
const h = (html) => { const t = document.createElement('template'); t.innerHTML = html.trim(); return t.content.firstChild; };
const clamp = (x, lo, hi) => Math.max(lo, Math.min(hi, x));
const css = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();
// Escape any backend/InfluxDB-sourced string before it goes into innerHTML (defence in depth — the
// data is the house's own internal feed, but never trust a string into markup).
const esc = (s) => { const d = document.createElement('div'); d.textContent = s == null ? '' : String(s); return d.innerHTML; };

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
const nowBlock = (tl) => { const now = Date.now(); let i = 0; for (let k = 0; k < tl.length; k++) if (new Date(tl[k].t).getTime() <= now) i = k; return i; };

// ---------- insight engine: human "why" ----------
function insights(store) {
  const plan = store['/api/plan/latest']?.data?.plan;
  if (!plan) return { headline: 'Warming up…', reasons: [] };
  const tl = plan.timeline || [];
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
    reasons.push(`Heating ${heatingZones.map((z) => esc(z.replace(/_/g, ' '))).join(', ')}${cheap ? ' on cheap power — pre-warming the slab before prices rise.' : ' to hold the comfort band.'}`);
  } else {
    reasons.push('No heating needed — all rooms are coasting within their comfort band.');
  }

  // upcoming cheapest / most expensive window
  if (st) {
    const future = tl.slice(i);
    const cheapest = future.reduce((a, x) => (x.import_price < a.import_price ? x : a), future[0] || b);
    if (cheapest && cheapest.t !== b.t) reasons.push(`Cheapest power coming up around ${fmt.hm(cheapest.t)} (${fmt.czk(cheapest.import_price * rate, 2)}/kWh).`);
  }

  const headline = `${m.label} — ${m.desc}. Power is <strong>${lvl.label.toLowerCase()}</strong> right now.`;
  return { headline, reasons, level: lvl, rate };
}

// ---------- routes ----------
const ROUTES = [
  { id: 'home',    name: 'Home',     ep: ['/api/live', '/api/plan/latest', '/api/state', '/api/zones', '/api/compare', '/api/history'] },
  { id: 'energy',  name: 'Energy',   ep: ['/api/plan/latest', '/api/live', '/api/history'] },
  { id: 'heating', name: 'Heating',  ep: ['/api/plan/latest', '/api/state', '/api/zones'] },
  { id: 'model',   name: 'Model',    ep: ['/api/calibration/gains', '/api/forecast/validation'] },
  { id: 'compare', name: 'Compare',  ep: ['/api/compare', '/api/plan/latest'] },
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
      <div class="card-head"><div class="card-title"><span class="ico">🧠</span> What the system is doing &amp; why</div><div class="card-sub" id="confidence"></div></div>
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
    const plan = store['/api/plan/latest']?.data?.plan;
    const zones = store['/api/zones']?.data || [];
    const state = store['/api/state']?.data?.zones || [];
    const cmp = store['/api/compare']?.data;

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
      $('#reasons').innerHTML = ins.reasons.map((r) => `<li><span class="dot"></span><span>${r}</span></li>`).join('');

      // confidence vs loxone
      if (cmp) {
        const agree = cmp.mode_agree;
        const txt = agree === true ? '✓ in step with the house controller' : agree === false ? '≠ differs from the house controller' : 'comparing…';
        $('#confidence').innerHTML = `<span class="badge ${agree === true ? 'green' : agree === false ? 'amber' : 'blue'}">${txt}</span>`;
      }

      // day chart
      this.dayChart(tl, rate, store);
    }

    // comfort grid
    const zmap = Object.fromEntries(zones.map((z) => [z.zone, z]));
    const smap = Object.fromEntries(state.map((s) => [s.zone, s.temp_c]));
    const fs = plan?.first_step || {};
    const heated = zones.length ? zones : Object.keys(smap).map((z) => ({ zone: z }));
    $('#zone-grid').innerHTML = heated.map((z) => {
      const t = smap[z.zone]; const c = comfort(t, zmap[z.zone]);
      const heating = (fs.heat_kw?.[z.zone] || 0) > 0.05;
      const band = zmap[z.zone] ? `${zmap[z.zone].t_min}–${zmap[z.zone].t_max}°` : '';
      return `<div class="zone ${heating ? 'heating' : ''}">
        <div class="zname"><span>${esc(z.zone.replace(/_/g, ' '))}</span>${heating ? '<span class="heat-dot">🔥</span>' : (c.cls ? `<span class="chip ${c.cls}" style="padding:1px 7px">${c.label}</span>` : '')}</div>
        <div class="ztemp">${fmt.temp(t)}</div>
        <div class="faint" style="font-size:0.72rem">comfort ${band}</div>
      </div>`;
    }).join('');

    const okZones = heated.filter((z) => comfort(smap[z.zone], zmap[z.zone]).cls === 'green').length;
    $('#comfort-sub').textContent = `${okZones}/${heated.length} rooms comfortable`;
  },
  // The day plan: measured history (solid) joined to the forecast (dashed) for both PV output and
  // battery SoC, over the price curve. The forecast rolls 24 h ahead, so once tomorrow's prices
  // publish (~14:00) it already reaches into the next day.
  dayChart(tl, rate, store) {
    const c = chart('home-chart'); if (!c) return;
    const pv = css('--yellow'), soc = css('--amber'), price = css('--blue');
    const priceData = tl.map((b) => [b.t, b.import_price * rate]);
    $('#day-legend').innerHTML = modeLegend();
    c?.setOption(Object.assign(baseOption(), {
      tooltip: planTooltip(tl),
      color: [price, pv, pv, soc, soc], // legend swatches follow the lines (series order), not ECharts' default palette
      legend: { show: true, data: ['PV', 'SoC', 'Price'], top: 0, textStyle: { color: css('--muted') }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 },
      grid: { left: 50, right: 56, top: 30, bottom: 30, containLabel: true },
      yAxis: [yAxis('Kč/kWh', { position: 'right' }), yAxis('kW · kWh', { position: 'left' })],
      series: [
        { name: 'Price', type: 'line', step: 'end', yAxisIndex: 0, data: priceData, smooth: false, symbol: 'none', lineStyle: { color: price, width: 2 }, areaStyle: { color: grad(price) },
          markArea: { silent: true, data: modeBands(tl) }, markLine: nowMark() },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: histData(store, 'pv_kw'), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 2 }, areaStyle: { color: grad(pv) } },
        { name: 'PV', type: 'line', yAxisIndex: 1, data: tl.map((b) => [b.t, b.pv_kw]), smooth: true, symbol: 'none', lineStyle: { color: pv, width: 1.5, type: 'dashed' } },
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
      <div class="card-head"><div class="card-title"><span class="ico">📋</span> Per-block plan</div><div class="card-sub">recommended Growatt mode — shadow only</div></div>
      <div style="overflow-x:auto"><table class="tbl" id="e-table"></table></div>
    </section>`;
  },
  update(store) {
    const plan = store['/api/plan/latest']?.data?.plan; if (!plan) return;
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
      + tl.filter((_, k) => k % 2 === 0).map((b, fi) => {
        const m = modeOf(b.slot); const isNow = fi === Math.floor(i / 2); // the 30-min row containing "now"
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
    const plan = store['/api/plan/latest']?.data?.plan;
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
      <section class="card"><div class="card-head"><div class="card-title"><span class="ico">🎯</span> Forward prediction vs measured</div><div class="card-sub" id="vmeta"></div></div><div class="chart" id="m-valid"></div></section>
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
      $('#vmeta').textContent = val.mean_rmse_k != null ? `mean RMSE ${fmt.n(val.mean_rmse_k, 2)} K · since ${fmt.hm(val.anchored_at)}` : '—';
      const worst = val.zones[0];
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
      chart('m-rmse')?.setOption({
        textStyle: { color: css('--muted') }, grid: { left: 90, right: 20, top: 10, bottom: 24 },
        tooltip: { trigger: 'axis', confine: true, axisPointer: { type: 'shadow' }, valueFormatter: (v) => typeof v === 'number' ? v.toFixed(2) : v },
        xAxis: { type: 'value', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        yAxis: { type: 'category', data: val.zones.map((z) => z.zone.replace(/_/g, ' ')).reverse(), axisLabel: { color: css('--muted') } },
        series: [{ type: 'bar', data: [...val.zones].reverse().map((z) => z.rmse_k), itemStyle: { color: css('--blue'), borderRadius: [0, 4, 4, 0] } }],
      }, true);
    } else {
      $('#vmeta').textContent = 'warming up — scoring needs ≥3 h of measured data after a snapshot';
    }

    if (cal) {
      $('#gmeta').textContent = cal.live?.fitted_at ? `fitted ${fmt.hm(cal.live.fitted_at)} · ${cal.window_days}-day window · re-fits every ${cal.recalibrate_hours}h` : 'config baseline';
      const live = cal.live?.gains_w || {}; const base = cal.config_baseline_w || {};
      const znames = [...new Set([...Object.keys(live), ...Object.keys(base)])].sort();
      chart('m-gains')?.setOption({
        textStyle: { color: css('--muted') }, grid: { left: 50, right: 20, top: 28, bottom: 60, containLabel: true },
        tooltip: { trigger: 'axis', confine: true, axisPointer: { type: 'shadow' }, valueFormatter: (v) => typeof v === 'number' ? v.toFixed(2) : v },
        legend: { top: 0, textStyle: { color: css('--muted') } },
        xAxis: { type: 'category', data: znames.map((z) => z.replace(/_/g, ' ')), axisLabel: { color: css('--muted'), rotate: 35 } },
        yAxis: { type: 'value', name: 'W', axisLabel: { color: css('--muted') }, splitLine: { lineStyle: { color: css('--surface-2') } } },
        series: [
          { name: 'live fit', type: 'bar', data: znames.map((z) => live[z] || 0), itemStyle: { color: css('--green'), borderRadius: [4, 4, 0, 0] } },
          { name: 'config baseline', type: 'bar', data: znames.map((z) => base[z] || 0), itemStyle: { color: css('--faint'), borderRadius: [4, 4, 0, 0] } },
        ],
      }, true);
    }
  },
};

// ---- COMPARE ----
const agreeLog = [];
screens.compare = {
  mount() {
    return `
    <div class="grid cols-3">
      <section class="card"><div class="kpi"><div class="kpi-label">Mode — shadow</div><div class="kpi-value" id="c-smode">—</div><div class="kpi-sub" id="c-agree"></div></div></section>
      <section class="card"><div class="kpi"><div class="kpi-label">Mode — loxone (live)</div><div class="kpi-value" id="c-lmode">—</div><div class="kpi-sub">the running house controller</div></div></section>
      <section class="card"><div class="kpi"><div class="kpi-label">Heating agreement</div><div class="kpi-value" id="c-heatpct">—</div><div class="kpi-sub" id="c-soc"></div></div></section>
    </div>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🤝</span> Agreement over time</div><div class="card-sub">accumulates while this page is open — the path to trust</div></div>
      <div class="chart short" id="c-rolling"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔥</span> Heating — shadow vs loxone relays</div></div>
      <div style="overflow-x:auto"><table class="tbl" id="c-table"></table></div>
    </section>`;
  },
  update(store) {
    const c = store['/api/compare']?.data; if (!c) return;
    $('#c-smode').textContent = modeLabel(c.shadow_mode);
    $('#c-lmode').textContent = modeLabel(c.loxone_mode);
    const a = c.mode_agree;
    $('#c-agree').innerHTML = `<span class="badge ${a === true ? 'green' : a === false ? 'amber' : 'blue'}">${a === true ? '✓ agree' : a === false ? '≠ differ' : 'unknown'}</span>`;
    $('#c-heatpct').textContent = c.heating_agreement_pct != null ? fmt.pct(c.heating_agreement_pct) : '—';
    $('#c-soc').textContent = (c.loxone_soc_kwh != null) ? `SoC loxone ${fmt.kw(c.loxone_soc_kwh, 1)} / shadow ${fmt.kw(c.shadow_next_soc_kwh, 1)} kWh` : '';

    if (a != null) { agreeLog.push([Date.now(), a ? 1 : 0]); if (agreeLog.length > 240) agreeLog.shift(); }
    chart('c-rolling')?.setOption({
      textStyle: { color: css('--muted') }, grid: { left: 40, right: 16, top: 16, bottom: 28 },
      tooltip: { trigger: 'axis', confine: true, formatter: (p) => `${new Date(p[0].value[0]).toLocaleTimeString([], { hour12: false })}<br>${p[0].value[1] ? 'agree' : 'differ'}` },
      xAxis: { type: 'time', axisLabel: { color: css('--muted') } },
      yAxis: { type: 'value', min: 0, max: 1, interval: 1, axisLabel: { color: css('--muted'), formatter: (v) => v ? 'agree' : 'differ' } },
      series: [{ type: 'line', step: 'end', symbol: 'none', data: agreeLog, lineStyle: { color: css('--blue') }, areaStyle: { color: css('--blue-soft') } }],
    }, true);

    $('#c-table').innerHTML = `<thead><tr><th>Zone</th><th>Shadow</th><th>Loxone relay</th><th>Agree</th></tr></thead><tbody>`
      + (c.heating || []).map((z) => `<tr><td>${esc(z.zone.replace(/_/g, ' '))}</td><td>${z.shadow_on ? '🔥 on' : 'off'}</td><td>${z.loxone_on == null ? '<span class="faint">—</span>' : z.loxone_on ? '🔥 on' : 'off'}</td><td>${z.agree == null ? '<span class="faint">—</span>' : z.agree ? '<span class="chip green" style="padding:1px 8px">✓</span>' : '<span class="chip red" style="padding:1px 8px">≠</span>'}</td></tr>`).join('') + '</tbody>';
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
      <div class="card-head"><div class="card-title"><span class="ico">🧾</span> Decision now (raw)</div><div class="card-sub">what a controller would apply — shadow only, nothing actuated</div></div>
      <div id="sys-decision"></div>
    </section>
    <section class="card span-full" style="margin-top:18px">
      <div class="card-head"><div class="card-title"><span class="ico">🔗</span> JSON API</div></div>
      <div class="muted" style="font-size:0.85rem">All data is served read-only at <a class="link" href="/api">/api</a>. Endpoints:
        <a class="link" href="/api/plan/latest">/api/plan/latest</a>, <a class="link" href="/api/live">/api/live</a>, <a class="link" href="/api/history">/api/history</a>, <a class="link" href="/api/compare">/api/compare</a>, <a class="link" href="/api/calibration/gains">/api/calibration/gains</a>, <a class="link" href="/api/forecast/validation">/api/forecast/validation</a>, <a class="link" href="/api/version">/api/version</a>.</div>
    </section>`;
  },
  async update(store) {
    const v = store['/api/version']?.data; const plan = store['/api/plan/latest']?.data?.plan;
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

// ============================================================ shell + loop
let current = null;
let timer = null;
const store = {};

async function refresh() {
  const r = current; if (!r) return;
  // always refresh readiness for the status dot
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
  $('#nav').innerHTML = ROUTES.map((r) => `<a href="#/${r.id}" class="${current && current.id === r.id ? 'active' : ''}">${r.name}</a>`).join('');
}

function tickClock() { $('#clock').textContent = new Date().toLocaleTimeString([], { hour12: false }); }

function init() {
  // theme toggle (persisted)
  const saved = localStorage.getItem('mpc-theme');
  if (saved) document.documentElement.setAttribute('data-theme', saved);
  $('#theme-toggle').onclick = () => {
    const cur = document.documentElement.getAttribute('data-theme') === 'light' ? 'dark' : 'light';
    document.documentElement.setAttribute('data-theme', cur); localStorage.setItem('mpc-theme', cur);
    disposeCharts(); refresh();
  };
  window.addEventListener('hashchange', () => go((location.hash.slice(2) || 'home')));
  updateNav();
  go(location.hash.slice(2) || 'home');
  tickClock(); setInterval(tickClock, 1000);
  timer = setInterval(refresh, 10000); // poll every 10s
  $('#footer').innerHTML = `MPC Home Control — read-only shadow monitor · data via the <a class="link" href="/api">JSON API</a>`;
}
document.addEventListener('DOMContentLoaded', init);
