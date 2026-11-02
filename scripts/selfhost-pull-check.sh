#!/usr/bin/env bash
# #2034 — the documented self-host path, as a stranger runs it, on this machine's
# own architecture: no registry login, an empty image cache, `pull` then `up -d`.
#
# `scripts/selfhost-smoke.sh` proves the same document through a source build,
# which takes 20 minutes and pulls none of the published images. This one takes
# about two, and it fails when an image the compose file names can no longer be
# pulled, the failure nothing noticed on #2034 because no lane pulled it.
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
# which cannot tag a build with one, and the source build is the AGPL promise.
for f in docker-compose.selfhost.yml docker-compose.yml; do
  while IFS= read -r ref; do
    case "$ref" in
      '${ROOMLER_IMAGE:-'*) ;;
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
for svc in mongo redis minio roomler coturn; do
  cid="$("${DC[@]}" ps -a -q "$svc")"
  state="$(docker inspect "$cid" --format '{{.State.Status}}')"
  restarts="$(docker inspect "$cid" --format '{{.RestartCount}}')"
  [ "$state" = running ] || bad "$svc is '$state' (restarts: $restarts): $(docker logs --tail 3 "$cid" 2>&1 | tr '\n' ' ')"
  [ "$restarts" = 0 ] || bad "$svc has restarted $restarts times: $(docker logs --tail 3 "$cid" 2>&1 | tr '\n' ' ')"
done
init="$("${DC[@]}" ps -a -q minio-init)"
[ "$(docker inspect "$init" --format '{{.State.ExitCode}}')" = 0 ] || bad "minio-init failed, so uploads have no bucket"

"${DC[@]}" down -v >/dev/null 2>&1
rm -f "$ENVF"
[ "$FAIL" = 0 ] && say "the documented path works on $(uname -m)"
exit "$FAIL"
