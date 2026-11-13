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
    # The inner half runs only beside this run's own token (see below).
    tokfile=$(mktemp)
    token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    printf '%s' "$token" >"$tokfile"
    trap 'rm -f "$log" "$tokfile"' EXIT
    set +e
    docker run --rm --network none \
        -v "$packaging:/src/packaging:ro" \
        -v "$script:/src/postinstall:ro" \
        -v "$here/$(basename "$0"):/src/check.sh:ro" \
        -v "$tokfile:/src/once-token:ro" \
        "$image" bash /src/check.sh --in-container "$token" 2>&1 | tee "$log"
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
# between cases, so it refuses to run anywhere but the container this script
# starts: that one alone holds /src/once-token, the token this run made.
# ---------------------------------------------------------------------------
if [ ! -f /src/once-token ] || [ -z "${2:-}" ] || [ "$(cat /src/once-token)" != "$2" ]; then
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
# The clock is the sum of the script's sleeps (see the sleep stand-in). A job
# booted out while it lingers stays in the domain until the clock reaches the
# time in draining/<job>, as a job draining after launchd's SIGTERM does.
clock=$(cat "$state/clock")
for d in "$state"/draining/*; do
    [ -f "$d" ] || continue
    [ "$clock" -lt "$(cat "$d")" ] || rm -f "$state/loaded/$(basename "$d")" "$d"
done
case "$sub" in
    print)
        f=$(job "$1")
        # print-fails/<job> holds how many answers launchctl still gives before
        # its prints of that job fail for a reason other than "no such service".
        pf="$state/print-fails/$(basename "$f")"
        if [ -f "$pf" ]; then
            left=$(cat "$pf")
            [ "$left" -gt 0 ] || { echo "Unexpected error: 5: Input/output error" >&2; exit 5; }
            echo $((left - 1)) >"$pf"
        fi
        [ -f "$f" ] || { echo "Could not find service \"${1##*/}\" in domain" >&2; exit 113; }
        if [ -f "$state/in-pieces/$(basename "$f")" ]; then
            # In pieces, as a launchctl whose answer outgrows one buffer writes
            # it: the job's lines, then more, each after a real pause. A reader
            # that stops at the match (`grep -q`) is gone by the next write,
            # which ends this writer on SIGPIPE (or fails, where SIGPIPE is
            # ignored), so its status is not 0. A reader that reads to the end
            # sees the whole answer and a 0.
            printf '%s = {\n' "$1" || exit
            while IFS= read -r line; do printf '%s\n' "$line" || exit; done <"$f"
            for n in 1 2 3 4 5 6 7 8 9 10; do
                /bin/sleep 0.05
                printf '\tendpoint %s = {}\n' "$n" || exit
            done
            printf '}\n'
        else
            # One write, as a buffered launchctl makes for an answer this short.
            printf '%s = {\n%s\n}\n' "$1" "$(cat "$f")"
        fi
        ;;
    bootout)
        f=$(job "$1")
        [ -f "$f" ] || { echo "Boot-out failed: 3: No such process" >&2; exit 3; }
        if [ -f "$state/linger/$(basename "$f")" ]; then
            mkdir -p "$state/draining"
            echo $((clock + $(cat "$state/linger/$(basename "$f")"))) >"$state/draining/$(basename "$f")"
        else
            rm -f "$f"
        fi
        ;;
    bootstrap)
        label=$(basename "$2" .plist)
        f=$(job "$1/$label")
        if [ -f "$state/fail-bootstrap-$label" ]; then
            echo "Bootstrap failed: 5: Input/output error" >&2; exit 5
        fi
        if [ -f "$state/draining/$(basename "$f")" ]; then
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

# The script's waits are bounded loops around `sleep`. Here a sleep takes no
# time and adds its whole seconds to a clock instead, which a lingering job
# (`lingers` below) and `slept` read. The script sees less time pass than on a
# Mac, where every launchctl call takes some too.
cat >"$BIN/sleep" <<'EOF'
#!/bin/bash
c=$(cat /standin/state/clock)
echo $((c + ${1%%.*})) >/standin/state/clock
EOF

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
    echo 0 >"$STATE/clock"
    : >"$CALLS"
}

console() { printf '%s' "$1" >"$STATE/console"; }
loaded() { printf '\tstate = %s\n' "$2" >"$STATE/loaded/$(printf '%s' "$1" | tr '/' '_')"; }
unloaded() { rm -f "$STATE/loaded/$(printf '%s' "$1" | tr '/' '_')"; }
# launchctl writes its answer about this job in pieces (see the stand-in).
in_pieces() { mkdir -p "$STATE/in-pieces"; : >"$STATE/in-pieces/$(printf '%s' "$1" | tr '/' '_')"; }
# The job stays in its domain for N seconds of the script's sleeps after its
# bootout, and a bootstrap meanwhile fails as it does during a drain.
lingers() { mkdir -p "$STATE/linger"; echo "$2" >"$STATE/linger/$(printf '%s' "$1" | tr '/' '_')"; }
# launchctl answers N prints of this job, then fails each one (not with "no such service").
print_fails_after() { mkdir -p "$STATE/print-fails"; echo "$2" >"$STATE/print-fails/$(printf '%s' "$1" | tr '/' '_')"; }
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
# A job still draining is on its way out, not loaded.
loaded_now() {
    j=$(printf '%s' "$1" | tr '/' '_')
    [ -f "$STATE/loaded/$j" ] && [ ! -f "$STATE/draining/$j" ] || fail "$1 is not loaded at the end"
}
slept() { [ "$(cat "$STATE/clock")" = "$1" ] || fail "slept $(cat "$STATE/clock") s, not $1 s"; }
slept_at_most() { [ "$(cat "$STATE/clock")" -le "$1" ] || fail "slept $(cat "$STATE/clock") s, more than $1 s"; }
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

# The root half's calls, in order, on a two-row Mac whose helper is idle. Both
# roads make them; the signed-in road makes the person's half before them.
ROOT_HALF_CALLS=$(cat <<EOF
launchctl bootout system/com.roomler.daemon
launchctl print system/com.roomler.daemon
launchctl bootstrap system $LD/com.roomler.daemon.plist
launchctl print system/com.roomler.update
launchctl bootout system/com.roomler.update
launchctl bootstrap system $LD/com.roomler.update.plist
EOF
)

# The root half's checks for an update of a two-row Mac: the daemon is
# reloaded on this payload's plist, and the helper re-armed the same way.
daemon_reloaded() {
    in_order "$DAEMON_OUT" "$DAEMON_IN"
    times_called 1 "$DAEMON_IN"
    loaded_now system/com.roomler.daemon
    is_the_payloads "$LD/com.roomler.daemon.plist"
    said "roomler system daemon installed"
}
# The update helper's three states, each with what the script must do to it.
# Loaded and idle: re-cycled, a bootout then a bootstrap on this payload's
# plist (it clears a latched failed init). The installer smoke no longer
# re-installs over a loaded helper (#3788), so these cases are CI's only
# cover of the re-cycle.
helper_recycled() {
    in_order "$HELPER_OUT" "$HELPER_IN"
    times_called 1 "$HELPER_OUT"
    times_called 1 "$HELPER_IN"
    loaded_now system/com.roomler.update
    is_the_payloads "$LD/com.roomler.update.plist"
    said "roomler update helper re-cycled"
}
# Not loaded: a bootstrap only, since a bootout has nothing to end.
helper_bootstrapped() {
    not_called "$HELPER_OUT"
    times_called 1 "$HELPER_IN"
    loaded_now system/com.roomler.update
    is_the_payloads "$LD/com.roomler.update.plist"
    said "roomler update helper installed (unattended self-update"
    not_said "re-cycled"
}
# Running: an update is in flight, and the job is this script's own ancestor.
# A bootout would end the install that is running, so it is never booted out.
helper_left_running() {
    not_called "$HELPER_OUT"
    not_called "$HELPER_IN"
    loaded_now system/com.roomler.update
    said "roomler update helper is running (this very install) — leaving it"
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
start "login window, helper idle: an update reloads the root daemon and re-cycles the helper"
login_window_two_rows
run
rc_is 0
persons_half_skipped
daemon_reloaded
helper_recycled
in_order "$DAEMON_IN" "$HELPER_OUT"
# Nothing lingered, so nothing waited.
slept 0
# The same lines in the same order as the signed-in road's root half.
[ "$(cat "$CALLS")" = "$ROOT_HALF_CALLS" ] || {
    fail "the calls differ from the root half of the signed-in road's record:"
    diff <(printf '%s\n' "$ROOT_HALF_CALLS") "$CALLS" | sed 's/^/    /' || true
}
done_case

start "login window, helper not loaded: an update bootstraps it, with no bootout"
login_window_two_rows
unloaded system/com.roomler.update
run
rc_is 0
persons_half_skipped
daemon_reloaded
helper_bootstrapped
done_case

# --- N2: the update helper's own run, nobody signed in ------------------------
start "login window, helper running: the helper that runs this install is never booted out"
login_window_two_rows
loaded system/com.roomler.update running
run
rc_is 0
persons_half_skipped
daemon_reloaded
helper_left_running
done_case

# launchctl's answer in pieces: the guard must still read "running". Piped into
# `grep -q` under pipefail it read "idle" and booted out the running install.
start "login window, helper running, its answer written in pieces: still never booted out"
login_window_two_rows
loaded system/com.roomler.update running
in_pieces system/com.roomler.update
run
rc_is 0
persons_half_skipped
daemon_reloaded
helper_left_running
done_case

# The helper is booted out only on a reading that it is loaded and idle. A
# print that fails (for a reason other than "no such service") is no reading.
start "login window, helper running, launchctl answers once and then fails: still never booted out"
login_window_two_rows
loaded system/com.roomler.update running
print_fails_after system/com.roomler.update 1
run
rc_is 0
daemon_reloaded
helper_left_running
done_case

start "login window, the helper's state cannot be read: it is left as it is, and the log says so"
login_window_two_rows
loaded system/com.roomler.update running
print_fails_after system/com.roomler.update 0
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
not_called "$HELPER_IN"
loaded_now system/com.roomler.update
said "WARNING: the update helper's state could not be read, so it is left as it is: launchctl print exited 5: Unexpected error: 5: Input/output error"
done_case

# --- N5: opted out of auto-update ---------------------------------------------
start "login window, opted out, helper idle: the opt-out marker removes the helper"
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

start "login window, opted out, helper running: the opt-out marker leaves it loaded"
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

start "login window, opted out, helper running, its answer written in pieces: it stays loaded"
login_window_two_rows
marker disable-auto-update
loaded system/com.roomler.update running
in_pieces system/com.roomler.update
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
loaded_now system/com.roomler.update
absent "$LD/com.roomler.update.plist"
said "the loaded job unloads at next boot"
done_case

start "login window, opted out, the helper's state cannot be read: it is not booted out"
login_window_two_rows
marker disable-auto-update
loaded system/com.roomler.update running
print_fails_after system/com.roomler.update 0
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
loaded_now system/com.roomler.update
absent "$LD/com.roomler.update.plist"
said "WARNING: its state could not be read, so it was not booted out"
done_case

start "login window, opted out, helper not loaded: the plist goes, and nothing is booted out"
login_window_two_rows
marker disable-auto-update
unloaded system/com.roomler.update
run
rc_is 0
daemon_reloaded
not_called "$HELPER_OUT"
not_called "$HELPER_IN"
absent "$LD/com.roomler.update.plist"
said "roomler update helper REMOVED"
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

# --- N7: a daemon that hangs on shutdown ---------------------------------------
# launchd answers the bootout with SIGTERM and kills the job with SIGKILL when
# its exit timeout runs out (20 s by default). This one leaves the domain 5 s
# after that. Before #3783's review the wait gave up at 15 s and the bootstraps
# at 22 s, and the daemon stayed unloaded.
start "login window, the daemon hangs on shutdown: it is loaded again once launchd has killed it"
login_window_two_rows
lingers system/com.roomler.daemon 25
run
rc_is 0
persons_half_skipped
daemon_reloaded
not_said "WARNING: daemon bootstrap failed"
# The wait ended when the daemon was gone, not at its bound.
slept 25
helper_recycled
done_case

start "login window, the daemon never leaves its domain: the wait is bounded, and the install goes on"
login_window_two_rows
lingers system/com.roomler.daemon 1000
run
rc_is 0
times_called 5 "$DAEMON_IN"
said "WARNING: daemon bootstrap failed after retries: Bootstrap failed: 5: Input/output error"
slept_at_most 40
helper_recycled
done_case

# The wait above outlasts launchd's default exit timeout, the one that applies
# while no plist sets its own.
start "the jobs the script replaces set no ExitTimeOut of their own"
for j in daemon agent desktop; do
    ! grep -q '<key>ExitTimeOut</key>' "/src/packaging/com.roomler.$j.plist" ||
        fail "com.roomler.$j.plist sets ExitTimeOut: the post-install's DRAIN_WAIT_MAX must outlast it"
done
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
# Every call, in order, of an update on a signed-in two-row Mac: the person's
# half, then the root half (ROOT_HALF_CALLS, above). It is the record of the
# script before #3783 (agent-v0.4.217) but for one line: that script read the
# helper's state twice, a print to see that it was loaded and a print to see
# whether it ran, and this one reads it once (the review's S2). The change made
# both roads share the root half and left this road as it was.
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
$ROOT_HALF_CALLS
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

start "signed in, helper idle: an update makes the same calls as before #3783 and re-cycles the helper"
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

start "signed in, helper not loaded: an update bootstraps it, with no bootout"
signed_in_two_rows
unloaded system/com.roomler.update
run
rc_is 0
said "roomlerd installed and started for tester (uid=501)"
daemon_reloaded
helper_bootstrapped
done_case

start "signed in, helper running: the helper that runs this install is never booted out"
signed_in_two_rows
loaded system/com.roomler.update running
run
rc_is 0
daemon_reloaded
helper_left_running
done_case

start "signed in, helper running, its answer written in pieces: still never booted out"
signed_in_two_rows
loaded system/com.roomler.update running
in_pieces system/com.roomler.update
run
rc_is 0
daemon_reloaded
helper_left_running
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
