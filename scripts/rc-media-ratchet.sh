#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 G ROX EOOD
#
# FR-135 §7.1 — the ratchet: the remote-desktop media path's debt may only fall
# while FR-135 pays it down.
#
# `encode/**` grew ×2.7 in 8.5 weeks (FR-135 §2.8), and a migration loses to
# that. So this counts what the plan exists to shrink, and fails when a count
# grows past its baseline:
#
#   agent_env_reads    calls of the env-knob readers (`node_env`, `node_env_os`,
#                      `env::flag`, `std::env::var`, `std::env::var_os`) in the
#                      agent's media path
#   viewer_knobs       distinct `roomler-rc-*` storage keys the viewer's media
#                      path names, as keys: one quoted two ways counts once
#   pump_lines_rtp, pump_lines_vp9_dc, pump_lines_ffmpeg_dc
#                      the length of each of the three copied pumps in peer.rs
#
# A count may rise only in a PR that raises its line in
# `scripts/rc-media-ratchet.baseline` AND names, on that line, the ledger row it
# adds to `docs/plans/rc-media-debt.tsv` and when that row retires:
#
#   pump_lines_ffmpeg_dc=3800 # r0:FOO-1: retires with R8's delete
#
# Given the base's baseline (RATCHET_BASE_REF, a git ref; CI passes a PR's
# base), a line whose number rose must name a ledger row, and every id a line
# names must be a row of the ledger. Without it (a local run, a push to master)
# only the ids are checked. A count that FALLS passes and prints the lower
# number, so the PR that earned it can lock it in.
#
# What it cannot see: a knob read through a shared helper (`env_u32` and the
# like) counts once, at the helper; test-only reads count like any other; and a
# pump or viewer file that is renamed or split must be renamed here too (a
# renamed pump stops the script, a renamed viewer file drops out of the count).
#
# Usage: scripts/rc-media-ratchet.sh [--print]
#   --print   print the current counts in the baseline's format, and exit 0
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BASELINE=scripts/rc-media-ratchet.baseline
LEDGER=docs/plans/rc-media-debt.tsv

agent_files() {
  git ls-files -- \
    'agents/roomlerd/src/peer.rs' \
    'agents/roomlerd/src/media_*.rs' \
    'agents/roomlerd/src/encode/*.rs' 'agents/roomlerd/src/encode/**/*.rs' \
    'agents/roomlerd/src/capture/*.rs' 'agents/roomlerd/src/capture/**/*.rs' \
    'agents/roomlerd/src/system_context/*.rs' 'agents/roomlerd/src/system_context/**/*.rs' \
    | sort -u
}

viewer_files() {
  git ls-files -- \
    'ui/src/composables/useRemoteControl.ts' \
    'ui/src/composables/videoCells.ts' \
    'ui/src/composables/mediaPlan.ts' \
    'ui/src/composables/frameRate.ts' \
    'ui/src/composables/rcBench.ts' \
    'ui/src/views/remote/RemoteControl.vue' \
    'ui/src/workers/rc-*.ts' \
    | { grep -v '\.spec\.ts$' || true; } | sort -u
}

# Occurrences of <ERE> in <files...>, not lines.
count_matches() {
  local re=$1; shift
  [ "$#" -gt 0 ] || { echo 0; return; }
  { grep -E -o -h -- "$re" "$@" 2>/dev/null || true; } | wc -l | tr -d ' '
}

# Distinct keys matching <ERE> in <files...>: the quote around each match is
# dropped first, so `'roomler-rc-x'` in code and a backticked mention in a
# comment are one key, not two.
count_distinct_keys() {
  local re=$1; shift
  [ "$#" -gt 0 ] || { echo 0; return; }
  { grep -E -o -h -- "$re" "$@" 2>/dev/null || true; } \
    | tr -d "\"'\`" | sort -u | wc -l | tr -d ' '
}

# Lines of `async fn <name>(` up to its closing `}` at column 0, or -1.
pump_lines() {
  awk -v name="$1" '
    index($0, "async fn " name "(") == 1 { start = NR; next }
    start && ($0 == "}" || $0 == "}\r") { print NR - start + 1; found = 1; exit }
    END { if (!found) print -1 }
  ' agents/roomlerd/src/peer.rs
}

# The number on a `<key>=<number> [# ...]` line, or empty when there is none.
number_of() {
  local v=${1#*=}
  v=${v%$'\r'}
  v=${v%%#*}
  v=${v%"${v##*[![:space:]]}"}
  case "$v" in '' | *[!0-9]*) echo "" ;; *) echo "$v" ;; esac
}

mapfile -t AGENT < <(agent_files)
mapfile -t VIEWER < <(viewer_files)

ENV_RE='\b(node_env|node_env_os)\(|\benv::flag\(|std::env::var(_os)?\('
KEY_RE="[\"'\`]roomler-rc-[a-z0-9.-]+[\"'\`]"

declare -A NOW
NOW[agent_env_reads]=$(count_matches "$ENV_RE" "${AGENT[@]}")
NOW[viewer_knobs]=$(count_distinct_keys "$KEY_RE" "${VIEWER[@]}")
NOW[pump_lines_rtp]=$(pump_lines media_pump)
NOW[pump_lines_vp9_dc]=$(pump_lines media_pump_vp9_444_dc)
NOW[pump_lines_ffmpeg_dc]=$(pump_lines media_pump_ffmpeg_dc)
KEYS=(agent_env_reads viewer_knobs pump_lines_rtp pump_lines_vp9_dc pump_lines_ffmpeg_dc)

if [ "${1:-}" = "--print" ]; then
  for k in "${KEYS[@]}"; do echo "$k=${NOW[$k]}"; done
  exit 0
fi

for k in "${KEYS[@]}"; do
  [ "${NOW[$k]}" -ge 0 ] || { echo "ratchet: cannot measure $k (a pump renamed or moved?)"; exit 2; }
done
[ -f "$BASELINE" ] || { echo "ratchet: $BASELINE is missing"; exit 2; }

# The base's baseline, when the caller names one.
BASE_TEXT=""
if [ -n "${RATCHET_BASE_REF:-}" ]; then
  if BASE_TEXT=$(git show "$RATCHET_BASE_REF:$BASELINE" 2>/dev/null); then
    echo "ratchet: comparing the baseline with $RATCHET_BASE_REF's."
  else
    BASE_TEXT=""
    echo "ratchet: $RATCHET_BASE_REF has no $BASELINE, so this change sets it."
  fi
fi

fail=0
for k in "${KEYS[@]}"; do
  line=$({ grep -E "^$k=" "$BASELINE" || true; } | tail -1)
  line=${line%$'\r'}
  [ -n "$line" ] || { echo "ratchet: $k has no baseline line"; fail=1; continue; }
  base=$(number_of "$line")
  if [ -z "$base" ]; then
    echo "ratchet: '$line' is not <key>=<number>, optionally followed by # <ledger id>: <when it retires>"
    fail=1
    continue
  fi
  if [ -n "$BASE_TEXT" ]; then
    was=$(number_of "$({ printf '%s\n' "$BASE_TEXT" | grep -E "^$k=" || true; } | tail -1)")
    if [ -n "$was" ] && [ "$base" -gt "$was" ]; then
      case "$line" in
        *'#'*) ;;
        *)
          echo "ratchet: $k's baseline rose from $was to $base without naming the ledger row it adds:"
          echo "  $k=$base # <ledger id>: <when it retires>"
          fail=1
          ;;
      esac
    fi
  fi
  now=${NOW[$k]}
  if [ "$now" -gt "$base" ]; then
    echo "ratchet: $k grew to $now, past its baseline $base."
    echo "  Shrink it, or raise its line in $BASELINE in this PR with the ledger row it adds:"
    echo "  $k=$now # <ledger id>: <when it retires>"
    fail=1
  elif [ "$now" -lt "$base" ]; then
    echo "ratchet: $k fell to $now from $base. Lock it in: $k=$now in $BASELINE."
  else
    echo "ratchet: $k=$now holds."
  fi
done

# A line that carries a comment must name a ledger row that exists.
while IFS= read -r l || [ -n "$l" ]; do
  l=${l%$'\r'}
  case "$l" in '' | '#'*) continue ;; esac
  case "$l" in *'#'*) ;; *) continue ;; esac
  id=$(printf '%s' "${l#*#}" | sed -E 's/^[[:space:]]*([^[:space:]:]+:[^[:space:]:]+).*/\1/')
  if ! awk -F'\t' -v id="$id" 'NR > 1 && $1 == id { found = 1 } END { exit !found }' "$LEDGER"; then
    echo "ratchet: '$l' names '$id', which is not a row of $LEDGER"
    fail=1
  fi
done < "$BASELINE"

exit "$fail"
