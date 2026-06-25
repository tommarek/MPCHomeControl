#!/bin/sh
# One-shot health verdict for the armed MPC control stack (brain + publisher + growatt).
# Prints "OK | <summary>" or "ANOMALY:<flags> | <summary>". Used by the monitoring watchdog.
D="${DOCKER:-/usr/local/bin/docker}"
LAN="${MPC_LAN_URL:-http://127.0.0.1:3000}"   # override per host (the brain's published API URL)
N=$($D ps --filter name=mpc-brain --filter name=mpc-publisher --filter name=mpc-growatt --format '{{.Names}}' | wc -l | tr -d ' ')
RZ=$(curl -s -m8 -o /dev/null -w '%{http_code}' "$LAN/readyz")
PLAN=$(curl -s -m8 "$LAN/api/plan/latest")
DEC=$(printf '%s' "$PLAN" | python3 "$(dirname "$0")/parse_plan.py")
PH=$(echo "$DEC" | cut -d'|' -f1); SLOT=$(echo "$DEC" | cut -d'|' -f2)
SOC=$(echo "$DEC" | cut -d'|' -f3); CHG=$(echo "$DEC" | cut -d'|' -f4); BAD=$(echo "$DEC" | cut -d'|' -f5)
GERR=$($D logs --since 11m mpc-growatt 2>&1 | grep -ciE 'GAVE UP|panic')
PFAIL=$($D logs --since 11m mpc-publisher 2>&1 | grep -ciE 'poll.*failed|panic')
ACKF=$($D logs --since 11m mpc-growatt 2>&1 | grep -c '"success":false')
# Live inverter telemetry: confirm the plan is actually being executed (e.g. discharging when told to).
TEL=$(timeout 8 $D exec mosquitto mosquitto_sub -t energy/solar -C 1 2>/dev/null | python3 -c "import sys,json
try:
 d=json.load(sys.stdin); print('|'.join([str(d.get('DischargePower',0)), str(d.get('ChargePower',0)), str(d.get('ACPowerToGrid',0))]))
except Exception: print('?|?|?')")
DIS=$(echo "$TEL" | cut -d'|' -f1); CHGW=$(echo "$TEL" | cut -d'|' -f2); EXP=$(echo "$TEL" | cut -d'|' -f3)
# Discharge stall: plan says discharge_to_grid with clear headroom (soc well above the floor), but the
# inverter isn't discharging — the command didn't take effect. (Numeric soc>2.8 guards the ~2 kWh floor;
# the leading-digit check tolerates a non-numeric soc.)
STALL=0
case "$SLOT" in discharge_to_grid)
  case "$DIS" in 0|0.0) case "$SOC" in 2.[0-7]*|2|1.*|0.*) ;; *) STALL=1;; esac;; esac;; esac
# `topoff` (charge_from_grid at ~full SoC) is informational: the stop-SoC caps the charge, no overcharge.
SUMMARY="containers=$N readyz=$RZ slot=$SLOT soc=$SOC chg=$CHG dis_w=$DIS exp_w=$EXP ph=$PH gerr=$GERR pfail=$PFAIL ackfail=$ACKF topoff=$BAD"
A=""
[ "$N" = "3" ] || A="$A containers_down"
[ "$RZ" = "200" ] || A="$A readyz"
[ "$PH" = "0" ] || A="$A placeholders"
[ "$GERR" = "0" ] || A="$A growatt_giveup_or_panic"
[ "$PFAIL" -lt 2 ] || A="$A publisher_failures"   # tolerate a single transient poll-miss (deadman has 120s headroom); trip on 2+
[ "$ACKF" = "0" ] || A="$A inverter_ack_failure"
[ "$STALL" = "0" ] || A="$A discharge_not_executing"
if [ -n "$A" ]; then echo "ANOMALY:$A | $SUMMARY"; else echo "OK | $SUMMARY"; fi
