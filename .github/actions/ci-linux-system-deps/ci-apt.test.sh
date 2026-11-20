#!/usr/bin/env bash
# Offline tests for ci-apt.sh (#3710): every subcommand against STUB binaries
# (getent, dig, resolvectl, ip, apt-get, dpkg) on PATH. Nothing here touches the
# real resolver, apt, iptables or sudo, so it runs as an ordinary user on a dev
# box (Git Bash or Linux) and in the self-test workflow's `offline` job.
#
#   bash .github/actions/ci-linux-system-deps/ci-apt.test.sh
#
# The helper is SOURCED, and each case then points its globals at a scratch
# directory (ETC_APT, BIN_DIR, BOUNDS_FILE, FAILOVER_FAILED, ARCH, SUDO). That
# is a test's view of the code that ships, not a knob: the shipped helper reads
# no environment variable to change its behaviour. The failure paths themselves
# are proven for real by the self-test's fault cells (ci-apt-selftest.yml).
#
# The run prints `N passed, M failed` and fails unless every case ran: a filter
# or a typo that ran nothing must not read as green.

set -uo pipefail

# A helper that installs ITSELF by the wrong path ($0 is this file when it is
# sourced) would run this suite again as its stand-in; stop that at once.
[ -z "${CI_APT_TEST_ACTIVE:-}" ] || { echo "ci-apt.test.sh re-entered: the helper ran the test as a stand-in" >&2; exit 3; }
export CI_APT_TEST_ACTIVE=1

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HELPER="$HERE/ci-apt.sh"
EXPECTED_CASES=30

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
STUB="$ROOT/stub"
mkdir -p "$STUB"

pass=0 failn=0 cases=0
ok() { pass=$((pass + 1)); }
bad() {
  failn=$((failn + 1))
  printf 'FAIL [%s] %s\n' "$CASE" "$*"
}
# expect_has TEXT NEEDLE [what]
expect_has() { case "$1" in *"$2"*) ok ;; *) bad "${3:-output} lacks '$2'" ;; esac; }
expect_not() { case "$1" in *"$2"*) bad "${3:-output} has '$2'" ;; *) ok ;; esac; }
expect_eq() { if [ "$1" = "$2" ]; then ok; else bad "${3:-value} is '$1', expected '$2'"; fi; }
count_of() { printf '%s\n' "$1" | grep -c -- "$2" || true; }

# --- stubs -------------------------------------------------------------------
# State lives in $S (reset per case): dead-system lists names the system
# resolver fails (or ALL), dead-dig lists servers that answer nothing (or ALL),
# heal-on-switch makes a `resolvectl dns LINK SERVER...` revive the system
# resolver, apt-*-rc set apt-get's exit codes. Every call is logged.
cat >"$STUB/getent" <<'EOF'
#!/usr/bin/env bash
echo "getent $*" >>"$S/calls.log"
if [ "$1" = passwd ]; then echo "$2:x:1001:1001::$S/home-$2:/bin/bash"; exit 0; fi
h=$2
if grep -qxE "ALL|$h" "$S/dead-system" 2>/dev/null; then exit 2; fi
echo "10.0.0.1        STREAM $h"
EOF
cat >"$STUB/dig" <<'EOF'
#!/usr/bin/env bash
echo "dig $*" >>"$S/calls.log"
srv="" host=""
for a in "$@"; do
  case "$a" in @*) srv=${a#@} ;; +* | A) ;; *) host=$a ;; esac
done
if grep -qxE "ALL|$srv" "$S/dead-dig" 2>/dev/null; then exit 9; fi
echo "10.0.0.2"
EOF
cat >"$STUB/resolvectl" <<'EOF'
#!/usr/bin/env bash
echo "resolvectl $*" >>"$S/calls.log"
case "$1" in
  dns | domain)
    if [ $# -eq 2 ]; then
      [ "$1" = dns ] && echo "Link 2 ($2): 168.63.129.16" || echo "Link 2 ($2): example.internal"
    elif [ "$1" = dns ] && [ -e "$S/heal-on-switch" ]; then
      rm -f "$S/dead-system"
    fi
    ;;
  status) echo "Global"; echo "Link 2 (eth0)"; echo "Current DNS Server: 168.63.129.16" ;;
esac
exit 0
EOF
cat >"$STUB/ip" <<'EOF'
#!/usr/bin/env bash
echo "ip $*" >>"$S/calls.log"
echo "1.1.1.1 via 10.1.0.1 dev eth0 src 10.1.0.4 uid 1001"
EOF
cat >"$STUB/apt-get" <<'EOF'
#!/usr/bin/env bash
echo "apt-get $*" >>"$S/apt.log"
for a in "$@"; do
  case "$a" in
    update) exit "$(cat "$S/apt-update-rc" 2>/dev/null || echo 0)" ;;
    --download-only) exit "$(cat "$S/apt-download-rc" 2>/dev/null || echo 0)" ;;
    --no-download) exit "$(cat "$S/apt-offline-rc" 2>/dev/null || echo 100)" ;;
  esac
done
exit "$(cat "$S/apt-install-rc" 2>/dev/null || echo 0)"
EOF
cat >"$STUB/dpkg" <<'EOF'
#!/usr/bin/env bash
echo "dpkg $*" >>"$S/calls.log"
exit 0
EOF
chmod +x "$STUB"/*

# --- fixtures: the three runner images' apt sources ---------------------------
# Copied from the self-test's healthy cells (the `image facts` step logs each
# image's sources whole), so a change of image shows up here as a diff.
fixture() { # <name> -> $ROOT/fx/<name>/etc/apt
  local d="$ROOT/fx/$1/etc/apt"
  mkdir -p "$d/sources.list.d"
  printf '%s' "$d"
}
FX_2604=$(fixture ubuntu-2604)
cat >"$FX_2604/sources.list.d/ubuntu.sources" <<'EOF'
Types: deb
URIs: mirror+file:/etc/apt/apt-mirrors.txt
Suites: resolute resolute-updates resolute-backports
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

## Ubuntu security updates. Aside from URIs and Suites,
## this should mirror your choices in the previous section.
Types: deb
URIs: mirror+file:/etc/apt/apt-mirrors.txt
Suites: resolute-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
cat >"$FX_2604/apt-mirrors.txt" <<'EOF'
http://azure.archive.ubuntu.com/ubuntu/	priority:1
https://archive.ubuntu.com/ubuntu/	priority:2
https://security.ubuntu.com/ubuntu/	priority:3
EOF
cat >"$FX_2604/sources.list.d/microsoft-prod.list" <<'EOF'
deb [arch=amd64,armhf,arm64 signed-by=/usr/share/keyrings/microsoft-prod.gpg] https://packages.microsoft.com/ubuntu/24.04/prod noble main
EOF

FX_2204=$(fixture ubuntu-2204)
cat >"$FX_2204/sources.list" <<'EOF'
deb mirror+file:/etc/apt/apt-mirrors.txt jammy main restricted
# deb-src http://archive.ubuntu.com/ubuntu/ jammy main restricted
deb mirror+file:/etc/apt/apt-mirrors.txt jammy-updates main restricted
deb mirror+file:/etc/apt/apt-mirrors.txt jammy universe
deb mirror+file:/etc/apt/apt-mirrors.txt jammy-security main restricted
EOF
cat >"$FX_2204/apt-mirrors.txt" <<'EOF'
http://azure.archive.ubuntu.com/ubuntu/	priority:1
https://archive.ubuntu.com/ubuntu/	priority:2
https://security.ubuntu.com/ubuntu/	priority:3
EOF
cat >"$FX_2204/sources.list.d/microsoft-prod.list" <<'EOF'
deb [arch=amd64,armhf,arm64] https://packages.microsoft.com/ubuntu/22.04/prod jammy main
EOF

FX_ARM=$(fixture ubuntu-2204-arm)
cat >"$FX_ARM/sources.list" <<'EOF'
deb http://ports.ubuntu.com/ubuntu-ports/ jammy main restricted
deb http://ports.ubuntu.com/ubuntu-ports/ jammy-updates main restricted
## deb-src http://ports.ubuntu.com/ubuntu-ports/ jammy universe
deb http://ports.ubuntu.com/ubuntu-ports/ jammy universe
deb http://ports.ubuntu.com/ubuntu-ports/ jammy-security main restricted
EOF
cat >"$FX_ARM/sources.list.d/microsoft-prod.list" <<'EOF'
deb [arch=amd64,armhf,arm64] https://packages.microsoft.com/ubuntu/22.04/prod jammy main
EOF

# Shapes no image ships today, each a parsing rule.
FX_ODD=$(fixture odd)
cat >"$FX_ODD/sources.list.d/odd.sources" <<'EOF'
# a disabled stanza must not count
Types: deb
URIs: http://disabled.archive.ubuntu.com/ubuntu
Suites: noble
Enabled: no

Types: deb deb-src
URIs: http://user:secret@eu-west-1.ec2.archive.ubuntu.com:8080/ubuntu
 https://ppa.launchpadcontent.net/some/ppa/ubuntu
Suites: noble
Components: main
EOF
cat >"$FX_ODD/sources.list.d/cd.list" <<'EOF'
deb cdrom:[Ubuntu 24.04]/ noble main
deb file:/srv/local ./
EOF
FX_EMPTY=$(fixture empty)

# --- per-case harness ----------------------------------------------------------
# run_case NAME FIXTURE ARCH -- COMMAND...   sets OUT (stdout+stderr) and RC.
run_case() {
  CASE=$1 fx=$2 arch=$3
  shift 4
  cases=$((cases + 1))
  S="$ROOT/state/$cases"
  mkdir -p "$S/bin" "$S/home-runner"
  export S
  : >"$S/calls.log"
  : >"$S/apt.log"
  [ -n "${SETUP:-}" ] && eval "$SETUP"
  OUT=$(
    PATH="$STUB:$PATH"
    # shellcheck source=ci-apt.sh
    . "$HELPER"
    ETC_APT=$fx
    ARCH=$arch
    BIN_DIR="$S/bin"
    BOUNDS_FILE="$S/ci-apt.bounds"
    FAILOVER_FAILED="$S/failover-failed"
    SUDO=()
    HOME="$S/home-runner"
    SUDO_USER=""
    [ -n "${PRE:-}" ] && eval "$PRE"
    "$@" 2>&1
  )
  RC=$?
  APT=$(cat "$S/apt.log")
  CALLS=$(cat "$S/calls.log")
  SETUP="" PRE=""
}

# --- host derivation -------------------------------------------------------
run_case hosts-2604 "$FX_2604" x86_64 -- derive_hosts
expect_eq "$RC" 0 rc
expect_has "$OUT" "required: azure.archive.ubuntu.com archive.ubuntu.com security.ubuntu.com"
expect_has "$OUT" "other: packages.microsoft.com"

run_case hosts-2204 "$FX_2204" x86_64 -- derive_hosts
expect_has "$OUT" "required: azure.archive.ubuntu.com archive.ubuntu.com security.ubuntu.com"
expect_has "$OUT" "other: packages.microsoft.com"

run_case hosts-2204-arm "$FX_ARM" aarch64 -- derive_hosts
expect_has "$OUT" "required: ports.ubuntu.com"
expect_not "$OUT" "azure.archive" "the arm64 image's host list"
expect_has "$OUT" "other: packages.microsoft.com"

run_case hosts-odd "$FX_ODD" x86_64 -- derive_hosts
expect_has "$OUT" "required: eu-west-1.ec2.archive.ubuntu.com" "userinfo and port stripped"
expect_has "$OUT" "other: ppa.launchpadcontent.net" "a continuation URI line"
expect_not "$OUT" "disabled.archive" "an Enabled: no stanza"
expect_not "$OUT" "cdrom" "a cdrom: source"

run_case hosts-none-x64 "$FX_EMPTY" x86_64 -- derive_hosts
expect_has "$OUT" "required: azure.archive.ubuntu.com archive.ubuntu.com security.ubuntu.com"

run_case hosts-none-arm "$FX_EMPTY" aarch64 -- derive_hosts
expect_has "$OUT" "required: ports.ubuntu.com"

# --- required versus reported ----------------------------------------------------
SETUP='echo packages.microsoft.com >"$S/dead-system"'
run_case other-dead-passes "$FX_2604" x86_64 -- cmd_reach
expect_eq "$RC" 0 "rc with only a non-archive name dead"
expect_has "$OUT" "packages.microsoft.com=FAILED"
expect_not "$OUT" "::error" "a dead non-archive name"

SETUP='echo ports.ubuntu.com >"$S/dead-system"; echo ALL >"$S/dead-dig"'
run_case arm-required-dead "$FX_ARM" aarch64 -- cmd_reach
expect_eq "$RC" 1 "rc with ports.ubuntu.com dead everywhere"
expect_eq "$(count_of "$OUT" '::error')" 1 "::error count"

# --- install: the caller's options on both phases ----------------------------------
run_case install-opts "$FX_2204" x86_64 -- cmd_install -y -qq --no-install-recommends -o APT::Keep-Downloaded-Packages=true -- libssl-dev pkg-config
expect_eq "$RC" 0 rc
expect_has "$APT" "-y -qq --no-install-recommends -o APT::Keep-Downloaded-Packages=true --yes --download-only install libssl-dev pkg-config" "the download phase"
expect_has "$APT" "-y -qq --no-install-recommends -o APT::Keep-Downloaded-Packages=true --yes install libssl-dev pkg-config" "the install phase"
expect_has "$APT" " update" "a refresh before the install"
expect_has "$CALLS" "dpkg --configure -a"

run_case install-keeps-recommends "$FX_2204" x86_64 -- cmd_install -y -- rpm
expect_eq "$RC" 0 rc
expect_not "$APT" "no-install-recommends" "an install whose caller kept recommends"

run_case install-bounds "$FX_2204" x86_64 -- cmd_install --refresh-bound 11 --download-bound 22 --install-bound 33 -y -- rpm
expect_has "$OUT" "bound of 22 s" "the download phase's bound"
expect_has "$OUT" "bound of 33 s" "the install phase's bound"
expect_not "$APT" "bound" "apt's own arguments"

run_case install-usage "$FX_2204" x86_64 -- cmd_install -y rpm
expect_eq "$RC" 2 "rc without --"

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"'
run_case install-all-dead "$FX_2204" x86_64 -- cmd_install -y -- hello
expect_eq "$RC" 1 rc
expect_eq "$(count_of "$OUT" '::error')" 1 "::error count"
expect_has "$OUT" "#3710"
expect_not "$APT" "--download-only" "a download attempted with no resolver"

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"; echo 0 >"$S/apt-offline-rc"'
run_case install-offline "$FX_2204" x86_64 -- cmd_install -y --no-install-recommends -- hello
expect_eq "$RC" 0 "rc when every package is already in the archive"
expect_has "$APT" "-y --no-install-recommends --yes --no-download install hello"
expect_not "$OUT" "::error"

SETUP='echo 1 >"$S/apt-download-rc"'
run_case install-download-fails "$FX_2204" x86_64 -- cmd_install -y -- hello
expect_eq "$RC" 1 rc
expect_eq "$(count_of "$OUT" '::error')" 1 "::error count"

# --- reach -------------------------------------------------------------------
run_case reach-healthy "$FX_2604" x86_64 -- cmd_reach
expect_eq "$RC" 0 rc
expect_has "$OUT" "resolvers in use"
expect_not "$OUT" "::warning"

SETUP='echo ALL >"$S/dead-system"; : >"$S/heal-on-switch"; echo 168.63.129.16 >"$S/dead-dig"'
run_case reach-failover "$FX_2604" x86_64 -- cmd_reach
expect_eq "$RC" 0 "rc after a failover that helped"
expect_has "$OUT" "::warning title=apt mirror DNS::"
expect_has "$CALLS" "resolvectl dns eth0 1.1.1.1 8.8.8.8"

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"'
run_case reach-all-dead "$FX_2604" x86_64 -- cmd_reach
expect_eq "$RC" 1 rc
expect_eq "$(count_of "$OUT" '::error')" 1 "::error count"

run_case reach-names "$FX_ARM" aarch64 -- cmd_reach deb.debian.org
expect_has "$OUT" "deb.debian.org="
expect_has "$OUT" "ports.ubuntu.com="

run_case reach-containers-x64 "$FX_2604" x86_64 -- cmd_reach --containers
expect_has "$OUT" "deb.debian.org="
expect_has "$OUT" "security.ubuntu.com="

run_case reach-containers-arm "$FX_ARM" aarch64 -- cmd_reach --containers
expect_has "$OUT" "deb.debian.org="
expect_not "$OUT" "security.ubuntu.com=" "the arm64 container names"

# --- prepare and fallback keep their contract ----------------------------------
run_case prepare-healthy "$FX_2604" x86_64 -- cmd_prepare
expect_eq "$RC" 0 rc
expect_has "$APT" "update -qq"
expect_not "$OUT" "::warning"
[ -e "$S/bin/apt-fast" ] && ok || bad "no stand-in under BIN_DIR"
[ -e "$S/ci-apt.bounds" ] && bad "a bounds file without bound options" || ok

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"'
run_case prepare-all-dead "$FX_2604" x86_64 -- cmd_prepare
expect_eq "$RC" 0 "rc (prepare never fails the step)"
expect_has "$OUT" "::warning title=apt mirrors unreachable::"
expect_not "$APT" "update" "a refresh with no resolver"

run_case prepare-bounds "$FX_2204" x86_64 -- cmd_prepare --refresh-bound 300 --download-bound 120 --install-bound 210
expect_eq "$RC" 0 rc
expect_has "$(cat "$S/ci-apt.bounds" 2>/dev/null)" "DOWNLOAD_BOUND=120" "the stand-in's bounds file"
expect_has "$(cat "$S/ci-apt.bounds" 2>/dev/null)" "INSTALL_BOUND=210" "the stand-in's bounds file"

run_case fallback-healthy "$FX_2604" x86_64 -- cmd_fallback libvpx-dev pkg-config
expect_eq "$RC" 0 rc
expect_has "$APT" "--no-install-recommends --yes --download-only install libvpx-dev pkg-config"

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"'
run_case fallback-all-dead "$FX_2604" x86_64 -- cmd_fallback libvpx-dev
expect_eq "$RC" 1 rc
expect_has "$OUT" "::error title=Ubuntu mirrors unreachable::"
expect_eq "$(count_of "$OUT" '::error')" 1 "::error count"

# --- the stand-in -------------------------------------------------------------
run_case standin-version "$FX_2604" x86_64 -- cmd_apt_fast --version
expect_has "$OUT" "ci-apt.sh stand-in"

SETUP='echo ALL >"$S/dead-system"; echo ALL >"$S/dead-dig"; mkdir -p "$S/home-runner/cache-apt-pkgs"'
PRE='MODE=standin'
run_case standin-all-dead "$FX_2604" x86_64 -- cmd_apt_fast --yes install hello=1.0
expect_eq "$RC" 100 rc
[ -d "$S/home-runner/cache-apt-pkgs" ] && bad "the action's work dir survived a failed install" || ok

SETUP='echo 1 >"$S/apt-download-rc"; mkdir -p "$S/home-runner/cache-apt-pkgs"'
PRE='MODE=standin'
run_case standin-install-fails "$FX_2604" x86_64 -- cmd_apt_fast --yes install hello=1.0
expect_eq "$RC" 1 rc
[ -d "$S/home-runner/cache-apt-pkgs" ] && bad "the action's work dir survived a failed install" || ok

SETUP='mkdir -p "$S/home-runner/cache-apt-pkgs"; printf "DOWNLOAD_BOUND=44\nINSTALL_BOUND=55\n" >"$S/ci-apt.bounds"'
PRE='MODE=standin; load_bounds_file'
run_case standin-ok-reads-bounds "$FX_2604" x86_64 -- cmd_apt_fast --yes install hello=1.0
expect_eq "$RC" 0 rc
[ -d "$S/home-runner/cache-apt-pkgs" ] && ok || bad "the action's work dir was removed after a good install"
expect_has "$OUT" "bound of 44 s" "the stand-in's download bound from the bounds file"

echo
echo "$cases cases, $pass passed, $failn failed"
if [ "$cases" -ne "$EXPECTED_CASES" ]; then
  echo "FAIL: ran $cases cases, expected $EXPECTED_CASES" >&2
  exit 1
fi
[ "$failn" -eq 0 ]
