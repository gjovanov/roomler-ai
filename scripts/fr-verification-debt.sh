#!/usr/bin/env bash
#
# fr-verification-debt.sh — an FR does not close on a merged PR.
#
# WHY THIS EXISTS
#
#   `CLAUDE.md` says it in three places and the FR workflow enforces it:
#   **CI green ≠ done.** An FR closes only when its acceptance criteria are
#   FIELD-VERIFIED, and since 2026-09-05 only when its docs exist too. The
#   acceptance criteria in `docs/fr/FR-*.md` are where that verification is
#   recorded — a ticked box is the claim that someone watched the thing work.
#
#   So a CLOSED issue whose spec still carries UNTICKED criteria is one of
#   exactly two things, and both matter:
#
#     1. the work WAS verified and nobody walked back to the checkboxes
#        -> the record is wrong, and the next reader cannot tell which
#     2. the work was NEVER verified and the issue closed on a green lane
#        -> precisely the lie this codebase is built to refuse
#
#   Nothing in this repository could tell those apart, or even notice. The
#   class was measured twice:
#
#     2026-09-01   8 closed FRs carrying 28 unticked criteria
#     2026-09-23  19 closed FRs carrying 74 unticked criteria
#
#   It did not decay. It more than doubled in three weeks, unobserved, while
#   `fr-registry-audit.sh` (the number-collision guard) was green throughout —
#   it audits a different axis entirely, the ledger row against the spec file.
#   This one audits the ISSUE against the spec.
#
#   ⚠️ Among the 74 is FR-73's AC9, the *docs* criterion. "Docs before close"
#      is a standing operator rule, and the one check that could have caught
#      its breach did not exist.
#
# WHY A SEPARATE SCRIPT FROM fr-registry-audit.sh
#
#   That one is hermetic: given a checkout it answers from the tree alone, with
#   no token and no network. It guards FR-number collisions between parallel
#   sessions, which has nothing to do with GitHub being reachable, and folding
#   a network call into it would let an API blip fail the collision guard.
#   This check cannot be hermetic — issue state lives on GitHub and nowhere
#   else — so it is its own script and its own CI job.
#
# THE BASELINE IS A PAIR, NOT A RATCHET
#
#   Borrowed from `name-audit.sh`, which learned it the hard way: a pure
#   "must not increase" ratchet happily passes a PR that pays off one FR's
#   debt and closes another FR with the same number of unticked criteria. The
#   total is unchanged and the guard says OK.
#
#   So each FR is pinned EXACTLY, in both directions:
#
#     count > pinned   -> new debt (criteria unticked, or an FR closed with them)
#     count < pinned   -> debt paid; lower the pin so the floor tracks the tree
#     pinned but open  -> stale entry; the FR reopened
#     pinned, no debt  -> stale entry
#     debt, not pinned -> new debt
#
#   Every entry in the baseline costs a reviewed line that SAYS WHY that FR
#   closed with criteria unticked. That is the whole point: the debt does not
#   become invisible, it becomes attributable. FR-29's entry, for instance,
#   records a criterion its own spec declares "NOT met — and not addressable
#   by P1" — honest, deliberate, and now written down where a guard can see it.
#
# USAGE
#   bash scripts/fr-verification-debt.sh                   # CI guard; non-zero on drift
#   bash scripts/fr-verification-debt.sh --summary         # report only, always exit 0
#   bash scripts/fr-verification-debt.sh --update-baseline # after debt is paid or accepted
#
# ⚠️ Deliberately NO `set -o pipefail`, for the reason `fr-registry-audit.sh`
#    records: FR-46's audit died exactly there. `grep` with no match exits 1,
#    and under `pipefail` that killed the script AT THE MOMENT IT SUCCEEDED —
#    empty stdout, empty stderr, a clean tree indistinguishable from a crash.
#    Here "no matches" is the HEALTHY answer for most of these searches.

set -eu

# ⚠️ MEASURED, not precautionary. `docs/fr/README.md` is prose — em-dashes,
#    emoji, and at least one stray `0xA8` that is not valid UTF-8 on its own.
#    In a UTF-8 locale a `sed` `.*` STOPS at that byte rather than running to
#    end of line, so a substitution that looks like it consumed the row left
#    the tail behind and the captured issue number came back as `788<0xA8>`.
#    The lookup then silently missed and eight closed FRs were reported as
#    "issue could not be resolved" instead of as debt — a guard under-reporting
#    while looking healthy, which is the failure this file exists to prevent.
#    Under `LC_ALL=C` every tool treats the file as bytes and `.` matches any
#    of them. The parsers below additionally never match against the prose
#    columns at all, so neither defence relies on the other.
export LC_ALL=C

cd "$(dirname "$0")/.."

FR_DIR="docs/fr"
README="$FR_DIR/README.md"
BASELINE="scripts/fr-ac-debt-baseline.txt"

MODE=check
case "${1:-}" in
    --summary)         MODE=summary ;;
    --update-baseline) MODE=update ;;
    "")                ;;
    *) echo "fr-verification-debt: unknown argument '$1'" >&2; exit 2 ;;
esac

[ -f "$README" ] || { echo "fr-verification-debt: $README not found" >&2; exit 2; }

# ── refuse to scan nothing ───────────────────────────────────────────────────
# `name-audit.sh` carries this guard for a measured reason: a scan that finds
# no files reports a clean tree in exactly the same words as a healthy one.
specs=$(ls -1 "$FR_DIR" 2>/dev/null | grep -E '^FR-[0-9]+-[a-z0-9-]+\.md$' | sort -u || true)
n_specs=$(printf '%s\n' "$specs" | grep -c . || true)
if [ "$n_specs" -eq 0 ]; then
    echo "FAIL  no FR specs found under $FR_DIR — refusing to report a clean tree." >&2
    exit 2
fi

# ── acceptance criteria, scoped to the section that holds them ───────────────
# Counting every checkbox in the file would sweep in phase tables and open
# decisions. The section is well defined: all 81 specs carry exactly one
# `##`-level heading whose text contains "acceptance criteria", in eight
# spellings ("## Acceptance criteria", "## 8. Acceptance criteria",
# "## Acceptance criteria (all field-verified)", …), and it runs to the next
# `##`. `###` sub-headings do NOT end it — several specs group criteria under
# them, and treating those as the end would silently undercount.
ac_counts() {
    awk '
        /^## .*[Aa]cceptance [Cc]riteria/ { in_ac = 1; next }
        /^## /                            { in_ac = 0 }
        in_ac && /^[[:space:]]*-[[:space:]]*\[[xX]\][[:space:]]/  { ticked++ }
        in_ac && /^[[:space:]]*-[[:space:]]*\[[[:space:]]\][[:space:]]/ { untick++ }
        END { printf "%d %d\n", ticked + 0, untick + 0 }
    ' "$1"
}

# ── the ledger is the FR -> issue map ────────────────────────────────────────
# Authoritative and offline: the row that claims the number also carries the
# issue it tracks. Reading it here rather than re-deriving from issue titles
# means a spec whose ledger row is missing is already `fr-registry-audit.sh`'s
# fault to report, not silently a different FR's.
#   | [FR-52](FR-52-cross-org-remote-access.md) | [#1100](https://…) | … |
#
# Split on `|` and read the FR and issue out of columns 2 and 3 ONLY. Both are
# pure ASCII link syntax; the prose that carries the awkward bytes lives in
# columns 4 and 5 and is never matched against. A whole-line regex would have
# to cross that prose to reach the end, which is exactly what broke.
fr_to_issue=$(awk -F'|' '
    /^\| \[FR-[0-9]+\]/ {
        if (match($2, /FR-[0-9]+/) == 0) next
        f = substr($2, RSTART + 3, RLENGTH - 3)
        if (match($3, /#[0-9]+/) == 0) next          # e.g. a `#TBD` row
        print f, substr($3, RSTART + 1, RLENGTH - 1)
    }' "$README" || true)

issue_for() { printf '%s\n' "$fr_to_issue" | awk -v n="$1" '$1 == n { print $2; exit }'; }

# ── issue state, the one fact that is not in the tree ────────────────────────
# One call, not one per FR. `gh` infers the repo from the checkout's remote,
# which is what makes this work unchanged in a fork or a worktree.
if ! command -v gh >/dev/null 2>&1; then
    echo "FAIL  \`gh\` is not on PATH, and issue state is the whole question here." >&2
    echo "      This check cannot be answered from the tree. Install gh, or run" >&2
    echo "      scripts/fr-registry-audit.sh for the offline half." >&2
    exit 2
fi

issue_states=$(gh issue list --state all --limit 500 --search "FR- in:title" \
                  --json number,state --jq '.[] | "\(.number) \(.state)"' 2>/dev/null || true)
if [ -z "$issue_states" ]; then
    echo "FAIL  gh returned no FR issues — refusing to report a clean tree." >&2
    echo "      Check authentication (GH_TOKEN) and network." >&2
    exit 2
fi

state_of() { printf '%s\n' "$issue_states" | awk -v n="$1" '$1 == n { print $2; exit }'; }

# ── the baseline ─────────────────────────────────────────────────────────────
pinned_for() {
    [ -f "$BASELINE" ] || return 0
    sed -nE 's/^FR-([0-9]+)=([0-9]+).*/\1 \2/p' "$BASELINE" | awk -v n="$1" '$1 == n { print $2; exit }'
}

faults=0
faults_out=""
# Collected rather than echoed as they are found: the faults are the ANSWER and
# belong after the evidence, not interleaved with the walk that produces it.
fault() { faults_out="$faults_out
  ✗ $*"; faults=$((faults + 1)); }

# ── walk every spec once, classifying as we go ───────────────────────────────
debt_lines=""     # FR-n=count for --update-baseline
ready=""          # open, every criterion ticked
unknown=""        # spec with no resolvable issue
n_debt=0
n_debt_criteria=0

for f in $specs; do
    n=$(printf '%s\n' "$f" | sed -E 's/^FR-([0-9]+)-.*/\1/')
    read -r ticked untick <<EOF
$(ac_counts "$FR_DIR/$f")
EOF

    iss=$(issue_for "$n")
    if [ -z "$iss" ]; then
        unknown="$unknown FR-$n"
        continue
    fi
    st=$(state_of "$iss")
    if [ -z "$st" ]; then
        unknown="$unknown FR-$n(#$iss)"
        continue
    fi

    if [ "$st" = "CLOSED" ] && [ "$untick" -gt 0 ]; then
        n_debt=$((n_debt + 1))
        n_debt_criteria=$((n_debt_criteria + untick))
        debt_lines="$debt_lines
FR-$n=$untick"
        pin=$(pinned_for "$n")
        if [ -z "$pin" ]; then
            fault "FR-$n (#$iss) is CLOSED with $untick unticked acceptance criteria, and is not in $BASELINE — an FR closes on FIELD VERIFICATION, not on a merged PR. Tick them with evidence, or pin them with a reason."
        elif [ "$untick" -gt "$pin" ]; then
            fault "FR-$n (#$iss) now carries $untick unticked criteria, pinned at $pin — $((untick - pin)) new."
        elif [ "$untick" -lt "$pin" ]; then
            fault "FR-$n (#$iss) carries $untick unticked criteria, pinned at $pin — debt was paid; lower the pin (--update-baseline) so the floor keeps tracking the tree."
        fi
    fi

    if [ "$st" = "OPEN" ] && [ "$untick" -eq 0 ] && [ "$ticked" -gt 0 ]; then
        ready="$ready FR-$n(#$iss,$ticked)"
    fi
done

# ── stale pins ───────────────────────────────────────────────────────────────
# A pin that no longer describes the tree is the failure mode (3) that
# `name-audit.sh` found on master: a floor protects only at the value it was
# last written to, and nothing forces it to move with the tree.
if [ -f "$BASELINE" ]; then
    while read -r pn pv; do
        [ -z "${pn:-}" ] && continue
        case "$debt_lines" in
            *"FR-$pn="*) ;;
            *) fault "$BASELINE pins FR-$pn=$pv, but FR-$pn carries no unticked criteria on a closed issue any more — remove the entry (--update-baseline)." ;;
        esac
    done <<EOF
$(sed -nE 's/^FR-([0-9]+)=([0-9]+).*/\1 \2/p' "$BASELINE")
EOF
fi

# ── --update-baseline ────────────────────────────────────────────────────────
if [ "$MODE" = update ]; then
    debt_kv=$(printf '%s\n' "$debt_lines" | grep -E '^FR-[0-9]+=' || true)

    # First creation only: lay down a skeleton. Everything after this is edited
    # SURGICALLY, never regenerated.
    if [ ! -f "$BASELINE" ]; then
        {
            echo "# FR acceptance-criteria debt — update with scripts/fr-verification-debt.sh --update-baseline"
            echo "#"
            echo "# Every line is a CLOSED FR whose spec still carries unticked acceptance"
            echo "# criteria. An FR closes on field verification, so each of these is either a"
            echo "# record that was never updated or a verification that never happened, and"
            echo "# the entry must say WHICH. Counts are pinned exactly, both directions."
            echo "#"
            echo "# ⚠️ Adding a line here is not how an FR closes. It is how an ALREADY-closed"
            echo "#    FR's debt is made attributable instead of invisible."
        } > "$BASELINE"
    fi

    # ⚠️ SURGICAL, not a rewrite — and that distinction was paid for once already
    #    in this very file. The first version regenerated the whole baseline from
    #    a fixed header plus the current counts, carrying per-entry reasons
    #    across. It passed its own test: all 19 reasons survived. What it
    #    destroyed was everything BETWEEN the entries — the provenance block
    #    saying how the file was seeded, the warning that a `## Result` comment
    #    is evidence about the ISSUE and not about these criteria, and the A/B/C
    #    grouping that carries the actual finding. The most valuable prose in the
    #    file, deleted by the command whose whole purpose was to preserve it.
    #
    #    So: comments, blank lines, grouping and ordering are NEVER touched. An
    #    entry whose count is unchanged is emitted byte-for-byte, so the diff
    #    shows only what really moved.
    tmp="$BASELINE.tmp.$$"
    awk -v debt="$debt_kv" '
        BEGIN {
            n = split(debt, arr, "\n")
            for (i = 1; i <= n; i++)
                if (arr[i] != "") { split(arr[i], kv, "="); cur[kv[1]] = kv[2] }
        }
        /^FR-[0-9]+=/ {
            key = $0; sub(/=.*/, "", key)
            if (!(key in cur)) {                       # debt paid, or FR reopened
                printf "  - dropped %s (no debt any more)\n", key > "/dev/stderr"
                next
            }
            seen[key] = 1
            v = $0; sub(/^FR-[0-9]+=/, "", v); sub(/[^0-9].*$/, "", v)
            if (v == cur[key]) { print; next }         # unchanged -> verbatim
            sub(/^FR-[0-9]+=[0-9]+/, key "=" cur[key])
            printf "  - repinned %s: %s -> %s\n", key, v, cur[key] > "/dev/stderr"
            print
            next
        }
        { print }
        END {
            for (k in cur)
                if (!(k in seen)) {
                    printf "%s=%s  # TODO: say why this FR closed with criteria unticked\n", k, cur[k]
                    printf "  - ADDED %s=%s — needs a reason before commit\n", k, cur[k] > "/dev/stderr"
                }
        }
    ' "$BASELINE" > "$tmp" && mv "$tmp" "$BASELINE"

    echo "$BASELINE updated in place — $n_debt FRs, $n_debt_criteria criteria pinned."
    echo "⚠️  Any line marked TODO records a count and nothing else. Say what it is."
    exit 0
fi

# ── report ───────────────────────────────────────────────────────────────────
echo "== closed FRs carrying unticked acceptance criteria =="
if [ "$n_debt" -eq 0 ]; then
    echo "  none"
else
    printf '%s\n' "$debt_lines" | grep -E '^FR-[0-9]+=' | sort -t- -k2 -n | sed 's/^/  /' || true
fi

echo
echo "== open FRs with every acceptance criterion ticked =="
if [ -z "$ready" ]; then
    echo "  none"
else
    # Not a fault. Closing also needs docs and, for many criteria, the
    # operator's own eyes — `autopilot` parks exactly here for that reason.
    for r in $ready; do echo "  · $r — candidate to close"; done
fi

if [ -n "$unknown" ]; then
    echo
    echo "== specs whose issue could not be resolved =="
    for u in $unknown; do echo "  ? $u — no ledger row, or the issue is not an \`FR-\` title"; done
fi

if [ "$faults" -ne 0 ]; then
    echo
    echo "== faults =="
    printf '%s\n' "$faults_out" | grep -v '^$' || true
fi

echo
echo "specs: $n_specs   debt: $n_debt FRs / $n_debt_criteria criteria   faults: $faults"

if [ "$faults" -ne 0 ] && [ "$MODE" = check ]; then
    echo
    echo "An FR closes when its acceptance criteria are FIELD-VERIFIED and its docs"
    echo "exist — not when its PR merges. Tick the criteria with linked evidence, or"
    echo "pin them in $BASELINE with a line saying why that FR closed without them."
    exit 1
fi
exit 0
