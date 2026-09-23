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
  ${extractFunction(src, 'isRelayOn')}
  ${extractFunction(src, 'solarSplitText')}
  ${extractFunction(src, 'heatingBlockClass')}
  ${extractFunction(src, 'mergeOnPeriods')}
  ${extractConst(src, 'WEEKDAYS')}
  ${extractConst(src, 'MONTHS')}
  ${extractFunction(src, 'localMidnights')}
  scope.relayDuty = relayDuty;
  scope.isNearTermBlock = isNearTermBlock;
  scope.isRelayOn = isRelayOn;
  scope.solarSplitText = solarSplitText;
  scope.heatingBlockClass = heatingBlockClass;
  scope.mergeOnPeriods = mergeOnPeriods;
  scope.localMidnights = localMidnights;
  `
)(scope);
const { relayDuty, isNearTermBlock, isRelayOn, solarSplitText, heatingBlockClass, mergeOnPeriods, localMidnights } = scope;

let passed = 0;
function check(desc, fn) {
  fn();
  passed++;
  console.log(`ok - ${desc}`);
}

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
