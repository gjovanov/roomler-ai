#!/usr/bin/env bash
#
# rc-shadow-report.sh — run FR-63's fleet shadow read where the data lives.
#
# WHY THIS EXISTS
#
#   FR-63's AC2 is judged on one release of `shadow_*` heartbeats across the
#   FLEET (docs/fr/FR-63-one-delay-based-rate-controller.md). Those heartbeats
#   live in `agent_logs` in production Mongo: every daemon and every RC worker
#   uploads its INFO lines there, for every org, and the collection keeps 7
#   days. Nothing else holds the whole population. The per-agent API
#   (`GET …/agent/{id}/logs`) takes only `limit` (<= 500 recent batches), and an
#   exec sweep of host logs reaches only exec-enabled, online hosts of one org,
#   which is a biased sample of exactly the proportion AC2 asks for.
#
#   So the read runs INSIDE the Mongo container, as mongosh, with the
#   container's own credentials. It is bounded by construction; see
#   `runMongosh` in rc-shadow-report.cjs. An unbounded read inside that same
#   container OOM-killed production Mongo once (2026-10-26).
#
# TARGETS (pick one)
#
#   --docker NAME [--seed FILE]  a throwaway local mongo:7 container: the
#                                rehearsal. --seed loads `rc-shadow-report.cjs
#                                --to-ejson` output (real log lines, stored the
#                                way the uploader stores them) before the read.
#   --kubectl --yes              where kubectl reaches namespace roomler-ai
#                                (on mars, as gjovanov).
#   --mars --yes                 from the dev box: ships both files to mars over
#                                `roomler ssh` and runs --kubectl there.
#
# OPTIONS
#
#   --since ISO --until ISO      the window (default: the last 7 days; at most 8)
#   --tenant HEX                 one org only (default: every org)
#   --min-windows N              windows a session needs to count toward a class
#   --json                       the machine-readable result instead of Markdown
#   --rows                       print the projected rows (NDJSON) instead of a
#                                report; `rc-shadow-report.cjs FILE` re-reads them
#
# A production read (--kubectl / --mars) is refused without --yes: it is a
# bounded read of the live database, and still the operator's to start. The
# mongodb pod's restart count is printed before and after it.

set -euo pipefail

# Git-Bash rewrites any argument that starts with `/` into a Windows path, so a
# container path like /tmp/rc-shadow-report.js reaches docker as C:/Users/…
# (seen on the first rehearsal). Both switches are no-ops off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

HERE="$(cd "$(dirname "$0")" && pwd)"
JS="$HERE/rc-shadow-report.cjs"
# Inside a container the copy is named .js on purpose: mongosh runs a positional
# argument as a script only when it ends in .js (anything else is a connection
# string).
NS="roomler-ai"
POD="mongodb-0"

die() { echo "rc-shadow-report: $*" >&2; exit 1; }

# A HOST path handed to a Windows program (docker.exe) must be converted by
# hand once the automatic rewrite above is off: /c/dev/… would reach it as
# C:\c\dev. Container paths stay literal. Off Windows this is the identity.
hostpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }

TARGET=""
CONTAINER=""
SEED=""
YES=0
PASS=()      # options handed on unchanged to the --kubectl run on mars
ENVS=()      # RC_SHADOW_* for the mongosh run

while [ $# -gt 0 ]; do
  case "$1" in
    --docker)      TARGET=docker; CONTAINER="${2:?--docker needs a container name}"; shift 2 ;;
    --kubectl)     TARGET=kubectl; shift ;;
    --mars)        TARGET=mars; shift ;;
    --seed)        SEED="${2:?--seed needs a file}"; shift 2 ;;
    --yes)         YES=1; shift ;;
    --since)       ENVS+=("RC_SHADOW_SINCE=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --until)       ENVS+=("RC_SHADOW_UNTIL=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --tenant)      ENVS+=("RC_SHADOW_TENANT=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --min-windows) ENVS+=("RC_SHADOW_MIN_WINDOWS=${2:?}"); PASS+=("$1" "$2"); shift 2 ;;
    --json)        ENVS+=("RC_SHADOW_JSON=1"); PASS+=("$1"); shift ;;
    --rows)        ENVS+=("RC_SHADOW_EMIT=rows"); PASS+=("$1"); shift ;;
    -h|--help)     sed -n '2,45p' "$0"; exit 0 ;;
    *)             die "unknown argument: $1 (--help)" ;;
  esac
done

[ -n "$TARGET" ] || die "pick a target: --docker NAME, --kubectl --yes or --mars --yes (--help)"
[ -f "$JS" ] || die "missing $JS"
[ -z "$SEED" ] || [ "$TARGET" = docker ] || die "--seed is for the --docker rehearsal only"

# Prefix every ENVS entry with -e for `docker exec`.
docker_env_args() { local a=(); for e in "${ENVS[@]+"${ENVS[@]}"}"; do a+=(-e "$e"); done; printf '%s\0' "${a[@]+"${a[@]}"}"; }

case "$TARGET" in
  docker)
    command -v docker >/dev/null || die "docker is not on PATH"
    docker cp "$(hostpath "$JS")" "$CONTAINER:/tmp/rc-shadow-report.js" >/dev/null
    if [ -n "$SEED" ]; then
      [ -f "$SEED" ] || die "missing seed $SEED"
      docker cp "$(hostpath "$SEED")" "$CONTAINER:/tmp/rc-shadow-seed.ejson" >/dev/null
      # The rehearsal mirrors production's agent_logs indexes, minus the TTL:
      # a seed of real lines older than 7 days would be deleted under the read.
      docker exec "$CONTAINER" mongosh --nodb --quiet --eval '
        const u = process.env.MONGO_INITDB_ROOT_USERNAME, p = process.env.MONGO_INITDB_ROOT_PASSWORD;
        const uri = u ? "mongodb://" + encodeURIComponent(u) + ":" + encodeURIComponent(p) + "@127.0.0.1:27017/?authSource=admin" : "mongodb://127.0.0.1:27017";
        const d = new Mongo(uri).getDB(process.env.RC_SHADOW_DB || "roomler2");
        d.agent_logs.createIndex({ tenant_id: 1, agent_id: 1, created_at: -1 });
        d.agent_logs.createIndex({ tenant_id: 1, source: 1, created_at: -1 });
        d.agent_logs.createIndex({ tenant_id: 1, session_id: 1 });
        d.agent_logs.createIndex({ created_at: 1 });
        const docs = require("fs").readFileSync("/tmp/rc-shadow-seed.ejson", "utf8").split("\n").filter(Boolean).map((l) => EJSON.parse(l));
        if (docs.length) d.agent_logs.insertMany(docs);
        print("seeded " + docs.length + " agent_logs batches");'
      docker exec "$CONTAINER" rm -f /tmp/rc-shadow-seed.ejson
    fi
    mapfile -d '' EARGS < <(docker_env_args)
    rc=0
    docker exec "${EARGS[@]+"${EARGS[@]}"}" "$CONTAINER" mongosh --nodb --quiet /tmp/rc-shadow-report.js || rc=$?
    docker exec "$CONTAINER" rm -f /tmp/rc-shadow-report.js || true
    exit "$rc"
    ;;

  kubectl)
    [ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"
    command -v kubectl >/dev/null || die "kubectl is not on PATH"
    restarts() { kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].restartCount}'; }
    before="$(restarts)"
    echo "mongodb pod $POD: restarts=$before before the read"
    kubectl -n "$NS" cp "$JS" "$POD:/tmp/rc-shadow-report.js" >/dev/null
    rc=0
    kubectl -n "$NS" exec "$POD" -- env "${ENVS[@]+"${ENVS[@]}"}" mongosh --nodb --quiet /tmp/rc-shadow-report.js || rc=$?
    kubectl -n "$NS" exec "$POD" -- rm -f /tmp/rc-shadow-report.js || true
    after="$(restarts)"
    echo "mongodb pod $POD: restarts=$after after the read"
    if [ "$after" != "$before" ]; then
      echo "rc-shadow-report: ⚠️ the mongodb pod RESTARTED during the read — treat this run as harmful, not as data:" >&2
      kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].lastState}' >&2 || true
      echo >&2
      exit 3
    fi
    exit "$rc"
    ;;

  mars)
    [ "$YES" = 1 ] || die "this reads PRODUCTION agent_logs; re-run with --yes to start it"
    command -v roomler >/dev/null || die "roomler is not on PATH"
    roomler ssh mars -- 'mkdir -p ~/fr63-shadow && cat > ~/fr63-shadow/rc-shadow-report.cjs' < "$JS"
    roomler ssh mars -- 'cat > ~/fr63-shadow/rc-shadow-report.sh' < "$HERE/rc-shadow-report.sh"
    rc=0
    roomler ssh mars -- "bash ~/fr63-shadow/rc-shadow-report.sh --kubectl --yes $(printf '%q ' "${PASS[@]+"${PASS[@]}"}")" || rc=$?
    roomler ssh mars -- 'rm -rf ~/fr63-shadow' || true
    exit "$rc"
    ;;
esac
