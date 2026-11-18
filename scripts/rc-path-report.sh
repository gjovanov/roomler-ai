#!/usr/bin/env bash
#
# rc-path-report.sh — run FR-148's path report where the data lives.
#
# WHY THIS EXISTS
#
#   FR-148 P0 asks, per remote-desktop session, what it opened at beside what
#   its pair had already carried, and whether the encoder followed the plan
#   (docs/fr/FR-148-path-measured-plan-follows.md). The answer is in the agents'
#   own log lines, and those live in `agent_logs` in production Mongo: every
#   daemon and every RC worker uploads its INFO lines there, for every org, and
#   the collection keeps 7 days. The per-agent API returns at most 500 recent
#   batches, and an exec sweep reaches only exec-enabled, online hosts of one
#   org.
#
#   So the read runs as mongosh against production Mongo, OUTSIDE the Mongo
#   container, through prod-mongo-read.sh: a port-forward and a throwaway
#   client container, so the client's memory never counts against the
#   database's limit. It is bounded by construction; see "BOUNDED BY
#   CONSTRUCTION" in rc-path-report.cjs. An unbounded read whose client ran
#   inside the Mongo container OOM-killed production Mongo once (2026-10-26).
#
# TARGETS (pick one)
#
#   --docker NAME [--seed FILE]  a throwaway local mongo:7 container: the
#                                rehearsal. --seed loads `rc-path-report.cjs
#                                --fixtures` or `--to-ejson` output (log lines
#                                stored the way the uploader stores them).
#   --kubectl --yes              where kubectl reaches namespace roomler-ai and
#                                docker runs (on mars, as gjovanov).
#   --mars --yes                 from the dev box: ships the report and
#                                prod-mongo-read.sh to mars over one `roomler ssh`
#                                and runs --kubectl there.
#
# OPTIONS
#
#   --since ISO --until ISO      the window (default: the last 7 days; at most 8)
#   --tenant HEX                 one org only (default: every org)
#   --agent HEX                  one agent only; repeat for more (default: all)
#   --label HEX=NAME             print an agent whose id ends with HEX as NAME;
#                                repeat for more. Names are never read from the
#                                database, so a device has a name only here.
#   --json                       the machine-readable result instead of Markdown
#   --rows                       print the projected rows (NDJSON) instead of a
#                                report; `rc-path-report.cjs FILE` re-reads them
#   --explain                    print the plan of one day's query (the index it
#                                uses, and whether a blocking SORT stage appears)
#                                instead of reading
#   --timeline                   ONE device's window line by line, every session
#                                in time order (rc-path-timeline.cjs). Needs
#                                --tenant, ONE --agent and --from/--to, at most
#                                24 h apart (split a longer day). --rows and
#                                --explain work as above; --json does not.
#   --from ISO --to ISO          the same as --since and --until
#   --limit N                    --timeline only: stop after N rows (default
#                                60,000) and print TRUNCATED
#
# A production read (--kubectl / --mars) is refused without --yes: it is a
# bounded read of the live database, and still the operator's to start. The
# mongodb pod's restart count and memory are printed before and after it, on
# stderr, and a restart fails the run (exit 3).

set -euo pipefail

# Git-Bash rewrites any argument that starts with `/` into a Windows path, so a
# container path like /tmp/rc-path-report.js would reach docker as C:/Users/….
# Both switches are no-ops off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

HERE="$(cd "$(dirname "$0")" && pwd)"
JS="$HERE/rc-path-report.cjs"
# Inside a container the copy is named .js on purpose: mongosh runs a positional
# argument as a script only when it ends in .js (anything else is a connection
# string).
# The production client: a port-forward and a throwaway mongo:7 container, never
# mongodb-0.
READ="$HERE/prod-mongo-read.sh"

die() { echo "rc-path-report: $*" >&2; exit 1; }

# A HOST path handed to a Windows program (docker.exe) must be converted by
# hand once the automatic rewrite above is off: /c/dev/… would reach it as
# C:\c\dev. Container paths stay literal. Off Windows this is the identity.
hostpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }

HEX24='^[0-9a-fA-F]{24}$'
LABEL='^[0-9a-fA-F]{1,24}=[A-Za-z0-9._-]{1,32}$'

TARGET=""
CONTAINER=""
SEED=""
YES=0
AGENTS=""
LABELS=""
PASS=()      # options handed on unchanged to the --kubectl run on mars
ENVS=()      # RC_PATH_* for the mongosh run
TIMELINE=0
NTENANT=0
NAGENTS=0
NWINDOW=0
JSON=0

while [ $# -gt 0 ]; do
  case "$1" in
    --docker)  TARGET=docker; CONTAINER="${2:?--docker needs a container name}"; shift 2 ;;
    --kubectl) TARGET=kubectl; shift ;;
    --mars)    TARGET=mars; shift ;;
    --seed)    SEED="${2:?--seed needs a file}"; shift 2 ;;
    --yes)     YES=1; shift ;;
    --since|--from) ENVS+=("RC_PATH_SINCE=${2:?}"); PASS+=("$1" "$2"); NWINDOW=$((NWINDOW + 1)); shift 2 ;;
    --until|--to)   ENVS+=("RC_PATH_UNTIL=${2:?}"); PASS+=("$1" "$2"); NWINDOW=$((NWINDOW + 1)); shift 2 ;;
    --tenant)
      [[ "${2:-}" =~ $HEX24 ]] || die "--tenant takes a 24-hex tenant id"
      ENVS+=("RC_PATH_TENANT=$2"); PASS+=("$1" "$2"); NTENANT=$((NTENANT + 1)); shift 2 ;;
    --agent)
      [[ "${2:-}" =~ $HEX24 ]] || die "--agent takes a 24-hex agent id"
      AGENTS="${AGENTS:+$AGENTS,}$2"; PASS+=("$1" "$2"); NAGENTS=$((NAGENTS + 1)); shift 2 ;;
    --label)
      [[ "${2:-}" =~ $LABEL ]] || die "--label takes HEX=NAME (HEX up to 24 hex digits; NAME letters, digits, '.', '_' or '-')"
      LABELS="${LABELS:+$LABELS,}$2"; PASS+=("$1" "$2"); shift 2 ;;
    --json)    ENVS+=("RC_PATH_JSON=1"); PASS+=("$1"); JSON=1; shift ;;
    --rows)    ENVS+=("RC_PATH_EMIT=rows"); PASS+=("$1"); shift ;;
    --explain) ENVS+=("RC_PATH_EXPLAIN=1"); PASS+=("$1"); shift ;;
    --timeline) TIMELINE=1; PASS+=("$1"); shift ;;
    --limit)
      [[ "${2:-}" =~ ^[1-9][0-9]{0,5}$ ]] || die "--limit takes a row count (1 to 200000)"
      ENVS+=("RC_PATH_LIMIT=$2"); PASS+=("$1" "$2"); shift 2 ;;
    -h|--help) sed -n '2,61p' "$0"; exit 0 ;;
    *)         die "unknown argument: $1 (--help)" ;;
  esac
done
[ -z "$AGENTS" ] || ENVS+=("RC_PATH_AGENTS=$AGENTS")
[ -z "$LABELS" ] || ENVS+=("RC_PATH_LABELS=$LABELS")
if [ "$TIMELINE" = 1 ]; then
  # One device, one window: the read rc-path-timeline.cjs is bounded for.
  JS="$HERE/rc-path-timeline.cjs"
  [ "$NTENANT" = 1 ] && [ "$NAGENTS" = 1 ] || die "--timeline reads one device: --tenant HEX and ONE --agent HEX"
  [ "$NWINDOW" = 2 ] || die "--timeline needs the window: --from ISO --to ISO"
  [ "$JSON" = 0 ] || die "--json is not available with --timeline"
fi

[ -n "$TARGET" ] || die "pick a target: --docker NAME, --kubectl --yes or --mars --yes (--help)"
[ -f "$JS" ] || die "missing $JS"
[ -z "$SEED" ] || [ "$TARGET" = docker ] || die "--seed is for the --docker rehearsal only"

# Every ENVS entry as `-e NAME=VALUE` for `docker exec`. A plain array, so no
# option at all hands docker no argument at all (a NUL-joined list of nothing
# would still be one empty argument, read as the container's name).
EARGS=()
for e in "${ENVS[@]+"${ENVS[@]}"}"; do EARGS+=(-e "$e"); done

case "$TARGET" in
  docker)
    command -v docker >/dev/null || die "docker is not on PATH"
    docker cp "$(hostpath "$JS")" "$CONTAINER:/tmp/rc-path-report.js" >/dev/null
    if [ -n "$SEED" ]; then
      [ -f "$SEED" ] || die "missing seed $SEED"
      docker cp "$(hostpath "$SEED")" "$CONTAINER:/tmp/rc-path-seed.ejson" >/dev/null
      # The rehearsal mirrors production's agent_logs indexes, minus the TTL:
      # a seed of lines older than 7 days would be deleted under the read.
      docker exec "$CONTAINER" mongosh --nodb --quiet --eval '
        const u = process.env.MONGO_INITDB_ROOT_USERNAME, p = process.env.MONGO_INITDB_ROOT_PASSWORD;
        const uri = u ? "mongodb://" + encodeURIComponent(u) + ":" + encodeURIComponent(p) + "@127.0.0.1:27017/?authSource=admin" : "mongodb://127.0.0.1:27017";
        const d = new Mongo(uri).getDB("roomler2");
        d.agent_logs.createIndex({ tenant_id: 1, agent_id: 1, created_at: -1 });
        d.agent_logs.createIndex({ tenant_id: 1, source: 1, created_at: -1 });
        d.agent_logs.createIndex({ tenant_id: 1, session_id: 1 });
        d.agent_logs.createIndex({ created_at: 1 });
        const docs = require("fs").readFileSync("/tmp/rc-path-seed.ejson", "utf8").split("\n").filter(Boolean).map((l) => EJSON.parse(l));
        if (docs.length) d.agent_logs.insertMany(docs);
        print("seeded " + docs.length + " agent_logs batches");'
      docker exec "$CONTAINER" rm -f /tmp/rc-path-seed.ejson
    fi
    rc=0
    docker exec "${EARGS[@]+"${EARGS[@]}"}" "$CONTAINER" mongosh --nodb --quiet /tmp/rc-path-report.js || rc=$?
    docker exec "$CONTAINER" rm -f /tmp/rc-path-report.js || true
    exit "$rc"
    ;;

  kubectl)
    [ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"
    [ -f "$READ" ] || die "missing $READ"
    # The runner prints the mongodb pod's restart count and memory before and
    # after, and exits 3 when the pod restarted: treat that run as harmful, not
    # as data.
    EOPTS=()
    for e in "${ENVS[@]+"${ENVS[@]}"}"; do EOPTS+=(--env "$e"); done
    exec bash "$READ" --kubectl --yes "${EOPTS[@]+"${EOPTS[@]}"}" "$JS"
    ;;

  mars)
    [ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"
    [ -f "$READ" ] || die "missing $READ"
    command -v roomler >/dev/null || die "roomler is not on PATH"
    # One `roomler ssh`, carrying the files on stdin: a device has a ceiling on
    # SSH requests per caller, and four in a row meet it.
    OPTS=""
    [ "${#PASS[@]}" = 0 ] || OPTS="$(printf '%q ' "${PASS[@]}")"
    rc=0
    tar -C "$HERE" -cf - rc-path-report.cjs rc-path-timeline.cjs rc-path-report.sh prod-mongo-read.sh |
      roomler ssh mars -- "D=\$(mktemp -d ~/fr148-path.XXXXXX) && tar -xf - -C \"\$D\" && { bash \"\$D/rc-path-report.sh\" --kubectl --yes $OPTS; rc=\$?; rm -rf \"\$D\"; exit \$rc; }" || rc=$?
    exit "$rc"
    ;;
esac
