#!/usr/bin/env bash
#
# prod-mongo-read.sh — run a read of production Mongo with its client OUTSIDE
# the database container.
#
# usage: scripts/prod-mongo-read.sh --mars --yes [options] SCRIPT.js|-
#
# WHY THIS EXISTS
#
#   `kubectl exec -i mongodb-0 -- mongosh` starts the client inside the database
#   container, so the client's own memory counts against mongod's limit (2 GiB).
#   An unbounded read run that way OOM-killed production Mongo for about 80 s
#   (2026-10-26). Two `$text` phrase counts run that way took the container to
#   within 2.3 MB of its limit (2026-11-10, #3272). This runner keeps the client
#   off the database's cgroup and off its node: a `kubectl port-forward` to
#   mongodb-0, bound to 127.0.0.1:27017 on the host that runs it, and a throwaway
#   mongo:7 container on that host's network with a memory limit of its own.
#   docs/production-mongo-reads.md has the whole method.
#
# WHAT A SCRIPT SEES
#
#   What it saw inside mongodb-0, so a read written for that runs unchanged:
#   - MONGO_INITDB_ROOT_USERNAME, MONGO_INITDB_ROOT_PASSWORD and
#     MONGO_INITDB_DATABASE in process.env, from the secret mongodb-0 reads;
#   - the server at 127.0.0.1:27017 (here, the forward);
#   - and `db`, already open on the app database (roomler2, or --db NAME).
#   mongosh runs the script as a FILE, never on stdin, so the REPL cannot run
#   half a statement. SCRIPT `-` reads the script from stdin.
#
# TARGETS (pick one)
#
#   --mars --yes      from the dev box: ships this file and the script to mars
#                     over one `roomler ssh` and runs --kubectl there. The CLI is
#                     $ROOMLER_CLI, else /c/Program Files/Roomler/roomler.exe,
#                     else `roomler` on PATH.
#   --kubectl --yes   on a Linux host where kubectl reaches namespace roomler-ai
#                     and docker runs (mars, as gjovanov).
#   --print-context   print the kube context every kubectl call here pins, or
#                     refuse; the other reads in scripts/ pin theirs from it.
#
# THE CLUSTER IS NAMED, NEVER INHERITED
#
#   Every kubectl call carries --context: $ROOMLER_KUBE_CONTEXT when set, else
#   kubernetes-admin@kubernetes, the production cluster's context on mars. A
#   host whose kubeconfig has no such context is refused before any cluster
#   call, so a run on another machine can never reach whatever cluster that
#   machine has current (2026-11-10: a test run's kubectl on a dev box reached
#   for an unrelated cluster's credentials).
#
# OPTIONS
#
#   --env NAME=VALUE  a setting for the script's process.env; repeat for more.
#                     Never a secret: it is an argument of docker on the host.
#   --db NAME         the database `db` opens (default roomler2)
#
# GUARDS
#
#   Refused without --yes. Before and after the read it prints, on stderr,
#   mongodb-0's restart count and its container's memory.current, memory.max
#   and memory.events (one `cat` in mongodb-0; `kubectl top` missed a spike to
#   99.9 % on 2026-11-10, the cgroup's own files do not). A restart or a new
#   oom_kill fails the run with exit 3: treat its output as harmful, not as
#   data. The run is refused when anything already listens on port 27017. When
#   it ends, however it ends, it stops the forward, removes the container and
#   the credential files, and prints the proof; an incomplete teardown exits 5.
#
#   The credentials never reach argv, a log or the output. kubectl decodes them
#   into mode-600 files in a mode-700 directory, docker mounts that directory
#   read-only, and the container's shell reads them into its own environment,
#   so `docker inspect` does not show them either.
#
#   This moves the CLIENT out. The SERVER still runs the query inside mongodb-0,
#   and its page cache still counts there, so a read must still be bounded: an
#   indexed $match, a $limit, maxTimeMS, a server-side $group, and
#   estimatedDocumentCount() before any collection you have not sized.
#
# EXIT  0 or the script's own code · 2 refused · 3 mongodb-0 restarted or
#       OOM-killed · 5 teardown incomplete
#
# examples
#   scripts/prod-mongo-read.sh --mars --yes scripts/rtp-reach-read.cjs | grep -o 'OUT .*'
#   scripts/prod-mongo-read.sh --mars --yes - <<'JS'
#   print('agent_logs ~' + db.agent_logs.estimatedDocumentCount());
#   JS

set -euo pipefail

# Git-Bash rewrites any argument that starts with `/` into a Windows path. No-ops
# off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

NS="${ROOMLER_NS:-roomler-ai}"
POD="mongodb-0"
SECRET="mongodb-secret"
# Every read in scripts/ connects to 127.0.0.1:27017, as it did inside the pod.
PORT=27017
# Production's own image, so the client is the mongosh the pod carried.
IMAGE="${MONGO_CLIENT_IMAGE:-mongo:7}"
CLIENT_MEMORY="1g"
CRED_KEYS="MONGO_INITDB_ROOT_USERNAME MONGO_INITDB_ROOT_PASSWORD MONGO_INITDB_DATABASE"

HERE="$(cd "$(dirname "$0")" && pwd)"
SELF="$HERE/$(basename "$0")"

die() { echo "prod-mongo-read: $*" >&2; exit 2; }
say() { echo "prod-mongo-read: $*" >&2; }

# The context every kubectl call pins. `config get-contexts` reads the local
# kubeconfig only: nothing reaches a cluster before the name is known to exist.
KUBE_CONTEXT="${ROOMLER_KUBE_CONTEXT:-kubernetes-admin@kubernetes}"
KUBECTL_BIN=""
kube_context_ok() {
  KUBECTL_BIN="$(type -P kubectl)" || die "kubectl is not on PATH"
  "$KUBECTL_BIN" config get-contexts -o name 2>/dev/null | grep -qxF -- "$KUBE_CONTEXT" ||
    die "no kube context '$KUBE_CONTEXT' here: run it on mars (--mars from the dev box), or set ROOMLER_KUBE_CONTEXT to the production cluster's context"
}
# Every kubectl below goes through this, so none can use the current context.
kubectl() {
  [ -n "$KUBECTL_BIN" ] || die "internal: kubectl before the context check"
  "$KUBECTL_BIN" --context "$KUBE_CONTEXT" "$@"
}

if [ "${1:-}" = --print-context ]; then
  kube_context_ok
  printf '%s\n' "$KUBE_CONTEXT"
  exit 0
fi

TARGET=""
YES=0
DB="roomler2"
SCRIPT=""
ENVS=()   # NAME=VALUE for the script's process.env
PASS=()   # options handed on unchanged to the --kubectl run on mars

while [ $# -gt 0 ]; do
  case "$1" in
    --kubectl) TARGET=kubectl; shift ;;
    --mars)    TARGET=mars; shift ;;
    --yes)     YES=1; shift ;;
    --env)
      [[ "${2:-}" =~ ^[A-Z][A-Z0-9_]*= ]] || die "--env takes NAME=VALUE, NAME in upper case"
      case "$2" in MONGO_*|PMR_*) die "--env cannot set ${2%%=*}" ;; esac
      ENVS+=("$2"); PASS+=("$1" "$2"); shift 2 ;;
    --db)
      [[ "${2:-}" =~ ^[A-Za-z0-9_-]+$ ]] || die "--db takes a database name"
      DB="$2"; PASS+=("$1" "$2"); shift 2 ;;
    -h|--help) sed -n '2,84p' "$0"; exit 0 ;;
    -)         [ -z "$SCRIPT" ] || die "one script only"; SCRIPT="-"; shift ;;
    -*)        die "unknown option: $1 (--help)" ;;
    *)         [ -z "$SCRIPT" ] || die "one script only"; SCRIPT="$1"; shift ;;
  esac
done

[ -n "$TARGET" ] || die "pick a target: --mars --yes or --kubectl --yes (--help)"
[ "$YES" = 1 ] || die "this reads PRODUCTION Mongo; re-run with --yes to start it"
[ -n "$SCRIPT" ] || die "name the script to run, or - to read it from stdin"
[ "$SCRIPT" = - ] || [ -f "$SCRIPT" ] || die "no such script: $SCRIPT"

if [ "$TARGET" = mars ]; then
  CLI="${ROOMLER_CLI:-}"
  if [ -z "$CLI" ]; then
    if [ -x "/c/Program Files/Roomler/roomler.exe" ]; then CLI="/c/Program Files/Roomler/roomler.exe"
    else CLI="$(command -v roomler || true)"; fi
  fi
  [ -n "$CLI" ] || die "no roomler CLI: set ROOMLER_CLI"
  T="$(mktemp -d)"
  trap 'rm -rf "$T"' EXIT
  cp "$SELF" "$T/prod-mongo-read.sh"
  # mongosh runs a positional argument as a script only when it ends in .js.
  if [ "$SCRIPT" = - ]; then cat > "$T/script.js"; else cp "$SCRIPT" "$T/script.js"; fi
  # One `roomler ssh`, both files on stdin: a device caps SSH requests per caller.
  OPTS=""
  [ "${#PASS[@]}" = 0 ] || OPTS="$(printf '%q ' "${PASS[@]}")"
  rc=0
  tar -C "$T" -cf - prod-mongo-read.sh script.js |
    "$CLI" ssh mars -- "D=\$(mktemp -d ~/prod-mongo-read.XXXXXX) && tar -xf - -C \"\$D\" && { bash \"\$D/prod-mongo-read.sh\" --kubectl --yes $OPTS\"\$D/script.js\"; rc=\$?; rm -rf \"\$D\"; exit \$rc; }" || rc=$?
  exit "$rc"
fi

# --kubectl: on mars.
for c in docker ss base64; do command -v "$c" >/dev/null || die "$c is not on PATH"; done
kube_context_ok

listening() { ss -Htln "sport = :$PORT" 2>/dev/null | grep -q .; }
restarts() { kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo '?'; }
# memory.current, memory.max, then memory.events, as one block of text.
cgroup_read() {
  kubectl -n "$NS" exec "$POD" -- cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory.events 2>/dev/null || true
}
cgroup_line() {
  printf '%s\n' "$1" | awk '
    NR == 1 { c = $1 } NR == 2 { m = $1 }
    NR > 2 && $1 == "max" { x = $2 } NR > 2 && $1 == "oom" { o = $2 } NR > 2 && $1 == "oom_kill" { k = $2 }
    END {
      if (c !~ /^[0-9]+$/) { print "memory unreadable"; exit }
      p = (m ~ /^[0-9]+$/ && m > 0) ? sprintf(" (%.1f%% of memory.max %s)", 100 * c / m, m) : " (memory.max " m ")"
      printf "memory.current=%s%s, memory.events max=%s oom=%s oom_kill=%s\n", c, p, x, o, k
    }'
}
oom_kills() { printf '%s\n' "$1" | awk 'NR > 2 && $1 == "oom_kill" { print $2 }'; }

before_restarts="$(restarts)"
before_cgroup="$(cgroup_read)"
say "before: $POD restarts=$before_restarts, $(cgroup_line "$before_cgroup")"
! listening || die "something already listens on port $PORT here; the reads connect to 127.0.0.1:$PORT, so the forward needs it"

umask 077
D="$(mktemp -d -t prod-mongo-read.XXXXXX)"
CNAME="prod-mongo-read-$$-$RANDOM"
PF=""
TORN=0
TEARDOWN_OK=1

# Stops the forward, removes the container and the credential files, and proves
# each. Runs once: after the read, or from the EXIT trap when the run dies early.
teardown() {
  [ "$TORN" = 0 ] || return 0
  TORN=1
  docker rm -f "$CNAME" >/dev/null 2>&1 || true
  if [ -n "$PF" ]; then
    kill "$PF" 2>/dev/null || true
    wait "$PF" 2>/dev/null || true
  fi
  rm -rf "$D"
  local pf_gone=yes ctr_gone=yes files_gone=yes
  if [ -n "$PF" ] && kill -0 "$PF" 2>/dev/null; then pf_gone=no; fi
  if listening; then pf_gone=no; fi
  if [ -n "$(docker ps -aq --filter "name=^${CNAME}\$" 2>/dev/null)" ]; then ctr_gone=no; fi
  if [ -e "$D" ]; then files_gone=no; fi
  say "teardown: port-forward stopped and port $PORT free=$pf_gone, container $CNAME gone=$ctr_gone, credential files gone=$files_gone"
  if [ "$pf_gone$ctr_gone$files_gone" != yesyesyes ]; then
    TEARDOWN_OK=0
    say "⚠️ teardown incomplete: stop what is left by hand (kill ${PF:-?}; docker rm -f $CNAME; rm -rf $D)"
  fi
}
trap 'teardown; [ "$TEARDOWN_OK" = 1 ] || exit 5' EXIT
trap 'exit 130' INT TERM HUP

mkdir "$D/cred" "$D/js"
for k in $CRED_KEYS; do
  # Only the key's NAME is an argument; the value goes from the pipe to the file.
  kubectl -n "$NS" get secret "$SECRET" -o "go-template={{index .data \"$k\"}}" | base64 -d > "$D/cred/$k" 2>/dev/null ||
    die "could not read $k from secret $SECRET"
done
[ -s "$D/cred/MONGO_INITDB_ROOT_USERNAME" ] && [ -s "$D/cred/MONGO_INITDB_ROOT_PASSWORD" ] ||
  die "secret $SECRET holds no root credentials"

if [ "$SCRIPT" = - ]; then cat > "$D/js/script.js"; else cp "$SCRIPT" "$D/js/script.js"; fi
cat > "$D/js/preamble.js" <<'JS'
// prod-mongo-read.sh: what mongodb-0 gave its own shell, through the forward.
db = connect('mongodb://' + encodeURIComponent(process.env.MONGO_INITDB_ROOT_USERNAME) + ':'
  + encodeURIComponent(process.env.MONGO_INITDB_ROOT_PASSWORD) + '@127.0.0.1:27017/' + process.env.PMR_DB
  + '?authSource=admin&directConnection=true&appName=prod-mongo-read');
JS

# The binary itself, not the wrapper: $! must be the forward's own pid.
"$KUBECTL_BIN" --context "$KUBE_CONTEXT" -n "$NS" port-forward "pod/$POD" "$PORT:27017" --address 127.0.0.1 > "$D/forward.log" 2>&1 &
PF=$!
for _ in $(seq 1 60); do
  grep -q "^Forwarding from 127.0.0.1:$PORT " "$D/forward.log" && break
  kill -0 "$PF" 2>/dev/null || break
  sleep 0.25
done
if ! grep -q "^Forwarding from 127.0.0.1:$PORT " "$D/forward.log"; then
  sed 's/^/  forward: /' "$D/forward.log" >&2
  die "the port-forward did not come up"
fi
# The listener must be this run's forward, not something that took the port since.
ss -Htlnp "sport = :$PORT" 2>/dev/null | grep -q "pid=$PF," || die "port $PORT is not this run's port-forward"
say "forward: kubectl port-forward pid $PF, 127.0.0.1:$PORT -> $POD:27017"

DENV=(-e "PMR_DB=$DB")
for e in "${ENVS[@]+"${ENVS[@]}"}"; do DENV+=(-e "$e"); done
say "client: $IMAGE as container $CNAME, host network, memory limit $CLIENT_MEMORY"
rc=0
docker run --rm --name "$CNAME" --network host \
  --memory "$CLIENT_MEMORY" --memory-swap "$CLIENT_MEMORY" --pids-limit 256 \
  -v "$D/cred:/run/prod-mongo-read/cred:ro" -v "$D/js:/run/prod-mongo-read/js:ro" \
  "${DENV[@]}" "$IMAGE" sh -c '
    for k in MONGO_INITDB_ROOT_USERNAME MONGO_INITDB_ROOT_PASSWORD MONGO_INITDB_DATABASE; do
      export "$k=$(cat "/run/prod-mongo-read/cred/$k")"
    done
    exec mongosh --nodb --quiet /run/prod-mongo-read/js/preamble.js /run/prod-mongo-read/js/script.js' || rc=$?

teardown
after_restarts="$(restarts)"
after_cgroup="$(cgroup_read)"
say "after: $POD restarts=$after_restarts, $(cgroup_line "$after_cgroup")"
if [ "$after_restarts" != "$before_restarts" ] || [ "$(oom_kills "$after_cgroup")" != "$(oom_kills "$before_cgroup")" ]; then
  say "⚠️ $POD RESTARTED, was OOM-killed, or could not be read after the read: treat this run as harmful, not as data"
  kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].lastState}' >&2 2>/dev/null || true
  echo >&2
  exit 3
fi
[ "$TEARDOWN_OK" = 1 ] || exit 5
exit "$rc"
