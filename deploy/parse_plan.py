#!/usr/bin/env python3
# Reads an /api/plan/latest envelope on stdin, prints "ph|slot|soc|chg|bad" for healthcheck.sh.
import sys, json
try:
    d = json.load(sys.stdin).get('data', {})
    m = d.get('first_step', {}).get('mode', {})
    tl = d.get('timeline', [])
    # soc_kwh is the END-of-block SoC (the block's planned discharge already subtracted) — up to
    # ~1.3 kWh below what the inverter holds right now at 5.3 kW / 15-min blocks, which blinded
    # the discharge-stall floor guard. Reconstruct the block's STARTING SoC from its flows
    # (charge stores ~sqrt(eta), discharge draws ~1/sqrt(eta); 0.95 ~= sqrt(0.9) round-trip).
    soc = None
    if tl:
        b0 = tl[0]
        soc = b0.get('soc_kwh')
        if soc is not None:
            dt_h = 0.25
            soc += b0.get('discharge_kw', 0) * dt_h / 0.95 - b0.get('charge_kw', 0) * dt_h * 0.95
    # "kept the previous …" entries are ADVISORY (a refresh blip where the last-good model was
    # retained — the plan is still real); counting them as placeholders would trip the watchdog on
    # every transient DB hiccup. Genuine placeholders (flat curves, neutral calibration) still count.
    ph = [x for x in d.get('placeholder_inputs', []) if 'kept the previous' not in str(x)]
    slot = m.get('slot')
    chg = m.get('charge_kw')
    bad = 1 if (soc is not None and soc > 9.0 and slot == 'charge_from_grid') else 0
    deg = 1 if d.get('degraded') else 0
    rlx = 1 if d.get('relaxed') else 0
    print('|'.join([str(len(ph)), str(slot), str(soc), str(chg), str(bad), str(deg), str(rlx)]))
except Exception as e:
    print('|'.join(['ERR', 'parse', str(e).replace('|', ' '), '0', '1', '1', '1']))
