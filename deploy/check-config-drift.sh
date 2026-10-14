#!/bin/sh
# Refuse a deploy whose config.json5 would silently overwrite comfort targets changed live.
#
# The brain rewrites the `comfort: { ... }` block of the LIVE config.json5 when a target is edited
# from Home Assistant (POST /api/comfort/<zone>). A deploy that copies the repo's config.json5 over
# it would undo those edits. Call this before the swap in deploy-live-with-rollback.sh:
#
#   sh ./check-config-drift.sh config.json5 config.json5.new || exit $?
#
# Exit 0: the comfort blocks match (or ALLOW_CONFIG_DRIFT=1). Exit 4: they differ (diff printed).
# Exit 2: usage / unreadable file / no comfort block.
set -eu

live=${1:?usage: check-config-drift.sh <live config.json5> <new config.json5>}
new=${2:?usage: check-config-drift.sh <live config.json5> <new config.json5>}

# Print the top-level `comfort: {` block: from its first line to the line where the brace depth
# returns to zero. Braces inside `//` comments are ignored.
extract() {
  awk '
    function code(s) { i = index(s, "//"); return i ? substr(s, 1, i - 1) : s }
    {
      c = code($0)
      if (!on && c ~ /^[ \t]*comfort:[ \t]*\{/) on = 1
      if (on) {
        print $0
        n = gsub(/\{/, "{", c); m = gsub(/\}/, "}", c)
        depth += n - m
        if (depth <= 0) exit
      }
    }
  ' "$1"
}

for f in "$live" "$new"; do
  [ -r "$f" ] || { echo "check-config-drift: cannot read $f" >&2; exit 2; }
  [ -n "$(extract "$f")" ] || { echo "check-config-drift: no comfort block in $f" >&2; exit 2; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
extract "$live" > "$tmp/live"
extract "$new" > "$tmp/new"

if diff -u "$tmp/live" "$tmp/new" > "$tmp/diff"; then
  exit 0
fi

echo "check-config-drift: the comfort block of $new differs from the live $live:" >&2
cat "$tmp/diff" >&2
if [ "${ALLOW_CONFIG_DRIFT:-}" = 1 ]; then
  echo "check-config-drift: ALLOW_CONFIG_DRIFT=1 set, continuing" >&2
  exit 0
fi
echo "check-config-drift: pull the live config into the repo first (see CLAUDE.md Deployment), or set ALLOW_CONFIG_DRIFT=1 to overwrite it." >&2
exit 4
