#!/usr/bin/env bash
# scripts/aws-testbox/ssh.sh — ssh to the box up.sh made, with the run's own key and known_hosts.
#   ssh.sh                  an interactive shell
#   ssh.sh '<command>'      run one command
#   ssh.sh 'bash -s' < f    run a local script on the box
# Env: AWS_PROFILE, AWS_REGION and TESTBOX_NAME as for up.sh (the address comes from the state file).
set -uo pipefail
D="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$D/lib.sh"
load_state
[ -n "${IP:-}" ] || { echo "no box recorded in $ST — run up.sh first" >&2; exit 1; }
tb_ssh "$@"
