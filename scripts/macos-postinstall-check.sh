#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# The macOS package's post-install script, run on both of its roads (#3783).
#
# agents/roomlerd/packaging/macos/postinstall runs as root on every enrolled Mac
# at every update, and the update helper is what runs it, often with nobody
# signed in. Its two roads differ in one thing: whether the person's half (the
# login items in gui/<uid>, the menu-bar app) is touched. Everything root-owned,
# the root daemon's reload and the update helper's re-arm, runs on both.
#
# This runs the REAL script, byte for byte, as root in a throwaway Linux
# container whose / stands in for the Mac's: the bundle's plists under
# /Library/Roomler, the markers under /etc/roomler, a person's home under
# /Users. Stand-ins on PATH answer for launchctl and the other macOS tools and
# record every call, and each case checks what the script did on its road.
# `launchctl` keeps the loaded jobs in files, so a bootout of a job that is not
# loaded fails as it does on a Mac.
#
# What it cannot show: real launchd (a drain after a bootout, a latched failed
# init, the process group a bootout ends), macOS's own tools and its
# /bin/bash 3.2. The Mac lab reads those on a release candidate.
#
# Usage: scripts/macos-postinstall-check.sh [--image IMAGE] [SCRIPT]
#   SCRIPT   the post-install script to run. Default: the repo's. An older one
#            reads how it behaved:
#              git show origin/master:agents/roomlerd/packaging/macos/postinstall > /tmp/pi
#              scripts/macos-postinstall-check.sh /tmp/pi
#   --image  the container image. Default debian:12.
# Needs docker. Exits non-zero when any case fails, and then prints each failed
# case's calls and output.
set -euo pipefail

if [ "${1:-}" != --in-container ]; then
    image=debian:12
    script=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --image) image="$2"; shift 2 ;;
            -h|--help) sed -n '3,30p' "$0"; exit 0 ;;
            *) script="$1"; shift ;;
        esac
    done
    here=$(cd "$(dirname "$0")" && pwd)
    packaging="$here/../agents/roomlerd/packaging/macos"
    packaging=$(cd "$packaging" && pwd)
    script=${script:-$packaging/postinstall}
    script=$(cd "$(dirname "$script")" && pwd)/$(basename "$script")
    [ -f "$script" ] || { echo "no script at $script" >&2; exit 2; }
    echo "post-install script: $script"
    echo "image: $image"
    # No network: nothing in here has a reason to reach anything. The verdict
    # is the last line as well as docker's status: a docker CLI that is a shim
    # (WSL's is one) can answer 0 for a container that exited 1.
    log=$(mktemp)
    trap 'rm -f "$log"' EXIT
    set +e
    docker run --rm --network none \
        -v "$packaging:/src/packaging:ro" \
        -v "$script:/src/postinstall:ro" \
        -v "$here/$(basename "$0"):/src/check.sh:ro" \
        "$image" bash /src/check.sh --in-container 2>&1 | tee "$log"
    st=${PIPESTATUS[0]}
    set -e
    if [ "$st" = 0 ] && [ "$(tail -n 1 "$log")" = "every case passed" ]; then
        exit 0
    fi
    echo "macos-postinstall-check: FAILED (docker exit $st)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# In the container. This half deletes /Library, /Applications and /Users
# between cases, so it refuses to run anywhere else.
# ---------------------------------------------------------------------------
if [ ! -f /.dockerenv ] && [ ! -f /run/.containerenv ]; then
    echo "refusing: --in-container runs only inside the container this script starts" >&2
    exit 2
fi
[ "$(uname -s)" = Linux ] || { echo "refusing: not a Linux container" >&2; exit 2; }

S=/standin
BIN=$S/bin
STATE=$S/state
CALLS=$S/calls
OUT=$S/out
REAL_STAT=$(command -v stat)
mkdir -p "$BIN" /usr/sbin

# The person at the console when someone is signed in: uid 501, as on a Mac.
grep -q '^tester:' /etc/passwd || echo 'tester:x:501:20:tester:/Users/tester:/bin/sh' >>/etc/passwd
# `chown root:wheel` needs the group by name; on a Mac it is gid 0.
grep -q '^wheel:' /etc/group || echo 'wheel:x:0:' >>/etc/group

# launchctl: records the call, keeps each loaded job as a file under
# $STATE/loaded (named after domain/label), and answers as launchd does.
cat >"$BIN/launchctl" <<'EOF'
#!/bin/bash
echo "launchctl $*" >>/standin/calls
state=/standin/state
job() { printf '%s/loaded/%s' "$state" "$(printf '%s' "$1" | tr '/' '_')"; }
sub=${1:-}; shift || true
case "$sub" in
    print)
        f=$(job "$1")
        [ -f "$f" ] || { echo "Could not find service \"${1##*/}\" in domain" >&2; exit 113; }
        # One write, as a buffered launchctl makes for an answer this short.
        # Written line by line, a reader that stops early (`grep -q`) ends the
        # writer on SIGPIPE, and under the script's pipefail that reads as
        # "not running" (noted on #3783; whether launchctl can be caught that
        # way is not known).
        printf '%s = {\n%s\n}\n' "$1" "$(cat "$f")"
        ;;
    bootout)
        f=$(job "$1")
        [ -f "$f" ] || { echo "Boot-out failed: 3: No such process" >&2; exit 3; }
        rm -f "$f"
        ;;
    bootstrap)
        label=$(basename "$2" .plist)
        f=$(job "$1/$label")
        if [ -f "$state/fail-bootstrap-$label" ]; then
            echo "Bootstrap failed: 5: Input/output error" >&2; exit 5
        fi
        [ -f "$2" ] || { echo "Bootstrap failed: 2: No such file or directory" >&2; exit 2; }
        [ ! -f "$f" ] || { echo "Bootstrap failed: 37: Operation already in progress" >&2; exit 37; }
        pid=$(( $(cat "$state/pid") + 1 )); echo "$pid" >"$state/pid"
        printf '\tstate = running\n\tpid = %s\n' "$pid" >"$f"
        ;;
esac
EOF

# stat: the BSD `stat -f FORMAT FILE` forms the script uses, answered by this
# image's stat; /dev/console's owner is the case's console user.
cat >"$BIN/stat" <<EOF
#!/bin/bash
[ "\${1:-}" = -f ] || exec $REAL_STAT "\$@"
if [ "\$3" = /dev/console ]; then cat /standin/state/console; exit 0; fi
fmt=\$(printf '%s' "\$2" | sed -e 's/%Su/%U/g; s/%Sg/%G/g; s/%Lp/%a/g; s/%l/%h/g')
exec $REAL_STAT -c "\$fmt" "\$3"
EOF

cat >"$BIN/dscl" <<'EOF'
#!/bin/bash
echo "NFSHomeDirectory: /Users/${3##*/}"
EOF

# Nothing of the companion's runs in here: pkill and pgrep match nothing.
for tool in pkill pgrep; do
    printf '#!/bin/bash\necho "%s $*" >>/standin/calls\nexit 1\n' "$tool" >"$BIN/$tool"
done

# The script's waits are bounded loops around `sleep`; here they need no time.
printf '#!/bin/bash\nexit 0\n' >"$BIN/sleep"

# The script names these two by full path. `sudo -n -u '#<uid>' -- cmd …`
# runs cmd as that uid and its primary group, as on a Mac.
cat >/usr/bin/sudo <<'EOF'
#!/bin/bash
echo "sudo $*" >>/standin/calls
[ "$1" = -n ] && shift
[ "$1" = -u ] || exit 64
uid=${2#\#}; shift 2
[ "$1" = -- ] && shift
gid=$(awk -F: -v u="$uid" '$3 == u { print $4; exit }' /etc/passwd)
exec chroot --userspec="$uid:$gid" --groups="$gid" / "$@"
EOF
printf '#!/bin/bash\necho "dseditgroup $*" >>/standin/calls\nexit 1\n' >/usr/sbin/dseditgroup
chmod 755 "$BIN"/* /usr/bin/sudo /usr/sbin/dseditgroup

# ---------------------------------------------------------------------------
# A case: a fresh Mac, the case's setup, one run of the script, its checks.
#
# A check that finds nothing must count as a failure and let the next check
# run, so errexit is off from here on. The EXIT trap makes any stop before the
# last line a failure, whatever status it carried.
# ---------------------------------------------------------------------------
set +e +o pipefail
finished=0
trap 'st=$?; if [ "$finished" != 1 ]; then echo "the check stopped before its end (status $st)"; exit 1; fi' EXIT
RES=/Library/Roomler/roomlerd.app/Contents/Resources
LD=/Library/LaunchDaemons
LA=/Users/tester/Library/LaunchAgents
failed_cases=0
CASE=""
CASE_FAILED=0

fresh_mac() {
    rm -rf /Library /Applications /Users /etc/roomler /var/log/roomler "$STATE"
    mkdir -p "$RES" "$LD" "$STATE/loaded" /Users/tester/Library
    cp /src/packaging/com.roomler.*.plist "$RES/"
    chown -R tester /Users/tester
    echo 1000 >"$STATE/pid"
    : >"$CALLS"
}

console() { printf '%s' "$1" >"$STATE/console"; }
loaded() { printf '\tstate = %s\n' "$2" >"$STATE/loaded/$(printf '%s' "$1" | tr '/' '_')"; }
marker() { mkdir -p /etc/roomler && touch "/etc/roomler/$1"; }
enrolled() { mkdir -p /etc/roomler && printf '%s\n' "${1:-}" >/etc/roomler/config.toml; }
# What an earlier install left: both LaunchDaemons, older than this payload's.
earlier_install() {
    for j in daemon update; do
        echo '<!-- an earlier release -->' >"$LD/com.roomler.$j.plist"
    done
}
# The person's two login items, placed by an earlier install as that person.
persons_items() {
    mkdir -p "$LA"
    for j in agent desktop; do
        echo '<!-- an earlier release -->' >"$LA/com.roomler.$j.plist"
        chmod 644 "$LA/com.roomler.$j.plist"
    done
    chown -R tester /Users/tester
}
companion_app() { mkdir -p /Applications/Roomler.app; }
persons_tree() { (cd /Users && find . -exec stat -c '%n %U %a %s' {} \; | sort && find . -type f -exec md5sum {} \; | sort); }

start() {
    CASE="$1"
    CASE_FAILED=0
    fresh_mac
}

run() {
    PERSONS_BEFORE=$(persons_tree)
    # PackageKit gives package scripts a bare environment and these arguments.
    (cd / && env -i PATH="$BIN:/usr/sbin:/usr/bin:/sbin:/bin" \
        bash /src/postinstall /tmp/roomler.pkg / / /) >"$OUT" 2>&1
    RC=$?
}

fail() {
    echo "FAIL [$CASE] $*"
    CASE_FAILED=1
}
rc_is() { [ "$RC" = "$1" ] || fail "exit code $RC, not $1"; }
rc_is_not() { [ "$RC" != "$1" ] || fail "exit code $RC"; }
said() { grep -qF -- "$1" "$OUT" || fail "the log does not say: $1"; }
not_said() { ! grep -qF -- "$1" "$OUT" || fail "the log says: $1"; }
called() { grep -qxF -- "$1" "$CALLS" || fail "no call: $1"; }
not_called() { ! grep -qxF -- "$1" "$CALLS" || fail "a call: $1"; }
no_call_with() { ! grep -qF -- "$1" "$CALLS" || fail "a call naming '$1': $(grep -F -- "$1" "$CALLS" | head -1)"; }
times_called() {
    n=$(grep -cxF -- "$2" "$CALLS" || true)
    [ "$n" = "$1" ] || fail "$n calls, not $1: $2"
}
in_order() {
    a=$(grep -nxF -- "$1" "$CALLS" | head -1 | cut -d: -f1)
    b=$(grep -nxF -- "$2" "$CALLS" | tail -1 | cut -d: -f1)
    [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ] || fail "not in this order: $1, then $2"
}
loaded_now() { [ -f "$STATE/loaded/$(printf '%s' "$1" | tr '/' '_')" ] || fail "$1 is not loaded at the end"; }
is_the_payloads() { cmp -s "$RES/$(basename "$1")" "$1" || fail "$1 is not this payload's plist"; }
absent() { [ ! -e "$1" ] || fail "$1 is still there"; }
persons_untouched() { [ "$(persons_tree)" = "$PERSONS_BEFORE" ] || fail "something under /Users changed"; }

done_case() {
    if [ "$CASE_FAILED" = 1 ]; then
        failed_cases=$((failed_cases + 1))
        echo "  --- calls ---"; sed 's/^/    /' "$CALLS"
        echo "  --- output (exit $RC) ---"; sed 's/^/    /' "$OUT"
    else
        echo "ok   [$CASE]"
    fi
}

DAEMON_OUT="launchctl bootout system/com.roomler.daemon"
DAEMON_IN="launchctl bootstrap system $LD/com.roomler.daemon.plist"
HELPER_OUT="launchctl bootout system/com.roomler.update"
HELPER_IN="launchctl bootstrap system $LD/com.roomler.update.plist"

# The root half's checks for an update of a two-row Mac: the daemon is
# reloaded on this payload's plist, and the helper re-armed the same way.
daemon_reloaded() {
    in_order "$DAEMON_OUT" "$DAEMON_IN"
    times_called 1 "$DAEMON_IN"
    loaded_now system/com.roomler.daemon
    is_the_payloads "$LD/com.roomler.daemon.plist"
    said "roomler system daemon installed"
}
helper_recycled() {
    in_order "$HELPER_OUT" "$HELPER_IN"
    loaded_now system/com.roomler.update
    is_the_payloads "$LD/com.roomler.update.plist"
    said "roomler update helper re-cycled"
}
# Nobody signed in: nothing of the person's half, said in the log.
persons_half_skipped() {
    said "NO GUI session is logged in"
    no_call_with "gui/"
    no_call_with "sudo "
    no_call_with "pkill "
    no_call_with "pgrep "
    persons_untouched
}

# A Mac at the login window, enrolled with both rows, as every update finds it.
login_window_two_rows() {
    console root
    marker enable-daemon
    enrolled
    earlier_install
    persons_items
    companion_app
    loaded system/com.roomler.daemon running
    loaded system/com.roomler.update "not running"
}

# --- N1: an update at the login window ---------------------------------------
start "login window: an update reloads the root daemon and re-arms the helper"
login_window_two_rows
run
rc_is 0
persons_half_skipped
daemon_reloaded
helper_recycled
in_order "$DAEMON_IN" "$HELPER_OUT"
done_case

# --- N2: the update helper's own run, nobody signed in ------------------------
start "login window: the helper that runs this install is left running"
login_window_two_rows
loaded system/com.roomler.update running
run
rc_is 0
persons_half_skipped
daemon_reloaded
not_called "$HELPER_OUT"
not_called "$HELPER_IN"
loaded_now system/com.roomler.update
said "roomler update helper is running (this very install) — leaving it"
done_case

# --- N5: opted out of auto-update ---------------------------------------------
start "login window: the opt-out marker removes the idle helper"
login_window_two_rows
marker disable-auto-update
run
rc_is 0
persons_half_skipped
daemon_reloaded
called "$HELPER_OUT"
not_called "$HELPER_IN"
absent "$LD/com.roomler.update.plist"
said "roomler update helper REMOVED"
done_case

start "login window: the opt-out marker leaves a running helper loaded"
login_window_two_rows
marker disable-auto-update
loaded system/com.roomler.update running
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
absent "$LD/com.roomler.update.plist"
said "the loaded job unloads at next boot"
done_case

# --- the daemon's opt-in means the same at the login window --------------------
start "login window: no enable-daemon marker removes an earlier root daemon"
login_window_two_rows
rm -f /etc/roomler/enable-daemon
run
rc_is 0
persons_half_skipped
called "$DAEMON_OUT"
not_called "$DAEMON_IN"
absent "$LD/com.roomler.daemon.plist"
said "roomler system daemon REMOVED"
helper_recycled
done_case

# --- N4: a first install at the login window -----------------------------------
start "login window, first install, no daemon asked for"
console root
run
rc_is 0
persons_half_skipped
no_call_with "com.roomler.daemon"
absent "$LD/com.roomler.daemon.plist"
called "$HELPER_IN"
said "roomler update helper installed"
done_case

start "login window, first install, the daemon asked for, not enrolled"
console root
marker enable-daemon
run
rc_is 0
persons_half_skipped
# Nothing is loaded yet, so the bootout fails, and that is not an error.
called "$DAEMON_OUT"
daemon_reloaded
said "NOT ENROLLED YET"
called "$HELPER_IN"
said "roomler update helper installed"
done_case

start "login window, first install, the daemon asked for, enrolled"
console root
marker enable-daemon
enrolled
run
rc_is 0
persons_half_skipped
daemon_reloaded
not_said "NOT ENROLLED YET"
called "$HELPER_IN"
done_case

start "no console user at all"
console ""
marker enable-daemon
enrolled
run
rc_is 0
said "console user is 'unknown'"
persons_half_skipped
daemon_reloaded
called "$HELPER_IN"
done_case

# --- failures on the login-window road -----------------------------------------
start "login window: a bootstrap that keeps failing is said, and the install goes on"
login_window_two_rows
touch "$STATE/fail-bootstrap-com.roomler.daemon"
run
rc_is 0
times_called 5 "$DAEMON_IN"
said "WARNING: daemon bootstrap failed after retries: Bootstrap failed: 5: Input/output error"
said "Start it by hand: sudo launchctl bootstrap system $LD/com.roomler.daemon.plist"
helper_recycled
done_case

start "login window: a plist that cannot be copied stops the script before any bootout"
login_window_two_rows
rm -rf "$LD"
echo "not a folder" >"$LD"
run
rc_is_not 0
not_called "$DAEMON_OUT"
loaded_now system/com.roomler.daemon
said "$LD/com.roomler.daemon.plist"
done_case

# --- N3: the signed-in road, as it was -----------------------------------------
# Every call, in order, of an update on a signed-in two-row Mac. It is the
# record of the script before #3783 (agent-v0.4.217): the change made both
# roads share the root half and left this road as it was.
SIGNED_IN_CALLS=$(cat <<EOF
sudo -n -u #501 -- /bin/mkdir -p $LA
sudo -n -u #501 -- /bin/cp $RES/com.roomler.agent.plist $LA/com.roomler.agent.plist
sudo -n -u #501 -- /bin/chmod 644 $LA/com.roomler.agent.plist
launchctl bootout gui/501/com.roomler.agent
launchctl print gui/501/com.roomler.agent
launchctl bootstrap gui/501 $LA/com.roomler.agent.plist
sudo -n -u #501 -- /bin/mkdir -p $LA
sudo -n -u #501 -- /bin/cp $RES/com.roomler.desktop.plist $LA/com.roomler.desktop.plist
sudo -n -u #501 -- /bin/chmod 644 $LA/com.roomler.desktop.plist
pkill -TERM -U 501 -f /Roomler\.app/Contents/MacOS/roomler-desktop( |$)
launchctl bootout gui/501/com.roomler.desktop
launchctl print gui/501/com.roomler.desktop
launchctl bootstrap gui/501 $LA/com.roomler.desktop.plist
launchctl print gui/501/com.roomler.desktop
launchctl print gui/501/com.roomler.desktop
launchctl bootout system/com.roomler.daemon
launchctl print system/com.roomler.daemon
launchctl bootstrap system $LD/com.roomler.daemon.plist
launchctl print system/com.roomler.update
launchctl print system/com.roomler.update
launchctl bootout system/com.roomler.update
launchctl bootstrap system $LD/com.roomler.update.plist
EOF
)

signed_in_two_rows() {
    console tester
    marker enable-daemon
    enrolled
    earlier_install
    persons_items
    companion_app
    loaded gui/501/com.roomler.agent running
    loaded gui/501/com.roomler.desktop running
    loaded system/com.roomler.daemon running
    loaded system/com.roomler.update "not running"
}

start "signed in: an update makes the same calls as before #3783"
signed_in_two_rows
run
rc_is 0
[ "$(cat "$CALLS")" = "$SIGNED_IN_CALLS" ] || {
    fail "the calls differ from the signed-in road's record:"
    diff <(printf '%s\n' "$SIGNED_IN_CALLS") "$CALLS" | sed 's/^/    /' || true
}
said "roomlerd installed and started for tester (uid=501)"
said "Roomler menu-bar app started for tester"
not_said "NO GUI session"
daemon_reloaded
helper_recycled
loaded_now gui/501/com.roomler.agent
is_the_payloads "$LA/com.roomler.agent.plist"
is_the_payloads "$LA/com.roomler.desktop.plist"
done_case

start "signed in: the helper that runs this install is left running"
signed_in_two_rows
loaded system/com.roomler.update running
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
said "roomler update helper is running (this very install) — leaving it"
done_case

start "signed in: the root daemon owns the GUI worker, so the agent's login item goes"
signed_in_two_rows
enrolled "macos_supervise_gui_worker = true"
run
rc_is 0
called "launchctl bootout gui/501/com.roomler.agent"
not_called "launchctl bootstrap gui/501 $LA/com.roomler.agent.plist"
absent "$LA/com.roomler.agent.plist"
said "is owned by the ROOT DAEMON"
daemon_reloaded
done_case

echo
finished=1
if [ "$failed_cases" -gt 0 ]; then
    echo "$failed_cases case(s) failed"
    exit 1
fi
echo "every case passed"
