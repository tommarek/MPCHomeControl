#!/usr/bin/env node
// Self-contained unit check for item H's relay duty-cycle display helpers in app.js (acceptance H1).
//
// There is no JS test framework / package.json in this repo (the dashboard has no build step — see
// CLAUDE.md), and app.js can't be `require()`d directly: it's a browser script whose very last line
// (`document.addEventListener('DOMContentLoaded', init)`) runs unconditionally at load and needs a
// real DOM. Instead, this pulls the two pure functions (`relayDuty`, `isNearTermBlock`, and their
// dependency `clamp`) out of the REAL source text by brace-matching on their declaration and
// evaluates them in an isolated scope — so this tests the actual shipped code, not a hand-copied
// duplicate that could silently drift from it.
//
// Run: `node src/dashboard/dashboard_test.js`. Exit 0 = every check passed (each printed as it runs);
// a failed assertion throws and the process exits non-zero.
'use strict';
// Pin the local time zone before any `Date` work below: `ledgerSummaryRows`/`localDateKey` derive
// "today"/"yesterday" from the REAL local zone (the house IS the browser, per the Lead's decision —
// see app.js), which would otherwise make this file's pass/fail depend on whatever zone happens to
// run it.
process.env.TZ = 'UTC';
const fs = require('fs');
const path = require('path');
const assert = require('assert');

const APP_JS = path.join(__dirname, 'app.js');
const src = fs.readFileSync(APP_JS, 'utf8');

/** Extract one top-level `function <name>(...) { ... }` declaration's exact source, by counting
 * braces from its opening one to the matching close. Robust to the object-literal `{ }` braces
 * inside `relayDuty`'s body (a naive "up to the first `\n}`" regex would stop early); assumes there
 * are no unbalanced `{`/`}` characters inside a string literal or comment within the body, which
 * holds for both functions as written. */
function extractFunction(source, name) {
  const marker = `function ${name}(`;
  const start = source.indexOf(marker);
  assert(start !== -1, `${name} not found in app.js`);
  const bodyStart = source.indexOf('{', start);
  assert(bodyStart !== -1, `${name}: no opening brace found`);
  let depth = 0;
  for (let i = bodyStart; i < source.length; i++) {
    if (source[i] === '{') depth++;
    else if (source[i] === '}') {
      depth--;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  throw new Error(`${name}: unbalanced braces`);
}

/** Extract a single-line `const <name> = ...;` declaration (for `clamp`, an arrow-function one-liner
 * with no braces at all, so `extractFunction`'s brace-matching doesn't apply). */
function extractConst(source, name) {
  const m = source.match(new RegExp(`const ${name} = [^\\n]*?;`));
  assert(m, `${name} not found in app.js (expected a single-line const)`);
  return m[0];
}

// Evaluate in one isolated function scope (not global eval) so `relayDuty`'s reference to `clamp`
// resolves via closure, without pulling in anything else from app.js — no DOM, no fetch, no globals.
const scope = {};
// eslint-disable-next-line no-new-func
new Function(
  'scope',
  `
  ${extractConst(src, 'clamp')}
  ${extractFunction(src, 'relayDuty')}
  ${extractFunction(src, 'isNearTermBlock')}
  ${extractConst(src, 'HEAT_ON_KW')}
  ${extractFunction(src, 'isRelayOn')}
  ${extractFunction(src, 'solarSplitText')}
  ${extractFunction(src, 'heatingBlockClass')}
  ${extractFunction(src, 'mergeOnPeriods')}
  ${extractConst(src, 'WEEKDAYS')}
  ${extractConst(src, 'MONTHS')}
  ${extractFunction(src, 'localMidnights')}
  ${extractFunction(src, 'localDateKey')}
  ${extractFunction(src, 'ledgerSummaryRows')}
  ${extractFunction(src, 'ledgerCostLine')}
  ${extractConst(src, 'FEATURE_STATUS_CLASS')}
  ${extractConst(src, 'FEATURE_KIND_CLASS')}
  ${extractConst(src, 'FEATURE_VERDICT_CLASS')}
  ${extractConst(src, 'featureStatusClass')}
  ${extractConst(src, 'featureKindClass')}
  ${extractConst(src, 'featureVerdictClass')}
  ${extractFunction(src, 'featureDate')}
  ${extractFunction(src, 'featureReleaseLabels')}
  ${extractConst(src, 'featureGraduatedLabel')}
  ${extractConst(src, 'featureHasValue')}
  ${extractFunction(src, 'featureChartEmpty')}
  ${extractFunction(src, 'featureTableEmpty')}
  ${extractFunction(src, 'featureSeriesData')}
  ${extractFunction(src, 'featureCell')}
  ${extractFunction(src, 'featureHasData')}
  scope.relayDuty = relayDuty;
  scope.isNearTermBlock = isNearTermBlock;
  scope.isRelayOn = isRelayOn;
  scope.solarSplitText = solarSplitText;
  scope.heatingBlockClass = heatingBlockClass;
  scope.mergeOnPeriods = mergeOnPeriods;
  scope.localMidnights = localMidnights;
  scope.localDateKey = localDateKey;
  scope.ledgerSummaryRows = ledgerSummaryRows;
  scope.ledgerCostLine = ledgerCostLine;
  scope.featureStatusClass = featureStatusClass;
  scope.featureKindClass = featureKindClass;
  scope.featureVerdictClass = featureVerdictClass;
  scope.featureDate = featureDate;
  scope.featureReleaseLabels = featureReleaseLabels;
  scope.featureGraduatedLabel = featureGraduatedLabel;
  scope.featureHasValue = featureHasValue;
  scope.featureChartEmpty = featureChartEmpty;
  scope.featureTableEmpty = featureTableEmpty;
  scope.featureSeriesData = featureSeriesData;
  scope.featureCell = featureCell;
  scope.featureHasData = featureHasData;
  `
)(scope);
const { featureStatusClass, featureKindClass, featureVerdictClass, featureDate, featureReleaseLabels, featureGraduatedLabel, featureHasValue, featureChartEmpty, featureTableEmpty, featureSeriesData, featureCell, featureHasData } = scope;
const { relayDuty, isNearTermBlock, isRelayOn, solarSplitText, heatingBlockClass, mergeOnPeriods, localMidnights, localDateKey, ledgerSummaryRows, ledgerCostLine } = scope;

let passed = 0;
function check(desc, fn) {
  fn();
  passed++;
  console.log(`ok - ${desc}`);
}

// ---- Floor plan helpers (Home screen card) ----
const fpScope = {};
// eslint-disable-next-line no-new-func
new Function(
  'scope',
  `
  const esc = (s) => String(s == null ? '' : s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  ${extractConst(src, 'clamp')}
  ${extractConst(src, 'HEAT_ON_KW')}
  ${extractFunction(src, 'isRelayOn')}
  ${['FP_STALE_S', 'FP_THETA', 'FP_SY', 'FP_LIFT', 'FP_GAP', 'FP_SLAB', 'FP_X_MIN', 'FP_X_MAX', 'fpFloors'].map((n) => extractConst(src, n)).join('\n')}
  ${[
    'fpToLab', 'fpFromLab', 'fpFromLch', 'fpAnchors', 'fpScaleLch', 'fpScaleColor', 'fpDesaturate', 'fpRoomState', 'fpScaleX', 'fpFill', 'fpScalePct', 'fpStatusPhrase', 'fpAgo',
    'fpHeatSource', 'fpAria', 'fpHouseSummary', 'fpSensorErrText', 'fpStats', 'fpStatsHtml', 'fpAtticRows', 'fpNorthDeg', 'fpOpeningShape', 'fpPaneShape', 'fpLayout', 'planMatrix', 'project', 'fpScreenToPlan', 'fpBounds', 'fpLabelBox',
    'fpLabelScale', 'fpShowName', 'fpResolveLabels', 'fpLabelPrio', 'fpCompactBox', 'fpPlaceCard', 'fpScrollDelta', 'fpAttention', 'fpNum', 'fpPresenceText', 'fpGauge', 'fpPlanChart', 'fpFacts', 'fpCardHtml',
  ].map((n) => extractFunction(src, n)).join('\n')}
  Object.assign(scope, { FP_STALE_S, FP_X_MIN, FP_X_MAX, fpToLab, fpFromLab, fpAnchors, fpScaleLch, fpScaleColor, fpDesaturate, fpRoomState, fpScaleX, fpFill, fpScalePct, fpStatusPhrase,
    fpStats, fpStatsHtml, fpAtticRows, fpNorthDeg, fpOpeningShape, fpPaneShape,
    fpAgo, fpAria, fpHouseSummary, fpSensorErrText, fpLayout, planMatrix, project, fpScreenToPlan, fpBounds, fpLabelBox, fpLabelScale, fpShowName,
    fpAttention, fpPresenceText, fpGauge, fpPlanChart, fpFacts, fpCardHtml, fpResolveLabels, fpLabelPrio, fpCompactBox, fpPlaceCard, fpScrollDelta });
  `
)(fpScope);
const { FP_X_MIN, FP_X_MAX, fpToLab, fpFromLab, fpAnchors, fpScaleLch, fpScaleColor, fpDesaturate, fpRoomState, fpScaleX, fpFill, fpScalePct, fpStatusPhrase, fpAgo, fpAria, fpHouseSummary, fpSensorErrText,
  fpStats, fpStatsHtml, fpAtticRows, fpNorthDeg, fpOpeningShape, fpPaneShape,
  fpLayout, planMatrix, project, fpScreenToPlan, fpBounds, fpLabelBox, fpLabelScale, fpShowName, fpAttention, fpPresenceText,
  fpGauge, fpPlanChart, fpFacts, fpCardHtml, FP_STALE_S, fpResolveLabels, fpLabelPrio, fpCompactBox, fpPlaceCard, fpScrollDelta } = fpScope;

const room = (over) => ({
  zone: 'kitchen', name: 'Kitchen', floor: 'ground', heated: true, area_m2: 24.5, volume_m3: 62, ach: 0.5, max_heat_kw: 1.8,
  measured: { temp_c: 22, at: 'x', age_s: 120 }, band: { t_min_now: 21, t_max_now: 24, overheat_c: 0, t_max_boost_now: 24 },
  heating: { on: false, kw: 0 }, heating_today_kwh: null, humidity: null, presence: null, estimate_c: 22.1, sensor_failed: false, ...over,
});

check('fpRoomState: inside the band is d=0, below floor negative, above ceiling positive', () => {
  assert.deepStrictEqual([fpRoomState(room()).kind, fpRoomState(room()).d], ['in', 0]);
  const cold = fpRoomState(room({ measured: { temp_c: 19.5, age_s: 5 } }));
  assert.strictEqual(cold.kind, 'cold');
  assert(Math.abs(cold.d - -1.5) < 1e-9);
  const warm = fpRoomState(room({ measured: { temp_c: 25.2, age_s: 5 } }));
  assert.strictEqual(warm.kind, 'warm');
  assert(Math.abs(warm.d - 1.2) < 1e-9);
  // the 0.1 K tolerance matches the dashboard's comfort(): a hair outside still reads as in band
  assert.strictEqual(fpRoomState(room({ measured: { temp_c: 20.95, age_s: 5 } })).kind, 'in');
});
check('fpRoomState: the overheat tier is its own kind (banking heat), beyond it is warm', () => {
  const band = { t_min_now: 20, t_max_now: 21, overheat_c: 1.5, t_max_boost_now: 22.5 };
  assert.strictEqual(fpRoomState(room({ band, measured: { temp_c: 22, age_s: 5 } })).kind, 'boost');
  assert.strictEqual(fpRoomState(room({ band, measured: { temp_c: 23, age_s: 5 } })).kind, 'warm');
  // a boost ceiling without a configured allowance is ignored
  assert.strictEqual(fpRoomState(room({ band: { ...band, overheat_c: 0 }, measured: { temp_c: 22, age_s: 5 } })).kind, 'warm');
});
check('fpRoomState: no sensor / no band / stale are flagged, never coloured as a temperature', () => {
  assert.strictEqual(fpRoomState(room({ measured: null })).kind, 'nosensor');
  assert.strictEqual(fpRoomState(room({ band: null })).kind, 'noband');
  assert.strictEqual(fpRoomState(room({ band: null })).temp, 22);
  assert.strictEqual(fpRoomState(undefined).kind, 'nosensor');
  assert.strictEqual(fpRoomState(room({ measured: { temp_c: 22, age_s: FP_STALE_S + 1 } })).stale, true);
  assert.strictEqual(fpRoomState(room({ measured: { temp_c: 22, age_s: FP_STALE_S - 1 } })).stale, false);
  for (const theme of ['dark', 'light']) {
    const A = fpAnchors(theme);
    assert.strictEqual(fpFill(fpRoomState(room({ measured: null })), theme), A.none);
    assert.strictEqual(fpFill(fpRoomState(room({ band: null })), theme), A.none);
  }
});
check('fpStatusPhrase: plain-words statuses', () => {
  const ph = (over) => fpStatusPhrase(fpRoomState(room(over)));
  assert.strictEqual(ph({}), 'in comfort band');
  assert.strictEqual(ph({ measured: { temp_c: 20.2, age_s: 5 } }), '0.8 K below floor');
  assert.strictEqual(ph({ measured: { temp_c: 25.2, age_s: 5 } }), '1.2 K above ceiling');
  assert.strictEqual(ph({ measured: null }), 'no sensor');
  assert.strictEqual(ph({ band: null }), 'no comfort band');
  assert.strictEqual(ph({ measured: { temp_c: 22, age_s: FP_STALE_S + 5 } }), 'in comfort band, stale reading');
  assert.match(ph({ band: { t_min_now: 20, t_max_now: 21, overheat_c: 1, t_max_boost_now: 22 }, measured: { temp_c: 21.6, age_s: 5 } }), /^banking heat/);
});
const lchOf = (hex) => { const [L, a, b] = fpToLab(hex); return [L, Math.hypot(a, b), (Math.atan2(b, a) * 180 / Math.PI + 360) % 360]; };
check('fpScaleLch: hue falls steadily from cold to hot; chroma low in the band and growing outward; lightness monotonic per arm', () => {
  for (const theme of ['dark', 'light']) {
    let prevH = Infinity;
    for (let x = FP_X_MIN; x <= FP_X_MAX + 1e-9; x += 0.05) {
      const h = fpScaleLch(x, theme)[2];
      assert(h <= prevH + 1e-9, `${theme}: hue rose at x=${x.toFixed(2)} (${h} > ${prevH})`);
      prevH = h;
    }
    const at = (x) => fpScaleLch(x, theme);
    // cold arm blue, warm arm amber -> red, the band itself teal -> sage -> sand
    assert(at(-3)[2] > 240 && at(4)[2] < 45 && at(2.5)[2] > 55 && at(2.5)[2] < 80, `${theme}: arm hues`);
    assert(at(0)[2] > 190 && at(1)[2] < 110, `${theme}: floor end cool, ceiling end warm`);
    // calm inside the band: every in-band chroma is below both arms' first full step
    const bandMax = Math.max(...[0, 0.25, 0.5, 0.75, 1].map((x) => at(x)[1]));
    assert(bandMax < 0.06 && bandMax < at(-1.5)[1] / 2 && bandMax < at(2.5)[1] / 2, `${theme}: band chroma ${bandMax}`);
    // chroma grows outward on each arm
    assert(at(-3)[1] >= at(-1.5)[1] && at(-1.5)[1] > at(0)[1] && at(4)[1] > at(2.5)[1] && at(2.5)[1] > at(1)[1], `${theme}: chroma`);
    // lightness: the dark theme glows brighter outward, the light theme darkens outward
    const dir = theme === 'dark' ? 1 : -1;
    for (const [inner, outer] of [[0, -1.5], [-1.5, -3], [1, 2.5], [2.5, 4]]) assert(dir * (at(outer)[0] - at(inner)[0]) > 0, `${theme}: lightness ${inner}->${outer}`);
    assert.strictEqual(fpScaleColor(4, theme), fpScaleColor(9, theme), 'warm end clamps');
    assert.strictEqual(fpScaleColor(-3, theme), fpScaleColor(-12, theme), 'cold end clamps');
  }
});
check('fpScaleX: below floor in kelvin, inside the band as the floor-to-ceiling fraction, above it from 1 up', () => {
  const x = (T, band) => fpScaleX(fpRoomState(room({ band, measured: { temp_c: T, age_s: 5 } })));
  const b = { t_min_now: 20, t_max_now: 24, overheat_c: 0, t_max_boost_now: 24 };
  assert.strictEqual(x(20, b), 0);
  assert.strictEqual(x(22, b), 0.5);
  assert.strictEqual(x(24, b), 1);
  assert.strictEqual(x(24.1, b), 1, 'the 0.1 K tolerance still reads as in band, pinned at the ceiling');
  assert(Math.abs(x(18.5, b) - -1.5) < 1e-9);
  assert.strictEqual(x(10, b), FP_X_MIN);
  assert(Math.abs(x(24.6, b) - 1.6) < 1e-9, 'no allowance, under 1 K over: 1 + the excess over the ceiling');
  assert(Math.abs(x(25.2, b) - 3.2) < 1e-9, 'no allowance, 1 K over or more: past the overheat tier');
  // the overheat tier's colour is the banking rooms' alone: no warm room lands within 0.5 of it
  const allow = { t_min_now: 20, t_max_now: 24, overheat_c: 1, t_max_boost_now: 25 };
  for (let T = 24.15; T < 30; T += 0.01) {
    for (const band of [b, allow]) {
      const st = fpRoomState(room({ band, measured: { temp_c: T, age_s: 5 } }));
      if (st.kind === 'warm') assert(Math.abs(fpScaleX(st) - 2.5) >= 0.5 - 1e-9, `T=${T.toFixed(2)} warm at ${fpScaleX(st)}`);
    }
  }
  assert.strictEqual(x(40, b), FP_X_MAX);
  assert.strictEqual(x(21, { t_min_now: 21, t_max_now: 21, overheat_c: 0 }), 0.5, 'a zero-width band sits mid-scale');
  assert.strictEqual(fpScaleX(fpRoomState(room({ measured: null }))), null);
  assert.strictEqual(fpScaleX(fpRoomState(room({ band: null }))), null);
  // the legend positions: monotonic, band in the middle 40 %
  assert.deepStrictEqual([fpScalePct(FP_X_MIN), fpScalePct(0), fpScalePct(1), fpScalePct(FP_X_MAX)], [0, 30, 70, 100]);
  for (let v = FP_X_MIN; v < FP_X_MAX; v += 0.1) assert(fpScalePct(v + 0.1) > fpScalePct(v));
});
check('fpFill: boost tier uses one warm step; stale keeps lightness but drops chroma; OKLab round-trips', () => {
  const band = { t_min_now: 20, t_max_now: 21, overheat_c: 2, t_max_boost_now: 23 };
  const a = fpFill(fpRoomState(room({ band, measured: { temp_c: 21.4, age_s: 5 } })), 'dark');
  const b = fpFill(fpRoomState(room({ band, measured: { temp_c: 22.9, age_s: 5 } })), 'dark');
  assert.strictEqual(a, fpScaleColor(2.5, 'dark'));
  assert.strictEqual(a, b);
  const live = fpToLab(fpFill(fpRoomState(room({ measured: { temp_c: 19, age_s: 5 } })), 'dark'));
  const stale = fpToLab(fpFill(fpRoomState(room({ measured: { temp_c: 19, age_s: FP_STALE_S + 1 } })), 'dark'));
  assert(Math.abs(live[0] - stale[0]) < 0.02);
  assert(Math.hypot(stale[1], stale[2]) < Math.hypot(live[1], live[2]) * 0.3);
  const hex = fpFromLab(fpToLab('#3a7bd5'));
  assert.strictEqual(hex, '#3a7bd5', 'OKLab round-trip');
  assert.strictEqual(fpDesaturate('#808080', 0.1), '#808080');
  // in band, a room near its floor is cooler in hue than one near its ceiling (the in-band grading)
  const nearFloor = lchOf(fpFill(fpRoomState(room({ measured: { temp_c: 21.2, age_s: 5 } })), 'dark'));
  const nearCeil = lchOf(fpFill(fpRoomState(room({ measured: { temp_c: 23.8, age_s: 5 } })), 'dark'));
  assert(nearFloor[2] > nearCeil[2] + 60, `hues ${nearFloor[2]} vs ${nearCeil[2]}`);
});
// Sweeps every band shape across cold, band, overheat tier and beyond, on the shipped hex colours.
// (Tolerances: 8-bit hex quantisation of the interpolated colour; hue is noisier at the band's low chroma.)
check('colour is monotonic in temperature: hue never turns cooler as it warms, and the warm arm never gets milder', () => {
  const bands = [
    { t_min_now: 21, t_max_now: 24, overheat_c: 1, t_max_boost_now: 25 },
    { t_min_now: 20, t_max_now: 21, overheat_c: 2.5, t_max_boost_now: 23.5 },
    { t_min_now: 18, t_max_now: 25, overheat_c: 0, t_max_boost_now: 25 },
    { t_min_now: 20.5, t_max_now: 22.5, overheat_c: 0, t_max_boost_now: 22.5 },
  ];
  for (const theme of ['dark', 'light']) {
    for (const band of bands) {
      let prevX = -Infinity, prevH = Infinity, prevC = -1;
      for (let T = band.t_min_now - 4; T <= band.t_max_boost_now + 4; T += 0.05) {
        const st = fpRoomState(room({ band, measured: { temp_c: T, age_s: 5 } }));
        const xv = fpScaleX(st);
        assert(xv >= prevX - 1e-9, `${theme} ${JSON.stringify(band)} T=${T.toFixed(2)}: scale position fell`);
        const [, c, h] = lchOf(fpFill(st, theme));
        assert(h <= prevH + 3, `${theme} ${JSON.stringify(band)} T=${T.toFixed(2)}: hue ${h.toFixed(1)} rose from ${prevH.toFixed(1)}`);
        if (xv > 1) {
          assert(c >= prevC - 2e-3, `${theme} T=${T.toFixed(2)}: warm chroma ${c.toFixed(4)} < ${prevC.toFixed(4)}`);
          prevC = c;
        }
        prevX = xv; prevH = Math.min(prevH, h);
      }
    }
    // the boundary the Refuter probed: 24.2 (banking heat) is not warmer than 25.2 (over the allowance)
    const chroma = (hex) => lchOf(hex)[1];
    const probe = (T) => fpFill(fpRoomState(room({ band: bands[0], measured: { temp_c: T, age_s: 5 } })), theme);
    assert(chroma(probe(25.2)) > chroma(probe(24.2)), `${theme}: over-limit is stronger than banking`);
    assert.strictEqual(probe(24.2), fpScaleColor(2.5, theme), 'the overheat tier sits on one warm step');
    assert.strictEqual(probe(24.9), probe(24.2));
    assert.strictEqual(probe(40), fpScaleColor(FP_X_MAX, theme), 'far beyond the allowance it saturates at the hottest step');
    assert.strictEqual(probe(5), fpScaleColor(FP_X_MIN, theme), 'far below the floor it saturates at the coldest step');
  }
});
check('fpFacts: the drawn area, flagged when the model area (volume / floor height) disagrees by over 15 %', () => {
  const area = (over, ctx) => fpFacts(room(over), ctx).find((x) => x[0] === 'Area');
  assert.deepStrictEqual(area({ area_m2: 15.2, volume_m3: 21.9 }, { floorHeight: 2.3 }), ['Area', '15.2 m²', '(as drawn)']);
  assert.deepStrictEqual(area({ area_m2: 9.8, volume_m3: 21.9 }, { floorHeight: 2.3 }), ['Area', '9.8 m²', ''], 'within 15 %: no qualifier');
  assert.deepStrictEqual(area({ area_m2: 15.2, volume_m3: 21.9 }, { floorHeight: 0 }), ['Area', '15.2 m²', ''], 'no floor height (roof): nothing to compare');
  assert.deepStrictEqual(area({ area_m2: 15.2, volume_m3: null }, { floorHeight: 2.3 }), ['Area', '15.2 m²', ''], 'no model volume');
  assert(fpCardHtml(room({ area_m2: 15.2, volume_m3: 21.9 }), { floorHeight: 2.3 }).includes('(as drawn)'));
});
check('fpAgo / fpAria / fpPresenceText', () => {
  assert.strictEqual(fpAgo(30), 'just now');
  assert.strictEqual(fpAgo(300), '5 min ago');
  assert.strictEqual(fpAgo(7200), '2 h ago');
  assert.strictEqual(fpAgo(null), '');
  assert.strictEqual(fpAria(room()), 'Kitchen, 22.0 °C, in comfort band');
  assert.strictEqual(fpAria(room({ heating: { on: true, kw: 1 } })), 'Kitchen, 22.0 °C, in comfort band, heating per plan');
  assert.strictEqual(fpPresenceText({ motion: true, age_s: 300 }), 'motion 5 min ago');
  assert.strictEqual(fpPresenceText({ motion: false, age_s: 7200 }), 'quiet 2 h');
  assert.strictEqual(fpPresenceText({ motion: true, age_s: 10 }), 'motion now');
  assert.strictEqual(fpPresenceText(null), null);
});
check('fpHouseSummary: plain words, singular/plural, nothing hidden', () => {
  const heating = { on: true, kw: 1 };
  const cold = (z) => room({ zone: z, measured: { temp_c: 19, age_s: 5 } });
  assert.strictEqual(fpHouseSummary([room({ zone: 'a', heating }), room({ zone: 'b', heating }), room({ zone: 'c', heating }), cold('d')]), '3 rooms heating per plan, 1 below its floor');
  assert.strictEqual(fpHouseSummary([room()]), 'No room heating planned, all in comfort band');
  assert.strictEqual(fpHouseSummary([room({ heating })]), '1 room heating per plan, all in comfort band');
  assert.strictEqual(fpHouseSummary([cold('a'), cold('b'), room({ zone: 'c', measured: { temp_c: 26, age_s: 5 } })]), 'No room heating planned, 2 below their floor, 1 above its ceiling');
  assert.strictEqual(fpHouseSummary([room({ measured: { temp_c: 22, age_s: FP_STALE_S + 1 } })]), 'No room heating planned, 1 stale sensor, all in comfort band');
  assert.strictEqual(fpHouseSummary([]), 'No plan block, heating unknown');
});
check('null heating at now is unknown (no plan block), never "off" or a made-up 0 kW', () => {
  const rows = [room({ heating: null }), room({ zone: 'g', heated: false, heating: null })];
  assert.strictEqual(fpHouseSummary(rows), 'No plan block, heating unknown, all in comfort band');
  const html = fpStatsHtml(fpStats(rows));
  assert(html.includes('<b>—</b>') && html.includes('no plan block') && !html.includes('0.00') && !html.includes('no heating planned'));
  assert(fpStatsHtml(fpStats([room({ heating: null, at: { T: 0, kind: 'plan' } })])).includes('no plan block'));
  const card = fpCardHtml(room({ heating: null }), {});
  assert(card.includes('no plan block') && !card.includes('>off<'));
});
check('a failed sensor read says so instead of "no sensor"', () => {
  const failed = room({ measured: null, sensor_failed: true });
  assert.strictEqual(fpStatusPhrase(fpRoomState(failed)), 'sensor read failed');
  assert.strictEqual(fpStatusPhrase(fpRoomState(room({ measured: null }))), 'no sensor');
  assert(fpCardHtml(failed, {}).includes('model estimate, sensor read failed'));
  // the banner: a partial failure leaves other rooms their readings
  assert(fpSensorErrText([failed, room()]).startsWith('Some sensor reads failed'));
  assert(fpSensorErrText([failed, room({ zone: 'g', measured: null })]).startsWith('The sensor read failed'));
  // a failed humidity/presence bucket marks no room; a failed roof sensor marks none on the plan
  assert.strictEqual(fpSensorErrText([room(), room({ zone: 'g' })]), 'A sensor read failed; no room temperature is affected.');
  assert(fpSensorErrText([room(), room({ zone: 'attic', floor: 'roof', measured: null, sensor_failed: true })]).startsWith('A roof sensor read failed'));
});
check('fpAttention: worst first, then heating rooms, capped', () => {
  const rows = [
    room({ zone: 'a', name: 'A', measured: { temp_c: 20.5, age_s: 5 } }),
    room({ zone: 'b', name: 'B', measured: { temp_c: 18, age_s: 5 } }),
    room({ zone: 'c', name: 'C', heating: { on: true, kw: 1.25 } }),
    room({ zone: 'd', name: 'D' }),
  ];
  const att = fpAttention(rows, 6);
  assert.deepStrictEqual(att.map((x) => [x.zone, x.text]), [['b', '−3.0 K'], ['a', '−0.5 K'], ['c', '1.3 kW planned']]);
  assert.strictEqual(fpAttention(rows, 2).length, 2);
  assert.deepStrictEqual(fpAttention(null), []);
});
check('away from now the heating reads as measured (past) or planned then (plan), never as now', () => {
  const at = (kind) => ({ T: 0, kind });
  const heating = { on: true, kw: 1.25 };
  const past = [room({ zone: 'a', heating, at: at('past') }), room({ zone: 'b', heating: null, at: at('past') })];
  assert.strictEqual(fpHouseSummary(past), '1 room heating (measured), all in comfort band');
  assert.strictEqual(fpAria(past[0]), 'Kitchen, 22.0 °C, in comfort band, heating (measured)');
  assert.deepStrictEqual(fpAttention(past).map((x) => x.text), ['1.3 kW measured']);
  const ps = fpStatsHtml(fpStats(past));
  assert(ps.includes('heating then, 1 room') && ps.includes('Measured, that block') && !ps.includes('now'));
  const unknown = [room({ heating: null, at: at('past') })];
  assert.strictEqual(fpHouseSummary(unknown), 'Heating not recorded, all in comfort band');
  assert(fpStatsHtml(fpStats(unknown)).includes('heating not recorded') && fpStatsHtml(fpStats(unknown)).includes('<b>—</b>'));
  const plan = fpStatsHtml(fpStats([room({ heating, at: at('plan') })]));
  assert(plan.includes('heating then, 1 room') && plan.includes('Planned, that block'));
  // the band /api/rooms carries is the one in force now: the card says so away from now
  assert(fpCardHtml(room({ at: at('plan') }), {}).includes('in comfort band, against the current band'));
  assert(!fpCardHtml(room(), {}).includes('current band'));
});
check('fpStats: rooms in band, coldest, planned heating and the area-weighted mean, over banded rooms only', () => {
  const rows = [
    room({ zone: 'a', name: 'A', area_m2: 10, measured: { temp_c: 22, age_s: 5 } }),
    room({ zone: 'b', name: 'B', area_m2: 30, measured: { temp_c: 20, age_s: 5 }, heating: { on: true, kw: 0.8 } }),
    room({ zone: 'g', name: 'Garage', floor: 'ground', band: null, measured: { temp_c: 12, age_s: 5 } }),
    room({ zone: 'attic', name: 'Attic', floor: 'roof', band: { t_min_now: 0, t_max_now: 40 }, measured: { temp_c: 5, age_s: 5 } }),
    room({ zone: 'c', name: 'C', area_m2: 10, measured: null, heating: { on: true, kw: 0.5 } }),
  ];
  const s = fpStats(rows);
  assert.deepStrictEqual([s.banded, s.inBand, s.heating], [2, 1, 2]);
  assert(Math.abs(s.kw - 1.3) < 1e-9);
  assert.deepStrictEqual(s.coldest, { zone: 'b', name: 'B', temp: 20 }, 'the garage (no band) and the attic (roof) never count as coldest');
  assert(Math.abs(s.meanC - (10 * 22 + 30 * 20) / 40) < 1e-9);
  const empty = fpStats([]);
  assert.deepStrictEqual([empty.banded, empty.coldest, empty.meanC, empty.kw], [0, null, null, 0]);
  const html = fpStatsHtml(fpStats([room({ name: '<i>K</i>' })]));
  assert(html.includes('1<small>/1</small>') && html.includes('coldest, &lt;i&gt;K') && html.includes('no heating planned now') && !/NaN|null|undefined/.test(html));
  assert(fpStatsHtml(empty).includes('—') && !/NaN|null|undefined/.test(fpStatsHtml(empty)));
  assert(fpStatsHtml(s).includes('heating now, 2 rooms') && fpStatsHtml(s).includes('1.30<small>kW'));
});
check('fpAtticRows: measured first, else the estimate marked est., else a dash', () => {
  const rows = fpAtticRows([
    room({ zone: 'attic', name: 'Attic', floor: 'roof', band: null, measured: { temp_c: 19.94, age_s: 60 } }),
    room({ zone: 'garrage_attic', name: 'Garage attic', floor: 'roof', band: null, measured: null, estimate_c: 19.65 }),
    room({ zone: 'x', name: 'X', floor: 'roof', band: null, measured: null, estimate_c: null }),
    room({ zone: 'kitchen' }),
  ]);
  assert.deepStrictEqual(rows.map((r) => [r.zone, r.value, r.est]), [['attic', '19.9°', false], ['garrage_attic', '19.6°', true], ['x', '—', false]]);
  assert.strictEqual(fpAtticRows([room({ floor: 'roof', band: null, measured: { temp_c: 18, age_s: FP_STALE_S + 1 } })])[0].stale, true);
  assert.deepStrictEqual(fpAtticRows(null), []);
});
check('fpNorthDeg: the compass needle follows the drawing (north 50 degrees counter-clockwise from up in the plan frame)', () => {
  assert(Math.abs(fpNorthDeg([1, 0, 0, 1, 0, 0]) - -50) < 1e-9);
  assert(Math.abs(fpNorthDeg([1, 0, 0, 1, 7, -3]) - -50) < 1e-9, 'translation does not turn it');
  const c = Math.cos(Math.PI / 2), s = Math.sin(Math.PI / 2);
  assert(Math.abs(fpNorthDeg([c, s, -s, c, 0, 0]) - 40) < 1e-9, 'a 90 degree clockwise screen rotation turns it by 90');
  const L = fpLayout({ floors: [{ id: 'ground', outline: [[0, 0], [10, 0], [10, 5], [0, 5]] }, { id: 'first', outline: [[0, 0], [10, 0], [10, 5], [0, 5]] }] }, false);
  const d = fpNorthDeg(L.stacked[0]);
  assert(isFinite(d) && d < 0 && d > -180, `stacked ${d}`);
});

// A minimal geometry shaped like floorplan.json: a ground footprint with a garage wing and a first floor over its left part.
const GEOM = {
  floors: [
    { id: 'ground', outline: [[0, 0], [22, 0], [22, 15], [15, 15], [15, 8], [0, 8]], rooms: [{ zone: 'kitchen', label: [4.5, 5.7], polygon: [[0, 4], [9, 4], [9, 8], [0, 8]] }] },
    { id: 'first', outline: [[0, 0], [22, 0], [22, 8], [0, 8]], rooms: [{ zone: 'guestroom', label: [2.5, 3], polygon: [[0, 0], [6, 0], [6, 8], [0, 8]] }] },
    { id: 'roof', outline: [], rooms: [{ zone: 'attic', polygon: [[0, 0], [22, 0], [22, 8], [0, 8]] }, { zone: 'garrage_attic', polygon: [[15, 8], [22, 8], [22, 15], [15, 15]] }] },
  ],
};
const near = (a, b, eps = 1e-9) => a.every((v, i) => Math.abs(v - b[i]) < eps);
check('planMatrix: t=1 is the requested mode, t=0 the other, in between interpolates', () => {
  for (const narrow of [false, true]) {
    const L = fpLayout(GEOM, narrow);
    assert.strictEqual(L.stacked.length, 2, 'two floors drawn; the roof is not a layer');
    for (let i = 0; i < 2; i++) {
      assert(near(planMatrix('stacked', i, 1, L), L.stacked[i]));
      assert(near(planMatrix('stacked', i, 0, L), L.flat[i]));
      assert(near(planMatrix('flat', i, 1, L), L.flat[i]));
      assert(near(planMatrix('flat', i, 0, L), L.stacked[i]));
      assert(near(planMatrix('stacked', i, 0.3, L), planMatrix('flat', i, 0.7, L)));
      const mid = planMatrix('stacked', i, 0.5, L);
      mid.forEach((v, k) => assert(Math.abs(v - (L.stacked[i][k] + L.flat[i][k]) / 2) < 1e-9));
    }
  }
});
check('stacked projection: affine, not degenerate; flat is the true shape', () => {
  const L = fpLayout(GEOM, false);
  for (const m of L.stacked) assert(Math.abs(m[0] * m[3] - m[1] * m[2]) > 0.2, 'invertible');
  assert(near(L.flat[0].slice(0, 4), [1, 0, 0, 1]) && near(L.flat[1].slice(0, 4), [1, 0, 0, 1]));
  // screen-up faces azimuth 50 in the plan frame: plan -y maps toward the top of the screen in stacked mode
  const a = project([0, 0], L.stacked[0]), b = project([0, 8], L.stacked[0]);
  assert(b[1] > a[1], 'plan +y (the front of the house) is lower on screen');
});
check('stacked mode puts the first floor above the ground floor; the guest room label sits over the kitchen label', () => {
  const L = fpLayout(GEOM, false);
  const kitchen = project(GEOM.floors[0].rooms[0].label, L.stacked[0]);
  const guest = project(GEOM.floors[1].rooms[0].label, L.stacked[1]);
  assert(guest[1] < kitchen[1], 'guest room is higher on screen');
  assert(Math.abs(guest[0] - kitchen[0]) < 4, `horizontally close (${guest[0]} vs ${kitchen[0]})`);
  // the lifted first floor clears the ground footprint it covers (no overlap that would hide rooms)
  const bottom1 = Math.max(...GEOM.floors[1].outline.map((p) => project(p, L.stacked[1])[1]));
  const top0 = Math.min(...GEOM.floors[0].outline.filter((p) => p[1] <= 8).map((p) => project(p, L.stacked[0])[1]));
  assert(bottom1 < top0, 'first floor slab is entirely above the ground main block');
  // flat wide: side by side, same orientation; flat narrow: one under the other
  const wide = fpLayout(GEOM, false).flat, narrow = fpLayout(GEOM, true).flat;
  assert(wide[1][4] > 22 && wide[1][5] === 0 && wide[0][4] === 0);
  assert(narrow[1][4] === 0 && narrow[0][5] > 8);
});
check('the shipped floorplan.json stacks with the guest room over the kitchen and the first floor clear of the ground main block', () => {
  const real = JSON.parse(fs.readFileSync(path.join(__dirname, 'floorplan.json'), 'utf8'));
  const L = fpLayout(real, false);
  const lab = (fl, zone) => real.floors[fl].rooms.find((r) => r.zone === zone).label;
  const kitchen = project(lab(0, 'kitchen'), L.stacked[0]);
  const guest = project(lab(1, 'guestroom'), L.stacked[1]);
  assert(guest[1] < kitchen[1] - 5, 'guest room label is well above the kitchen label');
  assert(Math.abs(guest[0] - kitchen[0]) < 4, `and within a few metres horizontally (${guest[0].toFixed(1)} vs ${kitchen[0].toFixed(1)})`);
  const main = real.floors[0].outline.filter((p) => p[1] <= 8.2);
  const bottom1 = Math.max(...real.floors[1].outline.map((p) => project(p, L.stacked[1])[1]));
  const top0 = Math.min(...main.map((p) => project(p, L.stacked[0])[1]));
  assert(bottom1 + 0.5 < top0, `slab bottom ${bottom1.toFixed(2)} above ground top ${top0.toFixed(2)}`);
  for (const f of real.floors) for (const r of f.rooms) assert(r.zone && r.label && r.polygon.length >= 3, `room ${r.zone}`);
});
check('fpOpeningShape / fpPaneShape: the cut through the wall, the floor marks, the standing glass', () => {
  const nums = (d) => (d.match(/-?\d+(\.\d+)?/g) || []).map(Number);
  const win = { kind: 'window', type: 'window', a: [1, 8], b: [2.5, 8], n: [0, 1], depth_m: 0.4, sill_m: 0.9, head_m: 2.3 };
  const w = fpOpeningShape(win);
  assert.strictEqual(w.rect, 'M1 7.8L2.5 7.8L2.5 8.2L1 8.2Z', 'the footprint spans both faces of the wall');
  assert.strictEqual(w.jambs, 'M1 7.8L1 8.2M2.5 7.8L2.5 8.2');
  assert.strictEqual(w.floor, '', 'a window leaves nothing on the floor');
  const pane = fpPaneShape(win, [0, -0.1], [0, -0.25]);
  assert.strictEqual(pane.sill, 'M1 7.9L2.5 7.9');
  assert.strictEqual(pane.pane, 'M1 7.9L2.5 7.9L2.5 7.75L1 7.75Z');
  assert.strictEqual(pane.frame, 'M1 7.9L1 7.75M2.5 7.9L2.5 7.75M1 7.75L2.5 7.75');
  // swing door: leaf from the hinge, arc from the shut position to the open leaf, swept the short way
  const door = { kind: 'door', type: 'swing', a: [2, 3], b: [2.8, 3], n: [0, 1], depth_m: 0.3, swing: { hinge: [2, 3.15], r: 0.8, leaf: [2, 2.35], closed: [2.8, 3.15] } };
  const d = fpOpeningShape(door);
  assert.strictEqual(d.floor, 'M2 3.15L2 2.35M2.8 3.15A0.8 0.8 0 0 0 2 2.35');
  const mirrored = fpOpeningShape({ ...door, swing: { ...door.swing, leaf: [2, 3.95] } });
  assert(mirrored.floor.endsWith('0 0 1 2 3.95'), 'the other swing side sweeps the other way');
  const slide = nums(fpOpeningShape({ ...door, type: 'sliding', swing: undefined }).floor);
  assert.strictEqual(slide.length, 8, 'two sliding leaves');
  assert(slide.every((v, i) => (i % 2 ? Math.abs(v - 3) <= 0.04 + 1e-9 : v >= 2 - 1e-9 && v <= 2.8 + 1e-9)), 'the leaves stay inside the opening');
  const sect = nums(fpOpeningShape({ ...door, type: 'sectional', swing: undefined }).floor);
  assert.deepStrictEqual(sect, [2, 2.91, 2.8, 2.91], 'the sectional leaf just inside the inner face (n points outside)');
});
check('the shipped floorplan.json: every room has a door, every opening is cut out of the walls, heights are sane', () => {
  const real = JSON.parse(fs.readFileSync(path.join(__dirname, 'floorplan.json'), 'utf8'));
  const inside = (pt, ring) => { let c = false; for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) { const [xi, yi] = ring[i], [xj, yj] = ring[j]; if ((yi > pt[1]) !== (yj > pt[1]) && pt[0] < (xj - xi) * (pt[1] - yi) / (yj - yi) + xi) c = !c; } return c; };
  for (const f of real.floors.filter((x) => x.id !== 'roof')) {
    const ops = f.openings;
    assert(ops.length > 0 && !('doors' in f.details) && !('windows' in f.details), `${f.id}: openings replace the old detail lists`);
    for (const r of f.rooms) assert(ops.some((o) => o.kind === 'door' && o.rooms.includes(r.zone)), `${f.id}/${r.zone} has no door`);
    for (const o of ops) {
      assert(['door', 'window'].includes(o.kind) && o.width_m > 0.5 && o.depth_m >= 0.08 && o.depth_m < 0.7, `${f.id} opening ${JSON.stringify(o)}`);
      assert(Math.abs(Math.hypot(o.b[0] - o.a[0], o.b[1] - o.a[1]) - o.width_m) < 0.02, 'width matches the jambs');
      assert.strictEqual(o.rooms.includes('outside'), o.azimuth_deg != null, 'exterior openings carry an azimuth');
      if (o.kind === 'window') assert(o.sill_m > 0 && o.head_m > o.sill_m && o.head_m <= f.height_m + 1e-9, `${f.id} window heights ${o.sill_m}/${o.head_m}`);
      if (o.type === 'swing') assert(o.swing && o.swing.r > 0.5, 'a swing door carries its arc');
      // the opening's centre (and a point near each jamb) is not covered by any wall ring
      const c = [(o.a[0] + o.b[0]) / 2, (o.a[1] + o.b[1]) / 2];
      const nearA = [o.a[0] + (o.b[0] - o.a[0]) * 0.1, o.a[1] + (o.b[1] - o.a[1]) * 0.1];
      const nearB = [o.a[0] + (o.b[0] - o.a[0]) * 0.9, o.a[1] + (o.b[1] - o.a[1]) * 0.9];
      for (const p of [c, nearA, nearB]) assert(!f.walls.some((w) => inside(p, w)), `${f.id} ${o.kind} at ${o.a}..${o.b}: wall still across it`);
    }
  }
  const ground = real.floors.find((x) => x.id === 'ground');
  const ext = ground.openings.filter((o) => o.kind === 'door' && o.rooms.includes('outside')).map((o) => `${o.type}:${o.rooms.join('/')}`).sort();
  assert.deepStrictEqual(ext, ['glazed:kitchen/outside', 'glazed:livingroom/outside', 'sectional:garrage/outside', 'sectional:garrage/outside', 'swing:entrance/outside', 'swing:garrage/outside']);
  assert(ground.openings.some((o) => o.type === 'swing' && o.rooms.join() === 'entrance,garrage'), 'the house <-> garage door');
  // owner, 2026-10-10: the WC, stair-hall and living-room NE windows are 20 cm clerestories under the ceiling
  const high = ground.openings.filter((o) => o.type === 'clerestory');
  assert.deepStrictEqual(high.map((o) => o.rooms.find((z) => z !== 'outside')).sort(), ['ground_hall', 'livingroom', 'toilet']);
  for (const o of high) assert(Math.abs(o.head_m - ground.height_m) < 1e-9 && Math.abs(o.head_m - o.sill_m - 0.2) < 1e-9 && o.azimuth_deg === 50, JSON.stringify(o));
  // first floor per the architect's 2NP plan (owner, 2026-10-10): the bathroom is the top-left corner room
  // (tape-measured 9.51 m2), entered from the hall, with no window; the bedroom opens from the hall and
  // has the SW window; the walk-in closet is entered from the bedroom
  const first = real.floors.find((x) => x.id === 'first');
  const bath = first.rooms.find((r) => r.zone === 'first_floor_bathroom');
  assert.strictEqual(bath.area_m2, 9.51);
  const doors = first.openings.filter((o) => o.kind === 'door').map((o) => o.rooms.join());
  for (const d of ['first_floor_bathroom,first_floor_hall', 'bedroom,first_floor_hall', 'bedroom,first_floor_closet']) assert(doors.includes(d), d);
  const windows = first.openings.filter((o) => o.kind === 'window').map((o) => o.rooms.find((z) => z !== 'outside'));
  assert(!windows.includes('first_floor_bathroom') && windows.includes('bedroom'), windows.join());
});
check('fpScreenToPlan inverts the linear part (the hover lift is exactly 4 px up on screen)', () => {
  const L = fpLayout(GEOM, false);
  for (const m of [L.stacked[0], L.flat[0], planMatrix('stacked', 0, 0.4, L)]) {
    const [px, py] = fpScreenToPlan(m, 0, -4);
    const back = [m[0] * px + m[2] * py, m[1] * px + m[3] * py];
    assert(near(back, [0, -4], 1e-9), `lift ${back}`);
  }
});
check('fpBounds contains every projected outline point and grows when stacked', () => {
  const L = fpLayout(GEOM, false);
  for (const s of [0, 0.5, 1]) {
    const b = fpBounds(GEOM, L, s, 30);
    GEOM.floors.filter((f) => f.id !== 'roof').forEach((f, i) => f.outline.forEach((p) => {
      const q = project(p, planMatrix('stacked', i, s, L));
      assert(q[0] >= b.x && q[0] <= b.x + b.w && q[1] >= b.y && q[1] <= b.y + b.h, `s=${s} point ${p} outside bounds`);
    }));
  }
  assert(fpBounds(GEOM, L, 1, 30).h > fpBounds(GEOM, L, 0, 30).h * 0.8);
});
check('label helpers: scale floor, pill size, name only when the room is big enough', () => {
  assert.strictEqual(fpLabelScale(40), 1);
  assert.strictEqual(fpLabelScale(5), 0.8);
  assert(fpLabelScale(23) > 0.8 && fpLabelScale(23) < 1);
  const withName = fpLabelBox('Living room', true, true, 0), bare = fpLabelBox('Living room', true, false, 0);
  assert(withName.h > bare.h && withName.w >= bare.w);
  assert(fpLabelBox('WC', true, false, 12).w > fpLabelBox('WC', true, false, 0).w);
  assert.strictEqual(fpShowName(40, 0.6, 30, 11), true);
  assert.strictEqual(fpShowName(2.9, 0.6, 12, 2), false);
});
check('fpResolveLabels: priority order, name dropped first, then compact, then hidden', () => {
  const lv = (w) => [{ name: 'full', w: 80, h: 31 }, { name: 'bare', w: 46, h: 21 }, { name: 'compact', w: 38, h: 17 }].map((l) => ({ ...l, w: l.w * w }));
  // two pills 65 px apart: the first keeps its full pill, the second cannot fit even a bare one (40+23+3 > 65) and goes compact
  const pair = fpResolveLabels([{ id: 'b', x: 65, y: 0, prio: 1, levels: lv(1) }, { id: 'a', x: 0, y: 0, prio: 0, levels: lv(1) }], 3);
  assert.deepStrictEqual(pair, { a: 'full', b: 'compact' });
  // far apart: everybody keeps the richest level
  assert.deepStrictEqual(fpResolveLabels([{ id: 'a', x: 0, y: 0, prio: 0, levels: lv(1) }, { id: 'b', x: 300, y: 0, prio: 1, levels: lv(1) }], 3), { a: 'full', b: 'full' });
  // a stack on one point: the lowest priority ends hidden, the room is still reported (never dropped from the result)
  const stack = fpResolveLabels([0, 1, 2].map((i) => ({ id: `r${i}`, x: 0, y: 0, prio: i, levels: lv(1) })), 3);
  assert.deepStrictEqual(stack, { r0: 'full', r1: 'hidden', r2: 'hidden' });
  // the bare level is chosen when it clears the neighbour but the full one does not
  const mid = fpResolveLabels([{ id: 'a', x: 0, y: 0, prio: 0, levels: lv(1) }, { id: 'b', x: 70, y: 0, prio: 1, levels: lv(1) }], 3);
  assert.strictEqual(mid.b, 'bare');
  // ties keep input order; no overlap between any two shown pills (the invariant)
  const items = Array.from({ length: 12 }, (_, i) => ({ id: `z${i}`, x: (i % 4) * 30, y: Math.floor(i / 4) * 14, prio: i % 3, levels: lv(1) }));
  const out = fpResolveLabels(items, 3);
  const shown = items.filter((it) => out[it.id] !== 'hidden').map((it) => ({ x: it.x, y: it.y, ...it.levels.find((l) => l.name === out[it.id]) }));
  for (let i = 0; i < shown.length; i++) for (let j = i + 1; j < shown.length; j++) {
    const a = shown[i], b = shown[j];
    assert(!(Math.abs(a.x - b.x) < (a.w + b.w) / 2 + 3 && Math.abs(a.y - b.y) < (a.h + b.h) / 2 + 3), `overlap ${i}/${j}`);
  }
  assert.strictEqual(Object.keys(out).length, 12);
});
check('fpLabelPrio: heated and out-of-band rooms first, then larger rooms; compact box is the smallest', () => {
  const heated = room({ heating: { on: true, kw: 1 } });
  const cold = room({ measured: { temp_c: 19, age_s: 5 } });
  const calm = room();
  assert(fpLabelPrio(heated, 5) < fpLabelPrio(calm, 40));
  assert(fpLabelPrio(cold, 5) < fpLabelPrio(calm, 40));
  assert(fpLabelPrio(calm, 40) < fpLabelPrio(calm, 10));
  const c = fpCompactBox(true, false), f = fpLabelBox('Kitchen', true, false, 0);
  assert(c.w < f.w && c.h < f.h);
  assert(fpCompactBox(true, true).w > c.w);
});
check('fpPlaceCard: beside the room on the freer side, never over it, always inside the viewport', () => {
  const R = (l, t, r, b) => ({ left: l, top: t, right: r, bottom: b });
  const inside = (c, w, h, vw, vh) => c.left >= 10 && c.top >= 10 && c.left + w <= vw - 10 && c.top + h <= vh - 10;
  const cov = (c, r, w, h) => c.left < r.right && c.left + w > r.left && c.top < r.bottom && c.top + h > r.top;
  let c = fpPlaceCard(R(100, 300, 300, 400), 300, 440, 1400, 900, 14, 10);
  assert.strictEqual(c.side, 'right');
  assert.strictEqual(c.left, 314);
  c = fpPlaceCard(R(900, 300, 1300, 400), 300, 440, 1400, 900, 14, 10);
  assert.strictEqual(c.side, 'left');
  assert.strictEqual(c.left, 900 - 14 - 300);
  // vertically centred on the room but clamped to the viewport
  c = fpPlaceCard(R(100, 20, 300, 60), 300, 440, 1400, 900, 14, 10);
  assert.strictEqual(c.top, 10);
  c = fpPlaceCard(R(100, 850, 300, 890), 300, 440, 1400, 900, 14, 10);
  assert.strictEqual(c.top, 900 - 440 - 10);
  // a wide room that leaves no room on either side: below or above, still not covering it
  const wide = R(20, 100, 1380, 300);
  c = fpPlaceCard(wide, 300, 440, 1400, 900, 14, 10);
  assert(!cov(c, wide, 300, 440) && inside(c, 300, 440, 1400, 900), `${c.side} ${c.left},${c.top}`);
  // sweep: whenever some placement can avoid the room, the answer does
  for (let l = 0; l <= 1100; l += 110) for (let t = 0; t <= 700; t += 140) {
    const r = R(l, t, l + 220, t + 160);
    const p = fpPlaceCard(r, 300, 440, 1400, 900, 14, 10);
    assert(inside(p, 300, 440, 1400, 900), `inside at ${l},${t}`);
    assert(!cov(p, r, 300, 440), `covers at ${l},${t}`);
  }
});
check('fpScrollDelta: keep the card title and the room clear of the sticky topbar', () => {
  assert.strictEqual(fpScrollDelta(300, 340, 120, 60, 800), 0);
  assert.strictEqual(fpScrollDelta(300, 340, 20, 60, 800), 20 - 72, 'title hidden under the bar, whole stretch fits: scroll up to reveal it');
  assert.strictEqual(fpScrollDelta(30, 60, -400, 60, 500), 30 - 72, 'title too far above to keep: at least free the room');
  assert.strictEqual(fpScrollDelta(700, 790, 100, 60, 800), 790 - 800 + 12, 'room below the fold: scroll down');
});
check('card html: values lead, null rows are omitted, backend strings are escaped, the chart is one temperature axis', () => {
  const r = room({
    name: '<b>Kitchen</b>', humidity: { pct: 54.2, age_s: 60 }, presence: { motion: false, age_s: 600 }, solar_scale: 0.92, disturbance_w: -74,
    warmth: { value_eur_per_kh: 0.0085, break_even_eur_per_kwh: 0.0553, kh: 23 }, heat_credit_eur_per_kwh: 0, plan_heat_kwh_12h: 3.25,
    heating: { on: true, kw: 1.2 }, heating_today_kwh: 0.5,
    plan: { t: ['2026-10-09T20:15:00Z', '2026-10-09T20:30:00Z'], dt_minutes: [15, 15], heat_kw: [1.2, 0], temp_c: [22.4, 22.6] },
  });
  const html = fpCardHtml(r, { floorName: 'Ground floor', rate: 24, nowMs: Date.parse('2026-10-09T20:20:00Z'), theme: 'dark' });
  assert(!html.includes('<b>Kitchen</b>') && html.includes('&lt;b&gt;Kitchen'));
  assert(html.includes('22.0') && html.includes('in comfort band') && html.includes('on, 1.20 kW') && html.includes('Heating now') && html.includes('committed plan, this block') && html.includes('0.50 kWh today'));
  assert(html.includes('54 %') && html.includes('quiet 10 min') && html.includes('× 0.92') && html.includes('−74 W'));
  assert.deepStrictEqual(fpFacts(r, {}).filter((x) => ['Solar gain', 'Disturbance'].includes(x[0])).map((x) => x[2]), ['model estimate', 'model estimate']);
  assert(html.includes('1.33 Kč/kWh'), 'break-even converted with the plan rate');
  assert(html.includes('0.204 Kč/K·h') && !html.includes('€'), 'warmth value in Kč too: one currency on the card');
  const noRate = fpCardHtml(r, { floorName: 'Ground floor', nowMs: Date.parse('2026-10-09T20:20:00Z'), theme: 'dark' });
  assert(noRate.includes('€0.0085/K·h') && noRate.includes('€0.055/kWh') && !noRate.includes('Kč'), 'no plan rate: both in €');
  assert(!html.includes('Heat credit'), 'a zero heat credit row is omitted');
  assert(html.includes('3.25 kWh heat planned') && html.includes('Next 12 h'));
  assert.strictEqual((html.match(/<svg class="fp-chart"/g) || []).length, 1);
  assert(!html.includes('NaN') && !html.includes('undefined') && !html.includes('null'));
  const bare = fpCardHtml(room({ measured: null, band: null, heating: null, estimate_c: null, area_m2: null, volume_m3: null, ach: null, max_heat_kw: null, heated: false }), {});
  assert(bare.includes('no sensor') && bare.includes('no heater') && !bare.includes('NaN') && !bare.includes('null'));
  assert.strictEqual(fpFacts(room({ area_m2: null, volume_m3: null, ach: null, max_heat_kw: null }), {}).length, 0);
});
check('plan chart and gauge: honest on missing data, finite numbers otherwise', () => {
  assert.strictEqual(fpPlanChart(room({ plan: null }), 0), '');
  assert.strictEqual(fpPlanChart(room({ plan: { t: [], dt_minutes: [], heat_kw: [], temp_c: [] } }), 0), '');
  const flat = fpPlanChart(room({ estimate_c: null, plan: { t: ['2026-10-09T20:15:00Z'], dt_minutes: [60], heat_kw: [0], temp_c: [22] } }), 0);
  assert(flat.includes('<svg') && !/NaN|Infinity/.test(flat));
  assert.strictEqual(fpGauge(fpRoomState(room({ band: null })), 22), '');
  const g = fpGauge(fpRoomState(room()), 22.1);
  assert(g.includes('floor 21°') && g.includes('ceiling 24°') && !/NaN/.test(g));
});

// ---- 3D view helpers (fp3d.js, lazy-loaded with three.js) ----
const fp3dSrc = fs.readFileSync(path.join(__dirname, 'fp3d.js'), 'utf8');
const fp3dScope = {};
// eslint-disable-next-line no-new-func
new Function(
  'scope',
  `
  ${['FP3D_DOOR_HEAD', 'FP3D_GARAGE_HEAD'].map((n) => extractConst(fp3dSrc, n)).join('\n')}
  ${['fp3dSceneDir', 'fp3dFloorLevels', 'fp3dOpeningParts', 'fp3dFootprint', 'fp3dLeaf', 'fp3dStrip', 'fp3dInside', 'fp3dWithin',
    'fp3dParseColor', 'fp3dDayFactor', 'fp3dLightDir', 'fp3dClockHour', 'fp3dFireboxParts', 'fp3dFlicker', 'fp3dSunTint', 'fp3dSegDist'].map((n) => extractFunction(fp3dSrc, n)).join('\n')}
  ${['FP_FIRE_W', 'FP_SUN_MIN_INC', 'FP_SUN_MIN_EL', 'FP_COMPASS_WORDS', 'FP_STEP_MS', 'FP_PAST_STEPS', 'FP_SERIES_WINDOW_MS', 'FP_HISTORY_WINDOW_MS', 'HEAT_ON_KW', 'FP_FULL_OLDER_MS', 'FP_FULL_MAX_LAG_MS', 'modeLabel'].map((n) => extractConst(src, n)).join('\n')}
  ${src.match(/const MODE = \{[\s\S]*?\n\};/)[0]}
  ${['fpFireLikely', 'fpFireboxPath', 'fpVoidRoom', 'fpSunPosition', 'fpLocalDayStart', 'fpSunTimes', 'fpPlanAzimuth', 'fpWallIncidence', 'fpOutlineFacades',
    'fpSunlitWindows', 'fpCompassWord', 'fpSunCaption', 'fpHm',
    'isRelayOn', 'fpBlockIndex', 'fpSeriesAt', 'fpWindowMeanAt', 'fpTimeKind', 'fpTimelineRange', 'fpTimeLabel', 'fpPlanValueAt', 'fpRoomAt', 'fpFullPlanUse', 'fpReadoutAt', 'fpReadoutItems'].map((n) => extractFunction(src, n)).join('\n')}
  Object.assign(scope, { fp3dSunPosition: fpSunPosition, fp3dSceneDir, fp3dFloorLevels, fp3dOpeningParts, fp3dFootprint, fp3dLeaf, fp3dStrip, fp3dInside, fp3dWithin,
    fp3dParseColor, fp3dDayFactor, fp3dLightDir, fp3dClockHour, fp3dFireboxParts, fp3dFlicker, fp3dSunTint, fp3dSegDist, fpFireLikely, fpFireboxPath, fpVoidRoom,
    fpLocalDayStart, fpSunTimes, fpPlanAzimuth, fpWallIncidence, fpOutlineFacades, fpSunlitWindows, fpCompassWord, fpSunCaption, fpHm,
    fpBlockIndex, fpSeriesAt, fpWindowMeanAt, FP_SERIES_WINDOW_MS, fpTimeKind, fpTimelineRange, fpTimeLabel, fpPlanValueAt, fpRoomAt, fpFullPlanUse, fpReadoutAt, fpReadoutItems });
  `
)(fp3dScope);
const { fp3dSunPosition, fp3dSceneDir, fp3dFloorLevels, fp3dOpeningParts, fp3dFootprint, fp3dLeaf, fp3dStrip, fp3dInside, fp3dWithin,
  fp3dParseColor, fp3dDayFactor, fp3dLightDir, fp3dClockHour, fp3dFireboxParts, fp3dFlicker, fp3dSunTint, fp3dSegDist, fpFireLikely, fpFireboxPath, fpVoidRoom,
  fpLocalDayStart, fpSunTimes, fpPlanAzimuth, fpWallIncidence, fpOutlineFacades, fpSunlitWindows, fpCompassWord, fpSunCaption, fpHm,
  fpBlockIndex, fpSeriesAt, fpWindowMeanAt, FP_SERIES_WINDOW_MS, fpTimeKind, fpTimelineRange, fpTimeLabel, fpPlanValueAt, fpRoomAt, fpFullPlanUse, fpReadoutAt, fpReadoutItems } = fp3dScope;
const realGeom = JSON.parse(fs.readFileSync(path.join(__dirname, 'floorplan.json'), 'utf8'));
const within = (a, b, tol, what) => assert(Math.abs(a - b) <= tol, `${what}: ${a} vs ${b} (tol ${tol})`);

check('fpSunPosition: matches the NREL SPA reference case and the house at midsummer noon', () => {
  // Reda & Andreas, NREL SPA report, worked example: 2003-10-17 12:30:30 local (UTC-7), Golden CO.
  // Topocentric azimuth 194.34024 deg; elevation without refraction 39.872046 deg (zenith 50.11162 incl. refraction).
  const s = fp3dSunPosition(Date.parse('2003-10-17T19:30:30Z'), 39.742476, -105.1786);
  within(s.azimuth, 194.34024, 0.1, 'SPA azimuth');
  within(s.elevation, 39.872046, 0.1, 'SPA elevation');
  // the house (49.49 N, 17.43 E) at the June solstice: the day's highest sun is 90 - 49.49 + 23.44 deg, due south
  let best = { elevation: -99 };
  for (let m = 0; m < 1440; m++) {
    const p = fp3dSunPosition(Date.parse('2026-06-21T00:00:00Z') + m * 60000, 49.49, 17.43);
    if (p.elevation > best.elevation) best = p;
  }
  within(best.elevation, 63.95, 0.15, 'midsummer noon elevation');
  within(best.azimuth, 180, 1, 'midsummer noon azimuth');
  // morning sun in the east, evening in the west, night well below the horizon
  assert(fp3dSunPosition(Date.parse('2026-10-10T07:00:00Z'), 49.49, 17.43).azimuth < 135);
  assert(fp3dSunPosition(Date.parse('2026-10-10T14:30:00Z'), 49.49, 17.43).azimuth > 215);
  assert(fp3dSunPosition(Date.parse('2026-10-10T22:00:00Z'), 49.49, 17.43).elevation < -30);
});

check('fp3dSceneDir: the floorplan frame maps compass azimuths to scene directions (and agrees with the 2D compass)', () => {
  const up = realGeom.frame.up_azimuth_deg;
  assert.strictEqual(up, 50);
  const v = (az, el) => fp3dSceneDir(az, el, up).map((x) => Math.round(x * 1e6) / 1e6 + 0);
  assert.deepStrictEqual(v(50, 0), [0, 0, -1]); // plan screen-up is -z
  assert.deepStrictEqual(v(realGeom.frame.right_azimuth_deg, 0), [1, 0, 0]); // plan +x faces azimuth 140
  assert.deepStrictEqual(v(230, 0), [0, 0, 1]);
  assert.deepStrictEqual(v(123, 90), [0, 1, 0]); // the zenith
  // true north in the scene is the 2D plan's compass needle (fpNorthDeg on the unrotated plan)
  const n = fp3dSceneDir(0, 0, up), deg = fpNorthDeg([1, 0, 0, 1, 0, 0]) * Math.PI / 180;
  within(n[0], Math.sin(deg), 1e-9, 'north x');
  within(n[2], -Math.cos(deg), 1e-9, 'north z');
  // a 30 degree sun is 30 degrees up and unit length
  const s = fp3dSceneDir(212, 30, up);
  within(Math.hypot(...s), 1, 1e-12, 'unit');
  within(s[1], 0.5, 1e-12, 'elevation');
});

check('fp3dFloorLevels: storeys stack at their real heights, the upper one on a slab; the roof is not a storey', () => {
  const L = fp3dFloorLevels(realGeom, 0.25);
  assert.deepStrictEqual(Object.keys(L).sort(), ['first', 'ground']);
  assert.deepStrictEqual(L.ground, { base: 0, height: 2.55 });
  within(L.first.base, 2.8, 1e-9, 'first floor base');
  assert.strictEqual(L.first.height, 2.3);
});

check('fp3dOpeningParts: wall below sills and above heads, glass in windows, 20 cm clerestories, lintels over doors', () => {
  const r = (parts) => parts.map((p) => [p.kind, +p.y0.toFixed(3), +p.y1.toFixed(3)]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'window', type: 'window', sill_m: 0.9, head_m: 2.3 }, 2.55)), [['solid', 0, 0.9], ['glass', 0.9, 2.3], ['solid', 2.3, 2.55]]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'window', type: 'clerestory', sill_m: 2.35, head_m: 2.55 }, 2.55)), [['solid', 0, 2.35], ['glass', 2.35, 2.55]]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'window', type: 'window', sill_m: 0.8, head_m: 2.3 }, 2.3)), [['solid', 0, 0.8], ['glass', 0.8, 2.3]]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'door', type: 'swing' }, 2.55)), [['solid', 2, 2.55]]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'door', type: 'glazed', sill_m: 0, head_m: 2.3 }, 2.55)), [['glass', 0, 2.3], ['solid', 2.3, 2.55]]);
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'door', type: 'sectional' }, 2.55)), [['panel', 0, 2.2], ['solid', 2.2, 2.55]]);
  // a head above the storey is clamped to it, never a negative lintel
  assert.deepStrictEqual(r(fp3dOpeningParts({ kind: 'window', sill_m: 1, head_m: 3 }, 2.3)), [['solid', 0, 1], ['glass', 1, 2.3]]);
  // every real window fills its hole from the floor to the ceiling without a gap; every door keeps a hole below its head
  for (const f of realGeom.floors.filter((x) => x.id !== 'roof')) {
    for (const o of f.openings) {
      const parts = fp3dOpeningParts(o, f.height_m);
      for (let i = 1; i < parts.length; i++) within(parts[i].y0, parts[i - 1].y1, 1e-9, `${f.id} ${o.type} contiguous`);
      if (o.kind === 'window') { within(parts[0].y0, 0, 1e-9, 'window from floor'); within(parts[parts.length - 1].y1, f.height_m, 1e-9, 'window to ceiling'); }
      else if (o.type === 'swing' || o.type === 'sliding') assert(parts.every((p) => p.kind === 'solid' && p.y0 >= 1.9), `${f.id} door hole`);
    }
  }
});

check('fp3dFootprint / fp3dLeaf / fp3dStrip / fp3dInside / fp3dWithin: plan shapes for openings, door leaves, treads', () => {
  const o = realGeom.floors[0].openings[0]; // kitchen-office swing door, a [2.42,3.58] b [3.22,3.58], n [0,1], 0.3 deep
  const r = (pts) => pts.map((p) => p.map((x) => +x.toFixed(4)));
  assert.deepStrictEqual(r(fp3dFootprint(o)), [[2.42, 3.43], [3.22, 3.43], [3.22, 3.73], [2.42, 3.73]]);
  assert.deepStrictEqual(r(fp3dFootprint(o, 0.03)), [[2.42, 3.565], [3.22, 3.565], [3.22, 3.595], [2.42, 3.595]]);
  const [h0, closed] = fp3dLeaf(o, 0), [, open] = fp3dLeaf(o, 1), [, half] = fp3dLeaf(o, 0.5);
  assert.deepStrictEqual(h0, o.swing.hinge);
  within(Math.hypot(closed[0] - o.swing.closed[0], closed[1] - o.swing.closed[1]), 0, 1e-9, 'closed leaf');
  within(Math.hypot(open[0] - o.swing.leaf[0], open[1] - o.swing.leaf[1]), 0, 1e-9, 'open leaf');
  within(Math.hypot(half[0] - h0[0], half[1] - h0[1]), o.swing.r, 1e-9, 'leaf length');
  // half way between the closed (pointing -x) and the open (pointing -y) positions is 45 degrees between them
  within(Math.atan2(half[1] - h0[1], half[0] - h0[0]) * 180 / Math.PI, -135, 1e-6, 'half-open angle');
  assert.deepStrictEqual(r(fp3dStrip([0, 0], [2, 0], 0.1)), [[0, 0.05], [2, 0.05], [2, -0.05], [0, -0.05]]);
  const sq = [[0, 0], [1, 0], [1, 1], [0, 1]];
  assert(fp3dInside([0.5, 0.5], sq) && !fp3dInside([1.5, 0.5], sq));
  assert(fp3dWithin([1.04, 0.5], sq, 0.06) && !fp3dWithin([1.1, 0.5], sq, 0.06)); // a point just past an edge still counts
  // the first floor's stair lines all fall in its stairwell (drawn as a hole there), the ground floor's are treads
  const [g, f1] = realGeom.floors;
  const mid = (s) => [(s[0][0] + s[1][0]) / 2, (s[0][1] + s[1][1]) / 2];
  assert(f1.details.stairs.length > 0 && f1.details.stairs.every((s) => f1.voids.some((v) => fp3dWithin(mid(s), v.polygon, 0.06))));
  assert.strictEqual(g.voids.length, 0);
});

check('fp3dParseColor / fp3dDayFactor: tokens, twilight easing', () => {
  assert.deepStrictEqual(fp3dParseColor('#ABC'), { hex: '#aabbcc', alpha: 1 });
  assert.deepStrictEqual(fp3dParseColor(' #c3ccdc '), { hex: '#c3ccdc', alpha: 1 });
  assert.deepStrictEqual(fp3dParseColor('rgba(170, 195, 240, 0.32)'), { hex: '#aac3f0', alpha: 0.32 });
  assert.deepStrictEqual(fp3dParseColor('rgb(0 0 0 / 50%)'), { hex: '#000000', alpha: 0.5 });
  assert.strictEqual(fp3dParseColor('radial-gradient(red, blue)'), null);
  assert.strictEqual(fp3dParseColor(''), null);
  assert.strictEqual(fp3dDayFactor(-12), 0);
  assert.strictEqual(fp3dDayFactor(20), 1);
  within(fp3dDayFactor(0), 0.5, 1e-12, 'horizon is half light');
  assert(fp3dDayFactor(-3) < fp3dDayFactor(0) && fp3dDayFactor(0) < fp3dDayFactor(3));
});

check('fp3dLightDir: the light follows the sun, then swings over the top to the night fill without a jump', () => {
  const up = realGeom.frame.up_azimuth_deg, az = 255;
  const at = (el) => fp3dLightDir({ azimuth: az, elevation: el }, up);
  const deg = (a, b) => Math.acos(Math.min(1, a.reduce((s, x, i) => s + x * b[i], 0))) * 180 / Math.PI;
  for (const el of [6, 20, 45]) assert(deg(at(el).dir, fp3dSceneDir(az, el, up)) < 1e-6 && at(el).moon === 0, `the real sun at ${el}`);
  for (const el of [-4, -12]) assert(deg(at(el).dir, fp3dSceneDir(az + 180, 38, up)) < 1e-6 && at(el).moon === 1, `the night fill at ${el}`);
  let prev = at(10).dir;
  for (let el = 10; el >= -8; el -= 0.02) {
    const d = at(el).dir;
    within(Math.hypot(...d), 1, 1e-12, 'unit');
    assert(d[1] > 0, `above the table at ${el}`);
    assert(deg(prev, d) < 1, `no jump at ${el.toFixed(2)}`);
    prev = d;
  }
  assert.deepStrictEqual(fp3dLightDir(null, up), { dir: fp3dSceneDir(215, 40, up), moon: 0 });
});

check('fp3dClockHour: the sun-path ticks sit at the wall-clock hour, also on the days the clocks change', () => {
  const tz = process.env.TZ;
  process.env.TZ = 'Europe/Prague';
  try {
    // fall back on 2026-10-25 (CEST -> CET at 03:00), spring forward on 2026-03-29 (CET -> CEST at 02:00)
    const fall = fpLocalDayStart(Date.parse('2026-10-25T12:00:00Z')), spring = fpLocalDayStart(Date.parse('2026-03-29T12:00:00Z'));
    assert.strictEqual(fall, Date.parse('2026-10-24T22:00:00Z'));
    assert.strictEqual(fp3dClockHour(fall, 8), Date.parse('2026-10-25T07:00:00Z'));
    assert.strictEqual(fp3dClockHour(fall, 12), Date.parse('2026-10-25T11:00:00Z'));
    assert.strictEqual(fp3dClockHour(spring, 16), Date.parse('2026-03-29T14:00:00Z'));
    assert.notStrictEqual(fall + 12 * 3600000, fp3dClockHour(fall, 12)); // what elapsed hours would give
    assert.strictEqual(fp3dClockHour(fpLocalDayStart(Date.parse('2026-10-10T12:00:00Z')), 12), Date.parse('2026-10-10T10:00:00Z'));
  } finally {
    process.env.TZ = tz;
  }
});

check('fpSunTimes: sunrise and sunset at the house match NOAA within 2 min', () => {
  // NOAA GML "General Solar Position Calculations" sunrise/sunset equation (zenith 90.833 deg, sea level)
  // for 49.49 N 17.43 E, evaluated by hand outside this code: 2026-06-21 rise 02:43:00Z set 19:00:21Z;
  // 2026-12-21 rise 06:43:53Z set 14:52:41Z. (api.sunrise-sunset.org gives each about 2 min wider,
  // 02:41:30 / 19:02:40 and 06:41:45 / 14:54:52; it allows for the observer's height.)
  const ref = [['2026-06-21', '2026-06-21T02:43:00Z', '2026-06-21T19:00:21Z'], ['2026-12-21', '2026-12-21T06:43:53Z', '2026-12-21T14:52:41Z']];
  for (const [d, rise, set] of ref) {
    const t = fpSunTimes(Date.parse(`${d}T00:00:00Z`), 49.49, 17.43);
    within(t.sunrise / 60000, Date.parse(rise) / 60000, 2, `${d} sunrise`);
    within(t.sunset / 60000, Date.parse(set) / 60000, 2, `${d} sunset`);
  }
  // polar day: no crossing reported rather than a made-up time
  const polar = fpSunTimes(Date.parse('2026-06-21T00:00:00Z'), 78.2, 15.6);
  assert.deepStrictEqual(polar, { sunrise: null, sunset: null });
  // TZ is pinned to UTC above, so local midnight is UTC midnight here
  assert.strictEqual(fpLocalDayStart(Date.parse('2026-10-10T17:40:00Z')), Date.parse('2026-10-10T00:00:00Z'));
});

check('fpWallIncidence / fpOutlineFacades: sun on the outside faces from the floorplan frame', () => {
  const up = realGeom.frame.up_azimuth_deg;
  // the plan normal -> compass bearing agrees with every real outside opening's azimuth_deg
  for (const f of realGeom.floors) for (const o of f.openings || []) if (o.azimuth_deg != null) within(fpPlanAzimuth(o.n, up), o.azimuth_deg, 0.5, `${f.id} ${o.type} azimuth`);
  const sun = (azimuth, elevation) => ({ azimuth, elevation });
  // straight on, along the face, from behind, and below the horizon
  within(fpWallIncidence([0, 1], sun(230, 0.0001), up), 1, 1e-6, 'SW face, sun SW on the horizon');
  within(fpWallIncidence([0, 1], sun(230, 60), up), 0.5, 1e-9, 'high sun: cos 60');
  within(fpWallIncidence([0, 1], sun(140, 30), up), 0, 1e-9, 'sun along the face');
  assert.strictEqual(fpWallIncidence([0, 1], sun(50, 30), up), 0);
  assert.strictEqual(fpWallIncidence([0, 1], sun(230, -1), up), 0);
  assert.strictEqual(fpWallIncidence([0, 1], null, up), 0);
  // the ground outline: six edges, outward normals, the long SW face pointing +y
  const fac = fpOutlineFacades(realGeom.floors[0].outline);
  assert.strictEqual(fac.length, 6);
  const out = fac.map((x) => fpPlanAzimuth(x.n, up)).map(Math.round).sort((a, b) => a - b);
  assert.deepStrictEqual(out, [50, 140, 230, 230, 320, 320]); // the garage wing adds a SW and a NW face
  // reversing the ring's winding keeps the normals outward
  const rev = fpOutlineFacades([...realGeom.floors[0].outline].reverse()).map((x) => Math.round(fpPlanAzimuth(x.n, up))).sort((a, b) => a - b);
  assert.deepStrictEqual(rev, out);
  // the tint: white unlit, warmer (more red, less blue) in full sun
  assert.deepStrictEqual(fp3dSunTint(0), [1, 1, 1]);
  const t = fp3dSunTint(1);
  assert(t[0] > 1 && t[2] < 0.75 && t[1] < 1);
  within(fp3dSegDist([1, 1], [0, 0], [2, 0]), 1, 1e-12, 'segment distance');
});

check('fpSunlitWindows / fpSunCaption: which windows the sun reaches, the caption', () => {
  const floors = realGeom.floors.filter((f) => f.id !== 'roof'), up = realGeom.frame.up_azimuth_deg;
  // afternoon sun from the south-west: the living room's terrace glazing is lit, the NE clerestory is not
  const pm = fpSunlitWindows(floors, { azimuth: 225, elevation: 25 }, up);
  assert(pm.some((w) => w.zone === 'livingroom' && w.o.type === 'glazed'));
  assert(!pm.some((w) => w.o.type === 'clerestory'));
  assert(pm.every((w) => w.inc > 0.1 && w.o.rooms.includes('outside')));
  // a sun at 2 degrees counts as too low; at night nothing
  assert.deepStrictEqual(fpSunlitWindows(floors, { azimuth: 225, elevation: 2 }, up), []);
  assert.deepStrictEqual(fpSunlitWindows(floors, null, up), []);
  // morning from the north-east lights the clerestories (living room and hall), not the kitchen
  const am = fpSunlitWindows(floors, { azimuth: 55, elevation: 12 }, up).map((w) => w.zone);
  assert(am.includes('livingroom') && am.includes('ground_hall') && !am.includes('kitchen'));
  // the caption, day and night (TZ pinned to UTC above)
  const today = { sunrise: Date.parse('2026-10-10T05:05:00Z'), sunset: Date.parse('2026-10-10T16:10:00Z') };
  const next = { sunrise: Date.parse('2026-10-11T05:07:00Z'), sunset: null };
  assert.strictEqual(fpSunCaption({ azimuth: 187.4, elevation: 33.6 }, Date.parse('2026-10-10T11:00:00Z'), today, next, fpHm), '☀ 34° high, south (187°) · sunset 16:10');
  assert.strictEqual(fpSunCaption({ azimuth: 300, elevation: -20 }, Date.parse('2026-10-10T20:00:00Z'), today, next, fpHm), 'Night · sunrise 05:07');
  assert.strictEqual(fpSunCaption({ azimuth: 60, elevation: -20 }, Date.parse('2026-10-10T02:00:00Z'), today, next, fpHm), 'Night · sunrise 05:05');
  assert.strictEqual(fpSunCaption({ azimuth: 250, elevation: -3 }, Date.parse('2026-10-10T16:30:00Z'), today, next, fpHm), 'Twilight · sunrise 05:07');
  assert.strictEqual(fpSunCaption(null, 0, today, next, fpHm), '');
  assert.strictEqual(fpCompassWord(359), 'north');
  assert.strictEqual(fpCompassWord(220), 'south-west');
});

check('the fireplace: floorplan.json carries it, the 3D firebox parts, the honest "fire likely lit" call, the plan edge', () => {
  const [g, f1] = realGeom.floors;
  const fpl = g.features.find((x) => x.kind === 'fireplace');
  assert(fpl && fpl.zone === 'livingroom' && /dxf/.test(fpl.source) && /hand/.test(fpl.source));
  // the DXF flue (r 89 mm) sits inside the hand-placed block, and the chimney rises through the first floor
  const c = fpl.flue.center;
  assert(fp3dInside(c, fpl.polygon));
  within(fpl.flue.r, 0.089, 1e-9, 'flue r');
  const ch = f1.features.find((x) => x.kind === 'chimney');
  assert(ch && fp3dWithin(ch.flue.center, fpl.flue.polygon, 0.03), 'the 2NP flue stands over the 1NP one');
  // the firebox: a frame proud of both glass faces, one pane per face just outside it, between sill and head
  const fb = fpl.firebox, P = fp3dFireboxParts(fb);
  assert.deepStrictEqual(fb.faces.map((f) => f.n), [[-1, 0], [0, 1]]);
  assert.strictEqual(P.panes.length, 2);
  const [fx0, fy0] = P.frame[0], [fx1, fy1] = P.frame[2];
  within(fx0, fb.corner[0] - 0.015, 1e-9, 'frame proud of the -x face');
  within(fy1, fb.corner[1] + 0.015, 1e-9, 'frame proud of the +y face');
  assert(fx1 > fx0 && fy1 > fy0 && P.y0 === fb.sill_m && P.y1 === fb.head_m);
  for (const q of P.panes) assert(q.y0 > fb.sill_m && q.y1 < fb.head_m && Math.hypot(q.b[0] - q.a[0], q.b[1] - q.a[1]) > 0.4);
  within(P.panes[0].a[0], fb.corner[0] - 0.025, 1e-9, 'front pane outside the frame');
  assert(fpl.plinth.top_m === fb.sill_m && fp3dInside(fb.corner, fpl.plinth.polygon));
  // flicker stays in 0..1
  for (let t = 0; t < 20000; t += 37) { const k = fp3dFlicker(t); assert(k >= 0 && k <= 1); }
  // no sensor: only the estimator's unexplained heat above 400 W says "likely lit"
  assert.strictEqual(fpFireLikely(null), null);
  assert.strictEqual(fpFireLikely({ disturbance_w: null }), null);
  assert.strictEqual(fpFireLikely({ disturbance_w: 400 }), null);
  assert.deepStrictEqual(fpFireLikely({ disturbance_w: 812.6 }), { w: 813, text: 'fire likely lit (model estimate, +813 W unexplained heat)' });
  assert.strictEqual(fpFireboxPath({ faces: [{ a: [1, 2], b: [1, 3], n: [-1, 0] }] }), 'M0.97 2L0.97 3');
});

check('fpVoidRoom: the first-floor stairwell opens from the hall (so it takes the hall\'s fill, like the ground floor)', () => {
  const f1 = realGeom.floors[1];
  assert.strictEqual(fpVoidRoom(f1.voids[0].polygon, f1.rooms), 'first_floor_hall');
  assert.strictEqual(fpVoidRoom([[50, 50], [51, 50], [51, 51]], f1.rooms), null);
});

check('fpBlockIndex / fpSeriesAt / fpTimeKind / fpTimelineRange / fpTimeLabel: the timeline lookups', () => {
  // a multi-rate plan: two 15-min blocks, then hourly
  const starts = ['2026-10-10T10:00:00Z', '2026-10-10T10:15:00Z', '2026-10-10T10:30:00Z', '2026-10-10T11:30:00Z'].map(Date.parse);
  const dts = [15, 15, 60, 60];
  const at = (iso) => fpBlockIndex(starts, dts, Date.parse(iso));
  assert.deepStrictEqual(['2026-10-10T10:00:00Z', '2026-10-10T10:14:59Z', '2026-10-10T10:15:00Z', '2026-10-10T11:29:00Z', '2026-10-10T12:29:59Z'].map(at), [0, 0, 1, 2, 3]);
  assert.strictEqual(at('2026-10-10T09:59:59Z'), -1);
  assert.strictEqual(at('2026-10-10T12:30:00Z'), -1); // the last block ends at 12:30
  // a measured series: linear between points, exact on them, null outside and across a long gap
  const ser = [['2026-10-10T08:00:00Z', 20], ['2026-10-10T08:30:00Z', 21], ['2026-10-10T09:00:00Z', null], ['2026-10-10T11:00:00Z', 23]];
  const v = (iso, gap) => fpSeriesAt(ser, Date.parse(iso), gap == null ? 75 * 60000 : gap);
  assert.strictEqual(v('2026-10-10T08:30:00Z'), 21);
  within(v('2026-10-10T08:15:00Z'), 20.5, 1e-12, 'midway');
  assert.strictEqual(v('2026-10-10T07:59:00Z'), null);
  assert.strictEqual(v('2026-10-10T11:01:00Z'), null);
  assert.strictEqual(v('2026-10-10T10:00:00Z'), null); // 08:30 -> 11:00 is a 150-min gap (the null skipped)
  within(v('2026-10-10T10:00:00Z', 3 * 3600000), 22.2, 1e-9, 'interpolated across an allowed gap');
  assert.strictEqual(fpSeriesAt([], 0, 1), null);
  // a window mean stamped at its window end stands for the window's centre: 08:30 (the 08:00-08:30 mean) is 08:15
  assert.strictEqual(fpWindowMeanAt(ser, Date.parse('2026-10-10T08:15:00Z'), FP_SERIES_WINDOW_MS, 75 * 60000), 21);
  within(fpWindowMeanAt(ser, Date.parse('2026-10-10T07:52:30Z'), FP_SERIES_WINDOW_MS, 75 * 60000), 20.25, 1e-12, 'window-centred');
  // the newest stamp (11:00) is the server's read time, before the client's now: a step it still covers
  // reads that newest mean instead of falling off the end
  const mean = (iso) => fpWindowMeanAt(ser, Date.parse(iso), FP_SERIES_WINDOW_MS, 3 * 3600000);
  assert.deepStrictEqual(['2026-10-10T10:45:00Z', '2026-10-10T10:50:00Z', '2026-10-10T11:00:00Z'].map(mean), [23, 23, 23]);
  // an on-change sensor's quiet windows have no point: the newest mean holds up to maxGapMs past it
  // (the read at T + 15 min), then the step is unknown
  const quiet = (iso) => fpWindowMeanAt(ser, Date.parse(iso), FP_SERIES_WINDOW_MS, 75 * 60000);
  assert.deepStrictEqual(['2026-10-10T11:01:00Z', '2026-10-10T11:35:00Z', '2026-10-10T11:50:00Z', '2026-10-10T12:00:00Z'].map(quiet), [23, 23, 23, 23]);
  assert.strictEqual(quiet('2026-10-10T12:01:00Z'), null);
  assert.strictEqual(mean('2026-10-10T13:46:00Z'), null);
  assert.strictEqual(fpWindowMeanAt([...ser, ['2026-10-10T11:30:00Z', null]], Date.parse('2026-10-10T10:50:00Z'), FP_SERIES_WINDOW_MS, 3 * 3600000), 23, 'a trailing null is no sample');
  // the split and the range
  assert.deepStrictEqual([-3, 0, 5].map(fpTimeKind), ['past', 'now', 'plan']);
  const now = Date.parse('2026-10-10T10:07:00Z');
  const tl = [{ t: '2026-10-10T10:00:00Z', dt_minutes: 15 }, { t: '2026-10-10T10:15:00Z', dt_minutes: 15 }, { t: '2026-10-11T21:00:00Z', dt_minutes: 60 }];
  assert.deepStrictEqual(fpTimelineRange(now, tl), { min: -96, max: Math.floor((Date.parse('2026-10-11T22:00:00Z') - now) / 900000) - 1 });
  assert.deepStrictEqual(fpTimelineRange(now, []), { min: -96, max: 0 });
  // TZ is pinned to UTC above
  assert.strictEqual(fpTimeLabel(Date.parse('2026-10-10T14:30:00Z'), 'plan'), 'Sat 14:30 · plan');
  assert.strictEqual(fpTimeLabel(Date.parse('2026-10-09T22:00:00Z'), 'past'), 'Fri 22:00 · measured');
  assert.strictEqual(fpTimeLabel(0, 'now'), 'Now');
});

check('fpPlanValueAt: a block-end plan series at an instant, from the live value at now', () => {
  const starts = ['2026-10-10T10:00:00Z', '2026-10-10T10:15:00Z', '2026-10-10T10:30:00Z'].map(Date.parse);
  const dts = [15, 15, 60], vals = [6, 7, 10];
  const at = (iso, now, v0) => fpPlanValueAt(starts, dts, vals, Date.parse(iso), now == null ? null : Date.parse(now), v0);
  assert.strictEqual(at('2026-10-10T10:15:00Z'), 6, 'exact on a block end');
  within(at('2026-10-10T10:22:30Z'), 6.5, 1e-12, 'half-way through the second block');
  within(at('2026-10-10T11:00:00Z'), 8.5, 1e-12, 'half-way through the hourly block');
  assert.strictEqual(at('2026-10-10T10:05:00Z'), 6, 'before the first end with no live value: the nearest known');
  within(at('2026-10-10T10:10:00Z', '2026-10-10T10:05:00Z', 5), 5.5, 1e-12, 'the running block starts from the live value at now');
  within(at('2026-10-10T10:20:00Z', '2026-10-10T10:17:00Z', 6.4), 6.4 + 0.6 * 3 / 13, 1e-12, 'an end already behind now is dropped');
  assert.strictEqual(fpPlanValueAt(starts, dts, [null, null, 9], Date.parse('2026-10-10T10:20:00Z'), null, null), 9, 'nulls skipped');
  assert.strictEqual(fpPlanValueAt(starts, dts, [null, null, null], Date.parse('2026-10-10T10:20:00Z'), null, null), null);
});

check('fpRoomAt: a room at another time - planned temperature and heat ahead, measured behind, unknown stays unknown', () => {
  const r = room({
    plan: { t: ['2026-10-10T10:00:00Z', '2026-10-10T10:15:00Z', '2026-10-10T10:30:00Z'], dt_minutes: [15, 15, 60], heat_kw: [0, 1.2, 0], temp_c: [21.5, null, 22.4] },
  });
  assert.strictEqual(fpRoomAt(r, 0, 'now', {}), r); // now is the live room itself
  // temperatures are block-END values: T inside a block lies between the known ends around it
  const p = fpRoomAt(r, Date.parse('2026-10-10T10:20:00Z'), 'plan', {});
  assert.deepStrictEqual([p.measured.temp_c, p.heating.on, p.heating.kw], [21.56, true, 1.2]); // 21.5 at 10:15 .. 22.4 at 11:30
  assert.strictEqual(p.estimate_c, null);
  assert.strictEqual(fpRoomState(p).kind, 'in'); // the current band still applies
  const late = fpRoomAt(r, Date.parse('2026-10-10T11:00:00Z'), 'plan', {});
  assert.deepStrictEqual([late.measured.temp_c, late.heating.on], [22.04, false]);
  // with now known the running block starts from the live estimate (22.1 at 10:07)
  const soon = fpRoomAt(r, Date.parse('2026-10-10T10:11:00Z'), 'plan', { now: Date.parse('2026-10-10T10:07:00Z') });
  assert.strictEqual(soon.measured.temp_c, 21.8);
  // the live-only readings do not travel in time
  const fire = fpRoomAt(room({ ...r, disturbance_w: 900, solar_scale: 1.1 }), Date.parse('2026-10-10T10:20:00Z'), 'plan', {});
  assert.deepStrictEqual([fire.disturbance_w, fire.solar_scale], [null, null]);
  const beyond = fpRoomAt(r, Date.parse('2026-10-10T12:00:00Z'), 'plan', {});
  assert.deepStrictEqual([beyond.measured, beyond.heating], [null, null]);
  // solver dust under the relay threshold is not heating, ahead or behind
  const dust = room({ plan: { t: ['2026-10-10T10:00:00Z'], dt_minutes: [60], heat_kw: [0.03], temp_c: [21] } });
  assert.deepStrictEqual(fpRoomAt(dust, Date.parse('2026-10-10T10:20:00Z'), 'plan', {}).heating, { on: false, kw: 0.03 });
  // the past: the measured series (30-min means stamped at the window end, so 08:06 reads 08:21 on
  // the stamps) and the ledger's measured heat (0.3 kWh in 15 min = 1.2 kW)
  const src = {
    series: { kitchen: [['2026-10-10T08:00:00Z', 20.0], ['2026-10-10T08:30:00Z', 21.0]] },
    ledger: [{ t: '2026-10-10T08:00:00Z', dt_minutes: 15, measured: { heat_kwh: { kitchen: 0.3 } } }],
  };
  const past = fpRoomAt(r, Date.parse('2026-10-10T08:06:00Z'), 'past', src);
  within(past.measured.temp_c, 20.7, 1e-9, 'past temperature');
  assert.deepStrictEqual([past.heating.on, past.heating.kw], [true, 1.2]);
  assert.deepStrictEqual(past.at, { T: Date.parse('2026-10-10T08:06:00Z'), kind: 'past' });
  const noRow = fpRoomAt(r, Date.parse('2026-10-10T08:20:00Z'), 'past', src);
  assert.strictEqual(noRow.heating, null);
  // an unheated room keeps its (null) heating either way
  assert.strictEqual(fpRoomAt(room({ heated: false, heating: null, plan: null }), Date.parse('2026-10-10T08:06:00Z'), 'past', src).heating, null);
});

check('fpFullPlanUse: the last fetched whole plan stands in while a newer one loads, marked past 15 min, dropped past an hour', () => {
  const live = '2026-10-10T10:30:00Z';
  assert.strictEqual(fpFullPlanUse(live, live), 'current');
  assert.strictEqual(fpFullPlanUse('2026-10-10T10:29:00Z', live), 'current', 'one replan behind');
  assert.strictEqual(fpFullPlanUse('2026-10-10T10:15:00Z', live), 'current', 'exactly 15 min is not yet marked');
  assert.strictEqual(fpFullPlanUse('2026-10-10T10:14:00Z', live), 'older');
  assert.strictEqual(fpFullPlanUse('2026-10-10T09:30:00Z', live), 'older', 'exactly an hour is still shown');
  assert.strictEqual(fpFullPlanUse('2026-10-10T09:29:00Z', live), null);
  assert.strictEqual(fpFullPlanUse('2026-10-10T10:31:00Z', live), 'current', 'a reply newer than the live rooms');
  assert.strictEqual(fpFullPlanUse(null, live), null, 'none fetched yet');
  assert.strictEqual(fpFullPlanUse(live, null), null, 'no live plan');
  assert.strictEqual(fpFullPlanUse('garbage', live), null);
});

check('fpReadoutAt / fpReadoutItems: the house at the selected time', () => {
  const T = Date.parse('2026-10-10T12:20:00Z');
  const timeline = [{ t: '2026-10-10T12:15:00Z', dt_minutes: 15, import_price: 0.16, heat_kw: { a: 1.25, b: 0.5 }, soc_kwh: 6.24, pv_kw: 3.1, slot: 'sell_production' }];
  const plan = fpReadoutAt(T, 'plan', { timeline });
  assert.deepStrictEqual(plan, { price: 0.16, rate: null, heatKw: 1.75, socKwh: 6.24, pvKw: 3.1, outsideC: null, slot: 'sell_production', from: null });
  assert.deepStrictEqual(fpReadoutItems(plan, 25), [['Price', '4.00 Kč/kWh'], ['Heating', '1.8 kW'], ['Battery', '6.2 kWh'], ['PV', '3.1 kW'], ['Mode', 'Selling']]);
  assert.deepStrictEqual(fpReadoutItems(plan, null)[0], ['Price', '€0.160/kWh'], 'no rate known: EUR, never an assumed rate');
  // the planned battery is a block-end value too: from the live SoC at now (12:18) to 6.24 at 12:30
  const soc = fpReadoutAt(T, 'plan', { timeline, now: Date.parse('2026-10-10T12:18:00Z'), live: { soc_kwh: 7.44 } });
  within(soc.socKwh, 7.44 - 1.2 * 2 / 12, 1e-9, 'planned battery at T');
  // now: the live feed wins for PV, battery and outside
  const now = fpReadoutAt(T, 'now', { timeline, live: { solar_kw: 2.5, soc_kwh: 7, outside_temp_c: 11.26 } });
  assert.deepStrictEqual([now.pvKw, now.socKwh, now.outsideC, now.price], [2.5, 7, 11.26, 0.16]);
  // now mixes the committed plan with live readings: each value says which it is
  assert.deepStrictEqual(fpReadoutItems(now, 25).slice(1, 5).map((x) => x[1]), ['1.8 kW (plan)', '7.0 kWh (measured)', '2.5 kW (measured)', '11.3 °C (measured)']);
  assert.deepStrictEqual(fpReadoutItems(fpReadoutAt(T, 'now', { timeline }), 25).slice(1, 4).map((x) => x[1]), ['1.8 kW (plan)', '6.2 kWh (plan)', '3.1 kW (plan)'], 'no live feed: the plan, marked so');
  // the past: the ledger row (kWh per block to kW) and the history series (15-min means stamped at the
  // window end, so 20:05 reads 20:12:30 on the stamps)
  const P = Date.parse('2026-10-09T20:05:00Z');
  const src = {
    ledger: [
      { t: '2026-10-09T20:00:00Z', dt_minutes: 15, import_price: 0.2, eur_czk_rate: 24, slot: 'regular', measured: { pv_kwh: 0, heat_kwh: { a: 0.5, b: 0.25 } } },
      { t: '2026-10-09T20:15:00Z', dt_minutes: 15, import_price: 0.2, slot: 'regular', measured: { pv_kwh: 0, heat_kwh: {} } },
    ],
    history: { soc_kwh: [['2026-10-09T20:00:00Z', 5], ['2026-10-09T20:15:00Z', 4]], pv_kw: [] },
  };
  const past = fpReadoutAt(P, 'past', src);
  assert.deepStrictEqual([past.price, past.slot, past.pvKw, past.heatKw, past.outsideC], [0.2, 'regular', 0, 3, null]);
  within(past.socKwh, 5 - 5 / 6, 1e-9, 'past battery');
  assert.deepStrictEqual(fpReadoutItems(past, 25)[0], ['Price', '4.80 Kč/kWh'], 'the row\'s own recorded rate, not the plan\'s');
  // a scored row with no zone measured: heating unknown, not 0
  assert.strictEqual(fpReadoutAt(Date.parse('2026-10-09T20:20:00Z'), 'past', src).heatKw, null);
  // nothing known: dashes, no outside item
  const none = fpReadoutItems(fpReadoutAt(P, 'past', {}), 25);
  assert.deepStrictEqual(none.map((x) => x[1]), ['—', '—', '—', '—', '—']);
});

// ---- Indoor comfort rows (Home) ----
const czScope = {};
// eslint-disable-next-line no-new-func
new Function('scope', `
  ${extractConst(src, 'clamp')}
  ${extractConst(src, 'HEAT_ON_KW')}
  ${['bandNow', 'overheatCeiling', 'isRelayOn', 'fpToLab', 'fpFromLab', 'czGroups', 'czTrend', 'czFacts', 'czGauge', 'czMarkColour'].map((n) => extractFunction(src, n)).join('\n')}
  Object.assign(scope, { czGroups, czTrend, czFacts, czGauge, czMarkColour, fpToLab });
`)(czScope);
check('czGroups: ground and first floor as the plan reads (left to right, plan names), the rest under Other', () => {
  const real = JSON.parse(fs.readFileSync(path.join(__dirname, 'floorplan.json'), 'utf8'));
  const banded = new Set(['kitchen', 'office', 'livingroom', 'guestroom', 'room_2', 'mystery']);
  const g = czScope.czGroups(['livingroom', 'office', 'kitchen', 'room_2', 'guestroom', 'garrage', 'attic', 'mystery'], real, banded);
  assert.deepStrictEqual(g.map((x) => x.name), ['Ground floor', 'First floor', 'Other']);
  assert.deepStrictEqual(g[0].rows.map((r) => r.name), ['Office', 'Kitchen', 'Living room'], 'left to right as drawn, the plan names');
  assert.deepStrictEqual(g[1].rows.map((r) => r.zone), ['guestroom', 'room_2']);
  assert.deepStrictEqual(g[2].rows.map((r) => r.zone), ['attic', 'garrage', 'mystery'], 'no band (garage), the roof, and zones the plan does not draw');
  assert.strictEqual(g[2].rows[2].name, 'mystery');
  const noGeom = czScope.czGroups(['some_room'], null, new Set(['some_room']));
  assert.deepStrictEqual(noGeom.map((x) => [x.name, x.rows[0].name]), [['Other', 'some room']], 'before the geometry loads nothing is lost');
  assert.deepStrictEqual(czScope.czGroups([], real, banded), [], 'empty groups are dropped');
});
check('czTrend / czFacts: terse facts with full-sentence titles; the same rules as the old tiles', () => {
  const ser = [['2026-10-10T05:00:00Z', 21.0], ['2026-10-10T05:30:00Z', 21.2], ['2026-10-10T06:00:00Z', 21.6]];
  assert(Math.abs(czScope.czTrend(ser, 21.6) - 0.6) < 1e-9);
  assert.strictEqual(czScope.czTrend(ser.map(([t]) => [t, 21]), 21), null, 'flat: no trend');
  assert.strictEqual(czScope.czTrend(ser.slice(0, 2), 21), null);
  const hm = (iso) => iso.slice(11, 16);
  const zc = { t_min_now: 21, t_max_now: 24, overheat_c: 1, t_max_boost_now: 25 };
  const future = [
    { t: '2026-10-10T07:00:00Z', dt_minutes: 15, temp_c: { k: 21.05 }, heat_kw: { k: 0 } },
    { t: '2026-10-10T07:15:00Z', dt_minutes: 60, temp_c: { k: 21.4 }, heat_kw: { k: 1.2 } },
  ];
  const f = czScope.czFacts('k', zc, future, { heat_kw: { k: 0 } }, -283.4, hm);
  assert.deepStrictEqual(f.map((x) => [x.kind, x.text, x.cls]), [['forecast', '↓ 21.1° 07:15', 'zclose'], ['heat', '07:15 1.2 kWh', 'zheat'], ['alarm', '⚠ −283 W', 'zwarn']]);
  assert(f[0].title.includes('Model forecast low') && f[2].title.includes('unexplained'));
  // heating now: the current 15-min block counts as 0.25 h
  const now = czScope.czFacts('k', zc, future, { heat_kw: { k: 0.8 } }, 10, hm);
  assert.deepStrictEqual(now.map((x) => x.text), ['↓ 21.1° 07:15', 'now 1.4 kWh']);
  // a forecast peak inside the overheat allowance is banking, not a warning
  const warm = czScope.czFacts('k', zc, [{ t: '2026-10-10T07:00:00Z', dt_minutes: 15, temp_c: { k: 24.6 } }], {}, null, hm);
  assert.deepStrictEqual([warm[0].text, warm[0].cls], ['↑ 24.6° 07:15', '']);
  assert.deepStrictEqual(czScope.czFacts('k', null, [], {}, null, hm), []);
});
check('czGauge / czMarkColour: band, allowance and reading on one slim track; labels never collide', () => {
  const g = czScope.czGauge(22, { t_min_now: 21, t_max_now: 24, overheat_c: 1, t_max_boost_now: 25 }, '#123456');
  assert(g.includes('cz-band') && g.includes('cz-boost') && g.includes('background:#123456') && g.includes('overheat allowance to 25'));
  assert(!g.includes('cz-bedge'), 'a boost edge right next to the ceiling keeps its number in the title only');
  const wide = czScope.czGauge(22, { t_min_now: 20, t_max_now: 21, overheat_c: 3, t_max_boost_now: 24 }, '#000000');
  assert(wide.includes('cz-bedge'), 'a well separated boost edge is labelled');
  const lefts = (h) => (h.match(/left:(-?[\d.]+)%/g) || []).map((x) => parseFloat(x.slice(5)));
  for (const html of [g, wide, czScope.czGauge(30, { t_min_now: 18, t_max_now: 25 }, '#000000'), czScope.czGauge(10, { t_min_now: 18, t_max_now: 25 }, '#000000')]) {
    assert(lefts(html).every((v) => v >= 0 && v <= 100) && !/NaN/.test(html), 'every mark inside the track');
  }
  assert.strictEqual(czScope.czGauge(null, { t_min_now: 18, t_max_now: 25 }, '#000'), '');
  assert.strictEqual(czScope.czGauge(22, null, '#000'), '');
  // light theme: a pale in-band tint deepens into a readable mark, same hue; dark theme untouched
  const pale = '#d9ece4';
  const deep = czScope.czMarkColour(pale, 'light');
  const [L0, a0, b0] = czScope.fpToLab(pale), [L1, a1, b1] = czScope.fpToLab(deep);
  assert(L1 <= 0.745 && L1 < L0 && Math.hypot(a1, b1) > Math.hypot(a0, b0), `${pale} -> ${deep}`);
  assert(Math.abs(Math.atan2(b1, a1) - Math.atan2(b0, a0)) < 0.05, 'hue kept');
  const dim = '#3d4a44', lit = czScope.fpToLab(czScope.czMarkColour(dim, 'dark'));
  assert(lit[0] >= 0.615 && lit[0] > czScope.fpToLab(dim)[0], 'dark theme: a dim in-band tint lifts off the dark card');
});

// ---- New features helpers ----
check('feature status/kind/verdict classes map known values and fall back to neutral', () => {
  assert.strictEqual(featureStatusClass('live'), 'green');
  assert.strictEqual(featureStatusClass('staged'), 'amber');
  assert.strictEqual(featureStatusClass('bogus'), '');
  assert.strictEqual(featureKindClass('ab_replay'), 'amber');
  assert.strictEqual(featureKindClass('release_proof'), '');
  assert.strictEqual(featureKindClass(undefined), '');
  assert.strictEqual(featureVerdictClass('helped'), 'green');
  assert.strictEqual(featureVerdictClass('worse'), 'red');
  assert.strictEqual(featureVerdictClass('insufficient'), 'amber');
  assert.strictEqual(featureVerdictClass('off'), '');
  assert.strictEqual(featureVerdictClass('???'), '');
});
check('featureDate formats UTC dates and tolerates null / garbage', () => {
  assert.strictEqual(featureDate('2026-10-03T05:58:00Z'), '3 Oct 2026');
  assert.strictEqual(featureDate('2026-10-03T23:59:00Z'), '3 Oct 2026');
  assert.strictEqual(featureDate(null), '—');
  assert.strictEqual(featureDate('nope'), '—');
});
check('featureReleaseLabels pairs date and sha; a release without an instant shows the sha only', () => {
  assert.deepStrictEqual(featureReleaseLabels({ releases: [{ sha: 'abc1234', at: '2026-10-01T09:42:00Z' }, { sha: 'def5678', at: '2026-10-03T05:58:00Z' }] }),
    ['1 Oct 2026 · abc1234', '3 Oct 2026 · def5678']);
  assert.deepStrictEqual(featureReleaseLabels({ releases: [{ sha: 'abc1234', at: null }] }), ['abc1234']);
  assert.deepStrictEqual(featureReleaseLabels({ releases: [], shas: ['aaa'] }), ['aaa']);
  assert.deepStrictEqual(featureReleaseLabels({}), []);
});
check('featureGraduatedLabel pairs date and sha, or shows the sha alone', () => {
  assert.strictEqual(featureGraduatedLabel({ date: '2026-10-01', sha: '112ea92' }), '1 Oct 2026 · 112ea92');
  assert.strictEqual(featureGraduatedLabel({ date: null, sha: '49b8430' }), '49b8430');
});
check('featureChartEmpty: no x, no series, or only nulls is empty; one number is not', () => {
  assert.strictEqual(featureChartEmpty(null), true);
  assert.strictEqual(featureChartEmpty({ x: [], series: [{ name: 'a', values: [] }] }), true);
  assert.strictEqual(featureChartEmpty({ x: ['d'], series: [] }), true);
  assert.strictEqual(featureChartEmpty({ x: ['d1', 'd2'], series: [{ name: 'a', values: [null, null] }] }), true);
  assert.strictEqual(featureChartEmpty({ x: ['d1', 'd2'], series: [{ name: 'a', values: [null, 0] }] }), false);
});
check('featureTableEmpty / featureHasData follow the payload', () => {
  assert.strictEqual(featureTableEmpty({ columns: ['a'], rows: [] }), true);
  assert.strictEqual(featureTableEmpty({ columns: ['a'], rows: [[1]] }), false);
  assert.strictEqual(featureHasData({ charts: [], tables: [] }), false);
  assert.strictEqual(featureHasData({}), false);
  assert.strictEqual(featureHasData({ charts: [{ x: ['d'], series: [{ values: [null] }] }], tables: [] }), false);
  assert.strictEqual(featureHasData({ charts: [], tables: [{ columns: ['a'], rows: [['x']] }] }), true);
});
check('featureSeriesData keeps numbers and turns gaps into null; featureCell formats cells', () => {
  assert.deepStrictEqual(featureSeriesData([1, null, NaN, 0, undefined]), [1, null, null, 0, null]);
  assert.deepStrictEqual(featureSeriesData(null), []);
  assert.strictEqual(featureCell(null), '—');
  assert.strictEqual(featureCell(12), '12');
  assert.strictEqual(featureCell(0.4736), '0.474');
  assert.strictEqual(featureCell(0.043), '0.043', 'small values keep their precision');
  assert.strictEqual(featureCell(0.251), '0.251');
  assert.strictEqual(featureCell(-0.00412), '-0.00412', 'a small K delta does not round to 0.00');
  assert.strictEqual(featureCell(12.3456), '12.3');
  assert.strictEqual(featureCell(549.31), '549');
  assert.strictEqual(featureCell(2345.6), '2346');
  assert.strictEqual(featureCell(0), '0');
  assert.strictEqual(featureCell('12-24 h'), '12-24 h');
  assert.strictEqual(featureCell(Infinity), '—');
});

// ---- relayDuty ----

check('full power reads 100% duty / 4 on-blocks per hour', () => {
  const d = relayDuty(2.0, 2.0);
  assert.strictEqual(d.dutyPct, 100);
  assert.strictEqual(d.onBlocks, 4);
  assert.strictEqual(d.kw, 2.0);
});

check('zero power reads 0% duty', () => {
  const d = relayDuty(0, 2.0);
  assert.strictEqual(d.dutyPct, 0);
  assert.strictEqual(d.onBlocks, 0);
});

check('half of max reads 50% duty / 2 on-blocks (the raw kW average is still returned for the tooltip)', () => {
  const d = relayDuty(1.0, 2.0);
  assert.strictEqual(d.dutyPct, 50);
  assert.strictEqual(d.onBlocks, 2);
  assert.strictEqual(d.kw, 1.0);
});

check('a value above max clamps to 100% duty, never over-reporting', () => {
  const d = relayDuty(3.0, 2.0);
  assert.strictEqual(d.dutyPct, 100);
  assert.strictEqual(d.onBlocks, 4);
});

check('a non-finite or non-positive max_heat_kw degrades to 0 duty, not NaN/Infinity', () => {
  assert.deepStrictEqual(relayDuty(1.0, 0), { dutyPct: 0, onBlocks: 0, kw: 1.0 });
  assert.deepStrictEqual(relayDuty(1.0, -1), { dutyPct: 0, onBlocks: 0, kw: 1.0 });
  assert.deepStrictEqual(relayDuty(1.0, NaN), { dutyPct: 0, onBlocks: 0, kw: 1.0 });
  assert.deepStrictEqual(relayDuty(1.0, undefined), { dutyPct: 0, onBlocks: 0, kw: 1.0 });
});

check('a missing/non-finite heat_kw (zone absent from the block) reads as 0, not NaN', () => {
  const d = relayDuty(undefined, 2.0);
  assert.strictEqual(d.dutyPct, 0);
  assert.strictEqual(d.kw, 0);
});

// ---- isNearTermBlock ----
// item 9 (rework cycle 2, finding 9): the near-term window is blocks 0 and 1 -- 30 minutes
// (HEAT_COOL_PIN_BLOCKS on the brain side), not the "first two hours" this used to claim.

check('a block at the plan start is near-term', () => {
  assert.strictEqual(isNearTermBlock('2026-09-22T18:45:00Z', '2026-09-22T18:45:00Z'), true);
});

check('a block just under 30min out is still near-term', () => {
  assert.strictEqual(isNearTermBlock('2026-09-22T19:14:00Z', '2026-09-22T18:45:00Z'), true);
});

check('a block at exactly 30min out is far horizon', () => {
  assert.strictEqual(isNearTermBlock('2026-09-22T19:15:00Z', '2026-09-22T18:45:00Z'), false);
});

check('a block well beyond 30min out is far horizon', () => {
  assert.strictEqual(isNearTermBlock('2026-09-23T06:45:00Z', '2026-09-22T18:45:00Z'), false);
});

check('accepts epoch-ms numbers too (what an ECharts time-axis tooltip callback hands back)', () => {
  const start = Date.parse('2026-09-22T18:45:00Z');
  assert.strictEqual(isNearTermBlock(start + 15 * 60 * 1000, start), true); // +15min (block 1)
  assert.strictEqual(isNearTermBlock(start + 60 * 60 * 1000, start), false); // +1h
});

// ---- isRelayOn ----
// item 9: the near-term tooltip's "on"/"off" wording must match the PUBLISHER's actual relay rule
// (on_threshold_kw, default 0.05 kW), an absolute kW cutoff -- not a duty-percentage one.

check('kw at or below the 0.05kW threshold reads off', () => {
  assert.strictEqual(isRelayOn(0), false);
  assert.strictEqual(isRelayOn(0.05), false);
});

check('kw just above the 0.05kW threshold reads on, even at a low duty %', () => {
  // 0.06kW of a 2kW circuit is only 3% duty -- the old >=50%-duty rule would have said "off" here,
  // contradicting the publisher, which has already switched the relay ON.
  assert.strictEqual(isRelayOn(0.06), true);
});

check('full power reads on', () => {
  assert.strictEqual(isRelayOn(2.0), true);
});

check('non-finite kw reads off, not throwing', () => {
  assert.strictEqual(isRelayOn(undefined), false);
  assert.strictEqual(isRelayOn(NaN), false);
});

// ---- heatingBlockClass / mergeOnPeriods (item M: heating schedule on-period timeline) ----

check('a 15-min (fine) block with any nonzero duty reads as an exact on block', () => {
  const c = heatingBlockClass(1.5, 2.0, 15);
  assert.deepStrictEqual(c, { on: true, exact: true, kw: 1.5 });
});

check('a 15-min block at ~0 duty reads off', () => {
  assert.strictEqual(heatingBlockClass(0, 2.0, 15).on, false);
});

check('an hourly block at full duty (4/4) is exact, not fractional', () => {
  const c = heatingBlockClass(2.0, 2.0, 60);
  assert.deepStrictEqual(c, { on: true, exact: true, kw: 2.0 });
});

check('an hourly block at 3/4 duty is fractional, with the quarters count', () => {
  const c = heatingBlockClass(1.5, 2.0, 60); // 0.75 duty -> round(3) quarters
  assert.deepStrictEqual(c, { on: true, exact: false, quarters: 3, kw: 1.5 });
});

check('an hourly block that rounds down to 0 quarters reads off, not a zero-length period', () => {
  const c = heatingBlockClass(0.1, 2.0, 60); // 0.05 duty -> round(0.2) = 0
  assert.strictEqual(c.on, false);
});

const zone = 'livingroom';
const maxKw = 2.0;
const block = (tISO, dtMinutes, kw) => ({ t: tISO, dt_minutes: dtMinutes, heat_kw: { [zone]: kw } });

check('consecutive fine on-blocks merge into one period spanning start to the last block\'s end', () => {
  const tl = [
    block('2026-09-23T18:00:00Z', 15, 2.0),
    block('2026-09-23T18:15:00Z', 15, 2.0),
    block('2026-09-23T18:30:00Z', 15, 2.0),
  ];
  const periods = mergeOnPeriods(tl, zone, maxKw);
  assert.strictEqual(periods.length, 1);
  assert.strictEqual(periods[0].start, '2026-09-23T18:00:00Z');
  assert.strictEqual(periods[0].end, '2026-09-23T18:45:00.000Z');
  assert.strictEqual(periods[0].minutes, 45);
  assert.strictEqual(periods[0].exact, true);
  assert.ok(Math.abs(periods[0].kwh - 1.5) < 1e-9); // 3 * 15min at 2kW = 1.5 kWh
});

check('an off block in between splits two on-blocks into two periods', () => {
  const tl = [
    block('2026-09-23T18:00:00Z', 15, 2.0),
    block('2026-09-23T18:15:00Z', 15, 0),
    block('2026-09-23T18:30:00Z', 15, 2.0),
  ];
  const periods = mergeOnPeriods(tl, zone, maxKw);
  assert.strictEqual(periods.length, 2);
  assert.strictEqual(periods[0].minutes, 15);
  assert.strictEqual(periods[1].start, '2026-09-23T18:30:00Z');
});

check('a fractional hourly block never merges with a neighbouring exact block, and carries its own quarters', () => {
  const tl = [
    block('2026-09-23T18:00:00Z', 15, 2.0), // exact on
    block('2026-09-23T18:15:00Z', 60, 1.5), // hourly, 3/4 quarters -- fractional
    block('2026-09-23T19:15:00Z', 15, 2.0), // exact on again
  ];
  const periods = mergeOnPeriods(tl, zone, maxKw);
  assert.strictEqual(periods.length, 3);
  assert.strictEqual(periods[0].exact, true);
  assert.strictEqual(periods[1].exact, false);
  assert.strictEqual(periods[1].quarters, 3);
  assert.strictEqual(periods[1].minutes, 60);
  assert.strictEqual(periods[2].exact, true);
});

check('a zone absent from a block\'s heat_kw map reads as off, not throwing', () => {
  const tl = [{ t: '2026-09-23T18:00:00Z', dt_minutes: 15, heat_kw: {} }];
  assert.deepStrictEqual(mergeOnPeriods(tl, zone, maxKw), []);
});

check('an empty timeline yields no periods', () => {
  assert.deepStrictEqual(mergeOnPeriods([], zone, maxKw), []);
});

// ---- localMidnights (item M: day dividers) ----

check('a horizon spanning one local midnight finds exactly it, labelled with weekday + date', () => {
  // Prague summer offset +120min. 2026-09-23 is a Wednesday; local midnight -> 2026-09-23T22:00:00Z
  // is the instant of 2026-09-24T00:00 local.
  const start = Date.parse('2026-09-23T10:00:00Z');
  const end = Date.parse('2026-09-24T10:00:00Z');
  const mids = localMidnights(start, end, 120);
  assert.strictEqual(mids.length, 1);
  assert.strictEqual(mids[0].ms, Date.parse('2026-09-23T22:00:00Z'));
  assert.strictEqual(mids[0].label, 'Thu 24 Sep');
});

check('a 36h horizon finds two local midnights, in order', () => {
  const start = Date.parse('2026-09-23T10:00:00Z');
  const end = Date.parse('2026-09-24T22:00:00Z'); // 36h later
  const mids = localMidnights(start, end, 120);
  assert.strictEqual(mids.length, 2);
  assert.strictEqual(mids[0].label, 'Thu 24 Sep');
  assert.strictEqual(mids[1].label, 'Fri 25 Sep');
});

check('offset 0 (UTC) puts midnight exactly at the UTC day boundary', () => {
  const start = Date.parse('2026-09-23T10:00:00Z');
  const end = Date.parse('2026-09-24T01:00:00Z');
  const mids = localMidnights(start, end, 0);
  assert.strictEqual(mids.length, 1);
  assert.strictEqual(mids[0].ms, Date.parse('2026-09-24T00:00:00Z'));
});

check('a negative offset (west of UTC) is handled correctly', () => {
  // offset -300 (US Eastern-ish, UTC-5): local midnight = 05:00 UTC.
  const start = Date.parse('2026-09-23T10:00:00Z');
  const end = Date.parse('2026-09-24T10:00:00Z');
  const mids = localMidnights(start, end, -300);
  assert.strictEqual(mids.length, 1);
  assert.strictEqual(mids[0].ms, Date.parse('2026-09-24T05:00:00Z'));
});

check('an empty or inverted range yields no dividers', () => {
  assert.deepStrictEqual(localMidnights(100, 100, 120), []);
  assert.deepStrictEqual(localMidnights(200, 100, 120), []);
  assert.deepStrictEqual(localMidnights(NaN, 100, 120), []);
});

// ---- solarSplitText (item L: beam/diffuse split on the House page) ----

check('the brief\'s own example: a NE wall at mid-morning is diffuse-only (beam clamped to 0)', () => {
  assert.strictEqual(solarSplitText(0, 154), '154 W — 0 W direct · 154 W diffuse sky');
});

check('beam + diffuse both present', () => {
  assert.strictEqual(solarSplitText(300.4, 45.2), '346 W — 300 W direct · 45 W diffuse sky');
});

check('no sun at all reads as an all-zero label, not NaN', () => {
  assert.strictEqual(solarSplitText(0, 0), '0 W — 0 W direct · 0 W diffuse sky');
});

check('non-finite/negative components degrade to 0, never NaN or a negative watt figure', () => {
  assert.strictEqual(solarSplitText(undefined, 100), '100 W — 0 W direct · 100 W diffuse sky');
  assert.strictEqual(solarSplitText(NaN, NaN), '0 W — 0 W direct · 0 W diffuse sky');
  assert.strictEqual(solarSplitText(-5, 50), '50 W — 0 W direct · 50 W diffuse sky');
});

// ---- ledgerSummaryRows ----

check('no rows/by_day/misses at all reads as the empty state', () => {
  const s = ledgerSummaryRows({}, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.empty, true);
  assert.deepStrictEqual(s.modeRows, []);
  assert.strictEqual(s.today, null);
});

check('rows outside the trailing 24h window are excluded from the mode table', () => {
  const data = {
    rows: [
      { t: '2026-10-01T11:00:00Z', dt_minutes: 15, slot: 'regular', scored: true, planned: { charge_kw: 0, discharge_kw: 0, grid_export_kw: 0 }, measured: { charge_kwh: 0, discharge_kwh: 0, export_kwh: 0 } },
      { t: '2026-09-29T11:00:00Z', dt_minutes: 15, slot: 'regular', scored: true, planned: { charge_kw: 0, discharge_kw: 0, grid_export_kw: 0 }, measured: { charge_kwh: 0, discharge_kwh: 0, export_kwh: 0 } },
    ],
  };
  const s = ledgerSummaryRows(data, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.modeRows.length, 1);
  assert.strictEqual(s.modeRows[0].n, 1);
});

check('planned/measured kWh accumulate per mode, dt-weighted, from scored rows only', () => {
  const data = {
    rows: [
      {
        t: '2026-10-01T11:00:00Z', dt_minutes: 15, slot: 'discharge_to_grid', scored: true,
        planned: { charge_kw: 0, discharge_kw: 4.0, grid_export_kw: 4.0 },
        measured: { charge_kwh: 0, discharge_kwh: 0.1, export_kwh: 0.05 },
      },
      {
        t: '2026-10-01T11:15:00Z', dt_minutes: 15, slot: 'discharge_to_grid', scored: false,
        // unscored — a HUGE planned value here must NOT leak into the totals (it would otherwise
        // drag the apparent discharge efficacy toward zero for a reason that isn't a real miss).
        planned: { charge_kw: 0, discharge_kw: 400.0, grid_export_kw: 400.0 },
        measured: null,
      },
    ],
  };
  const s = ledgerSummaryRows(data, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.modeRows.length, 1);
  const m = s.modeRows[0];
  assert.strictEqual(m.n, 1, 'only the scored row counts');
  assert(Math.abs(m.dischargePlanned - 1.0) < 1e-9, `dischargePlanned was ${m.dischargePlanned}`);
  assert(Math.abs(m.dischargeMeasured - 0.1) < 1e-9, `dischargeMeasured was ${m.dischargeMeasured}`);
});

check('a degraded or relaxed row never feeds the mode table even if scored', () => {
  const data = {
    rows: [
      {
        t: '2026-10-01T11:00:00Z', dt_minutes: 15, slot: 'regular', scored: true, degraded: true,
        planned: { charge_kw: 0, discharge_kw: 5.0, grid_export_kw: 0 },
        measured: { charge_kwh: 0, discharge_kwh: 0, export_kwh: 0 },
      },
      {
        t: '2026-10-01T11:15:00Z', dt_minutes: 15, slot: 'regular', scored: true, relaxed: true,
        planned: { charge_kw: 0, discharge_kw: 5.0, grid_export_kw: 0 },
        measured: { charge_kwh: 0, discharge_kwh: 0, export_kwh: 0 },
      },
    ],
  };
  const s = ledgerSummaryRows(data, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.modeRows.length, 0);
});

check('today/yesterday are matched by the real site-local calendar date, not "newest in by_day"', () => {
  const data = {
    by_day: [
      // Deliberately out of order and with an older day present too — the match must be by exact
      // date, not by position or recency.
      { date: '2026-09-28', n_scored: 10, planned_cost_czk: 5, realized_cost_czk: 4 },
      { date: '2026-10-01', n_scored: 30, planned_cost_czk: 20, realized_cost_czk: 15 },
      { date: '2026-09-30', n_scored: 20, planned_cost_czk: 10, realized_cost_czk: null },
    ],
  };
  const s = ledgerSummaryRows(data, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.today.date, '2026-10-01');
  assert.strictEqual(s.yesterday.date, '2026-09-30');
});

check('a missing today reads honestly as null, never falling back to the newest available day', () => {
  const data = {
    by_day: [
      { date: '2026-09-29', n_scored: 10, planned_cost_czk: 5, realized_cost_czk: 4 },
      { date: '2026-09-30', n_scored: 20, planned_cost_czk: 10, realized_cost_czk: null },
    ],
  };
  // "now" is 2026-10-01 (just after local midnight) but no row exists for it yet — must read as
  // null, not silently substitute 2026-09-30 (already correctly reported as `yesterday`).
  const s = ledgerSummaryRows(data, '2026-10-01T00:05:00Z');
  assert.strictEqual(s.today, null);
  assert.strictEqual(s.yesterday.date, '2026-09-30');
});

check('misses are capped to the latest 3, in the order the server already sorted them', () => {
  const data = {
    misses: [
      { t: '2026-10-01T11:45:00Z', slot: 'discharge_to_grid', reason: 'a' },
      { t: '2026-10-01T11:30:00Z', slot: 'discharge_to_grid', reason: 'b' },
      { t: '2026-10-01T11:15:00Z', slot: 'discharge_to_grid', reason: 'c' },
      { t: '2026-10-01T11:00:00Z', slot: 'discharge_to_grid', reason: 'd' },
    ],
  };
  const s = ledgerSummaryRows(data, '2026-10-01T12:00:00Z');
  assert.strictEqual(s.latestMisses.length, 3);
  assert.strictEqual(s.latestMisses[0].reason, 'a');
});

// ---- ledgerCostLine ----

const stubFmt = { czk: (v) => `${v} Kč`, eur: (v) => `€${v}` };

check('a day with n_scored 0 reads honestly as no scored blocks yet, not a zero-cost line', () => {
  const html = ledgerCostLine('today', { n_scored: 0, planned_cost_czk: 0, realized_cost_czk: null }, stubFmt);
  assert.strictEqual(html, '<div>cost today: no scored blocks yet</div>');
});

check('a missing day (no entry at all for that date) reads the same honest message', () => {
  const html = ledgerCostLine('today', null, stubFmt);
  assert.strictEqual(html, '<div>cost today: no scored blocks yet</div>');
});

check('a scored day not yet realized shows the planned total pending realization', () => {
  const html = ledgerCostLine('yesterday', { n_scored: 5, planned_cost_czk: 10, realized_cost_czk: null }, stubFmt);
  assert.strictEqual(html, '<div>cost yesterday: planned 10 Kč → realized not yet scored</div>');
});

check('a fully scored day shows both planned and realized totals', () => {
  const d = { n_scored: 12, planned_cost_czk: 20, realized_cost_czk: 15, realized_cost_eur: 0.6 };
  const html = ledgerCostLine('today', d, stubFmt);
  assert.strictEqual(html, '<div>cost today: planned 20 Kč → realized 15 Kč (€0.6)</div>');
});

// ---- host adapter (MPC_HOST): every API call and asset URL goes through one seam ----
// Evaluate the REAL adapter source (the `window.MPC_HOST = ...` assignment plus its helpers) against a
// fake `window`, once with no host (the brain-served default) and once with a Home Assistant style stub.
function loadAdapter(fakeWindow) {
  const names = ['MPC_HOST_DEFAULTS', 'mpcHost', 'mpcUrl', 'mpcApi', 'mpcAsset', 'mpcHass'];
  const assign = src.match(/^window\.MPC_HOST = [^\n]*;$/m);
  assert(assign, 'window.MPC_HOST assignment not found in app.js');
  // eslint-disable-next-line no-new-func
  return new Function(
    'window',
    `${extractConst(src, names[0])}
     ${assign[0]}
     ${names.slice(1).map((n) => extractConst(src, n)).join('\n')}
     return { ${names.join(', ')} };`
  )(fakeWindow);
}

check('host adapter: with no MPC_HOST the defaults are same-origin and window.fetch is used unchanged', () => {
  const calls = [];
  const win = { fetch: (url, init) => { calls.push([url, init]); return Promise.resolve('brain'); } };
  const a = loadAdapter(win);
  assert.strictEqual(a.mpcUrl('/api/rooms?hours=48'), '/api/rooms?hours=48');
  assert.strictEqual(a.mpcAsset('three.min.js'), '/static/three.min.js');
  assert.strictEqual(a.mpcAsset('floorplan.json'), '/static/floorplan.json');
  assert.strictEqual(a.mpcHass(), null);
  const init = { cache: 'no-store' };
  a.mpcApi('/api/live', init);
  assert.deepStrictEqual(calls, [['/api/live', init]]);
  assert.strictEqual(win.MPC_HOST.apiBase, '');
  assert.strictEqual(win.MPC_HOST.assetBase, '/static');
});

check('host adapter: an MPC_HOST stub prefixes API + asset URLs and its fetch is used', () => {
  const calls = [];
  const hass = { states: {} };
  const win = {
    fetch: () => assert.fail('window.fetch must not be used when the host supplies fetch'),
    MPC_HOST: {
      apiBase: '/api/mpc_brain',
      assetBase: '/api/mpc_brain/static',
      fetch: (url, init) => { calls.push([url, init]); return Promise.resolve('ha'); },
      hass,
    },
  };
  const a = loadAdapter(win);
  assert.strictEqual(a.mpcUrl('/api'), '/api/mpc_brain/api');
  assert.strictEqual(a.mpcAsset('fp3d.js'), '/api/mpc_brain/static/fp3d.js');
  assert.strictEqual(a.mpcHass(), hass);
  a.mpcApi('/api/zones/series?hours=25', { cache: 'no-store' });
  a.mpcApi('/api/ev/car/preference', { method: 'POST' });
  assert.deepStrictEqual(calls.map((c) => c[0]), ['/api/mpc_brain/api/zones/series?hours=25', '/api/mpc_brain/api/ev/car/preference']);
  assert.strictEqual(calls[1][1].method, 'POST');
});

check('host adapter: a partial MPC_HOST keeps the defaults for the missing fields; hass is read live', () => {
  const win = { fetch: () => 'default', MPC_HOST: { apiBase: '/p' } };
  const a = loadAdapter(win);
  assert.strictEqual(a.mpcUrl('/api/x'), '/p/api/x');
  assert.strictEqual(a.mpcAsset('a.js'), '/static/a.js');
  assert.strictEqual(a.mpcApi('/api/x'), 'default');
  assert.strictEqual(a.mpcHass(), null);
  win.MPC_HOST.hass = { n: 2 }; // the host swaps in a fresh hass object on every state change
  assert.deepStrictEqual(a.mpcHass(), { n: 2 });
});

check('host adapter: no raw fetch( or absolute /api, /static request is left outside the adapter', () => {
  const lines = src.split('\n').map((l, i) => [i + 1, l]).filter(([, l]) => !/^\s*\/\//.test(l));
  const rawFetch = lines.filter(([, l]) => /(^|[^.\w])fetch\(/.test(l) && !/MPC_HOST_DEFAULTS/.test(l));
  assert.deepStrictEqual(rawFetch, [], `raw fetch( bypasses MPC_HOST: ${JSON.stringify(rawFetch)}`);
  const hrefs = lines.filter(([, l]) => /href="\/(api|static)/.test(l) || /\bsrc\s*=\s*['"]\/(api|static)/.test(l));
  assert.deepStrictEqual(hrefs, [], `absolute link bypasses MPC_HOST: ${JSON.stringify(hrefs)}`);
});

// ---- documented manual check (acceptance H1's alternative): the real timeline the Tester captured ----
//
// The tester's sample_plan_timeline.json is a Fable-pipeline artifact under /tmp, not part of this
// repo, so it may not exist for a later reader — this check is a bonus over real data when it's
// present (set MPC_SAMPLE_TIMELINE to point at a copy) and a documented no-op otherwise; either way
// the process still exits 0 as long as the synthetic checks above passed.
const SAMPLE =
  process.env.MPC_SAMPLE_TIMELINE ||
  '/tmp/fable-pipeline/heating-lookahead/tester/sample_plan_timeline.json';
if (fs.existsSync(SAMPLE)) {
  const envelope = JSON.parse(fs.readFileSync(SAMPLE, 'utf8'));
  const tl = envelope.data;
  assert(Array.isArray(tl) && tl.length > 0, 'sample_plan_timeline.json: expected a non-empty block array under .data');
  const zoneNames = Object.keys(tl[0].heat_kw || {});
  assert(zoneNames.length > 0, 'sample_plan_timeline.json: expected at least one heated zone in block 0');
  // The sample doesn't ship /api/zones' real per-zone max_heat_kw, so this checks the FORMULA's
  // behaviour holds over every real (block, zone) pair at a plausible relay rating, not the exact
  // house numbers (see docs/configuration.md for real per-zone heater limits).
  const ASSUMED_MAX_KW = 2.0;
  let sawFarHorizon = false,
    sawNonZeroDuty = false;
  for (const b of tl) {
    if (!isNearTermBlock(b.t, tl[0].t)) sawFarHorizon = true;
    for (const z of zoneNames) {
      const kw = b.heat_kw[z];
      assert(typeof kw === 'number' && isFinite(kw), `${z}@${b.t}: heat_kw must be a finite number, got ${kw}`);
      const d = relayDuty(kw, ASSUMED_MAX_KW);
      assert(d.dutyPct >= 0 && d.dutyPct <= 100, `${z}@${b.t}: dutyPct ${d.dutyPct} out of [0,100]`);
      assert(d.onBlocks >= 0 && d.onBlocks <= 4, `${z}@${b.t}: onBlocks ${d.onBlocks} out of [0,4]`);
      if (d.dutyPct > 0) sawNonZeroDuty = true;
    }
  }
  check(
    `relayDuty/isNearTermBlock hold over every block x zone in the real sample_plan_timeline.json ` +
      `(${tl.length} blocks x ${zoneNames.length} zones; far-horizon blocks present: ${sawFarHorizon}; ` +
      `any nonzero heat_kw in the sample: ${sawNonZeroDuty})`,
    () => {}
  );
} else {
  console.log(
    `skip - sample_plan_timeline.json not found at ${SAMPLE} (set MPC_SAMPLE_TIMELINE to point at a ` +
      `copy) — the synthetic checks above already covered the formulas`
  );
}

console.log(`\n${passed} check(s) passed.`);
