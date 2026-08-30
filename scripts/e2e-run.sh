#!/usr/bin/env bash
# Run some (or all) e2e specs against the standing `roomler-ai-e2e` stack, on
# demand. The nightly (`e2e-nightly.sh`) runs the whole suite and triages it;
# this is the one you want while working on a spec or bisecting a build.
#
# Usage, from the build host:
#   scripts/e2e-run.sh                             # whole suite, current image
#   scripts/e2e-run.sh '' e2e/mention.spec.ts      # one spec, current image
#   scripts/e2e-run.sh v20260829-abc123 e2e/conference.spec.ts e2e/chat.spec.ts
#
# The first argument pins the stack to an image tag. 🔑 That is what makes an
# A/B possible: the e2e namespace is deliberately NOT ArgoCD-managed, so any
# historical tag can be pinned and the SAME spec run against both sides. A test
# that has not been shown to fail on the broken build has proven nothing.
#
# Like the nightly, this drives the `pwrunner` sidecar INSIDE the app pod.
# ⚠️ Not a browser out here: this host has no route to the pod network (so no
# RTP ever arrives), and a Service URL is not a secure context (so
# `navigator.mediaDevices` does not exist). Both are why the conference specs
# "failed" for months. See docs/fr/FR-37-e2e-in-cluster.md.
set -uo pipefail

NS=roomler-ai-e2e
REPO="${REPO:-$HOME/wt-e2e-lane}"
TAG="${1:-}"; shift || true
SPECS=("$@")

note() { echo "[$(date -u +%H:%M:%SZ)] $*"; }
die()  { echo "ABORT: $*" >&2; exit 2; }

cd "$REPO" || die "no repo at $REPO (override with REPO=)"
git fetch origin --quiet || note "git fetch failed — running with the existing checkout"
git checkout --quiet --detach origin/master || note "could not detach at origin/master"
note "specs at $(git log --oneline -1)"

if [ -n "$TAG" ]; then
  note "pinning the stack to $TAG"
  kubectl -n "$NS" set image deploy/roomler2 "roomler2=registry.roomler.ai/roomler-ai:$TAG" >/dev/null \
    || die "could not set the image"
  kubectl -n "$NS" rollout status deploy/roomler2 --timeout=300s >/dev/null || die "the stack did not roll to $TAG"
fi
# ⚠️ Select the app container BY NAME. `containers[0]` is the sidecar now, and
# this line reported the browser image as "the stack" the first time it ran.
note "stack on $(kubectl -n "$NS" get deploy roomler2   -o jsonpath='{.spec.template.spec.containers[?(@.name=="roomler2")].image}')"

POD=$(kubectl -n "$NS" get pod -l app=roomler2 -o jsonpath='{.items[0].metadata.name}')
[ -n "$POD" ] || die "no roomler2 pod in $NS"
kubectl -n "$NS" get pod "$POD" -o jsonpath='{.spec.containers[*].name}' | tr ' ' '\n' | grep -qx pwrunner \
  || die "pod $POD has no pwrunner sidecar — apply k8s/overlays/e2e from the deploy repo"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
# ⚠️ minus e2e/video/ — that spec uses bun-only JSON-import syntax and kills
# collection under node, taking the whole run with it.
rsync -a --exclude node_modules --exclude dist --exclude test-results \
      --exclude playwright-report --exclude e2e/video "$REPO/ui/" "$WORK/ui/"
kubectl -n "$NS" exec "$POD" -c pwrunner -- rm -rf /work
kubectl -n "$NS" cp "$WORK/ui" "$POD":/work -c pwrunner >/dev/null 2>&1 || die "could not copy the specs in"

note "running ${SPECS[*]:-the whole suite}"
kubectl -n "$NS" exec "$POD" -c pwrunner -- bash -lc "
  cd /work && npm i --no-audit --no-fund --loglevel=error >/dev/null 2>&1 &&
  CI=1 E2E_BASE_URL=http://127.0.0.1 E2E_API_URL=http://127.0.0.1 \
  E2E_MAILPIT_URL=http://mailpit:8025 \
  npx playwright test ${SPECS[*]:-} --reporter=line --timeout=60000 --retries=0"
RC=$?
note "playwright exit $RC"
exit "$RC"
