#!/bin/sh
# Run the unified Loxone controller — subscribes mpc/control/loxone and emits the key=value;… UDP
# datagram (prepended with the MPCActive heartbeat) to the Miniserver. Actuates (sends UDP) ONLY with
# BOTH config armed:true AND MPC_CONTROLLER_ARM=i-understand-this-actuates. Dry-run otherwise: logs the
# would-send datagram, sends nothing.
#
#   sh ./run-loxone.sh                                               # dry-run
#   MPC_CONTROLLER_ARM=i-understand-this-actuates sh ./run-loxone.sh # armed → sends UDP to Loxone
#
# Before arming, the Loxone side must be wired (MPCActive digital-input pulse → Off-Delay watchdog →
# ALIVE; every MPC-driven VI gated `value AND ALIVE`; failsafe hold) and the Miniserver's own heating
# control turned OFF — see docs/controllers.md.
#
# DEPLOY: this is a template — copy it together with the controller's config (controllers/loxone/
# loxone.json5) into one directory (e.g. the server's mpc-docker/ctrl/) and run it from there. DIR
# defaults to the script's own directory and MUST hold loxone.json5 (it is bind-mounted into the
# container); run-growatt.sh / run-publisher.sh follow the same pattern. Override DOCKER / DIR per host.
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
ARM_ENV=""
if [ -n "$MPC_CONTROLLER_ARM" ]; then
  ARM_ENV="-e MPC_CONTROLLER_ARM=$MPC_CONTROLLER_ARM"
  echo "run-loxone.sh: ARM token present — controller will SEND UDP IF its config has armed:true."
else
  echo "run-loxone.sh: no ARM token — dry-run (logs the would-send datagram, sends nothing)."
fi
$DOCKER rm -f mpc-loxone 2>/dev/null
# shellcheck disable=SC2086
$DOCKER run -d --name mpc-loxone --restart unless-stopped \
  --network caddy_net \
  $ARM_ENV \
  -v "$DIR/loxone.json5:/app/config.json5:ro" \
  mpc-loxone
