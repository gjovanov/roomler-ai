#!/usr/bin/env bash
# #4500 — run a test command on a Windows CI runner under the bind audit
# (scripts/test-binds-audit.ps1): arm the audit, run the command, then report
# every UDP bind and TCP listen the named test binaries made. A bind off
# loopback fails the report (docs/testing.md, "How tests bind").
#
#   bash scripts/test-binds-run.sh '<image pattern>' <command...>
#   bash scripts/test-binds-run.sh 'roomlerd-*.exe' cargo test -p roomlerd --lib
#
# The report runs whether the command passed or not, so a failing suite still
# shows what it bound. Exit: the command's code if it failed, else the
# report's (1 a bind off loopback, 2 the audit saw nothing).
#
# CI only, as the .ps1 says: `arm` changes the machine's audit policy, and its
# canary binds 0.0.0.0.
set -u
[ $# -ge 2 ] || { echo "usage: $0 '<image pattern>' <command...>" >&2; exit 2; }
here=$(dirname "$0")
image=$1
shift
pwsh -NoProfile -File "$here/test-binds-audit.ps1" arm || exit $?
"$@"
rc=$?
pwsh -NoProfile -File "$here/test-binds-audit.ps1" report -Image "$image"
audit=$?
[ "$rc" -eq 0 ] || exit "$rc"
exit "$audit"
