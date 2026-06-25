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
# `topoff` (charge_from_grid at ~full SoC) is informational only: the stop-SoC caps the actual charge,
# so the inverter won't overcharge — not an anomaly. The real trips are operational failures.
SUMMARY="containers=$N readyz=$RZ slot=$SLOT soc=$SOC chg=$CHG ph=$PH gerr=$GERR pfail=$PFAIL ackfail=$ACKF topoff=$BAD"
A=""
[ "$N" = "3" ] || A="$A containers_down"
[ "$RZ" = "200" ] || A="$A readyz"
[ "$PH" = "0" ] || A="$A placeholders"
[ "$GERR" = "0" ] || A="$A growatt_giveup_or_panic"
[ "$PFAIL" = "0" ] || A="$A publisher_failures"
[ "$ACKF" = "0" ] || A="$A inverter_ack_failure"
if [ -n "$A" ]; then echo "ANOMALY:$A | $SUMMARY"; else echo "OK | $SUMMARY"; fi
