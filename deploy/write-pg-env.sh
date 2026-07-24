#!/bin/sh
# Generate mpc-docker/pg.env from the TeslaMate container's own database env — run ON the server.
# The DSN (with its password) is composed and written locally, chmod 600; it never travels or gets
# committed. run-container.sh sources pg.env on every launch, so a redeploy from any shell keeps
# the EV SoC source alive (plain env-forwarding silently dropped it on fresh-shell deploys).
#
# Usage:  DOCKER=/usr/local/bin/docker sh write-pg-env.sh [outdir]   (default outdir: this dir)
set -e
DOCKER="${DOCKER:-docker}"
OUT="${1:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}/pg.env"
U=$($DOCKER exec teslamate printenv DATABASE_USER)
P=$($DOCKER exec teslamate printenv DATABASE_PASS)
N=$($DOCKER exec teslamate printenv DATABASE_NAME)
H=$($DOCKER exec teslamate printenv DATABASE_HOST)
umask 077
printf 'export MPC_PG_TESLAMATE="host=%s port=5432 user=%s password=%s dbname=%s"\n' "$H" "$U" "$P" "$N" > "$OUT"
echo "wrote $OUT (host=$H, user=$U, db=$N)"
