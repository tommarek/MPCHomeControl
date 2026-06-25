#!/bin/sh
# Run the plan publisher — the NORTH bridge. It polls the read-only brain's /api/plan/latest and
# republishes per-block commands to the inert `mpc/control/...` MQTT namespace. This touches no
# hardware on its own (the device controllers translate mpc/control → real device commands), but it
# only emits when its config has `armed: true`.
#
# Runs on caddy_net so it reaches the brain (mpc-brain:3000) and the broker (mosquitto:1883) by name.
# The config is bind-mounted, so flip `armed` / edit topics with a host edit + restart (no rebuild).
#
# Override for your host:
#   DOCKER   docker binary           (default: docker)
#   DIR      dir holding publisher.json5 (default: this script's dir)
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
$DOCKER rm -f mpc-publisher 2>/dev/null
$DOCKER run -d --name mpc-publisher --restart unless-stopped \
  --network caddy_net \
  -v "$DIR/publisher.json5:/app/config.json5:ro" \
  mpc-publisher
