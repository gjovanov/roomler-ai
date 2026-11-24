#!/usr/bin/env bash
#
# fr-registry-audit.sh — keep `docs/fr/README.md` and `docs/fr/FR-*.md` in sync.
#
# WHY THIS EXISTS
#
#   The FR registry is not documentation. It is the mechanism that ARBITRATES
#   FR-number collisions: the standing rule is "claim the number by adding your
#   row to docs/fr/README.md in the SAME commit as the spec", precisely so git
#   rejects the loser's push as non-fast-forward instead of two sessions
#   discovering the clash after both have published. That only works while the
#   row and the spec travel together.
#
#   They stopped travelling together and nothing noticed. On 2026-09-02 master
#   carried an FR-52 row whose spec had NEVER existed on master: the row was
#   introduced by f2221779 — a PR about FR-49 and FR-50 — which had carried it
#   in as a passenger on a stale branch. The registry advertised a claimed
#   number, linked to a 404, and documented nothing, for two days. It also made
#   the real FR-52 PR unmergeable (both sides added the row), so it stalled
#   silently with every check green.
#
#   Four failures, each invisible to every other check in this repo:
#
#     1. a row whose spec does not exist  -> a claimed number documenting nothing
#     2. a spec with no row               -> an UNCLAIMED number: the collision
#                                            arbitration never armed, so two
#                                            sessions can still take it
#     3. the same number claimed twice    -> the collision that arbitration was
#                                            supposed to have prevented
#     4. a closed row whose spec has no   -> a closed spec is pinned history
#        preamble pin                        (#4413): without the pin, every
#                                            anchor pass that re-points line
#                                            numbers after a code move shifts
#                                            its anchors away from the code its
#                                            text describes
#
#   (2) is the one worth the script on its own. A missing row fails no build and
#   no test; it just leaves the next session free to pick the same number.
#
# USAGE
#   bash scripts/fr-registry-audit.sh            # audit, exit non-zero on any fault
#   bash scripts/fr-registry-audit.sh --summary  # report only, always exit 0
#
# ⚠️ Deliberately NO `set -o pipefail` around the greps below. FR-46's audit
#    died exactly there: `grep` with no match exits 1, and under `pipefail` that
#    killed the script AT THE MOMENT IT SUCCEEDED — with empty stdout and empty
#    stderr, so a clean tree looked identical to a crashed run. Here "no
#    matches" is the HEALTHY answer for every one of these searches, so each
#    grep is allowed to fail and its emptiness is tested explicitly.

set -eu

cd "$(dirname "$0")/.."

README="docs/fr/README.md"
SUMMARY=0
[ "${1:-}" = "--summary" ] && SUMMARY=1

[ -f "$README" ] || { echo "fr-registry-audit: $README not found" >&2; exit 2; }

faults=0
note() { echo "$@"; }
fault() { echo "  ✗ $*"; faults=$((faults + 1)); }

# ── the two sides of the registry ────────────────────────────────────────────
# Rows look like:  | [FR-52](FR-52-cross-org-remote-access.md) | [#1100](…) | …
# `|| true` on every extraction: an empty result is a legitimate state, and
# under `set -e` an unmatched grep would abort the run instead of reporting it.
linked=$(grep -oE '\(FR-[0-9]+-[a-z0-9-]+\.md\)' "$README" | tr -d '()' | sort -u || true)
specs=$(ls -1 docs/fr/ 2>/dev/null | grep -E '^FR-[0-9]+-[a-z0-9-]+\.md$' | sort -u || true)

# ── 1. every linked spec must exist ──────────────────────────────────────────
note "== rows whose spec is missing =="
for f in $linked; do
    [ -f "docs/fr/$f" ] || fault "$README links $f — but that file does not exist (a claimed number documenting nothing)"
done
[ "$faults" -eq 0 ] && note "  none"

before=$faults

# ── 2. every spec must be claimed by a row ───────────────────────────────────
note "== specs with no ledger row =="
for f in $specs; do
    case "$linked" in
        *"$f"*) ;;
        *) fault "docs/fr/$f has NO row in $README — the number is UNCLAIMED, so the collision arbitration never armed for it" ;;
    esac
done
[ "$faults" -eq "$before" ] && note "  none"

before=$faults

# ── 3. no number may be claimed twice ────────────────────────────────────────
# The dup case is the collision the ledger exists to prevent; if it reaches
# master, the arbitration has already failed and a human must renumber.
note "== numbers claimed more than once =="
dups=$(grep -oE '^\| \[FR-[0-9]+\]' "$README" | sort | uniq -d || true)
if [ -n "$dups" ]; then
    for d in $dups; do
        case "$d" in
            *FR-*) fault "$d appears as more than one row — renumber per the rule in $README (LOWER issue number keeps the FR)" ;;
        esac
    done
else
    note "  none"
fi

before=$faults

# ── 4. a closed FR's spec carries its preamble pin ──────────────────────────
# A closed FR spec is the record of the work: its `file:line` anchors read at the
# master commit its close read (#4413). The anchor passes that re-point line
# numbers after a code move leave a spec alone only when its preamble carries a
# pin, so the commit that closes an FR adds one:
#     **Anchors:** master `<sha>` (closed <date>)
# Closed: the row's status (its fourth cell), after any "*", "✅" and blanks,
# starts with "closed", in any case. A pin, as the anchor tools read one in a
# preamble: a line before the spec's first "##" heading, not itself a heading,
# that names "Anchors" (or "anchors") and either a commit (master `<hex>`,
# "anchors at `", "anchors on `") or a "(YYYY-MM-DD" date; or a commit line whose
# paragraph names "Anchors" on an earlier line (a header wrapped as
# "... · **Anchors:**" / "master `<sha>` · ...").
note "== closed FRs whose spec has no preamble pin =="
closed=$(awk -F'|' '
    /^\| \[FR-[0-9]+\]\(FR-[0-9]+-[a-z0-9-]+\.md\)/ {
        s = $5
        while (s ~ /^([ \t*]|✅)/) sub(/^([ \t*]|✅)/, "", s)
        if (tolower(substr(s, 1, 6)) == "closed" && match($2, /FR-[0-9]+-[a-z0-9-]+\.md/))
            print substr($2, RSTART, RLENGTH)
    }' "$README" | sort -u || true)
for f in $closed; do
    [ -f "docs/fr/$f" ] || continue   # check 1 reports a missing spec
    awk '
        /^##/ { exit }
        /^#/ || /^[ \t\r]*$/ { para = 0; next }
        {
            commit = ($0 ~ /anchors at `|anchors on `|master `[0-9a-f]+`/)
            dated = ($0 ~ /\(20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]/)
            named = ($0 ~ /[Aa]nchors/)
            if ((named && (commit || dated)) || (commit && para)) { found = 1; exit }
            if (named) para = 1
        }
        END { exit !found }' "docs/fr/$f" ||
        fault "docs/fr/$f is closed in $README, but its preamble has no pin: add \`**Anchors:** master \`<sha>\` (closed <date>)\` as its own paragraph under its CLOSED banner, naming the master commit the close reads"
done
[ "$faults" -eq "$before" ] && note "  none"

# ── verdict ──────────────────────────────────────────────────────────────────
n_specs=$(printf '%s\n' "$specs" | grep -c . || true)
n_rows=$(grep -cE '^\| \[FR-[0-9]+\]' "$README" || true)
echo
echo "specs: $n_specs   rows: $n_rows   faults: $faults"

if [ "$faults" -ne 0 ] && [ "$SUMMARY" -eq 0 ]; then
    echo
    echo "The FR registry is what arbitrates number collisions between parallel"
    echo "sessions. Add the row and the spec in the SAME commit — see the claim"
    echo "protocol at the top of $README. The commit that closes an FR pins its"
    echo "spec — see \"Closing an FR\" in $README."
    exit 1
fi
exit 0
