#!/usr/bin/env bash
# The apt half of the ci-linux-system-deps composite: reach the Ubuntu mirrors,
# refresh the index, bound the cached-apt action's cold path, and bound the
# fallback install. Everything here is BOUNDED INSIDE ITS OWN PROCESS TREE,
# because a step-level `timeout-minutes` kills nothing below the step and a
# composite step cannot carry one at all.
#
#   ci-apt.sh prepare           reach the mirrors (fail over to a public
#                               resolver if the runner's own cannot), refresh
#                               the index, install the apt-fast stand-in
#   ci-apt.sh fallback PKG...   the verify steps' install: bounded, one clear
#                               ::error when the mirrors cannot be reached
#   apt-fast ...                the stand-in, installed by `prepare` as
#                               /usr/local/bin/apt-fast (same file, other name)
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
# The four decisions
#
#   1. PROBE the names apt will use (/etc/apt/apt-mirrors.txt) through the libc
#      path apt itself uses (`getent`), in parallel: milliseconds when healthy,
#      ~10 s when not. Every later phase asks again instead of trusting an
#      earlier answer, so a resolver that heals mid-job is used.
#   2. FAIL OVER only when that probe fails, and only after a public resolver
#      has been seen to answer every one of the names: point the default
#      route's link at it (`resolvectl dns`), say so in a ::warning that names
#      the failed resolver, flush the cache and probe again. If the re-probe
#      still fails the link is put back as found. apt's verification is NEVER
#      weakened here (no --allow-unauthenticated, no [trusted=yes], no
#      Acquire::AllowInsecureRepositories): every index is checked against
#      Ubuntu's signing keys and every package against the index, so package
#      integrity does not rest on DNS. A poisoned answer can only make a
#      download fail, never make a wrong package install.
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
#
# No test knob lives here on purpose: the failure paths are exercised by making
# the resolvers really fail on a throwaway runner (iptables), so the code that
# ships is the code that was proven.

set -uo pipefail

# What apt reaches when /etc/apt/apt-mirrors.txt is missing or names nothing.
FALLBACK_HOSTS="azure.archive.ubuntu.com archive.ubuntu.com security.ubuntu.com"
# Tried in this order; every one that answers all the names is configured.
PUBLIC_RESOLVERS="1.1.1.1 8.8.8.8"
# Seconds. `getent` fails by itself after ~10 s on a dead resolver; this caps a
# resolver that never answers at all.
PROBE_TIMEOUT=25
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
# Where `prepare` puts the stand-in.
BIN_DIR=/usr/local/bin
# Left behind by a failover that did not help, so the later phases of the same
# job do not try it (and print it) again.
FAILOVER_FAILED=/tmp/ci-apt.failover-failed

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

# The hosts apt will contact, one per mirror line of the image's mirror list.
mirror_hosts() {
  local hosts=""
  if [ -r /etc/apt/apt-mirrors.txt ]; then
    hosts=$(sed -n 's#^[A-Za-z][A-Za-z0-9+.-]*://\([^/:[:space:]]*\).*#\1#p' /etc/apt/apt-mirrors.txt | sort -u | tr '\n' ' ')
  fi
  hosts=${hosts% }
  printf '%s' "${hosts:-$FALLBACK_HOSTS}"
}
HOSTS=$(mirror_hosts)

# 0 when EVERY mirror name resolves through the system resolver, by the libc
# path apt uses. Not "any": apt tries the mirrors in priority order, so a dead
# first mirror costs a resolver wait on every file even if the last one works.
probe_system() {
  local d h ip ms rc=0 line=""
  d=$(mktemp -d)
  for h in $HOSTS; do
    (
      s=$(date +%s%N)
      a=$(timeout "$PROBE_TIMEOUT" getent ahostsv4 "$h" 2>/dev/null | awk 'NR==1 {print $1}')
      e=$(date +%s%N)
      printf '%s %s\n' "${a:-FAILED}" "$(((e - s) / 1000000))" >"$d/$h"
    ) &
  done
  wait
  for h in $HOSTS; do
    ip=FAILED ms=0
    read -r ip ms <"$d/$h" || true
    line="$line $h=$ip(${ms}ms)"
    [ "$ip" = FAILED ] && rc=1
  done
  rm -rf "$d"
  log "system resolver:$line"
  return $rc
}

# 0 when `server` answers an IPv4 address for EVERY mirror name (a plain DNS
# query, not through the system resolver, so it shows what a switch would buy).
resolves_via() {
  local server=$1 d h ip rc=0 line=""
  d=$(mktemp -d)
  for h in $HOSTS; do
    (
      a=$(timeout 8 dig +time=3 +tries=1 +short A "$h" "@$server" 2>/dev/null | grep -E -m1 '^[0-9]+(\.[0-9]+){3}$')
      echo "${a:-FAILED}" >"$d/$h"
    ) &
  done
  wait
  for h in $HOSTS; do
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
# resolver answers every name.
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
  warn "apt mirror DNS" "this runner's resolver (${orig_dns:-none} on $link) did not resolve the Ubuntu mirrors ($HOSTS) while ${good[*]} did, so $link now uses ${good[*]} for the rest of this job (#3698). apt still verifies every index and package against Ubuntu's signing keys, so package integrity does not depend on DNS."
  return 0
}

# 0 when the mirror names resolve now: through the runner's own resolver, or
# after the failover above.
ensure_mirror_dns() {
  probe_system && return 0
  switch_resolver
}

# apt-get update exits 0 when index files failed to download, so its status
# cannot be trusted; the output is what says whether the index is fresh.
refresh_index() {
  local out rc t0
  out=$(mktemp)
  t0=$(date +%s)
  "${SUDO[@]}" timeout --kill-after=10 "$REFRESH_BOUND" apt-get "${APT_NET_OPTS[@]}" update -qq >"$out" 2>&1
  rc=$?
  cat "$out"
  log "apt-get update: rc=$rc in $(($(date +%s) - t0)) s"
  if [ "$rc" -ne 0 ] || grep -qE '^(W|E): (Failed to fetch|Some index files failed)' "$out"; then
    warn "apt index" "the index refresh did not complete (rc=$rc), so the image's older index stays in use; cache-apt-pkgs-action derives its keys from the versions it names, so the GTK set can MISS instead of hitting the entry master saved (#3698)"
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

# `prepare`: everything the later steps lean on, none of it able to fail the
# step. A cache HIT needs no mirror, so an unreachable one is a warning here and
# is judged where an install is actually needed.
cmd_prepare() {
  rm -f "$FAILOVER_FAILED"
  if "${SUDO[@]}" install -m 0755 "$0" "$BIN_DIR/ci-apt" && "${SUDO[@]}" ln -sf ci-apt "$BIN_DIR/apt-fast"; then
    log "apt-fast stand-in installed: $("$BIN_DIR/apt-fast" --version)"
  else
    warn "apt-fast stand-in" "could not install the stand-in under $BIN_DIR, so cache-apt-pkgs-action will fetch apt-fast itself and its cold path is NOT bounded (#3698)"
  fi
  if ensure_mirror_dns; then
    refresh_index
  else
    warn "apt mirrors unreachable" "no resolver on this runner resolves the Ubuntu mirrors ($HOSTS), so the index refresh is skipped (it would only wait out resolver timeouts). A cached-apt HIT still works; an install that needs the mirror fails fast instead of waiting for the job's timeout (#3698)."
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
    warn "apt cold path" "the cached apt set missed and no resolver on this runner resolves the Ubuntu mirrors ($HOSTS), so the install is not attempted: it would wait ~20 s per package for the resolver (#3698). The verify step that follows decides the outcome."
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

# `fallback PKG...`: the verify steps' direct install, bounded, with one error
# that says why.
cmd_fallback() {
  local t0 rc
  t0=$(date +%s)
  if ! ensure_mirror_dns; then
    fail "Ubuntu mirrors unreachable" "this runner cannot resolve the Ubuntu mirrors ($HOSTS), neither through its own resolver nor through $PUBLIC_RESOLVERS, and the cached apt step did not deliver the packages (no cache entry for its key, or its cold install failed). Nothing can install them until the names resolve again: re-run this job later (#3698). Failing now instead of waiting for the job's timeout."
    return 1
  fi
  # A bounded install that was killed can leave a package unpacked but not configured.
  "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive dpkg --configure -a || true
  refresh_index
  apt_install_bounded --no-install-recommends -- "$@"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    fail "apt install failed" "the direct install of the GTK/vpx packages failed (rc=$rc after $(($(date +%s) - t0)) s; 124 means a bound was hit). Re-run this job; if it repeats, read the apt output above (#3698)."
    return 1
  fi
  return 0
}

main() {
  if [ "$MODE" = standin ]; then
    cmd_apt_fast "$@"
    return $?
  fi
  case "${1:-}" in
    prepare)
      shift
      cmd_prepare "$@"
      ;;
    fallback)
      shift
      cmd_fallback "$@"
      ;;
    *)
      echo "usage: ci-apt.sh prepare | fallback PACKAGE..." >&2
      return 2
      ;;
  esac
}

# Sourceable for tests; run when executed (directly or as apt-fast).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
  exit $?
fi
