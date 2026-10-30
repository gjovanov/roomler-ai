#!/usr/bin/env bash
#
# fr65-stall-census.sh — run the FR-65 stall census on Windows fleet hosts.
#
# usage: scripts/fr65-stall-census.sh [--days N] [--rows] [--probes] <host> [<host> ...]
#
# --probes runs the caps-probe census instead (scripts/fr65-probe-census.ps1,
# FR-65 AC6): probe runs per daemon start, and how many were at steady state.
#
# Runs scripts/fr65-stall-census.ps1 ON each host through `roomler exec`, as an
# -EncodedCommand, so no quoting has to survive exec's argv re-split (it does
# not: a comma list or a quoted pattern arrives mangled — see the census docs in
# docs/stall-watch.md §7). Read-only on the host: it reads the daemon's dated
# logs and prints a summary per STALL class.
#
# It is the "same instrument" FR-65 AC7 asks for: the before of a change to the
# watch is this script on the build without it, the after is this script on the
# build with it, on the same hosts, over windows of the same length.
#
# ⚠️ An exec sweep is a biased sample — only hosts with exec on and reachable.
# Report per host; never turn it into a fleet-wide proportion.
set -euo pipefail

days=14
rows=0
which=stall
hosts=()
while [ $# -gt 0 ]; do
  case "$1" in
    --days) days="${2:?--days needs a number}"; shift 2 ;;
    --rows) rows=1; shift ;;
    --probes) which=probe; shift ;;
    -h|--help) sed -n '3,21p' "$0"; exit 0 ;;
    *) hosts+=("$1"); shift ;;
  esac
done
[ ${#hosts[@]} -gt 0 ] || { echo "usage: $0 [--days N] [--rows] [--probes] <host> ..." >&2; exit 2; }
case "$days" in ''|*[!0-9]*) echo "--days must be a number" >&2; exit 2 ;; esac

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/fr65-$which-census.ps1"
[ -f "$script" ] || { echo "missing $script" >&2; exit 2; }

prelude="\$Days = $days;"
[ "$rows" = 1 ] && prelude="$prelude \$Rows = \$true;"
# PowerShell's -EncodedCommand is base64 of UTF-16LE.
enc=$( { printf '%s\n' "$prelude"; cat "$script"; } | iconv -f UTF-8 -t UTF-16LE | base64 -w0 )

for h in "${hosts[@]}"; do
  echo "=== $h"
  rc=0
  # MSYS_NO_PATHCONV: Git-Bash would otherwise rewrite /X switches in the argv.
  out=$(MSYS_NO_PATHCONV=1 roomler exec "$h" -- powershell -NoProfile -NonInteractive \
    -EncodedCommand "$enc" 2>&1) || rc=$?
  # PowerShell appends a CLIXML progress record to stderr; it is noise.
  printf '%s\n' "$out" | grep -v '^#< CLIXML' | grep -v '^<Objs ' || true
  [ "$rc" = 0 ] || echo "(roomler exec to $h exited $rc)"
done
