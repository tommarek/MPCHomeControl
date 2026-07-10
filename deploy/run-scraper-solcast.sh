#!/bin/sh
# Run the Solcast scraper — writes the house's solar_forecast_history snapshots (hourly_json +
# p10/p90) on a budget-guarded local-hour schedule. Pure data collection: no actuation, no arm
# gate. REPLACES the loxone Solcast fetcher (one API-budget owner — see docs/scrapers.md).
#
# Build (same static-musl flow as the controllers):
#   PATH=/tmp/zigdir:$PATH cargo zigbuild --release -p mpc-scraper-solcast \
#       --target x86_64-unknown-linux-musl
#   docker build -f controller.Dockerfile --build-arg BIN=mpc-scraper-solcast -t mpc-scraper-solcast .
#
# Override for your host:
#   DOCKER           docker binary                  (default: docker)
#   DIR              dir holding scraper.json5      (default: this script's dir)
#   LOXONE_ENV       .env file exporting INFLUXDB_TOKEN
#   SOLCAST_API_KEY  the Solcast API key (required)
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
LOXONE_ENV="${LOXONE_ENV:?set LOXONE_ENV to the .env file holding INFLUXDB_TOKEN}"
: "${SOLCAST_API_KEY:?set SOLCAST_API_KEY}"
TOKEN=$(sed -n 's/^INFLUXDB_TOKEN=//p' "$LOXONE_ENV" | tr -d '"' | head -1)
[ -n "$TOKEN" ] || { echo "INFLUXDB_TOKEN not found in $LOXONE_ENV" >&2; exit 1; }
$DOCKER rm -f mpc-scraper-solcast 2>/dev/null
$DOCKER run -d --name mpc-scraper-solcast --restart unless-stopped \
  --network caddy_net \
  -e INFLUXDB_TOKEN="$TOKEN" \
  -e SOLCAST_API_KEY="$SOLCAST_API_KEY" \
  -v "$DIR/scraper.json5:/app/config.json5:ro" \
  mpc-scraper-solcast
