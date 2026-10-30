#!/usr/bin/env bash
# scripts/aws-testbox/lib.sh — sourced by up.sh, down.sh and ssh.sh. Never run on its own.
#
# One throwaway EC2 box per TESTBOX_NAME, in a VPC of its own. Every resource id goes into
# $TB_STATE_DIR/state.env the moment it exists, so down.sh removes exactly what up.sh made, even
# after an interruption; every resource also carries the tag roomler-testbox=<name>, so a lost
# state file is recoverable by `down.sh --sweep`.
#
# There are deliberately NO defaults for the account or the region: this repository is public, and
# a default would aim somebody's CLI at whatever account their default profile happens to be.

: "${AWS_PROFILE:?set AWS_PROFILE to a CLI profile for the test account (for example: aws login --profile <name>)}"
: "${AWS_REGION:?set AWS_REGION to the region to build in}"
export AWS_PROFILE AWS_REGION AWS_PAGER=""
# Git-Bash on Windows rewrites arguments that look like POSIX paths before aws.exe sees them;
# nothing passed below is a path, so switch the rewrite off.
export MSYS_NO_PATHCONV=1

TB_NAME="${TESTBOX_NAME:-testbox}"
case "$TB_NAME" in
  *[!a-z0-9-]* | "" | -*) echo "TESTBOX_NAME must be lowercase letters, digits and dashes: '$TB_NAME'" >&2; exit 2 ;;
esac
[ "${#TB_NAME}" -le 32 ] || { echo "TESTBOX_NAME longer than 32 characters" >&2; exit 2; }
TB_STATE_DIR="${TESTBOX_STATE_DIR:-$HOME/.roomler-testbox/$TB_NAME}"
ST="$TB_STATE_DIR/state.env"
TB_TAG_KEY=roomler-testbox
TB_USER="${TESTBOX_USER:-ubuntu}"

log() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*" >&2; }
die() { log "FAILED: $*"; exit 1; }

# save KEY VALUE — append to the state file at once and set it in this shell.
save() {
  printf '%s=%q\n' "$1" "$2" >> "$ST"
  printf -v "$1" '%s' "$2"
  log "saved $1=$2"
}

load_state() {
  # shellcheck disable=SC1090
  [ -f "$ST" ] && . "$ST"
  return 0
}

# The tag list every resource carries. EXPIRES is the box's own power-off deadline.
tags() {
  printf '{Key=Name,Value=roomler-testbox-%s},{Key=%s,Value=%s},{Key=purpose,Value=%s},{Key=expires,Value=%s},{Key=created-by,Value=roomler-scripts-aws-testbox}' \
    "$TB_NAME" "$TB_TAG_KEY" "$TB_NAME" "${TESTBOX_PURPOSE:-roomler field test}" "${EXPIRES:-unknown}"
}
spec() { printf 'ResourceType=%s,Tags=[%s]' "$1" "$(tags)"; }

# ssh to the box with the run's own key and known_hosts (never the operator's ~/.ssh).
tb_ssh() {
  ssh -i "$TB_STATE_DIR/id_ed25519" \
    -o IdentitiesOnly=yes -o BatchMode=yes \
    -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$TB_STATE_DIR/known_hosts" \
    -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 \
    "$TB_USER@${IP:?no IP in the state file}" "$@"
}
