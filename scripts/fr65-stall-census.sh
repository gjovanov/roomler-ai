#!/usr/bin/env bash
#
# fr65-stall-census.sh — run FR-65's stall census where the data lives.
#
# usage: scripts/fr65-stall-census.sh --mars --yes [options]
#
# The census reads `agent_logs` in production Mongo, a fleet-wide server read:
# nothing runs on a device. It runs INSIDE the Mongo container, as mongosh, with
# the container's own credentials. It is bounded by construction; see `runMongosh`
# in fr65-stall-census.cjs. An unbounded read inside that same container
# OOM-killed production Mongo once (2026-10-26).
#
# It is the "same instrument" FR-65 AC7 asks for: the before of a change to the
# watch is the build without it, the after is the build with it, over windows of
# the same length. `agent_logs` keeps 7 days, so the report splits by
# `agent_version` and one window can hold both.
#
# TARGETS (pick one)
#
#   --kubectl --yes              where kubectl reaches namespace roomler-ai
#                                (on mars, as gjovanov).
#   --mars --yes                 from the dev box: ships both files to mars over
#                                `roomler ssh` and runs --kubectl there.
#
# OPTIONS
#
#   --since ISO --until ISO      the window (default: the last 7 days; at most 8)
#   --agent HEX                  one device; repeat for more (default: every device)
#   --tenant HEX                 one org only (default: every org)
#   --rows                       also print one JSON line per STALL, classified
#   --probes                     the caps-probe census instead (FR-65 AC6)
#
# A production read is refused without --yes: it is a bounded read of the live
# database. The mongodb pod's restart count is printed before and after it.

set -euo pipefail

# Git-Bash rewrites any argument that starts with `/` into a Windows path. No-ops
# off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

HERE="$(cd "$(dirname "$0")" && pwd)"
JS="$HERE/fr65-stall-census.cjs"
# Inside the pod the copy is named .js on purpose: mongosh runs a positional
# argument as a script only when it ends in .js.
NS="roomler-ai"
POD="mongodb-0"

die() { echo "fr65-stall-census: $*" >&2; exit 1; }

TARGET=""
YES=0
AGENTS=""
PASS=()      # options handed on unchanged to the --kubectl run on mars
ENVS=()      # FR65_* for the mongosh run

while [ $# -gt 0 ]; do
  case "$1" in
    --kubectl) TARGET=kubectl; shift ;;
    --mars)    TARGET=mars; shift ;;
    --yes)     YES=1; shift ;;
    --since)   ENVS+=("FR65_SINCE=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --until)   ENVS+=("FR65_UNTIL=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --tenant)  ENVS+=("FR65_TENANT=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --agent)   AGENTS="${AGENTS:+$AGENTS,}${2:?}"; PASS+=("$1" "$2"); shift 2 ;;
    --rows)    ENVS+=("FR65_ROWS=1"); PASS+=("$1"); shift ;;
    --probes)  ENVS+=("FR65_PROBES=1"); PASS+=("$1"); shift ;;
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    *)         die "unknown argument: $1 (--help)" ;;
  esac
done
[ -z "$AGENTS" ] || ENVS+=("FR65_AGENTS=$AGENTS")

[ -n "$TARGET" ] || die "pick a target: --kubectl --yes or --mars --yes (--help)"
[ -f "$JS" ] || die "missing $JS"
[ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"

case "$TARGET" in
  kubectl)
    command -v kubectl >/dev/null || die "kubectl is not on PATH"
    restarts() { kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].restartCount}'; }
    before="$(restarts)"
    echo "mongodb pod $POD: restarts=$before before the read"
    kubectl -n "$NS" cp "$JS" "$POD:/tmp/fr65-stall-census.js" >/dev/null
    rc=0
    kubectl -n "$NS" exec "$POD" -- env "${ENVS[@]+"${ENVS[@]}"}" mongosh --nodb --quiet /tmp/fr65-stall-census.js || rc=$?
    kubectl -n "$NS" exec "$POD" -- rm -f /tmp/fr65-stall-census.js || true
    after="$(restarts)"
    echo "mongodb pod $POD: restarts=$after after the read"
    if [ "$after" != "$before" ]; then
      echo "fr65-stall-census: ⚠️ the mongodb pod RESTARTED during the read — treat this run as harmful, not as data:" >&2
      kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].lastState}' >&2 || true
      echo >&2
      exit 3
    fi
    exit "$rc"
    ;;

  mars)
    command -v roomler >/dev/null || die "roomler is not on PATH"
    # One `roomler ssh`, carrying both files on stdin: a device has a ceiling on
    # SSH requests per caller, and four in a row meet it.
    rc=0
    tar -C "$HERE" -cf - fr65-stall-census.cjs fr65-stall-census.sh |
      roomler ssh mars -- "D=\$(mktemp -d ~/fr65-census.XXXXXX) && tar -xf - -C \"\$D\" && { bash \"\$D/fr65-stall-census.sh\" --kubectl --yes $(printf '%q ' "${PASS[@]+"${PASS[@]}"}"); rc=\$?; rm -rf \"\$D\"; exit \$rc; }" || rc=$?
    exit "$rc"
    ;;
esac
