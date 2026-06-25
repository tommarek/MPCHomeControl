#!/usr/bin/env python3
# Reads an /api/plan/latest envelope on stdin, prints "ph|slot|soc|chg|bad" for healthcheck.sh.
import sys, json
try:
    d = json.load(sys.stdin).get('data', {})
    m = d.get('first_step', {}).get('mode', {})
    tl = d.get('timeline', [])
    soc = tl[0].get('soc_kwh') if tl else None
    ph = d.get('placeholder_inputs', [])
    slot = m.get('slot')
    chg = m.get('charge_kw')
    bad = 1 if (soc is not None and soc > 9.0 and slot == 'charge_from_grid') else 0
    print('|'.join([str(len(ph)), str(slot), str(soc), str(chg), str(bad)]))
except Exception as e:
    print('|'.join(['ERR', 'parse', str(e).replace('|', ' '), '0', '1']))
