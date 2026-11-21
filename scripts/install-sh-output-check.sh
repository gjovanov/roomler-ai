#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# What the served install script says about the release it resolved and the
# daemon it installed (#3881), and about the versions its binaries name
# (#3919), read against stand-ins.
#
# scripts/install.sh is what `curl -fsSL <server>/install.sh | sh -s -- ...`
# runs. It names the release that holds the file it downloads, as the server's
# feed (/api/agent/latest-release) names it, and after an install it prints
# the installed daemon's own `--version`. A re-run that finds the per-user
# daemon running says which version it runs or moves onto, and the tunnel
# role names its CLI's version. These lines are information. What the feed
# and the binaries say is printed only when it fits a small alphabet and 64
# characters, and in fixed words otherwise. No answer may change a command the
# script runs or its exit code.
#
# This runs the REAL script, piped into the shell as the one-liner does: the
# shell reads it from a pipe (bash one byte at a time), so a command in it that
# reads its stdin would eat the rest of the script. The shells: dash (Debian's
# /bin/sh), Debian's bash 5 in POSIX mode, and bash 3.2.57 in POSIX mode, the
# version of macOS's /bin/sh (a Linux build, copied out of the official image
# bash:3.2 as macos-postinstall-check.sh --bash copies it). It runs as a person
# (a privileged half as root) in a throwaway debian:12 container with no
# network. Stand-ins on PATH answer for curl (the feed and the downloads), sudo
# (which runs nothing but `test`, a question), dpkg, apt-get, systemctl,
# installer, launchctl, uname, hostname, the daemon and the tunnel CLI, and
# they record every call; a case can make one of their calls read its stdin
# first, as a binary could, or fail, as dpkg -i does where a dependency is
# missing. A running daemon's own image, /proc/$pid/exe, cannot be a stand-in
# (a script's is its interpreter), so where a case has one running, systemctl
# names a MainPID that leads /proc/$pid/exe to a stand-in.
# Each case checks the lines, the exit code, and that no control byte but
# newline and tab reached its output (stdout and stderr). The cases of one mode
# must record the same calls, byte for byte, whatever the feed's tag or the
# daemon's answer, and under every shell those calls must equal the mode's
# record in install-sh-output-check.calls, beside this script: a command the
# script adds or drops shows there.
# #3931 — the script is one function, main, which its last line alone calls,
# and whose first statement gives the shell /dev/null as stdin: a check reads
# that shape from the text. In every mode one case makes every call of every
# stand-in read its stdin first, and under dash a second one does so with the
# script padded so that one of dash's 8 KiB reads of the pipe ends inside the
# main block: the script runs to its last line, with the mode's calls. And a
# script cut off after each line of the main block, after sampled lines above
# it and after each byte of its last line runs nothing: no stand-in records a
# call. Each cut's exit code is printed.
#
# What it cannot show: a real package install, systemd, launchd, a real feed,
# Apple's /bin/sh, and BSD grep, sed and tr (the Mac cases run Debian's GNU
# tools). The Linux and Mac labs read those.
#
# Usage: scripts/install-sh-output-check.sh [--image IMAGE] [--shell SHELL]
#            [--keep DIR] [--rewrite-calls] [SCRIPT]
#   SCRIPT   the install script to run. Default: the repo's. An older one
#            reads how it behaved, and how its calls differ from the record:
#              git show origin/master:scripts/install.sh > /tmp/install.sh
#              scripts/install-sh-output-check.sh /tmp/install.sh
#   --shell  run the script under one shell only: dash, bash (Debian's bash 5,
#            --posix) or bash3.2 (bash 3.2.57, --posix). Default: all three.
#   --keep   copy each case's records and output to DIR/<shell>/<mode>/<case>/,
#            so that two scripts' calls can be compared with diff -r.
#   --rewrite-calls  write this run's calls over install-sh-output-check.calls,
#            when every other check passes.
#            Right only for a change meant to change the commands a mode runs.
#   --image  the container image. Default debian:12.
# Needs docker. Exits non-zero when any case fails, and then prints each failed
# case's output.
set -euo pipefail

if [ "${1:-}" != --in-container ]; then
    image=debian:12
    script=""
    shells="dash bash bash3.2"
    keep=""
    rewrite=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --image) image="$2"; shift 2 ;;
            --shell)
                case "$2" in
                    dash|bash|bash3.2) shells="$2" ;;
                    *) echo "--shell takes dash, bash or bash3.2" >&2; exit 2 ;;
                esac
                shift 2 ;;
            --keep) keep="$2"; shift 2 ;;
            --rewrite-calls) rewrite=1; shift ;;
            -h|--help) sed -n '3,59p' "$0"; exit 0 ;;
            *) script="$1"; shift ;;
        esac
    done
    here=$(cd "$(dirname "$0")" && pwd)
    script=${script:-$here/install.sh}
    script=$(cd "$(dirname "$script")" && pwd)/$(basename "$script")
    [ -f "$script" ] || { echo "no script at $script" >&2; exit 2; }
    calls=$here/install-sh-output-check.calls
    echo "install script: $script"
    echo "image: $image"
    # The verdict is the last line as well as docker's status: a docker CLI
    # that is a shim (WSL's is one) can answer 0 for a container that exited 1.
    log=$(mktemp)
    # The inner half runs only beside this run's own token (see below).
    tokfile=$(mktemp)
    token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    printf '%s' "$token" >"$tokfile"
    work=$(mktemp -d)
    trap 'rm -f "$log" "$tokfile"; rm -rf "$work"' EXIT
    extra=()
    if [ -n "$keep" ]; then
        mkdir -p "$keep"
        keep=$(cd "$keep" && pwd)
        extra+=(-v "$keep:/keep")
    fi
    if [ -f "$calls" ]; then
        extra+=(-v "$calls:/src/calls:ro")
    fi
    if [ "$rewrite" = 1 ]; then
        mkdir "$work/calls-out"
        extra+=(-v "$work/calls-out:/calls-out")
    fi
    case " $shells " in
        *" bash3.2 "*)
            # As macos-postinstall-check.sh --bash takes it: the binary and
            # every library its ldd names, links followed, copied out of the
            # official image. The directory is open to the person the script
            # runs as.
            mkdir -m 755 "$work/bash32"
            docker run --rm --network none -v "$work/bash32:/out" bash:3.2 sh -c \
                'b=$(command -v bash) && cp "$b" /out/bash && for l in $(ldd "$b" | grep -o "/[^ ]*"); do cp "$l" /out/ || exit; done' ||
                { echo "could not copy bash out of the image bash:3.2" >&2; exit 2; }
            extra+=(-v "$work/bash32:/opt/bash32:ro")
            ;;
    esac
    set +e
    docker run --rm --network none \
        -v "$script:/src/install.sh:ro" \
        -v "$here/$(basename "$0"):/src/check.sh:ro" \
        -v "$tokfile:/src/once-token:ro" \
        ${extra[@]+"${extra[@]}"} \
        "$image" bash /src/check.sh --in-container "$token" "$shells" "$rewrite" 2>&1 | tee "$log"
    st=${PIPESTATUS[0]}
    set -e
    if [ "$st" = 0 ] && [ "$(tail -n 1 "$log")" = "every case passed" ]; then
        if [ "$rewrite" = 1 ]; then
            cp "$work/calls-out/calls" "$calls"
            echo "rewrote $calls"
        fi
        exit 0
    fi
    echo "install-sh-output-check: FAILED (docker exit $st)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# In the container. This half writes stand-ins over /usr/bin/roomlerd and under
# /Library, so it refuses to run anywhere but the container this script starts:
# that one alone holds /src/once-token, the token this run made.
# ---------------------------------------------------------------------------
if [ ! -f /src/once-token ] || [ -z "${2:-}" ] || [ "$(cat /src/once-token)" != "$2" ]; then
    echo "refusing: --in-container runs only inside the container this script starts" >&2
    exit 2
fi
[ "$(uname -s)" = Linux ] || { echo "refusing: not a Linux container" >&2; exit 2; }
SHELLS=${3:-dash bash bash3.2}
REWRITE=${4:-0}
# Bash 3.2, when it was copied in: a musl build, whose loader goes where the
# binary looks for it and finds the rest beside the binary.
if [ -x /opt/bash32/bash ]; then
    for l in /opt/bash32/ld-musl-*.so.1; do
        [ -e "$l" ] || continue
        cp "$l" "/lib/${l##*/}"
        printf '/opt/bash32\n' >"/etc/$(basename "$l" .so.1).path"
    done
fi
case " $SHELLS " in
    *" bash3.2 "*)
        [ -x /opt/bash32/bash ] || { echo "refusing: bash 3.2 was not copied in" >&2; exit 2; }
        echo "bash3.2: $(/opt/bash32/bash -c 'echo "$BASH_VERSION"')" ;;
esac
echo "shells: $SHELLS ($(bash --version | head -n 1))"

S=/standin
BIN=$S/bin     # the stand-ins every case has
DPKG=$S/dpkg   # dpkg and apt-get, on PATH only where the host is a Debian
TOOLS=$S/tools # the real tools the script uses, and nothing else
C=$S/case      # this case's answers and records
RUNS=$S/runs   # every case's records, by shell, mode and case
mkdir -p "$BIN" "$DPKG" "$TOOLS" "$RUNS"

# The person who runs the one-liner: not root, so every privileged step goes
# through the sudo stand-in, which records it and runs nothing.
grep -q '^tester:' /etc/passwd || {
    echo 'tester:x:1000:1000:tester:/home/tester:/bin/sh' >>/etc/passwd
    echo 'tester:x:1000:' >>/etc/group
}
mkdir -p /home/tester && chown 1000:1000 /home/tester

for t in awk basename cat chmod cp cut dirname env find grep gzip head id ln ls \
    mkdir mv od rm rmdir sed sha256sum sort tail tar tee touch tr wc xargs; do
    ln -sf "$(command -v "$t")" "$TOOLS/$t"
done
ln -sf /usr/bin/dash "$TOOLS/sh"

# One stand-in for every tool the cases replace. It acts on the name it is
# called by and records its argv, one call a line, each argument in [ ].
cat >"$S/standin" <<'EOF'
#!/bin/bash
me=${0##*/}
C=/standin/case
line=$me
for a in "$@"; do line="$line [$a]"; done
case "$me" in
    curl | roomlerd | roomler) printf '%s\n' "$line" >>"$C/$me.log" ;;
    uname | hostname | mktemp | stat | sleep) ;; # answers, not actions
    *) printf '%s\n' "$line" >>"$C/calls.log" ;;
esac
# #3931 — a case can make every call of every stand-in read its stdin first.
if [ -f "$C/stdin.all" ]; then cat >/dev/null; fi
# #3925 — a case can make one call read its stdin first, as a binary could: the
# call of this name whose line holds the words in stdin.<name>.
if [ -f "$C/stdin.$me" ]; then
    case "$line" in *"$(cat "$C/stdin.$me")"*) cat >/dev/null ;; esac
fi
# #3926 — and make one call fail, as `dpkg -i` does where a dependency is
# missing, which sends the script on to apt-get: the call of this name whose
# line holds the words in fails.<name>.
if [ -f "$C/fails.$me" ]; then
    case "$line" in *"$(cat "$C/fails.$me")"*) exit 1 ;; esac
fi
case "$me" in
    uname)
        case "${1:-}" in
            -m) cat "$C/arch" ;;
            -r) echo 6.1.0 ;;
            *) cat "$C/os" ;;
        esac ;;
    hostname) echo check-host ;;
    stat)
        if [ "$*" = "-f %Su /dev/console" ]; then echo tester; else exec /usr/bin/stat "$@"; fi ;;
    mktemp)
        # The same names on every run, so that two scripts' calls compare.
        d=0 t=""
        for a in "$@"; do case "$a" in -d) d=1 ;; -*) ;; *) t=$a ;; esac; done
        [ -n "$t" ] || t=/tmp/tmp.XXXXXXXXXX
        case "$t" in /*) ;; *) t=/tmp/$t ;; esac
        n=$(($(cat "$C/mktemp.n" 2>/dev/null || echo 0) + 1))
        echo "$n" >"$C/mktemp.n"
        p=$(printf '%s' "$t" | sed 's/X*$//')$(printf '%06d' "$n")
        if [ "$d" = 1 ]; then mkdir -m 700 "$p"; else : >"$p"; fi
        printf '%s\n' "$p" ;;
    roomlerd)
        if [ "${1:-}" = --version ]; then
            # A case can make it read its stdin first, as a daemon could.
            if [ -f "$C/version.reads-stdin" ]; then cat >/dev/null; fi
            cat "$C/version.out"
            exit "$(cat "$C/version.rc")"
        fi
        # Its enroll's help names --one-row, as a release's since FR-43 P3d.
        if [ "$*" = "enroll --help" ]; then echo "      --one-row"; fi ;;
    sudo)
        # `test` is a question, not an action, so it is answered for real: a
        # Mac case then finds no root config and enrolls the root half.
        if [ "${1:-}" = test ]; then shift; test "$@"; exit; fi ;;
    roomler)
        if [ "${1:-}" = --version ]; then
            # A case can make its Nth --version (this one is logged above)
            # read its stdin first.
            if [ -f "$C/cli.reads-stdin" ] &&
                [ "$(grep -cx 'roomler \[--version\]' "$C/roomler.log")" = "$(cat "$C/cli.reads-stdin")" ]; then
                cat >/dev/null
            fi
            if [ -f "$C/cli.out" ]; then cat "$C/cli.out"; else echo "roomler 0.4.215"; fi
        fi ;;
    systemctl)
        # A case with a per-user daemon running names its MainPID.
        if [ "$*" = "--user show -p MainPID --value roomler.service" ] && [ -f "$C/mainpid" ]; then
            cat "$C/mainpid"
        fi ;;
    exe)
        # That daemon's own image, reached as /proc/$pid/exe.
        case "${1:-}" in
            --version)
                if [ -f "$C/running.reads-stdin" ]; then cat >/dev/null; fi
                cat "$C/running.out" ;;
            update-helper) if [ -f "$C/running.caps" ]; then cat "$C/running.caps"; fi ;;
        esac ;;
    curl)
        out="" url=""
        while [ $# -gt 0 ]; do
            case "$1" in
                -o) out=$2; shift 2; continue ;;
                http*) url=$1 ;;
            esac
            shift
        done
        case "$url" in
            */api/agent/latest-release) src=$C/feed.json ;;
            */api/tunnel/installer/*/health*) src=/dev/null ;;
            https://dl.example/* | */api/tunnel/installer/*) src=/standin/artifact ;;
            *) exit 22 ;;
        esac
        if [ -n "$out" ]; then cat "$src" >"$out"; else cat "$src"; fi ;;
esac
exit 0
EOF
chmod 755 "$S/standin"
for t in curl sudo systemctl installer launchctl uname hostname stat mktemp sleep roomler; do
    ln -sf "$S/standin" "$BIN/$t"
done
for t in dpkg apt-get; do ln -sf "$S/standin" "$DPKG/$t"; done
# The daemon where the script runs it: the .deb's path, and the macOS bundle's.
ln -sf "$S/standin" /usr/bin/roomlerd
mkdir -p /Library/Roomler/roomlerd.app/Contents/MacOS
ln -sf "$S/standin" /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd
# A running per-user daemon's MainPID: /proc/$PID/exe is $S/running/exe.
mkdir -p "$S/running"
ln -sf "$S/standin" "$S/running/exe"
PID=..$S/running
# What a privileged half takes with --config-from: the config its caller
# enrolled in its own process (the stand-in daemon never reads it).
printf 'agent_id = "check-agent"\n' >"$S/enrolled.toml"

# The feed: compact JSON as the server writes it, newest release first. Every
# asset is the same small file, so the digest checks out, and an asset's URL
# carries no tag, so that every variant of a mode downloads the same URL.
printf 'a stand-in for a release asset\n' >"$S/artifact"
DIGEST=$(sha256sum "$S/artifact" | cut -d' ' -f1)
SIZE=$(wc -c <"$S/artifact")
asset() {
    printf '{"name":"%s","browser_download_url":"https://dl.example/%s","size":%s,"digest":"sha256:%s"}' \
        "$1" "$1" "$SIZE" "$DIGEST"
}
LINUX="$(asset roomlerd-x86_64-unknown-linux-gnu.deb),$(asset roomlerd-x86_64-unknown-linux-gnu.tar.gz)"
LINUX="$LINUX,$(asset roomlerd-aarch64-unknown-linux-gnu.deb),$(asset roomlerd-aarch64-unknown-linux-gnu.tar.gz)"
TGZ="$(asset roomlerd-x86_64-unknown-linux-gnu.tar.gz),$(asset roomlerd-aarch64-unknown-linux-gnu.tar.gz)"
MAC=$(asset roomlerd-aarch64-apple-darwin.pkg)
rel() {
    printf '{"tag_name":"%s","draft":false,"prerelease":false,"published_at":"2026-11-12T10:00:00Z","assets":[%s]}' "$1" "$2"
}
ESC=$'\033'
NOTUTF8=$'\351' # a Latin-1 e-acute: not text in a UTF-8 locale
FF=$'\377'
LONG="agent-v$(printf '9%.0s' $(seq 193))" # 200 characters, every one in the alphabet
OLD=$(rel agent-v0.4.220 "$LINUX,$MAC")
feed() {
    case "$1" in
        normal) printf '[%s,%s]' "$(rel agent-v0.4.221 "$LINUX,$MAC")" "$OLD" ;;
        # The newest release lacks this host's file: the script takes it from
        # the older one, and that is the release to name.
        lacks-linux) printf '[%s,%s]' "$(rel agent-v0.4.221 "$MAC")" "$OLD" ;;
        lacks-mac) printf '[%s,%s]' "$(rel agent-v0.4.221 "$LINUX")" "$OLD" ;;
        esc) printf '[%s,%s]' "$(rel "agent-v0.4.221$ESC[31m" "$LINUX,$MAC")" "$OLD" ;;
        empty) printf '[%s,%s]' "$(rel "" "$LINUX,$MAC")" "$OLD" ;;
        long) printf '[%s,%s]' "$(rel "$LONG" "$LINUX,$MAC")" "$OLD" ;;
        # The release that holds the file has no tag_name, or one that a `{`
        # splits: unreadable, and never the newer release's tag.
        notag) printf '[%s,%s]' "$(rel agent-v0.4.222 "$MAC")" \
            "{\"draft\":false,\"prerelease\":false,\"published_at\":\"2026-11-12T10:00:00Z\",\"assets\":[$LINUX]}" ;;
        brace) printf '[%s,%s]' "$(rel agent-v0.4.222 "$MAC")" "$(rel 'agent{v0.4.221' "$LINUX")" ;;
        # The release that holds the file has a title before its tag, or a
        # byte on its line that is not text in the person's UTF-8 locale
        # (GNU grep withholds such a line there): never the newer one's tag.
        titled) printf '[%s,%s]' "$(rel agent-v0.4.222 "$MAC")" \
            "{\"name\":\"Roomler 0.4.221\",\"tag_name\":\"agent-v0.4.221\",\"assets\":[$LINUX]}" ;;
        badbody) printf '[%s,%s]' "$(rel agent-v0.4.222 "$MAC")" \
            "{\"tag_name\":\"agent-v0.4.221\",\"body\":\"caf$NOTUTF8\",\"assets\":[$LINUX]}" ;;
        badtag) printf '[%s,%s]' "$(rel agent-v0.4.222 "$MAC")" "$(rel "agent-v0.4.221$FF" "$LINUX")" ;;
        # No .deb anywhere: a Debian host falls back to the tarball.
        nodeb) printf '[%s,%s]' "$(rel agent-v0.4.221 "$TGZ,$MAC")" "$(rel agent-v0.4.220 "$TGZ,$MAC")" ;;
    esac
}

R="==> release named by the server's feed:"
V="==> installed daemon's own --version:"
N="==> NOTE: the installed daemon's"
UNREADABLE="unreadable (empty, over 64 characters, or outside A-Z a-z 0-9 . _ + -)"
VU="a version that is $UNREADABLE" # the same words, where a line names a version

FAILED=0
TOTAL=0
SH=""
# run CASE OS ARCH DEBIAN FEED ANSWER RC ARGS...: one run of the script under
# $SH. CASE is MODE/VARIANT; ANSWER and RC are what the daemon's --version
# prints and exits with, and READS_STDIN=1 before `run` makes it read its
# stdin first. RUNNING=<answer> before `run` (empty counts) puts a per-user
# daemon there, already running, whose own image prints that to --version and
# RUNNING_CAPS to update-helper --capabilities; RUNNING_READS_STDIN=1 makes
# that --version read its stdin first. CLI=<answer> is what the tunnel CLI
# prints to --version, and CLI_READS_STDIN=<n> makes its nth --version read
# its stdin first. READS='<stand-in> <words>' makes that stand-in read its
# stdin first in the call whose line holds the words; FAILS, in the same
# form, makes it exit 1 in that call. HAS_CONFIG=1 puts an
# earlier install's config at /etc/roomler/config.toml, HAS_PLIST=1 the root
# half's LaunchDaemon where the package puts it, and AS_ROOT=1 runs the script
# as root, as a privileged half runs. READS_ALL=1 makes every call of every
# stand-in read its stdin first, and SCRIPT=<file> pipes that file in place of
# the script (#3931). The records land in $RUNS/$SH/CASE.
run() {
    CASE=$1 os=$2 arch=$3 debian=$4 feedname=$5 answer=$6 answer_rc=$7
    shift 7
    rm -rf "$C" /tmp/roomler-install.* /etc/roomler /Library/LaunchDaemons
    mkdir -p "$C"
    if [ "${HAS_CONFIG:-0}" = 1 ]; then
        mkdir -p /etc/roomler
        printf 'agent_id = "check-earlier"\n' >/etc/roomler/config.toml
    fi
    if [ "${HAS_PLIST:-0}" = 1 ]; then
        mkdir -p /Library/LaunchDaemons
        : >/Library/LaunchDaemons/com.roomler.daemon.plist
    fi
    echo "$os" >"$C/os"
    echo "$arch" >"$C/arch"
    feed "$feedname" >"$C/feed.json"
    printf '%s' "$answer" >"$C/version.out"
    echo "$answer_rc" >"$C/version.rc"
    [ "${READS_STDIN:-0}" = 0 ] || : >"$C/version.reads-stdin"
    if [ -n "${RUNNING+set}" ]; then
        printf '%s\n' "$PID" >"$C/mainpid"
        printf '%s' "$RUNNING" >"$C/running.out"
        [ -z "${RUNNING_CAPS:-}" ] || printf '%s' "$RUNNING_CAPS" >"$C/running.caps"
        [ "${RUNNING_READS_STDIN:-0}" = 0 ] || : >"$C/running.reads-stdin"
    fi
    [ -z "${CLI+set}" ] || printf '%s' "$CLI" >"$C/cli.out"
    [ -z "${CLI_READS_STDIN:-}" ] || printf '%s\n' "$CLI_READS_STDIN" >"$C/cli.reads-stdin"
    [ -z "${READS:-}" ] || printf '%s' "${READS#* }" >"$C/stdin.${READS%% *}"
    [ -z "${FAILS:-}" ] || printf '%s' "${FAILS#* }" >"$C/fails.${FAILS%% *}"
    [ "${READS_ALL:-0}" = 0 ] || : >"$C/stdin.all"
    : >"$C/calls.log"
    : >"$C/curl.log"
    : >"$C/roomlerd.log"
    : >"$C/roomler.log"
    chown -R 1000:1000 "$C"
    path=$BIN:$TOOLS
    [ "$debian" = 0 ] || path=$BIN:$DPKG:$TOOLS
    case "$SH" in
        dash) shell=(/usr/bin/dash) ;;
        bash) shell=(/usr/bin/bash --posix) ;;
        bash3.2) shell=(/opt/bash32/bash --posix) ;;
    esac
    who=tester uid=1000 home=/home/tester
    [ "${AS_ROOT:-0}" = 0 ] || who=root uid=0 home=/root
    set +e
    cat "${SCRIPT:-/src/install.sh}" 2>/dev/null | setpriv --reuid=$uid --regid=$uid --clear-groups \
        env -i HOME=$home USER=$who LOGNAME=$who LANG=C.UTF-8 PATH="$path" \
        "${shell[@]}" -s -- --server https://roomler.example "$@" >"$C/out.txt" 2>&1
    echo "${PIPESTATUS[1]}" >"$C/rc"
    set -e
    WHY=""
}
want() { grep -Fxq -- "$1" "$C/out.txt" || WHY="$WHY"$'\n'"    missing: $1"; }
lines() {
    got=$(grep -cF -- "$1" "$C/out.txt" || true)
    [ "$got" = "$2" ] || WHY="$WHY"$'\n'"    $got line(s) carry '$1', want $2"
}
done_case() {
    rc=$(cat "$C/rc")
    [ "$rc" = 0 ] || WHY="$WHY"$'\n'"    exit code $rc, want 0"
    # Text, newline and tab only, on stdout and stderr alike: an ESC or any
    # other control byte there is what the alphabet is for. Bytes from 0x80
    # up stay allowed, as the script's own UTF-8 text.
    ctl=$(LC_ALL=C tr -d '\n\t\040-\176\200-\377' <"$C/out.txt" | wc -c)
    [ "$ctl" -eq 0 ] || WHY="$WHY"$'\n'"    $ctl control byte(s) besides newline and tab reached the output"
    mkdir -p "$RUNS/$SH/$CASE"
    cp "$C"/*.log "$C/out.txt" "$C/rc" "$RUNS/$SH/$CASE/"
    TOTAL=$((TOTAL + 1))
    if [ -z "$WHY" ]; then
        echo "PASS $SH $CASE"
    else
        FAILED=$((FAILED + 1))
        echo "FAIL $SH $CASE:$WHY"
        sed "s/$ESC/<ESC>/g; s/^/    | /" "$C/out.txt"
    fi
}
# One release line, one version line, and the NOTE N times (0 or 1).
says() { want "$R $1"; want "$V $2"; lines "$R" 1; lines "$V" 1; lines "$N" "$3"; }
# What a per-user re-run says of the daemon it found running, naming $1.
same() { want "==> the running daemon (pid $PID) already runs $1: nothing to restart"; }
self() { want "==> the running daemon (pid $PID) restarts onto $1 by itself, once nothing it serves would be cut"; }
restart() {
    want "==> restarting the running daemon (pid $PID): it runs $1, which cannot restart onto a new binary by itself (a remote or SSH session to this device drops for a few seconds)"
}

# #3931 — the script's shape, read from its text: one function, main, opened
# below nothing but comments and blank lines, whose first statement gives the
# shell /dev/null as stdin and whose last statement is exit; the line before
# the last closes it, and the last line, the only call, is `main "$@"` with
# nothing after its newline. So the shell has read all of the script before
# the first command runs, and no line after the call is ever run.
WHY=""
open=$(grep -n -m1 '^main() {' /src/install.sh | cut -d: -f1 || true)
if [ -z "$open" ]; then
    WHY="$WHY"$'\n'"    no line opens the function: main() {"
else
    [ "$(grep -c '^main() {' /src/install.sh)" = 1 ] || WHY="$WHY"$'\n'"    main() { opens more than once"
    above=$(head -n $((open - 1)) /src/install.sh | grep -cv '^#\|^[[:space:]]*$' || true)
    [ "$above" = 0 ] || WHY="$WHY"$'\n'"    $above line(s) above main() { are neither comment nor blank: they run before the script is read"
    # The first statement, on the opening line after the brace or on the first
    # line after it that is neither comment nor blank.
    first=$(sed -n "${open}p" /src/install.sh | sed 's/^main() {[[:space:]]*//; s/^#.*//')
    [ -n "$first" ] || first=$(tail -n +$((open + 1)) /src/install.sh | grep -v '^#\|^[[:space:]]*$' | head -n 1 || true)
    case "$first" in
        'exec </dev/null'*) ;;
        *) WHY="$WHY"$'\n'"    main's first statement is not exec </dev/null: $first" ;;
    esac
fi
[ "$(tail -c 1 /src/install.sh | od -An -tx1 | tr -d ' ')" = 0a ] ||
    WHY="$WHY"$'\n'"    the script does not end with the newline of its last line"
[ "$(tail -n 1 /src/install.sh)" = 'main "$@"' ] ||
    WHY="$WHY"$'\n'"    the last line is not main \"\$@\": $(tail -n 1 /src/install.sh)"
[ "$(tail -n 2 /src/install.sh | head -n 1)" = '}' ] ||
    WHY="$WHY"$'\n'"    the line before the last does not close main: $(tail -n 2 /src/install.sh | head -n 1)"
calls=$(grep -cE '^[[:space:]]*main([[:space:]]|;|$)' /src/install.sh || true)
[ "$calls" = 1 ] || WHY="$WHY"$'\n'"    $calls line(s) call main, want 1, the last"
last_statement=$(head -n -2 /src/install.sh | grep -v '^[[:space:]]*#\|^[[:space:]]*$' | tail -n 1 || true)
[ "$last_statement" = exit ] ||
    WHY="$WHY"$'\n'"    main's last statement is not exit: $last_statement"
TOTAL=$((TOTAL + 1))
if [ -z "$WHY" ]; then
    echo "PASS the script is one function, opened at line $open below comments alone, stdin /dev/null first, exit last, and its last line alone calls it"
else
    FAILED=$((FAILED + 1))
    echo "FAIL the script is not one function that its last line alone calls:$WHY"
fi

# #3931 — the script padded so that one of dash's reads of the pipe ends inside
# its main block. Dash reads a pipe 8192 bytes at a time, so a command there
# that reads its stdin would eat what lies past that read. The padding is
# comment lines after the first line, and it puts a multiple of 8192 bytes
# halfway between the main block's heading and the end.
pad_lines() { # N >= 2: comment lines of N bytes in all, none over 80
    local n=$1 l
    while [ "$n" -gt 0 ]; do
        l=$n
        [ "$l" -le 80 ] || l=80
        [ $((n - l)) != 1 ] || l=$((l - 1))
        printf '#%*s\n' $((l - 2)) '' | tr ' ' p
        n=$((n - l))
    done
}
main_at=$(LC_ALL=C grep -b -m1 '^# ─── main ─' /src/install.sh | cut -d: -f1)
size=$(wc -c </src/install.sh)
mid=$(((main_at + size) / 2))
pad=$(((8192 - mid % 8192) % 8192))
[ "$pad" -ge 2 ] || pad=$((pad + 8192))
PADDED=$S/install-padded.sh
{ head -n 1 /src/install.sh; pad_lines "$pad"; tail -n +2 /src/install.sh; } >"$PADDED"
echo "padded for dash: $pad bytes, the main block at bytes $((main_at + pad))-$((size + pad)), a read ending at $((mid + pad))"

# #3931 — the script cut off: after every 97th line above its main block, after
# each line of the main block, and after each byte of its last line but the
# newline. `cut_off` (in the loop below) pipes each one in.
CUTS=$S/cuts
mkdir -p "$CUTS"
nlines=$(wc -l </src/install.sh)
main_line=$(grep -n -m1 '^# ─── main ─' /src/install.sh | cut -d: -f1)
for k in $(seq 1 97 $((main_line - 1))) $(seq "$main_line" $((nlines - 1))); do
    head -n "$k" /src/install.sh >"$CUTS/$(printf 'line-%05d' "$k")"
done
last_line=$(tail -n 1 /src/install.sh)
for b in $(seq 1 $((${#last_line} - 1))); do
    { head -n $((nlines - 1)) /src/install.sh; printf '%s' "${last_line:0:b}"; } >"$CUTS/$(printf 'last-%03d' "$b")"
done
# cut_off MODE ARGS...: as run, each cut script in turn; none may record a call.
cut_off() {
    local m=$1 f k what n0 n1 n2 n127 nother
    shift
    n0=0 n1=0 n2=0 n127=0 nother=0
    for f in "$CUTS"/*; do
        SCRIPT=$f run "cut/$m" "$@"
        rc=$(cat "$C/rc")
        case "${f##*/}" in
            line-*) k=$((10#${f##*/line-})); what="after line $k of $nlines ($(sed -n "${k}p" /src/install.sh | cut -c1-40))" ;;
            last-*) what="inside the last line, after '$(cat "$f" | tail -n 1)'" ;;
        esac
        case "$rc" in 0) n0=$((n0 + 1)) ;; 1) n1=$((n1 + 1)) ;; 2) n2=$((n2 + 1)) ;; 127) n127=$((n127 + 1)) ;; *) nother=$((nother + 1)) ;; esac
        TOTAL=$((TOTAL + 1))
        ran=$(cat "$C/calls.log" "$C/curl.log" "$C/roomlerd.log" "$C/roomler.log")
        if [ -z "$ran" ]; then
            echo "PASS $SH cut $m $what: exit $rc, nothing ran"
        else
            FAILED=$((FAILED + 1))
            echo "FAIL $SH cut $m $what: exit $rc, and it ran:"
            printf '%s\n' "$ran" | sed 's/^/    | /'
        fi
    done
    echo "cut $m under $SH: $(ls "$CUTS" | wc -l) cuts; exit 0 x$n0, 1 x$n1, 2 x$n2, 127 x$n127, other x$nother"
}
# every MODE ARGS...: as run, the case MODE/stdin-all, every call of every
# stand-in reading its stdin first; under dash MODE/stdin-all-padded as well.
every() {
    local m=$1
    shift
    READS_ALL=1 run "$m/stdin-all" "$@"
    want "==> done."
    done_case
    [ "$SH" = dash ] || return 0
    READS_ALL=1 SCRIPT=$PADDED run "$m/stdin-all-padded" "$@"
    want "==> done."
    done_case
}

for SH in $SHELLS; do
    # A Debian host, download only: the release line names the release that
    # holds the .deb, or says in fixed words that its tag is unreadable.
    dl=(--role daemon --download-only)
    none="none (download-only installs nothing)"
    run deb-dl/normal Linux x86_64 1 normal '' 0 "${dl[@]}"; says agent-v0.4.221 "$none" 0; done_case
    run deb-dl/lacks Linux x86_64 1 lacks-linux '' 0 "${dl[@]}"; says agent-v0.4.220 "$none" 0; done_case
    run deb-dl/esc Linux x86_64 1 esc '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    run deb-dl/empty Linux x86_64 1 empty '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    run deb-dl/long Linux x86_64 1 long '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    run deb-dl/notag Linux x86_64 1 notag '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    run deb-dl/brace Linux x86_64 1 brace '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    # The holding release's own line, whatever it carries: a title before its
    # tag reads as unreadable, and a byte that is not UTF-8 text hides nothing.
    run deb-dl/titled Linux x86_64 1 titled '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    run deb-dl/badbody Linux x86_64 1 badbody '' 0 "${dl[@]}"; says agent-v0.4.221 "$none" 0; done_case
    run deb-dl/badtag Linux x86_64 1 badtag '' 0 "${dl[@]}"; says "$UNREADABLE" "$none" 0; done_case
    # A Debian host with no .deb in the feed takes the tarball, and names it.
    run deb-fallback/nodeb Linux x86_64 1 nodeb '' 0 "${dl[@]}"; says agent-v0.4.221 "$none" 0; done_case
    # No dpkg: the tarball.
    run tgz-dl/normal Linux x86_64 0 normal '' 0 "${dl[@]}"; says agent-v0.4.221 "$none" 0; done_case
    run tgz-dl/lacks Linux x86_64 0 lacks-linux '' 0 "${dl[@]}"; says agent-v0.4.220 "$none" 0; done_case
    # A Mac: the .pkg.
    run pkg-dl/normal Darwin arm64 0 normal '' 0 "${dl[@]}"; says agent-v0.4.221 "$none" 0; done_case
    run pkg-dl/lacks Darwin arm64 0 lacks-mac '' 0 "${dl[@]}"; says agent-v0.4.220 "$none" 0; done_case

    # A --system install: after it, the installed daemon's own --version. An
    # answer that is not readable, empty, or a failure is said in fixed words,
    # and the NOTE comes only where both are readable and differ.
    sys=(--role daemon --system --token check-token)
    bin=/usr/bin/roomlerd
    run deb-system/match Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; done_case
    run deb-system/older Linux x86_64 1 normal $'roomlerd 0.4.220\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.220 1
    want "$N 0.4.220 differs from the release this script resolved, agent-v0.4.221"; done_case
    run deb-system/exit1 Linux x86_64 1 normal '' 1 "${sys[@]}"
    says agent-v0.4.221 "no answer ($bin --version exited 1)" 0; done_case
    run deb-system/silent Linux x86_64 1 normal '' 0 "${sys[@]}"
    says agent-v0.4.221 "no answer ($bin --version printed nothing)" 0; done_case
    run deb-system/esc Linux x86_64 1 normal "roomlerd 0.4.221$ESC[2J"$'\n' 0 "${sys[@]}"
    says agent-v0.4.221 "$UNREADABLE" 0; done_case
    run deb-system/long Linux x86_64 1 normal "roomlerd $LONG"$'\n' 0 "${sys[@]}"
    says agent-v0.4.221 "$UNREADABLE" 0; done_case
    run deb-system/feed-esc Linux x86_64 1 esc $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says "$UNREADABLE" 0.4.221 0; done_case
    # A daemon whose --version reads its stdin, which in the one-liner is the
    # rest of the script: the script gives it none, and still ends.
    READS_STDIN=1 run deb-system/reads-stdin Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    # #3925 — the token enrollment every token install runs, here through
    # sudo, reads its stdin: the script gives it none, and still ends.
    READS='sudo [enroll]' run deb-system/reads-stdin-enroll Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    # #3926 — the package install, dpkg through sudo, reads its stdin, as its
    # configuration-file question would: the script gives it none, and still
    # ends.
    READS='sudo [dpkg]' run deb-system/reads-stdin-dpkg Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    # #3926 — dpkg fails (a dependency is missing) and apt-get installs the
    # package, its dpkg asking the same question; then apt-get reads its stdin.
    FAILS='sudo [dpkg]' run deb-system-apt/normal Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    FAILS='sudo [dpkg]' READS='sudo [apt-get] [install]' \
        run deb-system-apt/reads-stdin Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 "${sys[@]}"
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    # The per-user install, the desktop default, and a Mac's.
    run deb-user/match Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; done_case
    # #3925 — the enrollment, run as the person, and then the update helper's
    # question to the installed daemon (update-helper --capabilities), each
    # reads its stdin.
    READS='roomlerd [enroll]' run deb-user/reads-stdin-enroll Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    READS='roomlerd [update-helper]' run deb-user/reads-stdin-caps Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    run pkg/match Darwin arm64 0 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; done_case
    run pkg/exit1 Darwin arm64 0 normal '' 1 --role daemon --token check-token
    says agent-v0.4.221 "no answer (/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd --version exited 1)" 0
    done_case
    READS_STDIN=1 run pkg/reads-stdin Darwin arm64 0 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; want "==> done."; done_case

    # #3919 — a per-user re-run finds the daemon already running, and says
    # which version it runs or moves onto: as the binary names it when that
    # fits the alphabet, in fixed words otherwise. The script still compares
    # the two answers as they came, so the cases of a mode run the same calls.
    usr=(--role daemon --token check-token)
    new=$'roomlerd 0.4.221\n'
    old=$'roomlerd 0.4.220\n'
    # The same answer from the running image: nothing to restart.
    RUNNING=$new run deb-user-same/normal Linux x86_64 1 normal "$new" 0 "${usr[@]}"; same 0.4.221; done_case
    a="roomlerd 0.4.221$ESC[2J"$'\n'
    RUNNING=$a run deb-user-same/esc Linux x86_64 1 normal "$a" 0 "${usr[@]}"; same "$VU"; done_case
    a=$'roomlerd \n'
    RUNNING=$a run deb-user-same/empty Linux x86_64 1 normal "$a" 0 "${usr[@]}"; same "$VU"; done_case
    a="roomlerd $LONG"$'\n'
    RUNNING=$a run deb-user-same/long Linux x86_64 1 normal "$a" 0 "${usr[@]}"
    same "$VU"; lines "$LONG" 0; done_case
    # #3923 — the installed binary's --version, then the running image's, reads
    # its stdin, which in the one-liner is the rest of the script: the script
    # gives it none, and still ends.
    READS_STDIN=1 RUNNING=$new run deb-user-same/reads-stdin Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    same 0.4.221; says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    RUNNING_READS_STDIN=1 RUNNING=$new run deb-user-same/running-reads-stdin Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    same 0.4.221; says agent-v0.4.221 0.4.221 0; want "==> done."; done_case
    # Another answer, from a daemon that restarts onto a new version by
    # itself (it lists restart-on-replace): left to it.
    cap=$'restart-on-replace\n'
    RUNNING=$old RUNNING_CAPS=$cap run deb-user-self/normal Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    self 0.4.221; done_case
    RUNNING=$old RUNNING_CAPS=$cap run deb-user-self/esc Linux x86_64 1 normal "roomlerd 0.4.221$ESC[2J"$'\n' 0 "${usr[@]}"
    self "$VU"; done_case
    RUNNING=$old RUNNING_CAPS=$cap run deb-user-self/empty Linux x86_64 1 normal $'roomlerd \n' 0 "${usr[@]}"
    self "$VU"; done_case
    RUNNING=$old RUNNING_CAPS=$cap run deb-user-self/long Linux x86_64 1 normal "roomlerd $LONG"$'\n' 0 "${usr[@]}"
    self "$VU"; lines "$LONG" 0; done_case
    # #3925 — the running image's answer to update-helper --capabilities reads
    # its stdin.
    READS='exe [update-helper]' RUNNING=$old RUNNING_CAPS=$cap run deb-user-self/reads-stdin-caps Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    self 0.4.221; want "==> done."; done_case
    # Another answer, from a daemon that cannot: restarted, and the line names
    # what it runs. One that answers nothing runs "an unknown version", as before.
    RUNNING=$old run deb-user-restart/normal Linux x86_64 1 normal "$new" 0 "${usr[@]}"; restart 0.4.220; done_case
    RUNNING="roomlerd 0.4.220$ESC[2J"$'\n' run deb-user-restart/esc Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    restart "$VU"; done_case
    RUNNING='' run deb-user-restart/empty Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    restart "an unknown version"; done_case
    RUNNING="roomlerd $LONG"$'\n' run deb-user-restart/long Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    restart "$VU"; lines "$LONG" 0; done_case

    # #3925 — the privileged half of a --system install, run as root, finds an
    # earlier config there and has the daemon fold the caller's enrollment into
    # it: it asks whether the daemon can (adopt-enrollment --help), then has it
    # do so (--from). Either call reads its stdin.
    priv=(--role daemon --privileged-steps --system --config-from "$S/enrolled.toml")
    fold="==> a config already exists at /etc/roomler/config.toml — folding the new enrollment into it (operator settings kept)"
    AS_ROOT=1 HAS_CONFIG=1 run deb-privileged/normal Linux x86_64 1 normal "$new" 0 "${priv[@]}"
    says agent-v0.4.221 0.4.221 0; want "$fold"; want "==> done."; done_case
    AS_ROOT=1 HAS_CONFIG=1 READS='roomlerd [adopt-enrollment] [--help]' \
        run deb-privileged/reads-stdin-ask Linux x86_64 1 normal "$new" 0 "${priv[@]}"
    says agent-v0.4.221 0.4.221 0; want "$fold"; want "==> done."; done_case
    AS_ROOT=1 HAS_CONFIG=1 READS='roomlerd [adopt-enrollment] [--from]' \
        run deb-privileged/reads-stdin-fold Linux x86_64 1 normal "$new" 0 "${priv[@]}"
    says agent-v0.4.221 0.4.221 0; want "$fold"; want "==> done."; done_case
    # #3926 — as root the package install runs dpkg itself, and it reads its
    # stdin.
    AS_ROOT=1 HAS_CONFIG=1 READS='dpkg [-i]' \
        run deb-privileged/reads-stdin-dpkg Linux x86_64 1 normal "$new" 0 "${priv[@]}"
    says agent-v0.4.221 0.4.221 0; want "$fold"; want "==> done."; done_case
    # #3925 — a Mac given its root half as a second row (--daemon-token): that
    # half's enrollment, through sudo, reads its stdin.
    two=(--role daemon --token check-token --daemon-token check-token-2)
    rh="==> enrolling the privileged half as 'check-host-daemon' (second single-use token, overlay on)"
    HAS_PLIST=1 run pkg-two-row/normal Darwin arm64 0 normal "$new" 0 "${two[@]}"
    says agent-v0.4.221 0.4.221 0; want "$rh"; want "==> done."; done_case
    HAS_PLIST=1 READS='sudo [enroll]' run pkg-two-row/reads-stdin Darwin arm64 0 normal "$new" 0 "${two[@]}"
    says agent-v0.4.221 0.4.221 0; want "$rh"; want "==> done."; done_case
    # #3925 — a Mac as one row (--one-row): the daemon's enroll --help, which
    # says whether this release has --one-row, reads its stdin.
    one=(--role daemon --one-row --token check-token)
    row="==> enrolling this Mac as 'check-host' against https://roomler.example, as its one row, overlay on (token is single-use, never echoed)"
    run pkg-one-row/normal Darwin arm64 0 normal "$new" 0 "${one[@]}"
    says agent-v0.4.221 0.4.221 0; want "$row"; want "==> done."; done_case
    READS='roomlerd [enroll] [--help]' run pkg-one-row/reads-stdin Darwin arm64 0 normal "$new" 0 "${one[@]}"
    says agent-v0.4.221 0.4.221 0; want "$row"; want "==> done."; done_case

    # The tunnel role installs the CLI, which names itself as it always has;
    # neither #3881 line belongs to it. What the CLI answers is printed when
    # it fits the alphabet after its `roomler ` name (#3919).
    run tunnel-dl/normal Linux x86_64 1 normal '' 0 --role tunnel --download-only
    lines "$R" 0; lines "$V" 0; lines "$N" 0; done_case
    run tunnel/normal Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; lines "$N" 0; want "==> installed: roomler 0.4.215"; done_case
    CLI="roomler 0.4.215$ESC[2J"$'\n' run tunnel/esc Linux x86_64 1 normal '' 0 --role tunnel
    want "==> installed: $VU"; done_case
    CLI='' run tunnel/empty Linux x86_64 1 normal '' 0 --role tunnel
    want "==> installed: $VU"; done_case
    CLI="roomler $LONG"$'\n' run tunnel/long Linux x86_64 1 normal '' 0 --role tunnel
    want "==> installed: $VU"; lines "$LONG" 0; done_case
    # #3923 — the CLI's --version reads its stdin: the first one, which asks
    # whether it runs (the second asks again), and the third, which names it.
    CLI_READS_STDIN=1 run tunnel/reads-stdin-runs Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; want "==> installed: roomler 0.4.215"; want "==> done."; done_case
    CLI_READS_STDIN=3 run tunnel/reads-stdin-names Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; want "==> installed: roomler 0.4.215"; want "==> done."; done_case
    # #3926 — the CLI's package install, dpkg through sudo, reads its stdin.
    READS='sudo [dpkg]' run tunnel/reads-stdin-dpkg Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; want "==> installed: roomler 0.4.215"; want "==> done."; done_case
    # #3926 — dpkg fails and `apt-get -f install` runs, its dpkg asking the
    # same question; then apt-get reads its stdin.
    FAILS='sudo [dpkg]' run tunnel-apt/normal Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; want "==> installed: roomler 0.4.215"; want "==> done."; done_case
    FAILS='sudo [dpkg]' READS='sudo [apt-get]' run tunnel-apt/reads-stdin Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; want "==> installed: roomler 0.4.215"; want "==> done."; done_case
    # #3925 — given a token the CLI enrolls, and its enroll reads its stdin.
    tun="==> enrolling this tunnel client as 'check-host' against https://roomler.example (token is single-use, never echoed)"
    run tunnel-enroll/normal Linux x86_64 1 normal '' 0 --role tunnel --token check-token
    lines "$R" 0; lines "$V" 0; want "$tun"; want "==> done."; done_case
    READS='roomler [enroll]' run tunnel-enroll/reads-stdin Linux x86_64 1 normal '' 0 --role tunnel --token check-token
    lines "$R" 0; lines "$V" 0; want "$tun"; want "==> done."; done_case

    # #3931 — in every mode, every call of every stand-in reads its stdin first
    # (under dash a second time, padded): the script runs to its last line, and
    # the check below holds the calls to the mode's.
    every deb-dl Linux x86_64 1 normal '' 0 "${dl[@]}"
    every deb-fallback Linux x86_64 1 nodeb '' 0 "${dl[@]}"
    every tgz-dl Linux x86_64 0 normal '' 0 "${dl[@]}"
    every pkg-dl Darwin arm64 0 normal '' 0 "${dl[@]}"
    every deb-system Linux x86_64 1 normal "$new" 0 "${sys[@]}"
    FAILS='sudo [dpkg]' every deb-system-apt Linux x86_64 1 normal "$new" 0 "${sys[@]}"
    every deb-user Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    every pkg Darwin arm64 0 normal "$new" 0 "${usr[@]}"
    RUNNING=$new every deb-user-same Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    RUNNING=$old RUNNING_CAPS=$cap every deb-user-self Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    RUNNING=$old every deb-user-restart Linux x86_64 1 normal "$new" 0 "${usr[@]}"
    AS_ROOT=1 HAS_CONFIG=1 every deb-privileged Linux x86_64 1 normal "$new" 0 "${priv[@]}"
    HAS_PLIST=1 every pkg-two-row Darwin arm64 0 normal "$new" 0 "${two[@]}"
    every pkg-one-row Darwin arm64 0 normal "$new" 0 "${one[@]}"
    every tunnel-dl Linux x86_64 1 normal '' 0 --role tunnel --download-only
    every tunnel Linux x86_64 1 normal '' 0 --role tunnel
    FAILS='sudo [dpkg]' every tunnel-apt Linux x86_64 1 normal '' 0 --role tunnel
    every tunnel-enroll Linux x86_64 1 normal '' 0 --role tunnel --token check-token

    # The same calls, byte for byte, in every case of a mode: neither the
    # feed's tag nor the daemon's answer changes what the script runs or how
    # it exits.
    for mode in "$RUNS/$SH"/*/; do
        mode=${mode%/}
        first=""
        cases=0
        WHY=""
        for c in "$mode"/*/; do
            c=${c%/}
            cases=$((cases + 1))
            if [ -z "$first" ]; then first=$c; continue; fi
            for f in calls.log curl.log roomlerd.log roomler.log rc; do
                cmp -s "$first/$f" "$c/$f" || WHY="$WHY"$'\n'"    ${c##*/}/$f differs from ${first##*/}/$f"
            done
        done
        [ "$cases" -gt 1 ] || continue
        TOTAL=$((TOTAL + 1))
        if [ -z "$WHY" ]; then
            echo "PASS $SH ${mode##*/}: its $cases cases record the same calls and exit code"
        else
            FAILED=$((FAILED + 1))
            echo "FAIL $SH ${mode##*/}: the calls differ between cases:$WHY"
        fi
    done

    # #3931 — a script cut off in transit runs nothing, in a Linux system
    # install and in a Mac's per-user install: no stand-in records a call.
    cut_off deb-system Linux x86_64 1 normal "$new" 0 "${sys[@]}"
    cut_off pkg Darwin arm64 0 normal "$new" 0 "${usr[@]}"
done

# Every mode's calls as one record: its first case's (the check above holds
# the others to it), one line a call, each led by its mode and its log.
record() (
    LC_ALL=C
    export LC_ALL
    echo "# What each mode of scripts/install-sh-output-check.sh runs, under every shell."
    echo "# Rewrite it (--rewrite-calls) only for a change meant to change what a mode runs."
    for mode in "$RUNS/$1"/*/; do
        mode=${mode%/}
        for c in "$mode"/*/; do break; done
        c=${c%/}
        for f in calls curl roomlerd roomler; do
            sed "s|^|${mode##*/} $f: |" "$c/$f.log"
        done
        echo "${mode##*/} rc: $(cat "$c/rc")"
    done
)
for SH in $SHELLS; do record "$SH" >"$S/calls.$SH"; done
if [ "$REWRITE" = 1 ]; then
    # The record is one for every shell, so every shell must agree on it.
    TOTAL=$((TOTAL + 1))
    first=${SHELLS%% *}
    WHY=""
    for SH in $SHELLS; do cmp -s "$S/calls.$first" "$S/calls.$SH" || WHY="$WHY $SH"; done
    if [ -z "$WHY" ]; then
        cp "$S/calls.$first" /calls-out/calls
        echo "PASS the calls of every mode, recorded for install-sh-output-check.calls"
    else
        FAILED=$((FAILED + 1))
        echo "FAIL the calls under$WHY differ from those under $first: nothing recorded"
    fi
else
    # A checkout with CRLF endings still compares: the record is LF.
    for SH in $SHELLS; do
        TOTAL=$((TOTAL + 1))
        if [ ! -f /src/calls ]; then
            FAILED=$((FAILED + 1))
            echo "FAIL $SH: no install-sh-output-check.calls beside this script (--rewrite-calls writes it)"
        elif tr -d '\r' </src/calls | cmp -s - "$S/calls.$SH"; then
            echo "PASS $SH: every mode runs the calls install-sh-output-check.calls records"
        else
            FAILED=$((FAILED + 1))
            echo "FAIL $SH: the calls differ from install-sh-output-check.calls (- recorded, + this run):"
            { tr -d '\r' </src/calls | diff - "$S/calls.$SH" || true; } | sed -n 's/^</    -/p; s/^>/    +/p'
        fi
    done
fi

if [ -d /keep ]; then cp -r "$RUNS"/. /keep/; fi
if [ "$FAILED" = 0 ]; then
    echo "$TOTAL checks"
    echo "every case passed"
else
    echo "$FAILED of $TOTAL checks failed"
    exit 1
fi
