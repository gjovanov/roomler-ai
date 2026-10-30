#!/usr/bin/env bash
# scripts/aws-testbox/up.sh — provision ONE throwaway EC2 test box for a field test that needs hardware the lab
# does not have (FR-122: arm64 Linux with an NVIDIA encoder, a g5g). The box gets a VPC of its own (subnet,
# internet gateway, a security group open on tcp/22 to the listed sources only) and powers itself off at a
# deadline, which terminates it, whatever happens to the machine that started it. Idempotent: a re-run resumes
# from the state file. down.sh removes everything this made.
#
# Env (AWS_PROFILE and AWS_REGION are required; see lib.sh):
#   TESTBOX_NAME         state dir + tag value (default testbox)
#   TESTBOX_TYPE         instance type (default g5g.xlarge: Graviton2 + NVIDIA T4G)
#   TESTBOX_AMI          nvidia | ubuntu | ami-… (default: nvidia on a GPU type, ubuntu otherwise).
#                        nvidia = AWS's "Deep Learning Base OSS Nvidia Driver GPU AMI" (driver, NVENC libraries
#                        included), ubuntu = Canonical's Ubuntu 24.04 server image
#   TESTBOX_TTL_MIN      minutes until the box powers itself off and is terminated (default 240)
#   TESTBOX_DISK_GB      root volume size, raised to the image's own when that is bigger (default 40)
#   TESTBOX_SSH_CIDRS    comma-separated sources allowed on tcp/22 (default: this machine's public IPv4 /32)
#   TESTBOX_SSH_PUBKEYS  a file of extra public keys to authorise, one per line (a second orchestrator's)
#   TESTBOX_SPOT=1       a one-time spot instance instead of on-demand (its own vCPU quota)
#   TESTBOX_CIDR         the VPC's block (default 10.199.0.0/24; the subnet is its lower half)
#   TESTBOX_PURPOSE      free text for the purpose tag
set -uo pipefail
D="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$D/lib.sh"

TYPE="${TESTBOX_TYPE:-g5g.xlarge}"
TTL_MIN="${TESTBOX_TTL_MIN:-240}"
DISK_GB="${TESTBOX_DISK_GB:-40}"
CIDR="${TESTBOX_CIDR:-10.199.0.0/24}"
case "$TTL_MIN" in *[!0-9]* | "") die "TESTBOX_TTL_MIN must be whole minutes" ;; esac
[ "$TTL_MIN" -ge 15 ] && [ "$TTL_MIN" -le 720 ] || die "TESTBOX_TTL_MIN outside 15..720"

mkdir -p "$TB_STATE_DIR" || die "cannot create $TB_STATE_DIR"
chmod 700 "$TB_STATE_DIR" 2>/dev/null
touch "$ST"
load_state

WHO="$(aws sts get-caller-identity --query '[Account,Arn]' --output text 2>&1)" || die "no credentials for profile $AWS_PROFILE: $WHO"
log "account/caller: $WHO · region $AWS_REGION · box $TB_NAME · state $TB_STATE_DIR"

# ── what the type needs: architecture, vCPUs, GPU, an AZ that offers it, and the vCPU quota ──────────────────
read -r ARCH VCPUS GPU <<<"$(aws ec2 describe-instance-types --instance-types "$TYPE" \
  --query 'InstanceTypes[0].[ProcessorInfo.SupportedArchitectures[0],VCpuInfo.DefaultVCpus,GpuInfo.Gpus[0].Name]' \
  --output text 2>/dev/null)"
[ -n "${ARCH:-}" ] && [ "$ARCH" != None ] || die "unknown instance type $TYPE in $AWS_REGION"
AZ="$(aws ec2 describe-instance-type-offerings --location-type availability-zone \
  --filters Name=instance-type,Values="$TYPE" --query 'sort(InstanceTypeOfferings[].Location)[0]' --output text 2>&1)"
[ -n "$AZ" ] && [ "$AZ" != None ] || die "$TYPE is not offered in any availability zone of $AWS_REGION ($AZ)"
log "type $TYPE: $ARCH, $VCPUS vCPU, GPU ${GPU:-none}; zone $AZ"

case "$TYPE" in
  g* | vt*) QCODE="$([ "${TESTBOX_SPOT:-0}" = 1 ] && echo L-3819A6DF || echo L-DB2E81BA)"; QNAME="G and VT" ;;
  [acdhimrtz]*) QCODE="$([ "${TESTBOX_SPOT:-0}" = 1 ] && echo L-34B43A08 || echo L-1216C47A)"; QNAME="Standard" ;;
  *) QCODE="" ;;
esac
if [ -n "$QCODE" ]; then
  QUOTA="$(aws service-quotas get-service-quota --service-code ec2 --quota-code "$QCODE" --query 'Quota.Value' --output text 2>/dev/null)"
  QUOTA="${QUOTA%%.*}"
  case "$QUOTA" in
    '' | *[!0-9]*) log "could not read the $QNAME vCPU quota ($QCODE); trying anyway" ;;
    *) [ "$QUOTA" -ge "$VCPUS" ] || die "the $QNAME vCPU quota ($QCODE) is $QUOTA here, $TYPE needs $VCPUS. Ask for more:
  aws service-quotas request-service-quota-increase --service-code ec2 --quota-code $QCODE --desired-value $VCPUS"
       log "quota $QNAME ($QCODE): $QUOTA vCPU" ;;
  esac
fi

# ── the image ───────────────────────────────────────────────────────────────────────────────────────────────
AMI_KIND="${TESTBOX_AMI:-$([ -n "${GPU:-}" ] && [ "$GPU" != None ] && echo nvidia || echo ubuntu)}"
newest() { # owner name-pattern…: the newest image matching the first pattern that matches anything
  local owner="$1" pat id; shift
  for pat in "$@"; do
    id="$(aws ec2 describe-images --owners "$owner" --filters "Name=name,Values=$pat" "Name=architecture,Values=$ARCH" \
      Name=state,Values=available --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text 2>/dev/null)"
    [ -n "$id" ] && [ "$id" != None ] && { echo "$id"; return 0; }
  done
  return 1
}
if [ -z "${AMI:-}" ]; then
  case "$AMI_KIND" in
    ami-*) A="$AMI_KIND" ;;
    ubuntu) A="$(newest 099720109477 "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-$([ "$ARCH" = arm64 ] && echo arm64 || echo amd64)-server-*")" ;;
    nvidia)
      P="Deep Learning $([ "$ARCH" = arm64 ] && echo 'ARM64 ')Base OSS Nvidia Driver GPU AMI"
      # Ubuntu 24.04 first, the release the rig is rehearsed on: plain where the region has it, else Pro, whose
      # licence bills a few cents an hour. Then the newer and the older LTS.
      A="$(newest amazon "$P (Ubuntu 24.04) *" "$P (Ubuntu Pro 24.04) *" "$P (Ubuntu 26.04) *" \
        "$P (Ubuntu 22.04) *" "$P (Ubuntu Pro 22.04) *")" ;;
    *) die "TESTBOX_AMI must be nvidia, ubuntu or an ami- id" ;;
  esac
  [ -n "${A:-}" ] || die "no $AMI_KIND image for $ARCH in $AWS_REGION"
  save AMI "$A"
fi
AMI_NAME="$(aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].Name' --output text)"
ROOT_DEV="$(aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].RootDeviceName' --output text)"
[ -n "$ROOT_DEV" ] && [ "$ROOT_DEV" != None ] || die "cannot describe $AMI"
ROOT_GB="$(aws ec2 describe-images --image-ids "$AMI" \
  --query "Images[0].BlockDeviceMappings[?DeviceName=='$ROOT_DEV'].Ebs.VolumeSize | [0]" --output text)"
[ "${ROOT_GB:-0}" -gt "$DISK_GB" ] 2>/dev/null && DISK_GB="$ROOT_GB"
log "image $AMI ($AMI_NAME), root $ROOT_DEV ${DISK_GB} GB"

# ── the run's own SSH key, and the deadline ─────────────────────────────────────────────────────────────────
if [ ! -f "$TB_STATE_DIR/id_ed25519" ]; then
  ssh-keygen -q -t ed25519 -N '' -C "roomler-testbox-$TB_NAME" -f "$TB_STATE_DIR/id_ed25519" || die ssh-keygen
fi
if [ -z "${EXPIRES:-}" ]; then
  E="$(date -u -d "+$TTL_MIN minutes" '+%Y-%m-%dT%H:%MZ' 2>/dev/null || date -u -v+"$TTL_MIN"M '+%Y-%m-%dT%H:%MZ')"
  save EXPIRES "$E"
fi
ONCAL="$(printf '%s' "$EXPIRES" | sed -E 's/T/ /; s/Z$/:00 UTC/')"

# ── network: VPC, subnet, internet gateway, default route, security group ──────────────────────────────────
if [ -z "${VPC:-}" ]; then
  V="$(aws ec2 create-vpc --cidr-block "$CIDR" --tag-specifications "$(spec vpc)" --query Vpc.VpcId --output text)" || die create-vpc
  save VPC "$V"
fi
aws ec2 wait vpc-available --vpc-ids "$VPC" || die "vpc $VPC never became available"
if [ -z "${SUBNET:-}" ]; then
  SUB_CIDR="$(printf '%s' "$CIDR" | awk -F/ '{print $1 "/" ($2 + 1)}')"
  S="$(aws ec2 create-subnet --vpc-id "$VPC" --cidr-block "$SUB_CIDR" --availability-zone "$AZ" \
    --tag-specifications "$(spec subnet)" --query Subnet.SubnetId --output text)" || die create-subnet
  save SUBNET "$S"
fi
aws ec2 modify-subnet-attribute --subnet-id "$SUBNET" --map-public-ip-on-launch || die "map public ip on $SUBNET"
if [ -z "${IGW:-}" ]; then
  G="$(aws ec2 create-internet-gateway --tag-specifications "$(spec internet-gateway)" \
    --query InternetGateway.InternetGatewayId --output text)" || die create-internet-gateway
  save IGW "$G"
fi
ATTACHED="$(aws ec2 describe-internet-gateways --internet-gateway-ids "$IGW" --query 'InternetGateways[0].Attachments[0].VpcId' --output text)"
[ "$ATTACHED" = "$VPC" ] || aws ec2 attach-internet-gateway --internet-gateway-id "$IGW" --vpc-id "$VPC" || die "attach $IGW"
RT="$(aws ec2 describe-route-tables --filters Name=vpc-id,Values="$VPC" Name=association.main,Values=true \
  --query 'RouteTables[0].RouteTableId' --output text)"
HAS_DEFAULT="$(aws ec2 describe-route-tables --route-table-ids "$RT" \
  --query "RouteTables[0].Routes[?DestinationCidrBlock=='0.0.0.0/0'].GatewayId | [0]" --output text)"
[ "$HAS_DEFAULT" = "$IGW" ] || aws ec2 create-route --route-table-id "$RT" --destination-cidr-block 0.0.0.0/0 \
  --gateway-id "$IGW" >/dev/null || die "default route via $IGW"
if [ -z "${SG:-}" ]; then
  G="$(aws ec2 create-security-group --group-name "roomler-testbox-$TB_NAME" --vpc-id "$VPC" \
    --description "roomler testbox $TB_NAME: ssh from the listed sources only" \
    --tag-specifications "$(spec security-group)" --query GroupId --output text)" || die create-security-group
  save SG "$G"
fi
CIDRS="${TESTBOX_SSH_CIDRS:-}"
if [ -z "$CIDRS" ]; then
  MYIP="$(curl -fsS --max-time 10 https://checkip.amazonaws.com | tr -d '[:space:]')"
  [ -n "$MYIP" ] || die "cannot find this machine's public IP; set TESTBOX_SSH_CIDRS"
  CIDRS="$MYIP/32"
fi
IFS=, read -r -a CIDR_LIST <<<"$CIDRS"
for c in "${CIDR_LIST[@]}"; do
  c="$(printf '%s' "$c" | tr -d '[:space:]')"; [ -n "$c" ] || continue
  OUT="$(aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$c" 2>&1)" \
    && log "ssh allowed from $c" \
    || { printf '%s' "$OUT" | grep -q InvalidPermission.Duplicate && log "ssh already allowed from $c" || die "ingress $c: $OUT"; }
done

# ── the instance ────────────────────────────────────────────────────────────────────────────────────────────
if [ -z "${INSTANCE:-}" ]; then
  UD="$TB_STATE_DIR/user-data.yaml"
  {
    echo '#cloud-config'
    echo 'ssh_authorized_keys:'
    sed 's/^/  - /' "$TB_STATE_DIR/id_ed25519.pub"
    if [ -n "${TESTBOX_SSH_PUBKEYS:-}" ]; then
      grep -E '^(ssh-|ecdsa-)' "$TESTBOX_SSH_PUBKEYS" | sed 's/^/  - /' || die "no public keys in $TESTBOX_SSH_PUBKEYS"
    fi
    cat <<EOF
write_files:
  - path: /etc/systemd/system/testbox-ttl.service
    content: |
      [Unit]
      Description=roomler testbox: power off at the deadline (the instance then terminates)
      [Service]
      Type=oneshot
      ExecStart=/usr/bin/systemctl poweroff
  - path: /etc/systemd/system/testbox-ttl.timer
    content: |
      [Unit]
      Description=roomler testbox deadline $EXPIRES
      [Timer]
      OnCalendar=$ONCAL
      Persistent=true
      [Install]
      WantedBy=timers.target
runcmd:
  - [systemctl, daemon-reload]
  - [systemctl, enable, --now, testbox-ttl.timer]
EOF
  } > "$UD"
  UDPATH="$UD"
  command -v cygpath >/dev/null 2>&1 && UDPATH="$(cygpath -m "$UD")"
  MARKET=()
  [ "${TESTBOX_SPOT:-0}" = 1 ] && MARKET=(--instance-market-options \
    'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}')
  I="$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --count 1 \
    --subnet-id "$SUBNET" --security-group-ids "$SG" \
    --instance-initiated-shutdown-behavior terminate \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --block-device-mappings "DeviceName=$ROOT_DEV,Ebs={VolumeSize=$DISK_GB,VolumeType=gp3,DeleteOnTermination=true,Encrypted=true}" \
    --user-data "file://$UDPATH" "${MARKET[@]}" \
    --tag-specifications "$(spec instance)" "$(spec volume)" \
    --query 'Instances[0].InstanceId' --output text)" || die run-instances
  save INSTANCE "$I"
fi
aws ec2 wait instance-running --instance-ids "$INSTANCE" || die "$INSTANCE never reached running"
NEWIP="$(aws ec2 describe-instances --instance-ids "$INSTANCE" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)"
[ -n "$NEWIP" ] && [ "$NEWIP" != None ] || die "$INSTANCE has no public IP"
if [ "${IP:-}" != "$NEWIP" ]; then
  grep -v '^IP=' "$ST" > "$ST.tmp"; mv "$ST.tmp" "$ST"
  save IP "$NEWIP"
fi

log "waiting for ssh on $IP …"
for _ in $(seq 1 60); do tb_ssh true 2>/dev/null && break; sleep 5; done
tb_ssh true 2>/dev/null || die "no ssh on $IP after 5 min"
tb_ssh 'sudo cloud-init status --wait >/dev/null 2>&1; cloud-init status; systemctl is-active testbox-ttl.timer; uname -m; . /etc/os-release; echo "$PRETTY_NAME"' \
  | sed 's/^/  box: /' >&2
echo "UP OK box=$TB_NAME instance=$INSTANCE type=$TYPE ip=$IP deadline=$EXPIRES"
echo "  ssh:  TESTBOX_NAME=$TB_NAME $D/ssh.sh"
echo "  down: TESTBOX_NAME=$TB_NAME $D/down.sh"
