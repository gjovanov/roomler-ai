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

echo
echo "Installed $DST"
echo
echo "Next steps:"
echo "  1. Pin the image tag in $DST/kustomization.yaml (currently set to"
echo "     a hardcoded value; bump to the current prod tag)."
echo "  2. cd $DEPLOY_REPO && git add k8s/overlays/e2e && git commit -m 'feat(e2e): overlay'"
echo "  3. (Optional) git push  — ArgoCD does NOT watch this overlay."
echo "  4. kubectl apply -k $DST/"
echo "  5. bash scripts/e2e-k8s.sh smoke"
