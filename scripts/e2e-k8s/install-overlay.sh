#!/bin/bash
# One-shot installer that copies the e2e overlay template into the
# deploy repo on mars. Run from the local repo root on mars:
#   bash scripts/e2e-k8s/install-overlay.sh
# Then commit + push the deploy repo if you want gitops to track it
# (recommended: don't auto-sync via ArgoCD — keep manual-apply).
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
DEPLOY_REPO="${DEPLOY_REPO:-/home/gjovanov/roomler-ai-deploy}"
DST="$DEPLOY_REPO/k8s/overlays/e2e"

if [ ! -d "$DEPLOY_REPO" ]; then
  echo "deploy repo not found at $DEPLOY_REPO" >&2
  exit 1
fi

mkdir -p "$DST"
cp -v "$REPO_ROOT/scripts/e2e-k8s/overlay-template/"*.yaml "$DST/"

# Auto-track the current prod image tag — the e2e overlay should
# always validate the same image that's live on roomler.ai (per the
# user's "pin to current prod tag" decision in the plan). Read the
# tag from the prod overlay's kustomization.yaml and substitute it
# into the freshly-copied e2e kustomization.yaml. Idempotent — re-
# running the installer bumps the tag in place.
PROD_TAG=$(awk '/newTag:/ { print $2; exit }' "$DEPLOY_REPO/k8s/overlays/prod/kustomization.yaml")
if [ -n "$PROD_TAG" ]; then
  sed -i "s|newTag:.*|newTag: $PROD_TAG|" "$DST/kustomization.yaml"
  echo "Pinned e2e overlay to prod tag: $PROD_TAG"
else
  echo "WARNING: could not auto-detect prod tag; edit $DST/kustomization.yaml manually" >&2
fi

# Copy regcred from the prod namespace if not already present in
# roomler-ai-e2e. Without it the roomler2 deployment can't pull from
# `registry.roomler.ai`. Idempotent — `kubectl apply` is safe to
# re-run; `2>/dev/null || true` swallows the no-op case.
if kubectl get namespace roomler-ai-e2e >/dev/null 2>&1; then
  if ! kubectl -n roomler-ai-e2e get secret regcred >/dev/null 2>&1; then
    if kubectl -n roomler-ai get secret regcred >/dev/null 2>&1; then
      echo "Copying regcred secret from roomler-ai to roomler-ai-e2e"
      kubectl -n roomler-ai get secret regcred -o yaml \
        | sed 's/namespace: roomler-ai$/namespace: roomler-ai-e2e/' \
        | kubectl apply -f - >/dev/null
    else
      echo "WARNING: regcred missing in roomler-ai too; image pulls will fail" >&2
    fi
  fi
fi

echo
echo "Installed $DST"
echo
echo "Next steps:"
echo "  1. cd $DEPLOY_REPO && git add k8s/overlays/e2e && git commit -m 'feat(e2e): overlay'"
echo "  2. (Optional) git push  — ArgoCD does NOT watch this overlay."
echo "  3. kubectl apply -k $DST/"
echo "  4. bash scripts/e2e-k8s.sh smoke"
