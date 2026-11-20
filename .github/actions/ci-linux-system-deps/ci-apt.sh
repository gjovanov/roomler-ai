#!/usr/bin/env bash
# Every apt step of CI and the Linux release lanes, through one helper: reach
# the mirrors apt really uses, refresh the index, install, each phase BOUNDED
# INSIDE ITS OWN PROCESS TREE, because a step-level `timeout-minutes` kills
# nothing below the step and a composite step cannot carry one at all.
#
#   ci-apt.sh prepare [BOUND...]    reach the mirrors (fail over to a public
#                                   resolver if the runner's own cannot), refresh
#                                   the index, install the apt-fast stand-in
#   ci-apt.sh fallback PKG...       the composite's verify steps' install:
#                                   bounded, --no-install-recommends, one clear
#                                   ::error when the mirrors cannot be reached
#   ci-apt.sh install [BOUND...] [APT-OPTION...] -- PKG...
#                                   any other step's install: the same probe,
#                                   failover, `dpkg --configure -a`, bounded
#                                   refresh and bounded install, with exactly
#                                   the caller's apt options on both phases (no
#                                   option is added, so recommends stay wherever
#                                   the step had them)
#   ci-apt.sh reach [--containers] [HOST...]
#                                   the probe and failover alone, for a step
#                                   whose apt runs elsewhere (a container, or a
#                                   tool that calls apt-get itself); exits
#                                   non-zero with one ::error when the names do
#                                   not resolve, and prints the resolvers in use
#   apt-fast ...                    the stand-in, installed by `prepare` as
#                                   /usr/local/bin/apt-fast (same file, other name)
#
#   BOUND: --refresh-bound S, --download-bound S, --install-bound S (seconds),
#   for a lane whose step timeout or mirror needs other bounds than the
#   defaults below. On `prepare`, the download and install bounds are the
#   stand-in's: they are what the cached-apt action's cold path runs under.
#
# Why this exists (2026-11-11, #3698)
#
#   From ~13:45Z every `Rust lint (fmt, clippy)` job hung in the cached-apt step
#   until its ceiling (40 min), so no Rust PR could merge. The runners' resolver
#   (systemd-resolved's 127.0.0.53 stub, forwarding to Azure DNS 168.63.129.16)
#   stopped answering for *.ubuntu.com: `getent ahostsv4` failed after 10 s per
#   name and `dig @168.63.129.16` timed out, while `dig @1.1.1.1` and `@8.8.8.8`
#   answered the same names in 1-112 ms and github.com resolved everywhere.
#   Three things turned that into a 40-minute hang:
#
#   1. `apt-get update` exits 0 on a failed refresh (only `W:` lines, after
#      125-203 s), so the image's STALE index stayed in use, and the action
#      derives its cache key from the versions that index names: the GTK set
#      resolved to `c75f64db...` (never saved) instead of master's saved
#      `0731834309...`. A HIT needs no mirror at all (a hit on another lane's
#      set went green in the same outage); the refresh was the only thing
#      between this lane and one.
#   2. The cold path then ran `apt-get install` of ~170 pinned packages through
#      `apt-fast`, whose aria2c never starts here: apt-fast keeps only
#      `http(s)://` URIs and this image's sources are `mirror+file:`, so it is a
#      pass-through. Each package burned 3 mirrors x ~20 s of resolver
#      timeouts, serially: ~9 packages in 7 min. apt's own `Timeout` and
#      `Retries` do not cover the resolver's wait, so nothing in apt fires.
#   3. The action sends apt-fast's stdout to a file, so the job log was silent
#      from the manifest line to the cancel.
#
#   Measured by a throwaway probe workflow on the same image (ubuntu-26.04,
#   20261101.186.1; its run was 43045081799 on the branch that carries this
#   file). Healthy timing for scale: refresh 3-7 s, a cold composite 40-52 s,
#   a warm one 12-21 s.
#
#   The same day the release lanes failed the same way where no helper ran
#   (#3710): an arm64 bare-image container could not resolve ports.ubuntu.com,
#   and the wizard's arm64 refresh waited 8 minutes on it.
#
# The five decisions
#
#   1. PROBE the names apt will use, read from apt's own sources (one-line
#      .list files, deb822 .sources, and the list a `mirror+file:` URI names),
#      through the libc path apt itself uses (`getent`), in parallel:
#      milliseconds when healthy, ~10 s when not. The Ubuntu archive hosts are
#      REQUIRED; any other repository (packages.microsoft.com, a PPA) is
#      reported and never required, so a dead third-party name can never block
#      an install from the archive. Every later phase asks again instead of
#      trusting an earlier answer, so a resolver that heals mid-job is used.
#      Until #3710 only /etc/apt/apt-mirrors.txt was read, which the arm64
#      image does not have: its probe tested the amd64 trio, passed, and the
#      install then waited on ports.ubuntu.com.
#   2. FAIL OVER only when that probe fails, and only after a public resolver
#      has been seen to answer every one of the names: point the default
#      route's link at it (`resolvectl dns`), say so in a ::warning that names
#      the failed resolver, flush the cache and probe again. If the re-probe
#      still fails the link is put back as found. apt's verification is NEVER
#      weakened here (no --allow-unauthenticated, no [trusted=yes], no
#      Acquire::AllowInsecureRepositories): every index is checked against
#      Ubuntu's signing keys and every package against the index, so package
#      integrity does not rest on DNS. A poisoned answer can only make a
#      download fail, never make a wrong package install. A container does not
#      always see the switch (an image with a static /etc/resolv.conf hands
#      Docker only the upstream it went around), so a switch that helped also
#      writes its resolvers to $DOCKER_DNS for the container steps to pass.
#   3. STAND IN for apt-fast. The action runs only `apt-fast --version` and
#      `apt-fast --yes install <name=ver ...>` (its stdout is parsed for
#      `^Unpacking name:arch (ver)` lines), and downloads apt-fast from
#      ilikenwf/apt-fast's `master` when `--version` fails; having the stand-in
#      answer first drops that unpinned download too. The install is split in
#      two: a download-only phase, bounded, is the only one that waits on the
#      network, so a kill can never land inside dpkg and leave a half-installed
#      package; the unpack phase works from /var/cache/apt/archives.
#   4. DO NOT LET THE ACTION SAVE A FAILED INSTALL. lib.sh sets `set +e`, so the
#      action ignores the install's exit status, tars whatever got unpacked
#      (nothing, or a partial set) and SAVES it under the key: every later run
#      would HIT it, restore nothing, and pay for the fallback until the key
#      changed. That is the empty-set poisoning the composite's header
#      describes. On failure the stand-in removes the action's work directory
#      (~/cache-apt-pkgs), and actions/cache/save then has nothing to save.
#   5. WITH NO RESOLVER AT ALL, `install` still tries the local archive
#      (`apt-get --no-download`): packages a seed restored, or an earlier
#      download phase left, install against the image's index without the
#      network, verified as always. Otherwise it fails at once with one
#      ::error, instead of waiting ~20 s per package for the job's timeout.
#
# No test knob lives here on purpose: the failure paths are exercised by making
# the resolvers really fail on a throwaway runner (iptables, ci-apt-fault.sh in
# the self-test), so the code that ships is the code that was proven. The
# offline tests (ci-apt.test.sh) source this file and point its globals at
# fixtures; nothing here reads the environment to change what it does.

set -uo pipefail

# Where apt's configuration lives.
ETC_APT=/etc/apt
ARCH=$(uname -m)
# What apt reaches when its sources name no Ubuntu archive host at all.
FALLBACK_HOSTS_AMD64="azure.archive.ubuntu.com archive.ubuntu.com security.ubuntu.com"
FALLBACK_HOSTS_PORTS="ports.ubuntu.com"
# The mirrors of the stock images the release lanes' containers run
# (scripts/deb-bare-check.sh's ubuntu 22.04/24.04/26.04 and debian 12/13, and
# the bundle checks' ubuntu:24.04), per runner architecture: `reach --containers`.
CONTAINER_HOSTS_AMD64="archive.ubuntu.com security.ubuntu.com deb.debian.org"
CONTAINER_HOSTS_PORTS="ports.ubuntu.com deb.debian.org"
# A required name is an Ubuntu archive host.
ARCHIVE_HOST_RE='(^|\.)(archive|ports|security)\.ubuntu\.com$'
# Tried in this order; every one that answers all the names is configured.
PUBLIC_RESOLVERS="1.1.1.1 8.8.8.8"
# Seconds. `getent` fails by itself after ~10 s on a dead resolver; this caps a
# resolver that never answers at all.
PROBE_TIMEOUT=25
# A name that is only reported waits less.
OTHER_PROBE_TIMEOUT=5
# apt-get update, normally 3-7 s.
REFRESH_BOUND=90
# The network phase of an install, normally under 30 s for the 170-package set.
DOWNLOAD_BOUND=300
# The unpack phase works from the local archive; this only ends a hung
# maintainer script.
INSTALL_BOUND=600
# SIGKILL this long after the SIGTERM.
KILL_AFTER=30
# The runner image's own /etc/apt/apt.conf.d/zz-retries says the same
# (Retries 1, Timeout 15) and sorts AFTER the `80-ci-retries` this composite
# writes, so it wins; the bounded calls below do not depend on either file.
APT_NET_OPTS=(-o Acquire::Retries=1 -o Acquire::http::Timeout=15 -o Acquire::https::Timeout=15 -o DPkg::Lock::Timeout=60)
# Where `prepare` puts the stand-in, and the bounds it runs under.
BIN_DIR=/usr/local/bin
BOUNDS_FILE=/usr/local/etc/ci-apt.bounds
# Left behind by a failover that did not help, so the later phases of the same
# job do not try it (and print it) again.
FAILOVER_FAILED=/tmp/ci-apt.failover-failed
# Written by a failover that helped: the resolvers it switched to, as
# `docker run` arguments, one per line (`--dns`, then an address). Docker gives
# a container the host's /etc/resolv.conf minus its loopback entries, and where
# that file is static (some ubuntu-22.04-arm runners since 2026-11-16: an egress
# proxy, the stub, then the upstream) the copy holds only the upstream the
# switch went around, so a container started after it still cannot resolve
# (#3710). No failover, no file: a healthy job passes containers nothing new.
# It is not removed later in the job, because the switch is not either.
DOCKER_DNS=/tmp/ci-apt.docker-dns
# This file, by its own path (when sourced, $0 is the caller).
SELF=${BASH_SOURCE[0]}

if [ "$(id -u)" -eq 0 ]; then SUDO=(); else SUDO=(sudo); fi

MODE=standalone
[ "$(basename "$0")" = apt-fast ] && MODE=standin

# In the stand-in stdout is the action's install.log (it is parsed, and the job
# log never shows it), so everything meant for a reader goes to stderr there.
say() {
  if [ "$MODE" = standin ]; then printf '%s\n' "$*" >&2; else printf '%s\n' "$*"; fi
}
log() { say "ci-apt: $*"; }
warn() { say "::warning title=$1::$2"; }
fail() { say "::error title=$1::$2"; }

# --- the names apt will contact ------------------------------------------------

# Every URI of every enabled source, one per line.
source_uris() {
  local f
  for f in "$ETC_APT/sources.list" "$ETC_APT"/sources.list.d/*.list; do
    [ -f "$f" ] || continue
    # deb [opt=x opt=y] URI suite component...
    awk '{ sub(/#.*/, "") }
      $1 == "deb" || $1 == "deb-src" {
        i = 2
        if ($2 ~ /^\[/) { while (i <= NF && $i !~ /\]$/) i++; i++ }
        if (i <= NF) print $i
      }' "$f"
  done
  for f in "$ETC_APT"/sources.list.d/*.sources; do
    [ -f "$f" ] || continue
    # deb822: stanzas split by blank lines; `URIs:` may continue on indented
    # lines; a stanza with `Enabled: no` is skipped.
    awk 'function flush() { if (on) for (k = 1; k <= n; k++) print u[k]; n = 0; on = 1; inu = 0 }
      BEGIN { n = 0; on = 1 }
      /^[[:space:]]*#/ { next }
      /^[[:space:]]*$/ { flush(); next }
      /^[^[:space:]]/ { inu = 0 }
      tolower($1) == "uris:" { inu = 1; for (k = 2; k <= NF; k++) u[++n] = $k; next }
      tolower($1) == "enabled:" { if (tolower($2) == "no") on = 0; next }
      /^[[:space:]]/ && inu { for (k = 1; k <= NF; k++) u[++n] = $k; next }
      END { flush() }' "$f"
  done
}

# The host of each URI on stdin; a mirror+file: URI stands for the URIs in the
# file it names. Local sources (file:, cdrom:, copy:) have none.
uri_hosts() {
  local uri f
  while read -r uri; do
    case "$uri" in
      mirror+file:*)
        f=${uri#mirror+file:}
        case "$f" in /etc/apt/*) f="$ETC_APT${f#/etc/apt}" ;; esac
        [ -r "$f" ] && sed -n 's#^[[:space:]]*\([A-Za-z][A-Za-z0-9+.-]*://[^[:space:]]*\).*#\1#p' "$f"
        ;;
      file:* | cdrom:* | copy:*) ;;
      *://*) printf '%s\n' "$uri" ;;
    esac
  done | sed -E 's#^[A-Za-z][A-Za-z0-9+.-]*://##; s#/.*##; s#^.*@##; s#:[0-9]*$##' | awk 'NF && !seen[$0]++'
}

# Sets REQUIRED (the Ubuntu archive hosts apt's sources name, or the image's
# defaults when they name none) and OTHERS (every other host, reported only).
derive_hosts() {
  local h all
  REQUIRED="" OTHERS=""
  all=$(source_uris | uri_hosts)
  for h in $all; do
    if printf '%s' "$h" | grep -qE "$ARCHIVE_HOST_RE"; then REQUIRED="$REQUIRED $h"; else OTHERS="$OTHERS $h"; fi
  done
  REQUIRED=${REQUIRED# } OTHERS=${OTHERS# }
  if [ -z "$REQUIRED" ]; then
    case "$ARCH" in
      x86_64 | amd64) REQUIRED=$FALLBACK_HOSTS_AMD64 ;;
      *) REQUIRED=$FALLBACK_HOSTS_PORTS ;;
    esac
    log "apt's sources name no Ubuntu archive host; probing this architecture's defaults"
  fi
  HOSTS_READY=1
  log "mirror names from apt's sources: required: $REQUIRED; other: ${OTHERS:-none}"
}
HOSTS_READY=""
init_hosts() { [ -n "$HOSTS_READY" ] || derive_hosts; }

# Adds names to REQUIRED, keeping it free of repeats.
require_hosts() {
  local h
  for h in "$@"; do
    case " $REQUIRED " in *" $h "*) ;; *) REQUIRED="$REQUIRED $h" ;; esac
  done
  REQUIRED=${REQUIRED# }
}

# --- reaching them ------------------------------------------------------------

# 0 when EVERY required name resolves through the system resolver, by the libc
# path apt uses. Not "any": apt tries the mirrors in priority order, so a dead
# first mirror costs a resolver wait on every file even if the last one works.
# The other names are probed alongside, with a shorter wait, and only logged.
probe_system() {
  local d h ip ms rc=0 line="" t
  init_hosts
  d=$(mktemp -d)
  for h in $REQUIRED $OTHERS; do
    t=$PROBE_TIMEOUT
    case " $REQUIRED " in *" $h "*) ;; *) t=$OTHER_PROBE_TIMEOUT ;; esac
    (
      s=$(date +%s%N)
      a=$(timeout "$t" getent ahostsv4 "$h" 2>/dev/null | awk 'NR==1 {print $1}')
      e=$(date +%s%N)
      printf '%s %s\n' "${a:-FAILED}" "$(((e - s) / 1000000))" >"$d/$h"
    ) &
  done
  wait
  for h in $REQUIRED; do
    ip=FAILED ms=0
    read -r ip ms <"$d/$h" || true
    line="$line $h=$ip(${ms}ms)"
    [ "$ip" = FAILED ] && rc=1
  done
  for h in $OTHERS; do
    ip=FAILED ms=0
    read -r ip ms <"$d/$h" || true
    line="$line $h=$ip(${ms}ms,not required)"
  done
  rm -rf "$d"
  log "system resolver:$line"
  return $rc
}

# 0 when `server` answers an IPv4 address for EVERY required name (a plain DNS
# query, not through the system resolver, so it shows what a switch would buy).
resolves_via() {
  local server=$1 d h ip rc=0 line=""
  d=$(mktemp -d)
  for h in $REQUIRED; do
    (
      a=$(timeout 8 dig +time=3 +tries=1 +short A "$h" "@$server" 2>/dev/null | grep -E -m1 '^[0-9]+(\.[0-9]+){3}$')
      echo "${a:-FAILED}" >"$d/$h"
    ) &
  done
  wait
  for h in $REQUIRED; do
    ip=FAILED
    read -r ip <"$d/$h" || true
    line="$line $h=$ip"
    [ "$ip" = FAILED ] && rc=1
  done
  rm -rf "$d"
  log "@$server:$line"
  return $rc
}

# The link the default route leaves through (eth0 on the hosted runners).
default_link() {
  local l
  l=$(ip -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1)
  printf '%s' "${l:-eth0}"
}

# Called only after probe_system failed. 0 when, afterwards, the system
# resolver answers every required name.
switch_resolver() {
  local t link s orig_dns orig_domain
  local -a good=() dns_back=() domain_back=()
  if [ -e "$FAILOVER_FAILED" ]; then
    log "the public-resolver failover was tried earlier in this job and did not help"
    return 1
  fi
  for t in resolvectl dig ip; do
    command -v "$t" >/dev/null 2>&1 || {
      log "no '$t' on this runner: cannot confirm or apply a public resolver"
      return 1
    }
  done
  for s in $PUBLIC_RESOLVERS; do
    resolves_via "$s" && good+=("$s")
  done
  if [ "${#good[@]}" -eq 0 ]; then
    log "no public resolver ($PUBLIC_RESOLVERS) answers the mirror names either"
    : >"$FAILOVER_FAILED"
    return 1
  fi
  link=$(default_link)
  orig_dns=$(resolvectl dns "$link" 2>/dev/null | sed 's/^[^:]*: *//')
  orig_domain=$(resolvectl domain "$link" 2>/dev/null | sed 's/^[^:]*: *//')
  read -r -a dns_back <<<"$orig_dns"
  read -r -a domain_back <<<"$orig_domain"
  log "$link: resolvers [${orig_dns:-none}] -> [${good[*]}]"
  if ! "${SUDO[@]}" resolvectl dns "$link" "${good[@]}"; then
    log "resolvectl dns $link failed"
    : >"$FAILOVER_FAILED"
    return 1
  fi
  "${SUDO[@]}" resolvectl flush-caches
  if ! probe_system; then
    # The link may not be the one that receives names no other link claims.
    log "$link: routing every name to it ('~.') and asking again"
    "${SUDO[@]}" resolvectl domain "$link" '~.'
    "${SUDO[@]}" resolvectl flush-caches
    if ! probe_system; then
      log "$link: the switch did not help, putting the link back as it was"
      "${SUDO[@]}" resolvectl revert "$link"
      [ "${#dns_back[@]}" -gt 0 ] && "${SUDO[@]}" resolvectl dns "$link" "${dns_back[@]}"
      [ "${#domain_back[@]}" -gt 0 ] && "${SUDO[@]}" resolvectl domain "$link" "${domain_back[@]}"
      "${SUDO[@]}" resolvectl flush-caches
      : >"$FAILOVER_FAILED"
      return 1
    fi
  fi
  if printf -- '--dns\n%s\n' "${good[@]}" 2>/dev/null >"$DOCKER_DNS"; then
    log "the container steps pass these resolvers to docker from $DOCKER_DNS"
  else
    log "could not write $DOCKER_DNS: containers keep the host's resolv.conf"
  fi
  warn "apt mirror DNS" "this runner's resolver (${orig_dns:-none} on $link) did not resolve the mirrors apt uses ($REQUIRED) while ${good[*]} did, so $link now uses ${good[*]} for the rest of this job, and the container steps pass them to docker (#3698, #3710). apt still verifies every index and package against the archive's signing keys, so package integrity does not depend on DNS."
  return 0
}

# 0 when the mirror names resolve now: through the runner's own resolver, or
# after the failover above.
ensure_mirror_dns() {
  probe_system && return 0
  switch_resolver
}

# The resolvers the system resolver forwards to, for the log.
resolvers_in_use() {
  local link
  if command -v resolvectl >/dev/null 2>&1; then
    link=$(default_link)
    resolvectl dns "$link" 2>/dev/null | sed 's/^[^:]*: *//' | tr '\n' ' '
  else
    awk '$1 == "nameserver" {printf "%s ", $2}' /etc/resolv.conf 2>/dev/null
  fi
}

# --- apt -----------------------------------------------------------------------

# apt-get update exits 0 when index files failed to download, so its status
# cannot be trusted; the output is what says whether the index is fresh.
refresh_index() {
  local out rc t0
  out=$(mktemp)
  t0=$(date +%s)
  "${SUDO[@]}" timeout --kill-after=10 "$REFRESH_BOUND" apt-get "${APT_NET_OPTS[@]}" update -qq >"$out" 2>&1
  rc=$?
  cat "$out"
  log "apt-get update: rc=$rc in $(($(date +%s) - t0)) s (124 = bound of ${REFRESH_BOUND} s hit)"
  if [ "$rc" -ne 0 ] || grep -qE '^(W|E): (Failed to fetch|Some index files failed)' "$out"; then
    warn "apt index" "the index refresh did not complete (rc=$rc), so the image's older index stays in use: an install can ask for versions the mirror has dropped, and cache-apt-pkgs-action, which keys on the versions the index names, can MISS the entry master saved (#3698)"
  fi
  rm -f "$out"
}

# Two bounded phases, under DEBIAN_FRONTEND=noninteractive:
#   download-only: the only phase that waits on the network;
#   install: from /var/cache/apt/archives, so a kill never lands mid-download
#            of something dpkg is about to need, nor mid-dpkg on a slow mirror.
# apt_install_bounded OPTION... -- PACKAGE...   (--yes is always added)
apt_install_bounded() {
  local opts=() pkgs=() rc t0
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    opts+=("$1")
    shift
  done
  [ "${1:-}" = -- ] && shift
  pkgs=("$@")
  t0=$(date +%s)
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive timeout --kill-after="$KILL_AFTER" "$DOWNLOAD_BOUND" \
    apt-get "${APT_NET_OPTS[@]}" "${opts[@]}" --yes --download-only install "${pkgs[@]}"
  rc=$?
  log "download phase: rc=$rc in $(($(date +%s) - t0)) s (${#pkgs[@]} package specs; 124 = bound of ${DOWNLOAD_BOUND} s hit)"
  [ "$rc" -eq 0 ] || return "$rc"
  t0=$(date +%s)
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive timeout --kill-after="$KILL_AFTER" "$INSTALL_BOUND" \
    apt-get "${APT_NET_OPTS[@]}" "${opts[@]}" --yes install "${pkgs[@]}"
  rc=$?
  log "install phase: rc=$rc in $(($(date +%s) - t0)) s (124 = bound of ${INSTALL_BOUND} s hit)"
  return "$rc"
}

# With no resolver: install only what the local archive already holds. apt
# refuses before dpkg runs if any package is missing from it.
apt_install_offline() {
  local opts=() pkgs=() rc
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    opts+=("$1")
    shift
  done
  [ "${1:-}" = -- ] && shift
  pkgs=("$@")
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive timeout --kill-after="$KILL_AFTER" "$INSTALL_BOUND" \
    apt-get "${APT_NET_OPTS[@]}" "${opts[@]}" --yes --no-download install "${pkgs[@]}"
  rc=$?
  log "offline install from the local archive: rc=$rc"
  return "$rc"
}

# "a b c (+N more)" of a package list, for a message.
pkg_summary() {
  local n=$#
  if [ "$n" -le 4 ]; then printf '%s' "$*"; else printf '%s %s %s (+%d more)' "$1" "$2" "$3" $((n - 3)); fi
}

# Leading --refresh-bound/--download-bound/--install-bound S options: sets the
# bounds, records which were given in BOUNDS_GIVEN, and leaves the rest in REST.
parse_bounds() {
  BOUNDS_GIVEN=() REST=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --refresh-bound | --download-bound | --install-bound)
        if ! [[ "${2:-}" =~ ^[1-9][0-9]*$ ]]; then
          say "ci-apt.sh: $1 needs a number of seconds, not '${2:-}'" >&2
          return 2
        fi
        case "$1" in
          --refresh-bound) REFRESH_BOUND=$2 ;;
          --download-bound) DOWNLOAD_BOUND=$2 ;;
          --install-bound) INSTALL_BOUND=$2 ;;
        esac
        BOUNDS_GIVEN+=("$1")
        shift 2
        ;;
      *) break ;;
    esac
  done
  REST=("$@")
}

# The stand-in's bounds, as `prepare` recorded them. Read line by line and
# checked, never sourced.
load_bounds_file() {
  local k v
  [ -r "$BOUNDS_FILE" ] || return 0
  while IFS='=' read -r k v; do
    [[ "$v" =~ ^[1-9][0-9]*$ ]] || continue
    case "$k" in
      REFRESH_BOUND) REFRESH_BOUND=$v ;;
      DOWNLOAD_BOUND) DOWNLOAD_BOUND=$v ;;
      INSTALL_BOUND) INSTALL_BOUND=$v ;;
    esac
  done <"$BOUNDS_FILE"
}

# Where cache-apt-pkgs-action v1 keeps its work: `~/cache-apt-pkgs` of the user
# that runs the step. The stand-in runs as root through sudo, so take SUDO_USER.
action_work_dir() {
  local home=""
  if [ -n "${SUDO_USER:-}" ]; then home=$(getent passwd "$SUDO_USER" | cut -d: -f6); fi
  printf '%s' "${home:-${HOME:-}}/cache-apt-pkgs"
}

# See decision 4 in the header.
neutralize_cache_save() {
  local d
  d=$(action_work_dir)
  if [ "$d" != /cache-apt-pkgs ] && [ -d "$d" ]; then
    rm -rf "$d"
    log "removed $d: the action has no failed install to save under its cache key"
  fi
}

# --- subcommands ---------------------------------------------------------------

# `prepare`: everything the later steps lean on, none of it able to fail the
# step. A cache HIT needs no mirror, so an unreachable one is a warning here and
# is judged where an install is actually needed.
cmd_prepare() {
  local b
  parse_bounds "$@" || {
    warn "ci-apt prepare" "a bound option was not understood; nothing was prepared (#3710)"
    return 0
  }
  rm -f "$FAILOVER_FAILED"
  if "${SUDO[@]}" install -m 0755 "$SELF" "$BIN_DIR/ci-apt" && "${SUDO[@]}" ln -sf ci-apt "$BIN_DIR/apt-fast"; then
    log "apt-fast stand-in installed: $("$BIN_DIR/apt-fast" --version)"
  else
    warn "apt-fast stand-in" "could not install the stand-in under $BIN_DIR, so cache-apt-pkgs-action will fetch apt-fast itself and its cold path is NOT bounded (#3698)"
  fi
  if [ "${#BOUNDS_GIVEN[@]}" -gt 0 ]; then
    b=$(printf 'REFRESH_BOUND=%s\nDOWNLOAD_BOUND=%s\nINSTALL_BOUND=%s\n' "$REFRESH_BOUND" "$DOWNLOAD_BOUND" "$INSTALL_BOUND")
    if "${SUDO[@]}" mkdir -p "$(dirname "$BOUNDS_FILE")" && printf '%s\n' "$b" | "${SUDO[@]}" tee "$BOUNDS_FILE" >/dev/null; then
      log "the stand-in's bounds: refresh ${REFRESH_BOUND} s, download ${DOWNLOAD_BOUND} s, install ${INSTALL_BOUND} s"
    else
      warn "apt-fast stand-in" "could not record the stand-in's bounds in $BOUNDS_FILE; it runs under its defaults"
    fi
  else
    "${SUDO[@]}" rm -f "$BOUNDS_FILE"
  fi
  if ensure_mirror_dns; then
    refresh_index
  else
    warn "apt mirrors unreachable" "no resolver on this runner resolves the mirrors apt uses ($REQUIRED), so the index refresh is skipped (it would only wait out resolver timeouts). A cached-apt HIT still works; an install that needs the mirror fails fast instead of waiting for the job's timeout (#3698)."
  fi
  return 0
}

# `apt-fast ...` as the cached-apt action runs it.
cmd_apt_fast() {
  local a verb="" opts=() pkgs=() t0 rc
  case "${1:-}" in
    --version | -v | version)
      echo "apt-fast 0 (ci-apt.sh stand-in for #3698: bounded apt-get, no aria2c)"
      return 0
      ;;
  esac
  for a in "$@"; do
    if [ -z "$verb" ]; then
      case "$a" in
        -*) opts+=("$a") ;;
        *) verb=$a ;;
      esac
    else
      pkgs+=("$a")
    fi
  done
  if [ "$verb" != install ]; then
    "${SUDO[@]}" timeout --kill-after=10 "$REFRESH_BOUND" apt-get "${APT_NET_OPTS[@]}" "$@"
    return $?
  fi
  t0=$(date +%s)
  if ! ensure_mirror_dns; then
    warn "apt cold path" "the cached apt set missed and no resolver on this runner resolves the mirrors apt uses ($REQUIRED), so the install is not attempted: it would wait ~20 s per package for the resolver (#3698). The verify step that follows decides the outcome."
    neutralize_cache_save
    return 100
  fi
  apt_install_bounded "${opts[@]}" -- "${pkgs[@]}"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    warn "apt cold path" "the cached apt set missed and its cold install failed (rc=$rc after $(($(date +%s) - t0)) s; 124 means a bound was hit). The verify step that follows installs directly and decides the outcome."
    neutralize_cache_save
  fi
  return "$rc"
}

# `fallback PKG...`: the composite's verify steps' direct install, bounded, with
# one error that says why.
cmd_fallback() {
  local t0 rc
  t0=$(date +%s)
  if ! ensure_mirror_dns; then
    fail "Ubuntu mirrors unreachable" "this runner cannot resolve the mirrors apt uses ($REQUIRED), neither through its own resolver nor through $PUBLIC_RESOLVERS, and the cached apt step did not deliver the packages (no cache entry for its key, or its cold install failed). Nothing can install them until the names resolve again: re-run this job later (#3698). Failing now instead of waiting for the job's timeout."
    return 1
  fi
  # A bounded install that was killed can leave a package unpacked but not configured.
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive dpkg --configure -a || true
  refresh_index
  apt_install_bounded --no-install-recommends -- "$@"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    fail "apt install failed" "the direct install of $# package(s) ($(pkg_summary "$@")) failed (rc=$rc after $(($(date +%s) - t0)) s; 124 means a bound was hit). Re-run this job; if it repeats, read the apt output above (#3698)."
    return 1
  fi
  return 0
}

# `install [BOUND...] [APT-OPTION...] -- PKG...`: see the header.
cmd_install() {
  local opts=() pkgs=() t0 rc a seen=""
  parse_bounds "$@" || return 2
  for a in "${REST[@]}"; do
    if [ -z "$seen" ]; then
      if [ "$a" = -- ]; then seen=1; else opts+=("$a"); fi
    else
      pkgs+=("$a")
    fi
  done
  if [ -z "$seen" ] || [ "${#pkgs[@]}" -eq 0 ]; then
    say "usage: ci-apt.sh install [--refresh-bound S] [--download-bound S] [--install-bound S] [APT-OPTION...] -- PACKAGE..." >&2
    return 2
  fi
  t0=$(date +%s)
  # A bounded install (or a cached step's timeout) that was killed can leave a
  # package unpacked but not configured.
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive dpkg --configure -a || true
  if ! ensure_mirror_dns; then
    if apt_install_offline "${opts[@]}" -- "${pkgs[@]}"; then
      warn "apt offline install" "no resolver on this runner resolves the mirrors apt uses ($REQUIRED), but every package was already in the local archive, so they installed from it against the image's index, verified as usual (#3710)"
      return 0
    fi
    fail "Ubuntu mirrors unreachable" "this runner cannot resolve the mirrors apt uses ($REQUIRED), neither through its own resolver nor through $PUBLIC_RESOLVERS, and the local archive does not hold every package, so ${#pkgs[@]} package(s) ($(pkg_summary "${pkgs[@]}")) cannot install. Failing now instead of waiting on the resolver for the job's timeout: re-run this job later (#3710)."
    return 1
  fi
  refresh_index
  apt_install_bounded "${opts[@]}" -- "${pkgs[@]}"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    fail "apt install failed" "the install of ${#pkgs[@]} package(s) ($(pkg_summary "${pkgs[@]}")) failed (rc=$rc after $(($(date +%s) - t0)) s; 124 means a bound was hit) while the mirror names resolved. Re-run this job; if it repeats, read apt's output above (#3710)."
    return 1
  fi
  return 0
}

# `reach [--containers] [HOST...]`: see the header. The stub resolver's cache is
# flushed first, so the probe asks the upstream, as a container does.
cmd_reach() {
  local a extra=() c=()
  for a in "$@"; do
    case "$a" in
      --containers)
        case "$ARCH" in
          x86_64 | amd64) read -r -a c <<<"$CONTAINER_HOSTS_AMD64" ;;
          *) read -r -a c <<<"$CONTAINER_HOSTS_PORTS" ;;
        esac
        extra+=("${c[@]}")
        ;;
      -*)
        say "usage: ci-apt.sh reach [--containers] [HOST...]" >&2
        return 2
        ;;
      *) extra+=("$a") ;;
    esac
  done
  init_hosts
  [ "${#extra[@]}" -gt 0 ] && require_hosts "${extra[@]}"
  if command -v resolvectl >/dev/null 2>&1; then "${SUDO[@]}" resolvectl flush-caches >/dev/null 2>&1 || true; fi
  if ! ensure_mirror_dns; then
    fail "apt mirrors unreachable" "this runner cannot resolve $REQUIRED, neither through its own resolver nor through $PUBLIC_RESOLVERS, so nothing that needs those mirrors (an apt-get here, or in a container this job starts) can work. Failing now instead of waiting on the resolver: re-run this job later (#3710)."
    return 1
  fi
  log "the mirror names resolve; resolvers in use: $(resolvers_in_use)"
  if [ -s "$DOCKER_DNS" ]; then log "containers get: $(tr '\n' ' ' <"$DOCKER_DNS")(from $DOCKER_DNS)"; fi
  return 0
}

main() {
  if [ "$MODE" = standin ]; then
    load_bounds_file
    cmd_apt_fast "$@"
    return $?
  fi
  case "${1:-}" in
    prepare | fallback | install | reach)
      local sub=$1
      shift
      "cmd_$sub" "$@"
      ;;
    *)
      echo "usage: ci-apt.sh prepare [BOUND...] | fallback PACKAGE... | install [BOUND...] [APT-OPTION...] -- PACKAGE... | reach [--containers] [HOST...]" >&2
      return 2
      ;;
  esac
}

# Sourceable for tests; run when executed (directly or as apt-fast).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
  exit $?
fi
