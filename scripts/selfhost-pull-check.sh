#!/usr/bin/env bash
# #2034 — the documented self-host path, as a stranger runs it, on this machine's
# own architecture: no registry login, an empty image cache, `pull` then `up -d`.
#
# `scripts/selfhost-smoke.sh` proves the same document through a source build,
# which takes 20 minutes and pulls none of the published images. This one takes
# about two, and it fails when an image the compose file names can no longer be
# pulled, the failure nothing noticed on #2034 because no lane pulled it. Then two
# real browsers make a call, directly and over the relay (#3287), because "the
# containers are up" says nothing about media on this architecture.
#
#   bash scripts/selfhost-pull-check.sh
#
# Run it on a throwaway machine: it finishes with `down -v`.
set -uo pipefail

FILE=docker-compose.selfhost.yml
ENVF=.env.selfhost.check
FAIL=0

say() { echo "[$(date -u +%H:%M:%SZ)] $*"; }
bad() {
  echo "::error::$*"
  FAIL=1
}

# ── every image is pinned ────────────────────────────────────────────────────
# A digest, in both compose files. The one exception is the Roomler image, whose
# default is a tag we publish ourselves: a digest there breaks `up -d --build`,
# which cannot tag a build with one, and the source build is the AGPL promise. It
# still has to be an exact tag, or `git pull` stops being what moves it.
for f in docker-compose.selfhost.yml docker-compose.yml; do
  while IFS= read -r ref; do
    case "$ref" in
      '${ROOMLER_IMAGE:-'*':v'[0-9]*'}') ;;
      '${ROOMLER_IMAGE:-'*) bad "$f defaults the Roomler image to '$ref', not an exact v* tag" ;;
      *@sha256:*) ;;
      *) bad "$f names '$ref' with no digest: the next upstream push reaches every fresh install" ;;
    esac
  done < <(grep -E '^[[:space:]]*image:' "$f" | sed -E 's/^[[:space:]]*image:[[:space:]]*//; s/[[:space:]]+$//; s/\r$//')
done
[ "$FAIL" = 0 ] && say "every image in both compose files is pinned"

# ── a stranger's machine: no login, nothing cached ───────────────────────────
DOCKER_CONFIG="$(mktemp -d)"
export DOCKER_CONFIG
echo '{}' > "$DOCKER_CONFIG/config.json"
docker image prune -af >/dev/null 2>&1 || true
say "docker $(docker version --format '{{.Server.Version}}'), $(docker compose version --short), $(uname -m)"

# ── the document's cp + fill ─────────────────────────────────────────────────
cp .env.selfhost.example "$ENVF"
sed -i "s|^ROOMLER_JWT_SECRET=.*|ROOMLER_JWT_SECRET=$(openssl rand -hex 32)|" "$ENVF"
sed -i "s|^ROOMLER_TURN_SECRET=.*|ROOMLER_TURN_SECRET=$(openssl rand -hex 32)|" "$ENVF"
sed -i "s|^MONGO_ROOT_PASSWORD=.*|MONGO_ROOT_PASSWORD=$(openssl rand -hex 24)|" "$ENVF"
sed -i "s|^MINIO_ROOT_PASSWORD=.*|MINIO_ROOT_PASSWORD=$(openssl rand -hex 24)|" "$ENVF"
DC=(docker compose -f "$FILE" --env-file "$ENVF")

# ── the document's pull + up ─────────────────────────────────────────────────
T0=$(date +%s)
if ! "${DC[@]}" pull --quiet; then
  bad "docker compose pull failed: an image the compose file names cannot be pulled anonymously"
  exit 1
fi
say "pull took $(($(date +%s) - T0)) s"
if ! "${DC[@]}" up -d; then
  bad "docker compose up -d failed"
  "${DC[@]}" ps -a
  "${DC[@]}" logs --tail 40
  exit 1
fi

healthy=""
for _ in $(seq 1 90); do
  if curl -fsS -m 3 -o /dev/null http://localhost:8080/health 2>/dev/null; then
    healthy=1
    break
  fi
  sleep 2
done
if [ -n "$healthy" ]; then
  say "/health answered $(($(date +%s) - T0)) s after the first command: $(curl -fsS -m 3 http://localhost:8080/health)"
else
  bad "/health never answered within 3 min"
fi
code="$(curl -s -o /dev/null -w '%{http_code}' -m 5 http://localhost:8080/)"
[ "$code" = 200 ] || bad "GET / answered $code: the web app is not being served"

# ── every service is what it should be, after a crash loop has had time to show ──
sleep 30
say "services:"
"${DC[@]}" ps -a --format '  {{.Service}}  {{.State}}  {{.Status}}'
for svc in $("${DC[@]}" config --services); do
  [ "$svc" = minio-init ] && continue
  cid="$("${DC[@]}" ps -a -q "$svc")"
  state="$(docker inspect "$cid" --format '{{.State.Status}}')"
  restarts="$(docker inspect "$cid" --format '{{.RestartCount}}')"
  [ "$state" = running ] || bad "$svc is '$state' (restarts: $restarts): $(docker logs --tail 3 "$cid" 2>&1 | tr '\n' ' ')"
  [ "$restarts" = 0 ] || bad "$svc has restarted $restarts times: $(docker logs --tail 3 "$cid" 2>&1 | tr '\n' ' ')"
done
init="$("${DC[@]}" ps -a -q minio-init)"
[ "$(docker inspect "$init" --format '{{.State.ExitCode}}')" = 0 ] || bad "minio-init failed, so uploads have no bucket"

# ── a call, then a call over the relay ───────────────────────────────────────
# The browser runs on this machine, in the Playwright image pinned to the repo's
# runner, on the host network: the only way http://localhost:8080 is both the
# server's Origin and a secure context, and the address the SFU announces.
PW_VER="$(sed -nE 's/.*"@playwright\/test": "([0-9.]+)".*/\1/p' ui/package.json)"
PW_DIR="$(mktemp -d)"
cp -r ui/e2e ui/playwright.config.ts "$PW_DIR/"
printf '{"name":"selfhost-e2e","private":true,"devDependencies":{"@playwright/test":"%s"}}\n' "$PW_VER" > "$PW_DIR/package.json"
OVR=.selfhost-check.override.yml

# server_settings [environment line]: the server on the given settings. The rate
# limits are the profile matrix's one deviation: a Playwright run is one IP doing a
# whole team's work. It is a plain `up -d`, which is also what a release does: the
# server is recreated and coturn, in the server's namespace, has to follow. Naming
# the service here (`up -d roomler`) would leave coturn exited.
server_settings() {
  local out
  {
    printf 'services:\n  roomler:\n    environment:\n'
    printf '      ROOMLER__APP__RATE_LIMIT_PER_SEC: "200"\n      ROOMLER__APP__RATE_LIMIT_BURST: "5000"\n'
    printf '      ROOMLER__APP__AUTH_RATE_LIMIT_PER_MIN: "600"\n      ROOMLER__APP__AUTH_RATE_LIMIT_BURST: "600"\n'
    [ -n "${1:-}" ] && printf '      %s\n' "$1"
  } > "$OVR"
  if ! out="$("${DC[@]}" -f "$OVR" up -d 2>&1)"; then
    echo "$out" | tail -8
    return 1
  fi
  for _ in $(seq 1 60); do
    curl -fsS -m 3 -o /dev/null http://localhost:8080/health 2>/dev/null && return 0
    sleep 2
  done
  return 1
}

# browser_spec <spec> [docker run arguments]
browser_spec() {
  local spec="$1"
  shift
  docker run --rm --network host -v "$PW_DIR:/work" -w /work -e CI=1 \
    -e E2E_BASE_URL=http://localhost:8080 -e E2E_API_URL=http://localhost:8080 "$@" \
    "mcr.microsoft.com/playwright:v${PW_VER}-jammy" bash -lc \
    "npm i --no-audit --no-fund >/tmp/npm.log 2>&1 || { tail -20 /tmp/npm.log; exit 1; }
     npx playwright test $spec --reporter=line --retries=0"
}

say "a two-party call on the direct path"
server_settings || bad "the server did not come back with the browser rate limits"
browser_spec e2e/selfhost-collab.spec.ts || bad "the direct-path call failed on $(uname -m)"

say "a two-party call with the relay forced"
server_settings 'ROOMLER__TURN__FORCE_RELAY: "true"' || bad "the server did not come back with the relay forced"
# A stopped relay reaches the browser as peers stuck at `new`, which names nothing.
coturn="$(docker inspect "$("${DC[@]}" ps -a -q coturn)" --format '{{.State.Status}}' 2>/dev/null)"
[ "$coturn" = running ] || bad "coturn is '${coturn:-gone}' after the server was recreated, so no call can reach the relay"
if ! browser_spec e2e/selfhost-turn-relay.spec.ts -e E2E_EXPECT_TURN_RELAY=1; then
  bad "the call over the relay failed on $(uname -m)"
  "${DC[@]}" ps -a --format '  {{.Service}}  {{.State}}  {{.Status}}'
fi
"${DC[@]}" logs --no-log-prefix coturn 2>&1 | grep -E 'denied|403' | tail -8

if [ "$FAIL" != 0 ] && [ -d "$PW_DIR/test-results" ]; then
  mkdir -p selfhost-artifacts && cp -r "$PW_DIR/test-results" selfhost-artifacts/ 2>/dev/null
fi

"${DC[@]}" -f "$OVR" down -v >/dev/null 2>&1
rm -f "$ENVF" "$OVR"
[ "$FAIL" = 0 ] && say "the documented path works on $(uname -m)"
exit "$FAIL"
