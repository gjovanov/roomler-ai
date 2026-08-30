#!/usr/bin/env bash
# Prove check_shapes.py still catches every class it claims to.
#
# A guard nobody has watched FAIL is not evidence of anything, and this one has
# been wrong three times already in ways that all read fine:
#
#   * an allowlist entry meant to skip long numeric runs matched the corp asset
#     tags themselves, so the guard silently ignored the whole class it exists
#     for -- it reported "none found" on a tree that had them;
#   * it scanned only tracked files, so a name arriving in a NEW file sailed
#     past the very commit that introduced it;
#   * it flagged its own source, because sanitize.py's docstring used a real
#     machine as an example.
#
# All three were found by planting canaries, none by reading the code. Run this
# whenever SHAPES or ALLOW changes.
#
# ⚠️ The canaries are ASSEMBLED FROM FRAGMENTS below and never appear whole in
# this file. They must not: the guard scans new-but-unignored files, this is
# one, and a literal canary here would make the guard fail on a clean tree --
# a self-test that breaks the thing it tests.
set -u
cd "$(dirname "$0")/../../.." || exit 1
GUARD=".claude/skills/sanitize-hostnames/check_shapes.py"
CANARY="canary-selftest.md"
trap 'rm -f "$CANARY"' EXIT

D="DESK""TOP-AB12XY9"; L="LAP""TOP-QR34ST78"; W="W""IN-ABCDEFGHIJK"
P5="P""C51234";        P4="P""C5123";         C="CL""K00099887"
X="Someone-X""MG-BOX9"; M="Someones-Mac""Book-Pro"
CAUGHT=("$D" "$L" "$W" "$P5" "$P4" "$C" "$X" "$M")
# Alias forms the guard must NOT flag.
IGNORED=("WIN""HOST-A" "CORP""LAP-2" "Mac""Book-1" "ARGB""2101010" "XRGB""8888")

{ printf '%s\n' "${CAUGHT[@]}"; printf 'must not trip: %s\n' "${IGNORED[*]}"; } > "$CANARY"

out=$(python3 "$GUARD" --root . 2>&1); rc=$?
rm -f "$CANARY"

fail=0
for n in "${CAUGHT[@]}"; do
  grep -qF -- "$n" <<<"$out" || { echo "MISSED: $n"; fail=1; }
done
for n in "${IGNORED[@]}"; do
  grep -qE "  ${n}\$" <<<"$out" && { echo "FALSE POSITIVE: $n"; fail=1; }
done
[ "$rc" -eq 1 ] || { echo "expected exit 1 with canaries present, got $rc"; fail=1; }

if python3 "$GUARD" --root . >/dev/null 2>&1; then :; else
  echo "expected exit 0 on the clean tree; the guard flags something already committed"
  python3 "$GUARD" --root . 2>&1 | sed -n '1,8p'
  fail=1
fi

if [ "$fail" -eq 0 ]; then echo "sanitize-hostnames selftest: ok"; else
  echo "sanitize-hostnames selftest: FAILED"; fi
exit "$fail"
