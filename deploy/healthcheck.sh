#!/bin/sh
# One-shot health verdict for the MPC control stack (brain + publisher + growatt + loxone).
# Prints "OK | <summary>" or "ANOMALY:<flags> | <summary>". Used by the monitoring watchdog.
D="${DOCKER:-/usr/local/bin/docker}"
LAN="${MPC_LAN_URL:-http://127.0.0.1:3000}"   # override per host (the brain's published API URL)
N=$("$D" ps --filter name=mpc-brain --filter name=mpc-publisher --filter name=mpc-growatt --filter name=mpc-loxone --format '{{.Names}}' | wc -l | tr -d ' ')
RZ=$(curl -s -m8 -o /dev/null -w '%{http_code}' "$LAN/readyz")
PLAN=$(curl -s -m8 "$LAN/api/plan/latest")
DEC=$(printf '%s' "$PLAN" | python3 "$(dirname "$0")/parse_plan.py")
PH=$(echo "$DEC" | cut -d'|' -f1); SLOT=$(echo "$DEC" | cut -d'|' -f2)
SOC=$(echo "$DEC" | cut -d'|' -f3); CHG=$(echo "$DEC" | cut -d'|' -f4); BAD=$(echo "$DEC" | cut -d'|' -f5)
DEG=$(echo "$DEC" | cut -d'|' -f6); RLX=$(echo "$DEC" | cut -d'|' -f7)
# EV: a car detected on OUR wallbox whose SoC is unknown is dropped from the plan entirely — the
# charger silently never charges. This was invisible for three weeks (a stale-SoC regression only
# showed on overnight cost-optimised charging), so it gets its own flag. Counts chargers with
# on_our_charger && soc_pct == null; parse failure counts as 1 so a broken endpoint also alarms.
EVSOC=$(curl -s -m8 "$LAN/api/ev" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin).get("data", [])
    print(sum(1 for e in d if e.get("on_our_charger") and e.get("soc_pct") is None))
except Exception:
    print(1)
' 2>/dev/null || echo 1)
# Zone-temperature staleness WHILE HEATING: a Tree sensor that drops out holds its last value, so
# the MPC keeps heating a room it can no longer see — and the Miniserver's over-temp 2-point is
# blind to it too (it reads the same held value). Loxone logs temperature on CHANGE, so a stable
# idle room legitimately goes hours between samples (a bare age check nuisance-alarms); but a room
# being actively heated is changing temperature and MUST produce samples. Counts zones commanded
# to heat in the current plan block whose latest measured sample is >90 min old or missing from
# the series entirely — but only after the condition has PERSISTED 2 h (state file below): at the
# moment heating starts on a long-idle zone the latest sample is legitimately hours old, and the
# first change-logged sample only lands once the slab has moved the air (up to ~75 min with the
# 30-min series windows + cache), so a single stale observation is normal start-of-run behaviour,
# not a dead sensor. State is per-zone first-seen epochs in /tmp (reboot resets = detection
# restarts, acceptable). Fetch failure counts as 1 so a broken endpoint also alarms.
ZSTALE=$(MPC_LAN="$LAN" python3 -c '
import json, os, re, urllib.request
from datetime import datetime, timezone, timedelta
try:
    lan = os.environ["MPC_LAN"]
    def get(p):
        with urllib.request.urlopen(lan + p, timeout=8) as r:
            return json.load(r)
    now = datetime.now(timezone.utc)
    def ts(iso):  # API stamps ns fractions; host python (3.8) parses at most 6 digits
        return datetime.fromisoformat(re.sub(r"[.](\d{1,6})\d*", r".\1", iso).replace("Z", "+00:00"))
    tl = get("/api/plan/latest").get("data", {}).get("timeline", [])
    cur = None
    for b in tl:  # the block covering now; falls back to the first block
        if cur is None or ts(b["t"]) <= now:
            cur = b
        else:
            break
    heating = {z for z, kw in (cur or {}).get("heat_kw", {}).items() if kw > 0.05}
    fresh = set()
    for z in get("/api/zones/series").get("data", []):
        s = z.get("series") or []
        if s and (now - ts(s[-1][0])) <= timedelta(minutes=90):
            fresh.add(z["zone"])
    stale_now = heating - fresh
    state_path = "/tmp/mpc_hc_zstale.json"
    try:
        with open(state_path) as f:
            seen = {z: t for z, t in json.load(f).items() if z in stale_now}
    except Exception:
        seen = {}
    epoch = now.timestamp()
    for z in stale_now:
        seen.setdefault(z, epoch)
    with open(state_path, "w") as f:
        json.dump(seen, f)
    print(sum(1 for t in seen.values() if epoch - t >= 7200))
except Exception:
    print(1)
' 2>/dev/null || echo 1)
GERR=$("$D" logs --since 11m mpc-growatt 2>&1 | grep -ciE 'GAVE UP|panic')
PFAIL=$("$D" logs --since 11m mpc-publisher 2>&1 | grep -ciE 'poll.*failed|panic')
# What the controller ACTUALLY logs on a refused/failed inverter write: `NAKed` per attempt,
# `UNACKED!` on an armed action that never confirmed, `GAVE UP` after exhausting retries. The old
# grep looked for the raw `"success":false` payload, which the controller parses but never prints —
# the check was structurally always 0, and a sustained NAK storm was invisible to the watchdog.
# Only the TERMINAL failures: `NAKed` is logged per retry attempt and the first NAK is routinely
# transient (a specific powerrate NAK recurs benignly every sell window) — zero tolerance on it
# would page constantly. `UNACKED!`/`GAVE UP` mean an armed write genuinely did not confirm.
ACKF=$("$D" logs --since 11m mpc-growatt 2>&1 | grep -ciE 'UNACKED|GAVE UP')
LERR=$("$D" logs --since 11m mpc-loxone 2>&1 | grep -ciE 'GAVE UP|panic')
# Live inverter telemetry: confirm the plan is actually being executed (e.g. discharging when told to).
# A missing field prints '?' (not 0): defaulting to 0 would read a renamed/absent field as a
# genuine discharge stall and page falsely.
TEL=$(timeout 8 "$D" exec mosquitto mosquitto_sub -t energy/solar -C 1 2>/dev/null | python3 -c "import sys,json
try:
 d=json.load(sys.stdin); print('|'.join([str(d[k]) if k in d else '?' for k in ('DischargePower','ChargePower','ACPowerToGrid')]))
except Exception: print('?|?|?')")
DIS=$(echo "$TEL" | cut -d'|' -f1); CHGW=$(echo "$TEL" | cut -d'|' -f2); EXP=$(echo "$TEL" | cut -d'|' -f3)
# Discharge stall: plan says discharge_to_grid with clear headroom (soc well above the floor), but the
# inverter isn't discharging — the command didn't take effect. (Numeric soc>2.8 guards the ~2 kWh floor;
# the leading-digit check tolerates a non-numeric soc.)
STALL=0
case "$SLOT" in discharge_to_grid)
  case "$DIS" in 0|0.0) case "$SOC" in 2.[0-7]*|2|1.*|0.*) ;; *) STALL=1;; esac;; esac;; esac
# `topoff` (charge_from_grid at ~full SoC) is informational: the stop-SoC caps the charge, no overcharge.
SUMMARY="containers=$N readyz=$RZ slot=$SLOT soc=$SOC chg=$CHG dis_w=$DIS exp_w=$EXP ph=$PH deg=$DEG rlx=$RLX evsoc_missing=$EVSOC zstale=$ZSTALE gerr=$GERR pfail=$PFAIL ackfail=$ACKF lerr=$LERR topoff=$BAD"
A=""
[ "$N" = "4" ] || A="$A containers_down"
[ "$RZ" = "200" ] || A="$A readyz"
[ "$PH" = "0" ] || A="$A placeholders"
# A degraded or relaxed plan makes the publisher skip ALL commands — every controller then
# deadman-reverts and the house silently stops being MPC-controlled while readyz stays 200 and all
# containers run. These flags are the ONLY watchdog-visible signal of that state.
[ "$DEG" = "0" ] || A="$A plan_degraded"
[ "$RLX" = "0" ] || A="$A plan_relaxed"
[ "$EVSOC" = "0" ] || A="$A ev_soc_missing"
[ "$ZSTALE" = "0" ] || A="$A zone_temp_stale"
[ "$GERR" = "0" ] || A="$A growatt_giveup_or_panic"
[ "$PFAIL" -lt 2 ] || A="$A publisher_failures"   # tolerate a single transient poll-miss (deadman has 120s headroom); trip on 2+
[ "$ACKF" = "0" ] || A="$A inverter_ack_failure"
[ "$LERR" = "0" ] || A="$A loxone_giveup_or_panic"
[ "$STALL" = "0" ] || A="$A discharge_not_executing"
# The stall check is vacuous when telemetry can't be observed ('?' fields) — flag that state
# itself instead of silently passing the one execution check.
[ "$DIS" != "?" ] || A="$A telemetry_unavailable"
if [ -n "$A" ]; then echo "ANOMALY:$A | $SUMMARY"; else echo "OK | $SUMMARY"; fi
