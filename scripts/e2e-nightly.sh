#!/usr/bin/env bash
# Nightly Playwright sweep against the standing `roomler-ai-e2e` stack.
#
# The suite has NO GitHub CI lane (it needs a full backend + Mailpit), and
# when it never runs it rots: the first manual sweep (2026-07-28) found two
# shipped auth bugs. This script keeps it honest from the build host.
#
# What it does, in order:
#   1. Fast-forwards the repo clone (spec source of truth).
#   2. Points the e2e stack's roomler2 at the CURRENT PROD image tag (read
#      from the deploy repo's prod overlay) via `kubectl set image` — the
#      e2e namespace is deliberately NOT ArgoCD-managed.
#   3. Port-forwards the app (:18080) and Mailpit (:18025).
#   4. Runs the suite in the official Playwright container (spec dir copied
#      to a scratch tree minus e2e/video/ — that spec is bun-only syntax).
#   5. Diffs the failing specs against scripts/e2e-expected-failures.txt and
#      writes ~/e2e-nightly/LATEST (one summary line) + a dated full log.
#      Unexpected failures ⇒ exit 1 (and a GitHub issue, if `gh` is authed).
#
# Install (build host):
#   crontab: 30 3 * * * $HOME/roomler-ai/scripts/e2e-nightly.sh >> $HOME/e2e-nightly/cron.log 2>&1
set -uo pipefail

REPO="${REPO:-$HOME/roomler-ai}"
DEPLOY_REPO="${DEPLOY_REPO:-$HOME/roomler-ai-deploy}"
OUT="${OUT:-$HOME/e2e-nightly}"
NS=roomler-ai-e2e
PW_IMG="${PW_IMG:-mcr.microsoft.com/playwright:v1.58.2-jammy}"
APP_PORT=18080
MAIL_PORT=18025

STAMP=$(date -u +%Y%m%d-%H%M)
LOG="$OUT/$STAMP.log"
mkdir -p "$OUT"

note() { echo "[$(date -u +%H:%M:%SZ)] $*" | tee -a "$LOG"; }
fail_hard() { note "ABORT: $*"; echo "$STAMP INFRA-FAIL: $*" > "$OUT/LATEST"; exit 2; }

# ── 1. fresh specs ───────────────────────────────────────────────────
cd "$REPO" || fail_hard "repo missing"
git pull -q --ff-only || note "git pull failed — running with the existing checkout"

# ── 2. sync the e2e stack to the prod image ──────────────────────────
PRODTAG=$(awk '/newTag:/ {print $2; exit}' "$DEPLOY_REPO/k8s/overlays/prod/kustomization.yaml")
[ -n "$PRODTAG" ] || fail_hard "could not read prod newTag"
kubectl -n "$NS" set image deploy/roomler2 "roomler2=registry.roomler.ai/roomler-ai:$PRODTAG" >> "$LOG" 2>&1
kubectl -n "$NS" rollout status deploy/roomler2 --timeout=300s >> "$LOG" 2>&1 || fail_hard "e2e stack failed to roll to $PRODTAG"
note "e2e stack on $PRODTAG"

# ── 3. port-forwards (fresh each run; pods may have rolled) ──────────
pkill -f "kubectl.*port-forward.*$NS" 2>/dev/null
sleep 1
setsid -f kubectl -n "$NS" port-forward --address 127.0.0.1 svc/roomler2 "$APP_PORT:80" > "$OUT/pf-app.log" 2>&1
setsid -f kubectl -n "$NS" port-forward --address 127.0.0.1 svc/mailpit "$MAIL_PORT:8025" > "$OUT/pf-mail.log" 2>&1
sleep 3
curl -sf -o /dev/null "http://127.0.0.1:$APP_PORT/health" || fail_hard "app port-forward dead"
curl -sf -o /dev/null "http://127.0.0.1:$MAIL_PORT/api/v1/info" || fail_hard "mailpit port-forward dead"

# ── 4. run the suite ─────────────────────────────────────────────────
WORK="$OUT/ui-work"
rsync -a --delete --exclude node_modules --exclude dist --exclude test-results \
  --exclude playwright-report --exclude e2e/video "$REPO/ui/" "$WORK/"
docker run --rm --network host -v "$WORK:/work" -w /work \
  -e CI=1 \
  -e "E2E_BASE_URL=http://127.0.0.1:$APP_PORT" \
  -e "E2E_API_URL=http://127.0.0.1:$APP_PORT" \
  -e "E2E_MAILPIT_URL=http://127.0.0.1:$MAIL_PORT" \
  "$PW_IMG" bash -lc "npm i --no-audit --no-fund --loglevel=error && npx playwright test --reporter=line --timeout=60000" >> "$LOG" 2>&1
RC=$?
pkill -f "kubectl.*port-forward.*$NS" 2>/dev/null

# ── 5. triage against the expected-failures baseline ─────────────────
CLEAN=$(sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g' "$LOG" | tr '\r' '\n')
SUMMARY=$(echo "$CLEAN" | grep -E '^\s+[0-9]+ (passed|failed|flaky|skipped)' | tr -d ' ' | paste -sd' ' -)
FAILED=$(echo "$CLEAN" | grep -A400 '^  [0-9]* failed' | grep -oE '\[chromium\] › [^ ]+ › .*' | sed 's/ *$//' | sort -u)

UNEXPECTED=""
if [ -n "$FAILED" ]; then
  while IFS= read -r f; do
    [ -z "$f" ] && continue
    if ! grep -qFf "$REPO/scripts/e2e-expected-failures.txt" <<< "$f"; then
      UNEXPECTED+="$f"$'\n'
    fi
  done <<< "$FAILED"
fi

if [ -n "$UNEXPECTED" ]; then
  note "UNEXPECTED FAILURES:"
  echo "$UNEXPECTED" | tee -a "$LOG"
  echo "$STAMP REGRESSION ($SUMMARY) tag=$PRODTAG — see $LOG" > "$OUT/LATEST"
  if command -v gh > /dev/null 2>&1 && gh auth status > /dev/null 2>&1; then
    gh issue create --repo gjovanov/roomler-ai \
      --title "e2e nightly regression ($STAMP)" \
      --body "$(printf 'Image: %s\nSummary: %s\n\nUnexpected failures:\n```\n%s```\n' "$PRODTAG" "$SUMMARY" "$UNEXPECTED")" \
      >> "$LOG" 2>&1 || note "gh issue creation failed"
  fi
  exit 1
fi

echo "$STAMP OK ($SUMMARY) tag=$PRODTAG rc=$RC" > "$OUT/LATEST"
note "OK ($SUMMARY)"
# Keep the last 14 logs.
ls -1t "$OUT"/2*.log 2>/dev/null | tail -n +15 | xargs -r rm -f
