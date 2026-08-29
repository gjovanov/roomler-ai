#!/usr/bin/env bash
#
# peer-relay-port-audit-cron.sh — FR-19 weekly relay-firewall drift audit.
#
# Pushes the canonical peer-relay-port-audit.sh to each relay-serving node and
# runs `explain`, then files a GitHub issue on drift. Mirrors the deploy repo's
# mediasoup-rtc-forwarding-audit.sh, but over **Fleet RPC (`roomler exec`)**
# instead of ssh/scp: a relay host such as scw-m2-asahi is a Scaleway box that
# is NOT on the cluster SSH-CA, so the mesh's own control plane is the reach.
#
# Why: the relay port rule is host-local firewall state invisible to ArgoCD and
# the app. When it drifts (closed, or DNAT-stolen — the mars class), the relay
# binds a socket and answers nothing: members fall back to DERP/TURN and the pod
# is never offloaded, silently. This audit is the guard.
#
# Requires: an authed `roomler` CLI (a user token with EXEC_DEVICE on the org)
# and `gh`. ⚠️ Its production home must have BOTH. mars — where the mediasoup
# cron lives — cannot host it yet: its `roomler` is a LocalAPI client (daemon
# socket), not a user-authed Fleet-RPC caller, and the relay hosts are off the
# SSH-CA. Until an authed ops host is provisioned, run it from an operator
# context. Prove the fire path without issue churn: `DRY_RUN=1 PEER_RELAY_PORT=3479`.
#
# Env: PEER_RELAY_PORT (default 3478), PEER_RELAY_AGENTS ("label=agent-id …"),
#      DRY_RUN=1 (log "WOULD file issue" instead of creating one).

set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
CANON="$DIR/peer-relay-port-audit.sh"
PORT="${PEER_RELAY_PORT:-3478}"
DRY="${DRY_RUN:-0}"
# Relay-serving agents: label=agent-id. Expand this as relays are approved.
AGENTS="${PEER_RELAY_AGENTS:-scw-m2-asahi=6a7f91d64f1248bba31904ce}"
LOG_DIR="${LOG_DIR:-$HOME/peer-relay-port-audit}"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/latest.log"
: > "$LOG"

[ -r "$CANON" ] || { echo "FATAL: canonical check not found at $CANON" | tee -a "$LOG"; exit 3; }
B64="$(base64 -w0 "$CANON")"

FAILED=""
for entry in $AGENTS; do
  name="${entry%%=*}"
  aid="${entry#*=}"
  echo "=== $name ($aid) port=$PORT $(date -u +%FT%TZ) ===" >> "$LOG"
  # Push the check over the control WS, run it as root, clean up, propagate rc.
  if ! roomler exec "$aid" \
        "printf '%s' '$B64' | base64 -d > /tmp/pra.sh && sudo bash /tmp/pra.sh explain $PORT; rc=\$?; rm -f /tmp/pra.sh; exit \$rc" \
        >> "$LOG" 2>&1; then
    FAILED="$FAILED $name(drift)"
  fi
done

if [ -n "$FAILED" ]; then
  echo "AUDIT FAILED:$FAILED" >> "$LOG"
  TITLE="FR-19 peer-relay port drift:$FAILED"
  BODY="$(printf 'Weekly audit: the UDP relay port %s is not admitted by a SCOPED rule (or is DNAT-consumed) on:%s\n\nA relay there binds a socket and answers nothing — members fall back to DERP/TURN and the API pod is not offloaded, silently.\n\nFix: open a scoped `%s/udp` rule with no DNAT stealing it. Check: `scripts/peer-relay-port-audit.sh`.\n\nLog tail:\n```\n%s\n```' "$PORT" "$FAILED" "$PORT" "$(tail -40 "$LOG")")"
  if [ "$DRY" = 1 ]; then
    echo "[DRY_RUN] WOULD file GitHub issue: $TITLE" | tee -a "$LOG"
  elif command -v gh > /dev/null 2>&1 && gh auth status > /dev/null 2>&1; then
    gh issue create --repo gjovanov/roomler-ai --title "$TITLE" --body "$BODY" >> "$LOG" 2>&1 \
      || echo "gh issue creation failed" >> "$LOG"
  else
    echo "gh unavailable — drift NOT reported" >> "$LOG"
  fi
  exit 1
fi
echo "AUDIT OK $(date -u +%FT%TZ)" >> "$LOG"
