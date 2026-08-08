#!/bin/sh
# Run the read-only MPC brain container alongside the loxone stack (crash + reboot persistence).
# This is the read-only planner: it serves the dashboard/API and /api/plan/latest, and never
# actuates. The actuation path is the separate controller containers (see deploy/run-growatt.sh).
# Copy this next to the built binary + config.json5 + model.json5 and run it from there.
#
# config.json5 / model.json5 are mounted read-only from this dir, so edits take effect on restart
# (no image rebuild). The `data/` dir is a writable mount holding the forward-prediction snapshots
# (MPC_FORECAST_STORE) and the dashboard's EV preference overrides (MPC_EV_PREF_STORE), so both
# survive container recreation.
#
# Override these for your host (defaults assume this script sits in the build dir):
#   DOCKER       path to the docker binary           (default: docker)
#   DIR          dir holding the binary + json5s      (default: this script's directory)
#   LOXONE_ENV   path to the .env holding INFLUXDB_TOKEN
#   MPC_PORT     host port to publish the API on       (default: 127.0.0.1:3000)
#   MPC_PG_<NAME> a read-only Postgres DSN for a `data_sources` postgres connection (e.g.
#                MPC_PG_TESLAMATE="host=teslamate-db port=5432 user=teslamate password=… dbname=teslamate");
#                if exported, it is forwarded into the container. Keep the secret in the env, never here.
#                Env forwarding alone is fragile (a redeploy from a fresh shell silently drops the
#                DSN and the EV SoC goes dark) — so DSNs can also live in an optional `$DIR/pg.env`
#                (chmod 600, `export MPC_PG_…="…"` lines), sourced below. Generate it server-side
#                from the database's own env; never commit it.
DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
LOXONE_ENV="${LOXONE_ENV:-$DIR/.env}"
MPC_PORT="${MPC_PORT:-127.0.0.1:3000}"
# Both secret files (pg.env: a DB password; api.env: the write-auth token) are SOURCED, i.e. executed
# as shell code by this script — so a group-writable one is arbitrary code execution as the deploying
# user, and a group/world-readable one leaks the credential to every local account. Require 600/400
# for both. Note the stat fallback deliberately yields an UNSAFE sentinel: defaulting to "600" when
# `stat -c` is unavailable (BSD stat, a stripped image) would make the guard fail OPEN, which is
# worse than no guard because the operator never sees the error they are relying on.
require_600() {
  perms=$(stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1" 2>/dev/null || echo unknown)
  case "$perms" in
    600|400) return 0 ;;
    *)
      echo "run-container.sh: $1 is mode $perms; run 'chmod 600 $1' (it holds a secret)." >&2
      exit 1
      ;;
  esac
}
# shellcheck disable=SC1091
[ -f "$DIR/pg.env" ] && { require_600 "$DIR/pg.env"; . "$DIR/pg.env"; }
TOKEN=$(grep -E '^INFLUXDB_TOKEN=' "$LOXONE_ENV" 2>/dev/null | head -1 | cut -d= -f2- | tr -d '\r"')
if [ -z "$TOKEN" ]; then
  echo "run-container.sh: INFLUXDB_TOKEN not found in '$LOXONE_ENV'; set LOXONE_ENV and retry." >&2
  exit 1
fi
# Forward any MPC_PG_* DSNs present in the environment (passthrough form: docker reads the value from
# this process's env, so the space-containing secret never appears on a command line or in this file).
# `set` lists shell variables too, so a pg.env written without `export` is caught here and exported
# before the `env` scan below (which only ever sees exported ones — the same silent-drop trap).
for v in $(set | sed -nE 's/^(MPC_PG_[A-Z0-9_]+)=.*/\1/p'); do
  export "$v" 2>/dev/null || true
done
PG_ENV=""
for v in $(env | sed -nE 's/^(MPC_PG_[A-Z0-9_]+)=.*/\1/p'); do
  PG_ENV="$PG_ENV -e $v"
done
mkdir -p "$DIR/data"
# Optional live-dashboard dir: when $DIR/dashboard exists it is mounted read-only and the brain
# serves UI files from it (falling back per-file to the embedded copies) — a UI tweak is then
# "copy the file + hard-refresh", no container restart / MPC interruption.
DASH_OPTS=""
[ -d "$DIR/dashboard" ] && DASH_OPTS="-v $DIR/dashboard:/app/dashboard:ro -e MPC_DASHBOARD_DIR=/app/dashboard"
# Optional write-auth on the EV preference endpoints (docs/api.md): forwarded by NAME so the token
# never lands on a command line. Without this the documented `MPC_API_TOKEN` could never take
# effect in a shipped deployment — the mutating routes would stay open on the LAN.
# Persistent source first (pg.env is already sourced above; api.env keeps the token off the command
# line and out of git). Falling back to the ambient shell alone made write-auth vanish on any
# redeploy from a fresh shell — the same silent-drop that took the TeslaMate DSN out.
# `-e MPC_API_TOKEN` is the NAME-ONLY form: docker reads the value from THIS process's environment,
# so the token never lands on a command line. A sourced file using a plain `MPC_API_TOKEN=…`
# assignment sets a shell variable that is NOT exported — the flag was then added while docker set
# nothing, the brain started with the variable unset, and `require_api_token()` returns Ok
# unconditionally: the mutating EV routes silently open to the whole LAN while the operator believes
# write-auth is on. So export it explicitly and verify it actually reached the environment.
# api.env holds MPC_API_TOKEN as a single `export MPC_API_TOKEN="..."` line. Refuse to source it if
# it is group/world-readable: an operator following the hint below creates it by hand at the default
# umask (644), leaving the write-auth credential readable by every local user — pg.env is created
# 600 by write-pg-env.sh and this one deserves the same.
if [ -f "$DIR/api.env" ]; then
  require_600 "$DIR/api.env"
  # shellcheck disable=SC1091
  . "$DIR/api.env"
fi
AUTH_ENV=""
if [ -n "${MPC_API_TOKEN:-}" ]; then
  export MPC_API_TOKEN
  if env | grep -q '^MPC_API_TOKEN='; then
    AUTH_ENV="-e MPC_API_TOKEN"
  else
    echo "run-container.sh: MPC_API_TOKEN could not be exported; refusing to start with the \
mutating API routes unauthenticated." >&2
    exit 1
  fi
else
  echo "run-container.sh: no MPC_API_TOKEN — the mutating /api/ev routes will accept any LAN \
client (set one in $DIR/api.env to require auth)." >&2
fi
# Name-only passthrough for the influx token too — by VALUE it sat in the `docker run` argv, readable
# by any local user via ps//proc for the duration of the call, and landed in shell history. This is
# the convention the MPC_PG_* DSNs, MPC_API_TOKEN and both scraper scripts already follow; the token
# is the one that grants WRITE access to the house database, so it least belongs on a command line.
export INFLUXDB_TOKEN="$TOKEN"
$DOCKER rm -f mpc-brain 2>/dev/null
# shellcheck disable=SC2086
$DOCKER run -d --name mpc-brain --restart unless-stopped \
  --network caddy_net \
  -e INFLUX_HOST=http://influxdb:8086 -e MPC_BIND=0.0.0.0 -e INFLUXDB_TOKEN \
  -e MPC_FORECAST_STORE=/app/data/forecast_snapshots.json \
  -e MPC_EV_PREF_STORE=/app/data/ev_prefs.json \
  $PG_ENV $DASH_OPTS $AUTH_ENV \
  -v "$DIR/config.json5:/app/config.json5:ro" \
  -v "$DIR/model.json5:/app/model.json5:ro" \
  -v "$DIR/data:/app/data" \
  -p "$MPC_PORT:3000" \
  mpc-brain
