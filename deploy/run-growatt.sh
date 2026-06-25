#!/bin/sh
# Run the Growatt controller — the SOUTH bridge to the real inverter. It subscribes to the publisher's
# `mpc/control/growatt` commands and translates them into the real `energy/solar/command/...` Growatt
# MQTT commands.
#
# **It actuates the inverter ONLY when BOTH are true:** the config has `armed: true`, AND this script
# is run with `MPC_CONTROLLER_ARM=i-understand-this-actuates` in the environment. Otherwise it is
# dry-run (logs the would-send messages, touches nothing) — so the default below is dry-run, and
# arming is a deliberate, explicit act:
#
#   # dry-run (pre-flight: watch `docker logs -f mpc-growatt` for the would-send commands)
#   sh ./run-growatt.sh
#   # armed (only once loxone's Growatt control is OFF — never two controllers on one inverter)
#   MPC_CONTROLLER_ARM=i-understand-this-actuates sh ./run-growatt.sh
#
# Runs on caddy_net so it reaches the broker (mosquitto:1883). A `valid_until` deadman hands control
# back (failsafe) if the plan ever goes silent. The config is bind-mounted (edit + restart, no rebuild).
#
# Override for your host:
#   DOCKER   docker binary           (default: docker)
#   DIR      dir holding growatt.json5 (default: this script's dir)
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
ARM_ENV=""
if [ -n "$MPC_CONTROLLER_ARM" ]; then
  ARM_ENV="-e MPC_CONTROLLER_ARM=$MPC_CONTROLLER_ARM"
  echo "run-growatt.sh: ARM token present — controller will actuate IF its config has armed:true."
else
  echo "run-growatt.sh: no ARM token — dry-run (logs only, no actuation)."
fi
$DOCKER rm -f mpc-growatt 2>/dev/null
# shellcheck disable=SC2086
$DOCKER run -d --name mpc-growatt --restart unless-stopped \
  --network caddy_net \
  $ARM_ENV \
  -v "$DIR/growatt.json5:/app/config.json5:ro" \
  mpc-growatt
