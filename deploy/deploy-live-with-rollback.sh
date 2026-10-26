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
# config, stop the brain, swap them in, rebuild the image, restart the container through
# run-container.sh, then
# watch WATCH_S seconds for a fresh plan: a `[mpc] 20..` log line AND /readyz 200. Otherwise roll
# both files back and restart the old version.
#
# Environment (same names as run-container.sh): DOCKER (default docker), DIR (default: this
# script's directory), WATCH_S (default 300), READYZ_URL (default http://127.0.0.1:3000/readyz;
# use the LAN address when MPC_PORT is a LAN bind), plus whatever run-container.sh needs
# (MPC_PORT, LOXONE_ENV).
# Exit: 0 deployed, 1 rolled back, 2 missing input, 3 rollback FAILED (files need fixing by hand),
# 4 comfort-block drift, or the live config changed during the deploy.
set -u

DOCKER="${DOCKER:-docker}"
DIR="${DIR:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}"
WATCH_S="${WATCH_S:-300}"
READYZ_URL="${READYZ_URL:-http://127.0.0.1:3000/readyz}"
cd "$DIR" || exit 2

for f in mpc_home_control.new conf/config.json5.new conf/config.json5 mpc_home_control run-container.sh check-config-drift.sh; do
  [ -e "$f" ] || { echo "deploy: missing $f in $DIR" >&2; exit 2; }
done

# The drift check reads the backup, and the swap below refuses if the live file no longer matches
# it, so a comfort edit landing at any point before the swap aborts the deploy instead of being lost.
stamp=$(date +%Y%m%d-%H%M%S)
cp -p conf/config.json5 "conf/config.json5.pre-$stamp" || exit 2
sh ./check-config-drift.sh "conf/config.json5.pre-$stamp" conf/config.json5.new
rc=$?
if [ "$rc" -ne 0 ]; then
  rm -f "conf/config.json5.pre-$stamp"
  echo "deploy: aborted before any change (exit $rc)" >&2
  exit "$rc"
fi
cp -p mpc_home_control "mpc_home_control.pre-$stamp" || exit 2

restart() {
  $DOCKER build -t mpc-brain . >/dev/null && sh ./run-container.sh
}

# Restore by copy-to-temp + rename: the brain (root in the container) replaces conf/config.json5 on
# every comfort edit, so the live file may be root-owned and cannot be overwritten in place.
restore() {
  if ! { cp -p "$1" "$2.rollback-tmp" && mv -f "$2.rollback-tmp" "$2"; }; then
    echo "deploy: ROLLBACK FAILED: could not restore $2 from $1 -- fix it by hand" >&2
    exit 3
  fi
}

# Comfort targets edited from Home Assistant since the backup are reverted with the rest of the
# config (the backup is the one the old binary is known to run with); the diff is printed so they
# can be re-applied.
restore_files() {
  echo "deploy: ROLLING BACK to the pre-$stamp files" >&2
  restore "mpc_home_control.pre-$stamp" mpc_home_control
  drift=$(ALLOW_CONFIG_DRIFT='' sh ./check-config-drift.sh conf/config.json5 "conf/config.json5.pre-$stamp" 2>&1)
  case $? in
    0) ;;
    4)
      printf '%s\n' "$drift" | grep -v '^check-config-drift: pull the live config' >&2
      echo "deploy: comfort targets were edited during the deploy; REVERTING them to the pre-$stamp values (the '-' lines above) -- re-apply them from Home Assistant" >&2
      ;;
    *) printf '%s\n' "$drift" >&2 ;;
  esac
  restore "conf/config.json5.pre-$stamp" conf/config.json5
}

# The brain is stopped for the config swap and stays stopped until run-container.sh replaces it, so
# when a restart fails before that, the old container (image unchanged) is started again here.
start_old() {
  $DOCKER start mpc-brain >/dev/null || echo "deploy: could not start the old container, check it by hand" >&2
}

rollback() {
  restore_files
  if ! restart; then
    echo "deploy: rollback restart FAILED, check the container by hand" >&2
    start_old
  fi
  exit 1
}

# The live config is never replaced before this can run, so only the binary is restored: restoring
# the config too would revert a comfort edit that landed during the deploy. The image was not
# rebuilt, so starting the stopped container brings back the old version.
swap_failed() {
  echo "deploy: swapping in $1 FAILED; restarting the old container" >&2
  rm -f conf/config.json5.deploy-tmp
  start_old
  restore "mpc_home_control.pre-$stamp" mpc_home_control
  exit "${2:-1}"
}

# Swap in (rename within the same dir, so each file changes atomically). The staging names must
# differ from the brain's own comfort-edit temp files in conf/.
# The brain is stopped first, so no comfort edit can rename its file into place mid-swap. The live
# config is still moved aside and compared, which catches an edit that landed after the backup.
mv -f mpc_home_control.new mpc_home_control || swap_failed mpc_home_control
cp conf/config.json5.new conf/config.json5.deploy-tmp || swap_failed conf/config.json5
$DOCKER stop mpc-brain >/dev/null 2>&1 || echo "deploy: mpc-brain was not running" >&2
aside="conf/config.json5.swapped-$stamp"
mv conf/config.json5 "$aside" || swap_failed conf/config.json5
if cmp -s "$aside" "conf/config.json5.pre-$stamp" && ln conf/config.json5.deploy-tmp conf/config.json5; then
  rm -f conf/config.json5.deploy-tmp "$aside" conf/config.json5.new
else
  rc=4
  if [ -e conf/config.json5 ]; then
    rm -f "$aside"
  else
    cmp -s "$aside" "conf/config.json5.pre-$stamp" && rc=1
    mv "$aside" conf/config.json5 || {
      echo "deploy: could not move $aside back to conf/config.json5 -- fix it by hand" >&2
      rc=3
    }
  fi
  [ "$rc" -eq 4 ] && echo "deploy: live config changed during the deploy (a comfort edit?); re-pull it and redeploy" >&2
  swap_failed conf/config.json5 "$rc"
fi

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
