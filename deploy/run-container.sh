#!/bin/sh
# Run the read-only MPC brain container alongside the loxone stack (crash + reboot persistence).
# This is the read-only planner: it serves the dashboard/API and /api/plan/latest, and never
# actuates. The actuation path is the separate controller containers (see deploy/run-growatt.sh).
# Copy this next to the built binary + config.json5 + model.json5 and run it from there.
#
# config.json5 / model.json5 are mounted read-only from this dir, so edits take effect on restart
# (no image rebuild). The `data/` dir is a writable mount holding the forward-prediction snapshots
# (MPC_FORECAST_STORE), so the /api/forecast/validation history survives container recreation.
#
# Override these for your host (defaults assume this script sits in the build dir):
#   DOCKER       path to the docker binary           (default: docker)
#   DIR          dir holding the binary + json5s      (default: this script's directory)
#   LOXONE_ENV   path to the .env holding INFLUXDB_TOKEN
#   MPC_PORT     host port to publish the API on       (default: 127.0.0.1:3000)
#   MPC_PG_<NAME> a read-only Postgres DSN for a `data_sources` postgres connection (e.g.
#                MPC_PG_TESLAMATE="host=teslamate-db port=5432 user=teslamate password=… dbname=teslamate");
#                if exported, it is forwarded into the container. Keep the secret in the env, never here.
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
LOXONE_ENV="${LOXONE_ENV:-$DIR/.env}"
MPC_PORT="${MPC_PORT:-127.0.0.1:3000}"
TOKEN=$(grep -E '^INFLUXDB_TOKEN=' "$LOXONE_ENV" 2>/dev/null | head -1 | cut -d= -f2- | tr -d '\r"')
if [ -z "$TOKEN" ]; then
  echo "run-container.sh: INFLUXDB_TOKEN not found in '$LOXONE_ENV'; set LOXONE_ENV and retry." >&2
  exit 1
fi
# Forward any MPC_PG_* DSNs present in the environment (passthrough form: docker reads the value from
# this process's env, so the space-containing secret never appears on a command line or in this file).
PG_ENV=""
for v in $(env | sed -nE 's/^(MPC_PG_[A-Z0-9_]+)=.*/\1/p'); do
  PG_ENV="$PG_ENV -e $v"
done
mkdir -p "$DIR/data"
$DOCKER rm -f mpc-brain 2>/dev/null
# shellcheck disable=SC2086
$DOCKER run -d --name mpc-brain --restart unless-stopped \
  --network caddy_net \
  -e INFLUX_HOST=http://influxdb:8086 -e MPC_BIND=0.0.0.0 -e INFLUXDB_TOKEN="$TOKEN" \
  -e MPC_FORECAST_STORE=/app/data/forecast_snapshots.json \
  $PG_ENV \
  -v "$DIR/config.json5:/app/config.json5:ro" \
  -v "$DIR/model.json5:/app/model.json5:ro" \
  -v "$DIR/data:/app/data" \
  -p "$MPC_PORT:3000" \
  mpc-brain
