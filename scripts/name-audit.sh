#!/usr/bin/env bash
#
# name-audit.sh — FR-21's classifier and CI guard for retired product names.
#
#   docs/fr/FR-21-retire-obsolete-binary-names.md · issue #809
#
# WHAT IT IS FOR
#
#   The product renamed its binaries (`roomler-agent` -> `roomlerd`,
#   `roomler-agent-tray` -> `roomler-desktop`, `roomler-tunnel` -> `roomler`)
#   but ~1 765 references to the retired names survive across ~233 files.
#   Sweeping them blind is NOT safe: earlier slices (P3d A/B/C, P3e) left
#   deliberate DUAL-READ FALLBACKS so hosts enrolled under the old names keep
#   working, and every one of those looks exactly like an obsolete name.
#   Deleting one fails no build and no test — it strands part of the field at
#   the next update. The macOS `.app` bundle is the worst case: its name keys
#   the TCC grants, so renaming it silently drops Screen Recording and
#   Accessibility on every Mac.
#
#   So every occurrence must be in exactly one of two states:
#
#     * MIGRATED  — gone, replaced by the current name; or
#     * ANCHORED  — deliberately frozen, marked in-line with
#                   `RETIRED-NAME-ANCHOR`, carrying a reason.
#
#   "Occurrence nobody has looked at yet" is the state this script exists to
#   make impossible to keep.
#
# MARKING AN ANCHOR
#
#   Put the marker on or above the line, in whatever comment syntax the file
#   uses. It covers the marker line plus the next N lines (default 1):
#
#     // RETIRED-NAME-ANCHOR: pre-rename hosts still resolve this tree.
#     const OLD_APP: &str = "roomler-agent";
#
#     <!-- RETIRED-NAME-ANCHOR(12): the bundle name keys the macOS TCC grants;
#          renaming it drops Screen Recording + Accessibility fleet-wide. -->
#
#   A marker with no retired name under it is itself an error (`--check`
#   reports it), because a stale marker silently widens the exemption.
#
# THE GUARD IS A PAIR, NOT A RATCHET
#
#   A pure "unclassified == 0" check would be red for the whole program, and a
#   pure ratchet ("the count must not increase") is worse than useless: it
#   happily passes a PR that DELETES an anchor and adds one comment — exactly
#   the failure this whole FR exists to prevent. So `--check` asserts BOTH:
#
#     1. unclassified <= baseline   — catches new occurrences being ADDED
#     2. anchors      >= baseline   — catches existing anchors being DELETED
#
#   Together those are sound while the migration is in flight. `--check
#   --strict` replaces (1) with `unclassified == 0` and is what CI runs from
#   P5 onward.
#
# USAGE
#
#   scripts/name-audit.sh                    # same as --report
#   scripts/name-audit.sh --report           # every occurrence, grouped by file
#   scripts/name-audit.sh --summary          # counts only
#   scripts/name-audit.sh --check            # CI guard (ratchet + anchor floor)
#   scripts/name-audit.sh --check --strict   # CI guard from P5 (must be zero)
#   scripts/name-audit.sh --update-baseline  # after a phase lands
#
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

BASELINE_FILE="scripts/name-audit-baseline.txt"

# The retired names, as extended-regex alternatives. `roomler-agent` also
# covers `roomler-agent-tray` and `roomler-agent-core`; `roomler-tunnel` also
# covers `roomler-tunnel-installer`. The CamelCase and spaced forms are the
# Windows service/task identifiers and the WiX DisplayName — user-visible
# surfaces that belong in the inventory, not just the snake/kebab code forms.
TOKENS='roomler-agent|roomler_agent|ROOMLER_AGENT_|RoomlerAgent|Roomler Agent|roomler-tunnel|roomler_tunnel|Roomler Tunnel|roomler-installer'

# Files excluded from the scan, each for a reason that is about the FILE'S JOB,
# not about it being inconvenient. This list must stay this short: a growing
# exclusion list is the hand-maintained path list the marker scheme exists to
# replace.
#
#   * this script          — it contains every token by construction
#   * the baseline         — it records counts, and may quote a token
#   * the FR-21 spec       — its subject IS the retired names
is_excluded() {
    case "$1" in
        scripts/name-audit.sh) return 0 ;;
        "$BASELINE_FILE") return 0 ;;
        docs/fr/FR-21-*) return 0 ;;
        *) return 1 ;;
    esac
}

# ── scan ────────────────────────────────────────────────────────────────────
#
# Walks each candidate file once, tracking anchor coverage, and emits one
# tab-separated record per interesting line:
#
#   <kind>\t<file>\t<lineno>\t<text>
#
# where kind is UNCLASSIFIED, ANCHORED, or STALEMARKER.
scan() {
    local files
    files=$(git grep -I -l -E "$TOKENS" -- . || true)

    local f
    for f in $files; do
        is_excluded "$f" && continue
        TOKENS="$TOKENS" awk -v file="$f" '
            BEGIN { cover = 0; in_block = 0; span = 1 }
            {
                line = $0
                sub(/\r$/, "", line)
                bare = line
                sub(/^[ \t]*/, "", bare)

                is_marker = (line ~ /RETIRED-NAME-ANCHOR/)
                has_token = (line ~ ENVIRON["TOKENS"])
                # Comment syntaxes across the tree: Rust/TS //, shell/YAML/systemd #,
                # block-comment continuation *, /*, XML <!--, ini ;.
                is_comment = (bare ~ /^(\/\/|#|\*|\/\*|<!--|;)/)

                if (is_marker) {
                    # A marker covers its whole (possibly multi-line) comment block plus
                    # the next `span` code lines, default 1. That is what a reader means
                    # by "this comment explains the line below it", and it stops at the
                    # first code line so the exemption cannot silently widen.
                    span = 1
                    if (match(line, /RETIRED-NAME-ANCHOR\([0-9]+\)/)) {
                        n = substr(line, RSTART, RLENGTH)
                        gsub(/[^0-9]/, "", n)
                        span = n + 0
                    }
                    if (NR > cover) cover = NR
                    in_block = 1
                    marker_line[NR] = 1
                    marker_used[NR] = 0
                    last_marker = NR
                } else if (in_block) {
                    if (is_comment) {
                        cover = NR                       # still inside the comment block
                    } else {
                        cover = NR + span - 1            # first code line(s) under it
                        in_block = 0
                    }
                }

                if (has_token) {
                    if (NR <= cover) {
                        print "ANCHORED\t" file "\t" NR "\t" line
                        if (last_marker) marker_used[last_marker] = 1
                    } else {
                        print "UNCLASSIFIED\t" file "\t" NR "\t" line
                    }
                }
            }
            END {
                for (m in marker_line)
                    if (!marker_used[m])
                        print "STALEMARKER\t" file "\t" m "\tmarker covers no retired name"
            }
        ' "$f"
    done
}

# File and directory NAMES that still carry a retired name. Paths cannot carry
# an in-line marker, so they are counted and baselined separately.
scan_paths() {
    git ls-files | grep -E "$TOKENS" || true
}

read_baseline() {
    local key="$1" default="$2"
    if [ -f "$BASELINE_FILE" ]; then
        local v
        v=$(grep -E "^${key}=" "$BASELINE_FILE" | head -1 | cut -d= -f2 | tr -d ' \r') || true
        [ -n "${v:-}" ] && { echo "$v"; return; }
    fi
    echo "$default"
}

MODE=report
STRICT=0
for arg in "$@"; do
    case "$arg" in
        --report)          MODE=report ;;
        --summary)         MODE=summary ;;
        --check)           MODE=check ;;
        --update-baseline) MODE=update ;;
        --strict)          STRICT=1 ;;
        -h|--help)         sed -n '2,64p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

RESULTS=$(scan)
PATHS=$(scan_paths)

n_unclassified=$(printf '%s\n' "$RESULTS" | grep -c '^UNCLASSIFIED' || true)
n_anchored=$(printf '%s\n' "$RESULTS" | grep -c '^ANCHORED' || true)
n_stale=$(printf '%s\n' "$RESULTS" | grep -c '^STALEMARKER' || true)
n_paths=$(printf '%s\n' "$PATHS" | grep -c . || true)
n_files=$(printf '%s\n' "$RESULTS" | grep '^UNCLASSIFIED' | cut -f2 | sort -u | grep -c . || true)

case "$MODE" in
report)
    printf '%s\n' "$RESULTS" | grep '^UNCLASSIFIED' | \
        awk -F'\t' '{ if ($2 != last) { print ""; print "── " $2; last = $2 } printf "  %6s  %s\n", $3, substr($4, 1, 120) }'
    if [ "$n_stale" -gt 0 ]; then
        echo; echo "── stale anchor markers (cover no retired name)"
        printf '%s\n' "$RESULTS" | grep '^STALEMARKER' | awk -F'\t' '{ printf "  %s:%s\n", $2, $3 }'
    fi
    if [ "$n_paths" -gt 0 ]; then
        echo; echo "── file/folder names still carrying a retired name"
        printf '%s\n' "$PATHS" | sed 's/^/  /'
    fi
    echo
    ;&
summary)
    echo "unclassified : $n_unclassified occurrences in $n_files files"
    echo "anchored     : $n_anchored (deliberately frozen, marked)"
    echo "stale markers: $n_stale"
    echo "paths        : $n_paths file/folder names"
    ;;
update)
    {
        echo "# FR-21 name-audit baseline — regenerate with scripts/name-audit.sh --update-baseline"
        echo "# Every number here must go DOWN (except anchors, which must not)."
        echo "unclassified=$n_unclassified"
        echo "anchors=$n_anchored"
        echo "paths=$n_paths"
    } > "$BASELINE_FILE"
    echo "baseline written: unclassified=$n_unclassified anchors=$n_anchored paths=$n_paths"
    ;;
check)
    base_unclassified=$(read_baseline unclassified 999999)
    base_anchors=$(read_baseline anchors 0)
    base_paths=$(read_baseline paths 999999)
    rc=0

    if [ "$n_stale" -gt 0 ]; then
        echo "FAIL  $n_stale stale RETIRED-NAME-ANCHOR marker(s) cover no retired name."
        echo "      A marker with nothing under it silently widens the exemption."
        printf '%s\n' "$RESULTS" | grep '^STALEMARKER' | awk -F'\t' '{ printf "        %s:%s\n", $2, $3 }'
        rc=1
    fi

    if [ "$n_anchored" -lt "$base_anchors" ]; then
        echo "FAIL  anchors dropped $base_anchors -> $n_anchored."
        echo "      A deleted anchor strands pre-rename hosts and fails NO build and NO test."
        echo "      If an anchor is genuinely retired, lower it in $BASELINE_FILE in the same PR"
        echo "      and say in the PR body which field state no longer exists."
        rc=1
    fi

    if [ "$STRICT" -eq 1 ]; then
        if [ "$n_unclassified" -ne 0 ]; then
            echo "FAIL  $n_unclassified unclassified occurrence(s); strict mode requires 0."
            rc=1
        fi
    elif [ "$n_unclassified" -gt "$base_unclassified" ]; then
        echo "FAIL  unclassified rose $base_unclassified -> $n_unclassified."
        echo "      Use the current name, or mark the site with RETIRED-NAME-ANCHOR and say why."
        rc=1
    fi

    if [ "$n_paths" -gt "$base_paths" ]; then
        echo "FAIL  retired-name paths rose $base_paths -> $n_paths."
        rc=1
    fi

    if [ "$rc" -eq 0 ]; then
        echo "OK    unclassified=$n_unclassified (<= $base_unclassified)  anchors=$n_anchored (>= $base_anchors)  paths=$n_paths (<= $base_paths)"
    fi
    exit "$rc"
    ;;
esac
