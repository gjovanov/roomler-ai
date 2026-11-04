#!/usr/bin/env bash
# scripts/aws-testbox/down.sh — remove EXACTLY what up.sh created (the ids in the state file), in dependency
# order, then sweep the region for anything still carrying this box's tag and say what is left. Exits 0 only
# when nothing is left. The run's SSH key goes with it; the state file is kept, renamed, as the record of ids.
#
#   down.sh           delete what the state file lists, then report what the tag sweep still finds
#   down.sh --sweep   also delete whatever the tag sweep finds (a lost or partial state file)
#
# Env: AWS_PROFILE, AWS_REGION and TESTBOX_NAME as for up.sh.
set -uo pipefail
D="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$D/lib.sh"

SWEEP=0
case "${1:-}" in
  --sweep) SWEEP=1 ;;
  "") ;;
  *) echo "usage: down.sh [--sweep]" >&2; exit 2 ;;
esac
load_state

# retry N CMD… — EC2 releases a terminated instance's network interface a little after the instance reads
# "terminated", and a security group, subnet or VPC refuses deletion until then (DependencyViolation).
retry() {
  local n="$1" out; shift
  for _ in $(seq 1 "$n"); do
    out="$("$@" 2>&1)" && return 0
    case "$out" in
      *NotFound*) return 0 ;;
      *DependencyViolation*) sleep 10 ;;
      *) log "$*: $out"; return 1 ;;
    esac
  done
  log "$*: still $out"
  return 1
}

terminate() {
  [ $# -gt 0 ] || return 0
  aws ec2 terminate-instances --instance-ids "$@" >/dev/null 2>&1 && log "terminating $*"
  aws ec2 wait instance-terminated --instance-ids "$@" && log "terminated $*"
}

delete_igw() { # igw [vpc]
  local vpc="${2:-}"
  [ -n "$vpc" ] || vpc="$(aws ec2 describe-internet-gateways --internet-gateway-ids "$1" \
    --query 'InternetGateways[0].Attachments[0].VpcId' --output text 2>/dev/null)"
  if [ -n "$vpc" ] && [ "$vpc" != None ]; then
    aws ec2 detach-internet-gateway --internet-gateway-id "$1" --vpc-id "$vpc" >/dev/null 2>&1
  fi
  retry 12 aws ec2 delete-internet-gateway --internet-gateway-id "$1" && log "deleted internet gateway $1"
}

# ── 1. what the state file lists ────────────────────────────────────────────────────────────────────────────
[ -n "${INSTANCE:-}" ] && terminate "$INSTANCE"
[ -n "${SG:-}" ] && retry 18 aws ec2 delete-security-group --group-id "$SG" && log "deleted security group $SG"
[ -n "${IGW:-}" ] && delete_igw "$IGW" "${VPC:-}"
[ -n "${SUBNET:-}" ] && retry 12 aws ec2 delete-subnet --subnet-id "$SUBNET" && log "deleted subnet $SUBNET"
[ -n "${VPC:-}" ] && retry 12 aws ec2 delete-vpc --vpc-id "$VPC" && log "deleted vpc $VPC"

# ── 2. the tag sweep ────────────────────────────────────────────────────────────────────────────────────────
TAGF="Name=tag:$TB_TAG_KEY,Values=$TB_NAME"
q() { aws ec2 "$@" --output text 2>/dev/null | tr '\t' ' ' | sed 's/ *$//'; }
sweep() {
  L_I="$(q describe-instances --filters "$TAGF" Name=instance-state-name,Values=pending,running,shutting-down,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId')"
  L_V="$(q describe-volumes --filters "$TAGF" --query 'Volumes[].VolumeId')"
  L_S="$(q describe-security-groups --filters "$TAGF" --query 'SecurityGroups[].GroupId')"
  L_G="$(q describe-internet-gateways --filters "$TAGF" --query 'InternetGateways[].InternetGatewayId')"
  L_N="$(q describe-subnets --filters "$TAGF" --query 'Subnets[].SubnetId')"
  L_P="$(q describe-vpcs --filters "$TAGF" --query 'Vpcs[].VpcId')"
}
sweep
if [ "$SWEEP" = 1 ]; then
  # shellcheck disable=SC2086
  terminate $L_I
  for v in $L_V; do retry 12 aws ec2 delete-volume --volume-id "$v" && log "deleted volume $v"; done
  for s in $L_S; do retry 18 aws ec2 delete-security-group --group-id "$s" && log "deleted security group $s"; done
  for g in $L_G; do delete_igw "$g"; done
  for n in $L_N; do retry 12 aws ec2 delete-subnet --subnet-id "$n" && log "deleted subnet $n"; done
  for p in $L_P; do retry 12 aws ec2 delete-vpc --vpc-id "$p" && log "deleted vpc $p"; done
  sweep
fi
# A root volume is deleted with its instance, a few seconds after "terminated": look again before calling it left.
if [ -n "$L_V" ] && [ -z "$L_I" ]; then sleep 15; sweep; fi

LEFT="$L_I $L_V $L_S $L_G $L_N $L_P"
LEFT="$(printf '%s' "$LEFT" | tr -s ' ' | sed 's/^ //; s/ $//')"

# ── 3. the local half: the run's key goes, the ids stay on record ───────────────────────────────────────────
if [ -z "$LEFT" ]; then
  rm -f "$TB_STATE_DIR/id_ed25519" "$TB_STATE_DIR/id_ed25519.pub" "$TB_STATE_DIR/known_hosts" "$TB_STATE_DIR/user-data.yaml"
  [ -f "$ST" ] && mv "$ST" "$ST.done.$(date -u +%Y%m%dT%H%M%SZ)"
  echo "DOWN OK box=$TB_NAME: nothing tagged $TB_TAG_KEY=$TB_NAME is left in $AWS_REGION"
  exit 0
fi
echo "DOWN INCOMPLETE box=$TB_NAME, still tagged in $AWS_REGION: $LEFT"
[ "$SWEEP" = 1 ] || echo "  re-run with --sweep to delete those by tag"
exit 1
