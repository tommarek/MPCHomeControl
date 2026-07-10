#!/bin/sh
# Run the open-meteo scraper — writes the future-dated hourly weather forecast (incl. the
# radiation fields the MPC's solar model consumes) into InfluxDB. Pure data collection: no
# actuation, no arm gate. The Influx token is sourced from the loxone .env (never stored here).
#
# Build (same static-musl flow as the controllers):
#   PATH=/tmp/zigdir:$PATH cargo zigbuild --release -p mpc-scraper-openmeteo \
#       --target x86_64-unknown-linux-musl
#   docker build -f controller.Dockerfile --build-arg BIN=mpc-scraper-openmeteo -t mpc-scraper-openmeteo .
#
# Override for your host:
#   DOCKER      docker binary                  (default: docker)
#   DIR         dir holding scraper.json5      (default: this script's dir)
#   LOXONE_ENV  .env file exporting INFLUXDB_TOKEN
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
LOXONE_ENV="${LOXONE_ENV:?set LOXONE_ENV to the .env file holding INFLUXDB_TOKEN}"
TOKEN=$(sed -n 's/^INFLUXDB_TOKEN=//p' "$LOXONE_ENV" | tr -d '"' | head -1)
[ -n "$TOKEN" ] || { echo "INFLUXDB_TOKEN not found in $LOXONE_ENV" >&2; exit 1; }
$DOCKER rm -f mpc-scraper-openmeteo 2>/dev/null
$DOCKER run -d --name mpc-scraper-openmeteo --restart unless-stopped \
  --network caddy_net \
  -e INFLUXDB_TOKEN="$TOKEN" \
  -v "$DIR/scraper.json5:/app/config.json5:ro" \
  mpc-scraper-openmeteo
