#!/bin/bash
# Phase D V-D1 spike runner (THROWAWAY — spike branch only, not for merge).
# Runs the live single-relay test against production coturn from a fleet host.
# Reads coturn's static-auth-secret from k8s-cluster-multi/.env at runtime — the
# secret is NEVER stored in this script or echoed.
set -euo pipefail
cd "$(dirname "$0")/.."
S=$(grep '^COTURN_AUTH_SECRET=' ~/k8s-cluster-multi/.env | cut -d= -f2- | tr -d '"')
if [ -z "$S" ]; then echo "COTURN_AUTH_SECRET not found in ~/k8s-cluster-multi/.env" >&2; exit 1; fi
export ROOMLER_TEST_TURN_HOST=coturn.roomler.ai
export ROOMLER_TEST_TURN_SECRET="$S"
exec cargo test -p roomler-ai-tunnel-core --features overlay-l3 \
  single_relay_against_real_coturn_udp -- --ignored --nocapture
