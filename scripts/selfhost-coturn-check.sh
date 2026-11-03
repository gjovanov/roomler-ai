#!/usr/bin/env bash
# Is this compose file's coturn up, and does it really relay? (#3287)
#
# A fresh install's coturn crash-looped on a flag its image had dropped, and nothing
# noticed: /health stayed 200 and the server kept advertising the TURN URLs. With that
# flag gone, the same coturn stayed up and answered 403 to every relay to the public
# address, so a status read proves little either. This starts the file's own service,
# with its image and command and never a copy, watches it, then drives the image's own
# TURN client through the published port, with credentials minted the way the server
# mints them (crates/remote_control/src/turn_creds.rs).
#
# Usage: scripts/selfhost-coturn-check.sh [--file COMPOSE] [--env-file FILE] [--window SECONDS]
#   --file      the compose file. Default: docker-compose.selfhost.yml. docker-compose.yml,
#               the development stack, gets the checks up to the CreatePermission: it has
#               no deny list and no SFU, and it relays on every address of the host.
#   --env-file  passed to compose. Default: .env.selfhost beside a selfhost file.
#   --window    how long coturn must stay up, in seconds. Default: 30.
#
# It runs `up -d --no-build coturn`, which brings up what coturn needs, the server and its
# datastores among them, and leaves a running, unchanged service alone. It starts no
# image build, and it stops nothing and removes nothing of the stack's.
# Run it on the Linux host that runs the stack. The client uses the host's network, so
# it comes in through the published port as a real client does.
#
# Exits 0 when every check passes, 1 when one fails, 2 when it could not run.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FILE="$ROOT/docker-compose.selfhost.yml"
ENV_FILE=""
WINDOW=30
while [ $# -gt 0 ]; do
  case "$1" in
    --file) FILE=$2; shift 2 ;;
    --env-file) ENV_FILE=$2; shift 2 ;;
    --window) WINDOW=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
for tool in docker openssl base64 timeout; do
  command -v "$tool" >/dev/null || { echo "this check needs $tool" >&2; exit 2; }
done

SELFHOST=0
if [ "$(basename "$FILE")" = docker-compose.selfhost.yml ]; then
  SELFHOST=1
  : "${ENV_FILE:=$(dirname "$FILE")/.env.selfhost}"
fi
if [ -n "$ENV_FILE" ] && [ ! -f "$ENV_FILE" ]; then
  echo "no $ENV_FILE. docs/self-hosting.md's Quickstart makes it from .env.selfhost.example." >&2
  exit 2
fi
DC=(docker compose -f "$FILE")
[ -n "$ENV_FILE" ] && DC+=(--env-file "$ENV_FILE")

FAILS=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }
note() { echo "NOTE $*"; }
detail() { sed 's/^/     /'; }
verdict() {
  [ "$FAILS" = 0 ] && { echo "== coturn is healthy"; exit 0; }
  echo "== coturn is NOT healthy: $FAILS check(s) failed. docs/self-hosting.md, \"The TURN relay\", says what each one means."
  exit 1
}

OUT="$(mktemp)"
CLIENT="coturn-check-client-$$"
PEER="coturn-check-peer-$$"
cleanup() {
  docker rm -f "$CLIENT" "$PEER" >/dev/null 2>&1
  rm -f "$OUT"
}
trap cleanup EXIT

echo "== coturn from $(basename "$FILE")"
# `up -d` below starts a stopped coturn, and says nothing about why it had stopped.
WAS="$("${DC[@]}" ps -aq coturn 2>/dev/null)"
if [ -n "$WAS" ]; then
  case "$(docker inspect -f '{{.State.Status}}' "$WAS")" in
    # Restarting is Docker's own doing, and the watch below judges it.
    running | restarting) ;;
    *) note "coturn was not running, so this started it. It had stopped with: $(docker inspect -f 'exit {{.State.ExitCode}} {{.State.Error}}' "$WAS")" ;;
  esac
fi
"${DC[@]}" up -d --no-build coturn >"$OUT" 2>&1 || {
  fail "docker compose up"
  tail -5 "$OUT" | detail
  verdict
}
CID="$("${DC[@]}" ps -aq coturn)"
[ -n "$CID" ] || { echo "$(basename "$FILE") started no coturn container" >&2; exit 2; }

IMAGE="$(docker inspect -f '{{.Config.Image}}' "$CID")"
if [[ "$IMAGE" =~ ^coturn/coturn:[^@:]+@sha256:[0-9a-f]{64}$ ]]; then
  pass "the image is pinned: $IMAGE"
else
  fail "the image is not pinned by tag and digest: $IMAGE"
fi

# A crash-looping container can read `running` between two exits, so one status read
# proves nothing: it has to stay running with a count that holds still. A count above 0
# that holds still means it died and came back, as coturn's healthcheck makes it do after
# a server crash, so that is a note.
state() { docker inspect -f '{{.State.Status}} {{.RestartCount}}' "$CID" 2>/dev/null; }
crashed() {
  fail "coturn is not staying up: status=$1, RestartCount=$2"
  # The grep reads the whole log, because a plain tail hides the reason: an unrecognized
  # option prints the full help text after the line that says so.
  docker logs "$CID" 2>&1 | grep -aE 'unrecognized option|invalid option|Aborting|ERROR' \
    | sed -E 's/^[0-9T:.+-]+ //' | sort | uniq -c | sort -rn | head -5 | detail
  verdict
}
END=$(( $(date +%s) + WINDOW ))
FIRST=""
while :; do
  read -r STATUS RESTARTS <<<"$(state)"
  : "${FIRST:=$RESTARTS}"
  [ "$STATUS" = running ] && [ "$RESTARTS" = "$FIRST" ] || crashed "${STATUS:-gone}" "${RESTARTS:-?}"
  [ "$(date +%s)" -ge "$END" ] && break
  sleep 2
done
pass "it stayed up for ${WINDOW}s, RestartCount $RESTARTS"
[ "$RESTARTS" = 0 ] || note "coturn has restarted $RESTARTS time(s) since Docker last started it, and is not climbing now. Each server crash costs one."

# The service coturn takes its network from, as this file says.
network_of() {
  "${DC[@]}" config 2>/dev/null | awk -v s="  $1:" '
    $0 == s { inside = 1; next }
    inside && /^  [^ ]/ { exit }
    inside && /^    network_mode:/ { print $2; exit }'
}
OURS="$(network_of coturn)"

# coturn outlives the container it takes its network from. When the server's process dies,
# Docker brings it back in a new namespace and leaves coturn in the old one with no address:
# `docker ps` stays green and `up -d` sees nothing to do. coturn's healthcheck ends it, and
# its restart policy brings it back in. This says so if that has not happened (yet).
OWNER="${OURS#service:}"
if [ "$OWNER" != "$OURS" ]; then
  namespace() { docker exec "$1" readlink /proc/self/ns/net 2>/dev/null; }
  stuck() { # stuck <what> <compose command that repairs it>
    fail "$1"
    echo "     If this persists: docker compose -f $(basename "$FILE") --env-file $(basename "${ENV_FILE:-.env.selfhost}") $2"
    verdict
  }
  THEIRS="$(namespace "$("${DC[@]}" ps -q "$OWNER")")"
  [ -n "$THEIRS" ] || stuck "the $OWNER service, whose namespace coturn runs in, is not running" "logs $OWNER"
  [ "$(namespace "$CID")" = "$THEIRS" ] || stuck "coturn is running outside the $OWNER service's namespace, so nothing reaches it on a published port. Its healthcheck ends it within about 10 s, and it comes back inside" "restart coturn"
  [ -n "$(docker exec "$CID" hostname -I 2>/dev/null | tr -d ' ')" ] || stuck "the namespace coturn shares with the $OWNER service has no address" "restart $OWNER"
  pass "coturn is inside the $OWNER service's namespace, and it has an address"
fi

# The secret is what this coturn runs with, so a stale env file cannot make a healthy
# service look broken. It sits on turnserver's argv for as long as the service runs,
# so the moment it spends on openssl's adds no exposure.
arg_of() { docker inspect -f '{{range .Config.Cmd}}{{println .}}{{end}}' "$CID" | sed -n "s/^$1=//p" | tail -1; }
SECRET="$(arg_of --static-auth-secret)"
[ -n "$SECRET" ] || SECRET="$(docker exec "$CID" sed -n 's/^static-auth-secret=//p' /etc/turnserver.conf 2>/dev/null)"
[ -n "$SECRET" ] || { echo "found no static-auth-secret in coturn's command or /etc/turnserver.conf" >&2; exit 2; }

# client <flags…>: everything the client says lands in $OUT; returns its exit code. The
# entrypoint is overridden because the image's own runs each argument through `eval echo`,
# which swallows a bare `-e`.
client() {
  local user pass rc
  user="$(( $(date +%s) + 600 )):coturn-check"
  pass="$(printf '%s' "$user" | openssl dgst -sha1 -binary -hmac "$SECRET" | base64)"
  # Against a silent port the client retries forever. As PID 1 it would ignore the SIGTERM,
  # and `docker run` would wait for it, so the check needs --init, and -k behind that.
  # --no-even-port makes it take any free port, as a browser does.
  timeout -k 5 30 docker run --rm --init --name "$CLIENT" --network host --entrypoint turnutils_uclient "$IMAGE" \
    --no-even-port -u "$user" -w "$pass" "$@" 127.0.0.1 >"$OUT" 2>&1
  rc=$?
  [ "$rc" -ge 124 ] && [ "$rc" -ne 255 ] && docker rm -f "$CLIENT" >/dev/null 2>&1
  return "$rc"
}
said() {
  # A 508 got no further than the allocation, so it says nothing about what was being tried.
  grep -aq 'error 508' "$OUT" && echo "     coturn has no free relay port: every port of its range is held by an allocation"
  sed -E 's/^[0-9T:.+-]+ //; s/^(INFO|WARNING|ERROR) //' "$OUT" | grep -aE "$1" | sort -u | head -3 | detail
}
# The client exits 0 at 100% loss, so bytes are read from its own final count.
received() { sed -n 's/.*start_mclient: tot_send_msgs=[0-9]*, tot_recv_msgs=\([0-9]*\).*/\1/p' "$OUT" | tail -1; }
# Refusals reach `docker logs` only under --log-file=stdout; otherwise they are in a file in the container.
coturn_says() {
  { docker logs "$CID" 2>&1; docker exec "$CID" sh -c 'cat /var/tmp/turn_*.log' 2>/dev/null; } \
    | grep -a "$1" | tail -1 | sed -E 's/^[0-9T:.+-]+ //; s/session [0-9]+/session N/' | detail
}

# A peer nothing routes to, and not the public address, for the plain allocations. No
# message is sent: the client waits about 10 s for an echo from a peer that never answers.
# It prints the address it was given only under -v.
RELAY=""
for how in UDP TCP; do
  flags=(); [ "$how" = TCP ] && flags=(-t)
  client "${flags[@]}" -v -e 203.0.113.9 -n 0 -m 1 -l 120 -c
  rc=$?
  got="$(sed -n 's/.*Received relay addr: \([0-9.]*\):.*/\1/p' "$OUT" | head -1)"
  if [ "$rc" = 0 ] && [ -n "$got" ]; then
    pass "a client reached 3478 over $how and got a relay allocation on $got"
    RELAY="$got"
  else
    fail "no relay allocation over $how (client exit $rc)"
    said 'rror|refused|Forbidden|Unauthorized'
  fi
done
[ -n "$RELAY" ] || verdict

WANT="$(arg_of --external-ip | cut -d/ -f1)"
if [ -n "$WANT" ] && [[ "$WANT" != *'$'* ]] && [ "$WANT" != "$RELAY" ]; then
  fail "coturn hands out $RELAY as the relay address, but its --external-ip is $WANT"
fi
if [ "$RELAY" = 127.0.0.1 ]; then
  note "the relay address is 127.0.0.1, ROOMLER_PUBLIC_IP's default. Only a client on this host can use it: anyone else is told to send to their own loopback, and nothing logs that."
fi

# What every browser does first, and what a bare --external-ip on a bridge refused: it
# rewrites the public address to coturn's own private one before the peer check.
client -s -e "$RELAY" -r 3480 -n 0 -m 1 -l 120 -c
rc=$?
if [ "$rc" = 0 ] && ! grep -aq 'create permission error' "$OUT"; then
  pass "CreatePermission to the public address $RELAY is granted"
else
  fail "CreatePermission to the public address $RELAY is refused (client exit $rc), so no call can use this relay"
  said 'create permission error'
  coturn_says 'denied'
fi

# The development file stops here. On the host's network every address of the host is a
# relay address, and coturn rewrites the public address to the first of them, so relay to
# relay depends on where each allocation landed: that is the host, not the file.
[ "$SELFHOST" = 1 ] || verdict

# Two clients that can punch nothing, each relayed.
for how in UDP TCP; do
  flags=(); [ "$how" = TCP ] && flags=(-t)
  client "${flags[@]}" -y -n 20 -m 1 -l 120 -c
  rc=$?
  n="$(received)"
  if [ "$rc" = 0 ] && [ "${n:-0}" -gt 0 ]; then
    pass "relay to relay carried bytes, $how to the server: $n messages back"
  else
    fail "relay to relay carried nothing, $how to the server (client exit $rc, ${n:-0} messages back)"
    said 'error|rror:'
  fi
done

# A relay to the public address lands in coturn's own namespace, so the SFU has to live
# there too. Without that, every check above passes and a call gets no media.
if [ "$OURS" = service:roomler ]; then
  pass "coturn runs in the server's network namespace ($OURS)"
else
  fail "coturn (network_mode: ${OURS:-none}) does not run in the server's network namespace, so a relay to the public address cannot reach the SFU"
fi

# A listener where the SFU's sockets are, reached the way a browser reaches them.
if docker run -d --rm --name "$PEER" --network "container:$CID" --entrypoint turnutils_peer "$IMAGE" -p 3480 >/dev/null 2>&1; then
  sleep 1
  client -e "$RELAY" -r 3480 -n 20 -m 1 -l 120 -c
  rc=$?
  n="$(received)"
  if [ "$rc" = 0 ] && [ "${n:-0}" -gt 0 ]; then
    pass "a listener in coturn's namespace answered through the relay: $n messages back"
  else
    fail "a listener in coturn's namespace got nothing back through the relay (client exit $rc, ${n:-0} messages back)"
    said 'error|rror:'
  fi
  docker rm -f "$PEER" >/dev/null 2>&1
else
  fail "could not start a listener in coturn's namespace"
fi

# The deny list, and what coturn refuses by itself. A refusal counts only as its own
# literal answer: the client also exits non-zero for reasons that prove nothing.
# Over TCP to the server, because a client over UDP leaves its two allocations behind, each
# holding a relay port for 777 s. Every line here over UDP would cost a run of this check
# about 30 of the range's 101, and the third run in a row would fail on the leftovers of
# the first two. Over TCP they end with the connection.
refused() { # refused <peer> <why>
  [ "$1" = "$RELAY" ] && return
  client -t -s -e "$1" -r 3480 -n 0 -m 1 -l 120 -c
  local rc=$?
  if grep -aq 'create permission error 403' "$OUT"; then
    pass "refused, $2: $1"
  elif [ "$rc" = 0 ]; then
    fail "NOT refused, $2: $1"
  else
    fail "could not ask about $2, $1 (client exit $rc)"
    said 'create permission|rror'
  fi
}
refused 10.0.0.1 'private'
refused 172.16.0.1 'private'
refused 192.168.0.1 'private'
refused 100.64.0.1 'the overlay and CGNAT range'
refused 169.254.169.254 'the cloud metadata address'
refused 127.0.0.1 'loopback'
refused 224.0.0.1 'multicast'
# Every other address of this stack, the gateway above all: it reaches every listener on
# the host. Only the namespace's own address may pass, and only for coturn's relay ports and the SFU's.
OWN=" $(docker exec "$CID" hostname -I 2>/dev/null) "
SEEN=" "
for c in $("${DC[@]}" ps -q); do
  name="$(docker inspect -f '{{.Name}}' "$c")"
  for a in $(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{.Gateway}} {{end}}' "$c"); do
    case "$OWN$SEEN" in *" $a "*) continue ;; esac
    SEEN="$SEEN$a "
    refused "$a" "on this stack's network (${name#/})"
  done
done

# A TCP allocation would reach every TCP listener on the one address the deny list lets
# through, the API included. Nothing in the product asks for one: browsers and the agent's
# TURN client (the `turn` crate) request UDP only. TCP to the server is a different thing and stays.
client -T -e "$RELAY" -r 80 -n 0 -m 1 -l 120 -c
rc=$?
if grep -aq 'error 442' "$OUT"; then
  pass "a TCP relay allocation is refused (442)"
else
  fail "a TCP relay allocation was not refused with a 442 (client exit $rc)"
  said 'error|rror:|Received relay'
fi

read -r STATUS RESTARTS <<<"$(state)"
[ "$STATUS" = running ] && [ "$RESTARTS" = "$FIRST" ] || crashed "${STATUS:-gone}" "${RESTARTS:-?}"
verdict
