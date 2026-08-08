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
# umask covers CREATION only, so the secret is composed into a fresh temp file and moved into place
# below — never written into whatever mode an existing pg.env happened to have.
umask 077
# The DSN is written into a file that run-container.sh SOURCES, so the password must survive two
# layers intact:
#  1. shell — single-quote the value and escape any embedded single quote ('\'' idiom). Without
#     this a password containing $, `, " or \ would be expanded/mangled by the shell on `source`,
#     and one containing a quote could execute arbitrary code from a config file.
#  2. libpq — inside the DSN, backslash and single quote must be escaped and the value quoted, so a
#     password with a space or a quote still parses as one keyword/value pair.
libpq_escape() { printf '%s' "$1" | sed "s/\\\\/\\\\\\\\/g; s/'/\\\\'/g"; }
sh_squote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# EVERY keyword/value pair gets the libpq treatment, not just the password: user, dbname and host
# come from another container's environment too, and one containing a space, quote or backslash
# produced a DSN libpq mis-parses. run-container.sh then sources the file happily and the brain
# starts, with the only symptom a "postgres connect failed" line while the EV SoC source goes dark —
# the exact silent drop this script exists to prevent.
DSN="host='$(libpq_escape "$H")' port=5432 user='$(libpq_escape "$U")' \
password='$(libpq_escape "$P")' dbname='$(libpq_escape "$N")'"
# Write to a NEW temp file so `umask 077` actually applies, then move it into place (mv keeps the
# temp file's mode). `> "$OUT"` truncates an existing pg.env WITHOUT touching its mode, so a
# hand-made or restored 644 file received the password in the clear and only became 600 on the next
# line — a window that persists indefinitely if the script is interrupted between the two.
TMP="$OUT.tmp"
rm -f "$TMP"
printf 'export MPC_PG_TESLAMATE=%s\n' "$(sh_squote "$DSN")" > "$TMP"
chmod 600 "$TMP"
mv "$TMP" "$OUT"
echo "wrote $OUT (host=$H, user=$U, db=$N)"
