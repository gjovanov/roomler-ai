#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# What the served install script says about the release it resolved and the
# daemon it installed (#3881), read against stand-ins.
#
# scripts/install.sh is what `curl -fsSL <server>/install.sh | sh -s -- ...`
# runs. It names the release that holds the file it downloads, as the server's
# feed (/api/agent/latest-release) names it, and after an install it prints
# the installed daemon's own `--version`. Both lines are information. What the
# feed and the binary say is printed only when it fits a small alphabet and 64
# characters, and in fixed words otherwise. No answer may change a command the
# script runs or its exit code.
#
# This runs the REAL script, piped into the shell as the one-liner does, under
# dash (Debian's /bin/sh) and under bash --posix (macOS's /bin/sh is bash in
# POSIX mode). It runs as a person in a throwaway debian:12 container with no
# network. Stand-ins on PATH answer for curl (the feed and the downloads),
# sudo, dpkg, apt-get, systemctl, installer, launchctl, uname, hostname and the
# daemon, and they record every call. Each case checks the lines and the exit
# code. The cases of one mode must record the same calls, byte for byte,
# whatever the feed's tag or the daemon's answer.
#
# What it cannot show: a real package install, systemd, launchd, a real feed,
# and Apple's /bin/sh. The Linux and Mac labs read those.
#
# Usage: scripts/install-sh-output-check.sh [--image IMAGE] [--shell dash|bash] [--keep DIR] [SCRIPT]
#   SCRIPT   the install script to run. Default: the repo's. An older one
#            reads how it behaved:
#              git show origin/master:scripts/install.sh > /tmp/install.sh
#              scripts/install-sh-output-check.sh /tmp/install.sh
#   --shell  run the script under dash only, or under bash --posix only.
#            Default: both.
#   --keep   copy each case's records and output to DIR/<shell>/<mode>/<case>/,
#            so that two scripts' calls can be compared with diff -r.
#   --image  the container image. Default debian:12.
# Needs docker. Exits non-zero when any case fails, and then prints each failed
# case's output.
set -euo pipefail

if [ "${1:-}" != --in-container ]; then
    image=debian:12
    script=""
    shells="dash bash"
    keep=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --image) image="$2"; shift 2 ;;
            --shell)
                case "$2" in
                    dash|bash) shells="$2" ;;
                    *) echo "--shell takes dash or bash" >&2; exit 2 ;;
                esac
                shift 2 ;;
            --keep) keep="$2"; shift 2 ;;
            -h|--help) sed -n '3,38p' "$0"; exit 0 ;;
            *) script="$1"; shift ;;
        esac
    done
    here=$(cd "$(dirname "$0")" && pwd)
    script=${script:-$here/install.sh}
    script=$(cd "$(dirname "$script")" && pwd)/$(basename "$script")
    [ -f "$script" ] || { echo "no script at $script" >&2; exit 2; }
    echo "install script: $script"
    echo "image: $image"
    # The verdict is the last line as well as docker's status: a docker CLI
    # that is a shim (WSL's is one) can answer 0 for a container that exited 1.
    log=$(mktemp)
    # The inner half runs only beside this run's own token (see below).
    tokfile=$(mktemp)
    token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    printf '%s' "$token" >"$tokfile"
    trap 'rm -f "$log" "$tokfile"' EXIT
    extra=()
    if [ -n "$keep" ]; then
        mkdir -p "$keep"
        keep=$(cd "$keep" && pwd)
        extra=(-v "$keep:/keep")
    fi
    set +e
    docker run --rm --network none \
        -v "$script:/src/install.sh:ro" \
        -v "$here/$(basename "$0"):/src/check.sh:ro" \
        -v "$tokfile:/src/once-token:ro" \
        ${extra[@]+"${extra[@]}"} \
        "$image" bash /src/check.sh --in-container "$token" "$shells" 2>&1 | tee "$log"
    st=${PIPESTATUS[0]}
    set -e
    if [ "$st" = 0 ] && [ "$(tail -n 1 "$log")" = "every case passed" ]; then
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
SHELLS=${3:-dash bash}
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
            cat "$C/version.out"
            exit "$(cat "$C/version.rc")"
        fi ;;
    roomler)
        if [ "${1:-}" = --version ]; then echo "roomler 0.4.215"; fi ;;
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
        # No .deb anywhere: a Debian host falls back to the tarball.
        nodeb) printf '[%s,%s]' "$(rel agent-v0.4.221 "$TGZ,$MAC")" "$(rel agent-v0.4.220 "$TGZ,$MAC")" ;;
    esac
}

R="==> release named by the server's feed:"
V="==> installed daemon's own --version:"
N="==> NOTE: the installed daemon's"
UNREADABLE="unreadable (empty, over 64 characters, or outside A-Z a-z 0-9 . _ + -)"

FAILED=0
TOTAL=0
SH=""
# run CASE OS ARCH DEBIAN FEED ANSWER RC ARGS...: one run of the script under
# $SH. CASE is MODE/VARIANT; ANSWER and RC are what the daemon's --version
# prints and exits with. The records land in $RUNS/$SH/CASE.
run() {
    CASE=$1 os=$2 arch=$3 debian=$4 feedname=$5 answer=$6 answer_rc=$7
    shift 7
    rm -rf "$C" /tmp/roomler-install.*
    mkdir -p "$C"
    echo "$os" >"$C/os"
    echo "$arch" >"$C/arch"
    feed "$feedname" >"$C/feed.json"
    printf '%s' "$answer" >"$C/version.out"
    echo "$answer_rc" >"$C/version.rc"
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
    esac
    set +e
    setpriv --reuid=1000 --regid=1000 --clear-groups \
        env -i HOME=/home/tester USER=tester LOGNAME=tester LANG=C.UTF-8 PATH="$path" \
        "${shell[@]}" -s -- --server https://roomler.example "$@" </src/install.sh >"$C/out.txt" 2>&1
    echo $? >"$C/rc"
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
    # The per-user install, the desktop default, and a Mac's.
    run deb-user/match Linux x86_64 1 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; done_case
    run pkg/match Darwin arm64 0 normal $'roomlerd 0.4.221\n' 0 --role daemon --token check-token
    says agent-v0.4.221 0.4.221 0; done_case
    run pkg/exit1 Darwin arm64 0 normal '' 1 --role daemon --token check-token
    says agent-v0.4.221 "no answer (/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd --version exited 1)" 0
    done_case

    # The tunnel role installs the CLI, which names itself as it always has;
    # neither new line belongs to it.
    run tunnel-dl/normal Linux x86_64 1 normal '' 0 --role tunnel --download-only
    lines "$R" 0; lines "$V" 0; lines "$N" 0; done_case
    run tunnel/normal Linux x86_64 1 normal '' 0 --role tunnel
    lines "$R" 0; lines "$V" 0; lines "$N" 0; want "==> installed: roomler 0.4.215"; done_case

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
done

if [ -d /keep ]; then cp -r "$RUNS"/. /keep/; fi
if [ "$FAILED" = 0 ]; then
    echo "$TOTAL checks"
    echo "every case passed"
else
    echo "$FAILED of $TOTAL checks failed"
    exit 1
fi
