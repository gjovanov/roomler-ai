#!/usr/bin/env bash
#
# fr65-stall-census.sh — run FR-65's stall census where the data lives.
#
# usage: scripts/fr65-stall-census.sh --mars --yes [options]
#
# The census reads `agent_logs` in production Mongo, a fleet-wide server read:
# nothing runs on a device. Its mongosh runs OUTSIDE the Mongo container, through
# prod-mongo-read.sh (a port-forward and a throwaway client container), so the
# client's memory never counts against the database's limit. The read is still
# bounded by construction; see `runMongosh` in fr65-stall-census.cjs. An unbounded
# read whose client ran inside that container OOM-killed production Mongo once
# (2026-10-26).
#
# It is the "same instrument" FR-65 AC7 asks for: the before of a change to the
# watch is the build without it, the after is the build with it, over windows of
# the same length. `agent_logs` keeps 7 days, so the report splits by
# `agent_version` and one window can hold both.
#
# TARGETS (pick one)
#
#   --kubectl --yes              where kubectl reaches namespace roomler-ai and
#                                docker runs (on mars, as gjovanov).
#   --mars --yes                 from the dev box: ships the census and
#                                prod-mongo-read.sh to mars over one `roomler ssh`
#                                and runs --kubectl there.
#
# OPTIONS
#
#   --since ISO --until ISO      the window (default: the last 7 days; at most 8)
#   --agent HEX                  one device; repeat for more (default: every device)
#   --tenant HEX                 one org only (default: every org)
#   --rows                       also print one JSON line per STALL, classified
#   --probes                     the caps-probe census instead (FR-65 AC6)
#   --rekeys                     forced rekeys and route waves instead (FR-65 AC5)
#   --swaps                      what a capture-size change costs the pump (FR-65 AC3)
#   --canary                     the runtime canary beside the STALLs (FR-65 AC4)
#
# A production read is refused without --yes: it is a bounded read of the live
# database. The mongodb pod's restart count and memory are printed before and
# after it, and a restart fails the run (exit 3).

set -euo pipefail

# Git-Bash rewrites any argument that starts with `/` into a Windows path. No-ops
# off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

HERE="$(cd "$(dirname "$0")" && pwd)"
JS="$HERE/fr65-stall-census.cjs"
# The client: a port-forward and a throwaway mongo:7 container, never mongodb-0.
READ="$HERE/prod-mongo-read.sh"

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
    --rekeys|--swaps|--canary)
               ENVS+=("FR65_MODE=${1#--}"); PASS+=("$1"); shift ;;
    -h|--help) sed -n '2,41p' "$0"; exit 0 ;;
    *)         die "unknown argument: $1 (--help)" ;;
  esac
done
[ -z "$AGENTS" ] || ENVS+=("FR65_AGENTS=$AGENTS")

[ -n "$TARGET" ] || die "pick a target: --kubectl --yes or --mars --yes (--help)"
[ -f "$JS" ] || die "missing $JS"
[ -f "$READ" ] || die "missing $READ"
[ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"

case "$TARGET" in
  kubectl)
    # The runner prints the mongodb pod's restart count and memory before and
    # after, and exits 3 when the pod restarted: treat that run as harmful, not
    # as data.
    EOPTS=()
    for e in "${ENVS[@]+"${ENVS[@]}"}"; do EOPTS+=(--env "$e"); done
    exec bash "$READ" --kubectl --yes "${EOPTS[@]+"${EOPTS[@]}"}" "$JS"
    ;;

  mars)
    command -v roomler >/dev/null || die "roomler is not on PATH"
    # One `roomler ssh`, carrying the files on stdin: a device has a ceiling on
    # SSH requests per caller, and four in a row meet it.
    rc=0
    tar -C "$HERE" -cf - fr65-stall-census.cjs fr65-stall-census.sh prod-mongo-read.sh |
      roomler ssh mars -- "D=\$(mktemp -d ~/fr65-census.XXXXXX) && tar -xf - -C \"\$D\" && { bash \"\$D/fr65-stall-census.sh\" --kubectl --yes $(printf '%q ' "${PASS[@]+"${PASS[@]}"}"); rc=\$?; rm -rf \"\$D\"; exit \$rc; }" || rc=$?
    exit "$rc"
    ;;
esac
