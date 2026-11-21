#!/usr/bin/env bash
# The resolver-fault control of the ci-apt self-test (#3710), run ONLY on the
# self-test job's own throwaway GitHub-hosted runner (ci-apt-selftest.yml).
# Never on a dev box, a fleet host or anything that outlives the job: it adds
# iptables rules and nothing takes them away, because the runner is discarded.
#
# The fault is the one of 2026-11-11 (#3698): the runner's resolver stops
# answering for *.ubuntu.com while everything else keeps resolving. It drops DNS
# RESPONSES that carry a mirror name, in the raw table's PREROUTING (before
# conntrack, so a container's NATed answers are dropped too), for UDP and TCP
# source port 53, IPv4 and IPv6. It never matches anything but the mirror names:
# the runner's own connection to GitHub needs DNS.
#
#   ci-apt-fault.sh facts              the image, its apt sources, its resolver
#   ci-apt-fault.sh inject MODE        MODE: runner | all | ports (see below)
#   ci-apt-fault.sh prove MODE         fails unless the fault is in effect
#   ci-apt-fault.sh hits               fails unless the DROP rules matched packets
#   ci-apt-fault.sh source-hosts       the Ubuntu archive hosts apt's sources name,
#                                      read here independently of ci-apt.sh
#   ci-apt-fault.sh container-reach [DOCKER-RUN-ARG...]
#                                      does a container started now reach its
#                                      mirror (D2 of #3710)
#
#   runner  *.ubuntu.com, only from the runner's own upstream resolver(s)
#   all     *.ubuntu.com, from every source (no public resolver helps)
#   ports   ports.ubuntu.com only, from the upstream (the arm64 image's mirror)
set -uo pipefail

UBUNTU_COM='|06|ubuntu|03|com|'
PORTS_UBUNTU_COM='|05|ports|06|ubuntu|03|com|'

say() { printf 'ci-apt-fault: %s\n' "$*"; }
die() {
  printf '::error title=ci-apt self-test::%s\n' "$*"
  exit 1
}

default_link() {
  ip -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1
}

# Every resolver systemd-resolved forwards to: the default link's, then the
# global ones. Read from the runner, never hard-coded.
upstreams() {
  local link
  link=$(default_link)
  { resolvectl dns "$link" 2>/dev/null; resolvectl dns 2>/dev/null; } |
    sed 's/^[^:]*: *//' | tr ' ' '\n' | grep -E '^[0-9a-fA-F.:]+$' | grep -v '^$' | sort -u
}

cmd_facts() {
  local f
  echo "::group::image facts"
  say "ImageOS=${ImageOS:-?} ImageVersion=${ImageVersion:-?} arch=$(uname -m) runner=${RUNNER_NAME:-?}"
  grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release
  for f in /etc/apt/sources.list /etc/apt/sources.list.d/* /etc/apt/apt-mirrors.txt; do
    [ -f "$f" ] || continue
    echo "----- $f"
    cat "$f"
  done
  echo "----- tools"
  for f in resolvectl dig ip getent iptables ip6tables docker; do
    printf '%-10s %s\n' "$f" "$(command -v "$f" || echo MISSING)"
  done
  echo "----- resolvectl status"
  resolvectl status 2>&1 | head -40
  echo "----- /etc/resolv.conf ($(readlink /etc/resolv.conf || echo 'a file, not a link'))"
  cat /etc/resolv.conf
  echo "----- /run/systemd/resolve/resolv.conf"
  cat /run/systemd/resolve/resolv.conf 2>&1
  echo "----- upstreams: $(upstreams | tr '\n' ' ')"
  echo "::endgroup::"
}

drop() { # <iptables|ip6tables> <hex> [source]
  local t=$1 hex=$2 src=${3:-} p
  for p in udp tcp; do
    if [ -n "$src" ]; then
      sudo "$t" -t raw -I PREROUTING -p "$p" --sport 53 -s "$src" -m string --algo bm --hex-string "$hex" -j DROP
    else
      sudo "$t" -t raw -I PREROUTING -p "$p" --sport 53 -m string --algo bm --hex-string "$hex" -j DROP
    fi || die "$t could not add the $p rule (is xt_string missing on this image?): the fault did not apply, so this cell cannot count"
  done
}

cmd_inject() {
  local mode=${1:-} hex=$UBUNTU_COM u ups
  case "$mode" in
    runner | ports)
      [ "$mode" = ports ] && hex=$PORTS_UBUNTU_COM
      ups=$(upstreams)
      [ -n "$ups" ] || die "no upstream resolver found in resolvectl: cannot aim the fault"
      for u in $ups; do
        case "$u" in
          *:*) drop ip6tables "$hex" "$u" ;;
          *) drop iptables "$hex" "$u" ;;
        esac
        say "dropping DNS responses matching $hex from $u"
      done
      ;;
    all)
      drop iptables "$hex"
      drop ip6tables "$hex"
      say "dropping DNS responses matching $hex from every source"
      ;;
    *) die "inject: unknown mode '$mode'" ;;
  esac
  sudo resolvectl flush-caches
  sudo iptables -t raw -S PREROUTING
}

resolves() { timeout 25 getent ahostsv4 "$1" >/dev/null 2>&1; }
answers() { timeout 10 dig +time=3 +tries=1 +short A "$1" "@$2" 2>/dev/null | grep -qE '^[0-9]+(\.[0-9]+){3}$'; }

cmd_prove() {
  local mode=${1:-}
  sudo resolvectl flush-caches
  resolves github.com || die "github.com does not resolve: the fault reaches beyond the mirror names"
  case "$mode" in
    runner)
      ! resolves archive.ubuntu.com || die "archive.ubuntu.com still resolves through the system resolver: the fault did not apply"
      ! resolves ports.ubuntu.com || die "ports.ubuntu.com still resolves through the system resolver: the fault did not apply"
      answers archive.ubuntu.com 1.1.1.1 || die "1.1.1.1 does not answer archive.ubuntu.com: this is not the runner-only fault"
      ;;
    all)
      ! resolves archive.ubuntu.com || die "archive.ubuntu.com still resolves: the fault did not apply"
      ! answers archive.ubuntu.com 1.1.1.1 || die "1.1.1.1 still answers archive.ubuntu.com: the fault did not apply to public resolvers"
      ! answers archive.ubuntu.com 8.8.8.8 || die "8.8.8.8 still answers archive.ubuntu.com: the fault did not apply to public resolvers"
      ;;
    ports)
      ! resolves ports.ubuntu.com || die "ports.ubuntu.com still resolves: the fault did not apply"
      resolves archive.ubuntu.com || die "archive.ubuntu.com does not resolve: the fault is wider than ports.ubuntu.com"
      ;;
    *) die "prove: unknown mode '$mode'" ;;
  esac
  sudo resolvectl flush-caches
  say "fault '$mode' is in effect (github.com resolves; the mirror names do not, as intended)"
}

# Proof, at the end of a cell, that the fault was hit while the cell ran.
cmd_hits() {
  local n
  sudo iptables -t raw -L PREROUTING -v -n -x
  n=$(sudo iptables -t raw -L PREROUTING -v -n -x | awk '$3 == "DROP" {s += $1} END {print s + 0}')
  sudo ip6tables -t raw -L PREROUTING -v -n -x 2>/dev/null | awk '$3 == "DROP"' || true
  [ "$n" -gt 0 ] || die "the DROP rules matched no packet: nothing in this cell met the fault"
  say "the DROP rules matched $n packets"
}

# The Ubuntu archive hosts apt's sources name: one-line and deb822 sources, and
# the file a mirror+file: URI names. Deliberately simpler than ci-apt.sh's own
# reading, so the two can disagree.
cmd_source_hosts() {
  local f m
  {
    for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
      [ -f "$f" ] || continue
      grep -hoE '(https?|mirror\+file):[^ ]+' "$f"
    done
  } | while read -r m; do
    case "$m" in
      mirror+file:*) grep -hoE '^https?://[^/[:space:]]+' "${m#mirror+file:}" ;;
      *) printf '%s\n' "$m" ;;
    esac
  done | sed -E 's#^[a-z+]+://##; s#[/:].*##' | grep -E '(^|\.)(archive|ports|security)\.ubuntu\.com$' | sort -u
}

# Whether a container started NOW reaches its mirror: its resolv.conf, and its
# own `apt-get update`, judged by rc and output (a timeout prints nothing).
# Extra arguments go to `docker run` (e.g. --dns 1.1.1.1). Prints REACHED=yes|no.
cmd_container_reach() {
  docker run --rm "$@" ubuntu:24.04 sh -c '
    echo "--- the container'"'"'s resolv.conf"; grep -v "^#" /etc/resolv.conf
    timeout -k 10 90 apt-get update -qq >/tmp/u 2>&1; rc=$?
    tail -3 /tmp/u; echo "container apt-get update rc=$rc"
    if [ "$rc" -ne 0 ] || grep -qE "Temporary failure resolving|Could not resolve|Failed to fetch" /tmp/u; then echo REACHED=no; else echo REACHED=yes; fi'
}

case "${1:-}" in
  facts) cmd_facts ;;
  container-reach)
    shift
    cmd_container_reach "$@"
    ;;
  inject) cmd_inject "${2:-}" ;;
  prove) cmd_prove "${2:-}" ;;
  hits) cmd_hits ;;
  source-hosts) cmd_source_hosts ;;
  *) die "usage: ci-apt-fault.sh facts | inject MODE | prove MODE | hits | source-hosts | container-reach [ARG...]" ;;
esac
