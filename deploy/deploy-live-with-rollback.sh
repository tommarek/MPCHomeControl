#!/bin/sh
# Rollback-guarded deploy of the read-only brain. Run it from the deploy dir (the one holding
# Dockerfile, mpc_home_control, conf/, model.json5, run-container.sh, check-config-drift.sh).
#
# Inputs, copied there beforehand (see CLAUDE.md, Deployment):
#   mpc_home_control.new      the new static binary
#   conf/config.json5.new     the repo's config.json5
#
# Steps: refuse if the comfort block of conf/config.json5.new differs from the live one (targets
# edited from Home Assistant; override with ALLOW_CONFIG_DRIFT=1), back up the live binary and
# config, swap them in, rebuild the image, restart the container through run-container.sh, then
# watch WATCH_S seconds for a fresh plan: a `[mpc] 20..` log line AND /readyz 200. Otherwise roll
# both files back and restart the old version.
#
# Environment (same names as run-container.sh): DOCKER (default docker), DIR (default: this
# script's directory), WATCH_S (default 300), READYZ_URL (default http://127.0.0.1:3000/readyz;
# use the LAN address when MPC_PORT is a LAN bind), plus whatever run-container.sh needs
# (MPC_PORT, LOXONE_ENV).
# Exit: 0 deployed, 1 rolled back, 2 missing input, 4 comfort-block drift.
set -u

DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
WATCH_S="${WATCH_S:-300}"
READYZ_URL="${READYZ_URL:-http://127.0.0.1:3000/readyz}"
cd "$DIR" || exit 2

for f in mpc_home_control.new conf/config.json5.new conf/config.json5 mpc_home_control run-container.sh check-config-drift.sh; do
  [ -e "$f" ] || { echo "deploy: missing $f in $DIR" >&2; exit 2; }
done

sh ./check-config-drift.sh conf/config.json5 conf/config.json5.new
rc=$?
[ "$rc" -eq 0 ] || { echo "deploy: aborted before any change (exit $rc)" >&2; exit "$rc"; }

stamp=$(date +%Y%m%d-%H%M%S)
cp -p mpc_home_control "mpc_home_control.pre-$stamp" || exit 2
cp -p conf/config.json5 "conf/config.json5.pre-$stamp" || exit 2

restart() {
  $DOCKER build -t mpc-brain . >/dev/null && sh ./run-container.sh
}

rollback() {
  echo "deploy: ROLLING BACK to the pre-$stamp files" >&2
  cp -p "mpc_home_control.pre-$stamp" mpc_home_control
  cp -p "conf/config.json5.pre-$stamp" conf/config.json5
  restart || echo "deploy: rollback restart FAILED, check the container by hand" >&2
  exit 1
}

# Swap in (rename within the same dir, so each file changes atomically).
mv mpc_home_control.new mpc_home_control
cp conf/config.json5.new conf/config.json5.tmp && mv conf/config.json5.tmp conf/config.json5
rm -f conf/config.json5.new

since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
restart || rollback

waited=0
while [ "$waited" -lt "$WATCH_S" ]; do
  sleep 10
  waited=$((waited + 10))
  ready=$(curl -s -m 8 -o /dev/null -w '%{http_code}' "$READYZ_URL" || true)
  if [ "$ready" = 200 ] && $DOCKER logs --since "$since" mpc-brain 2>&1 | grep -q '^\[mpc\] 20'; then
    echo "OK: plan published after ${waited}s"
    exit 0
  fi
done
echo "deploy: no fresh plan within ${WATCH_S}s" >&2
rollback
