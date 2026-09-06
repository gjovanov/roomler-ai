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

D="DESK""TOP-AB12XY9"; L="LAP""TOP-QR34ST78"; W="W""IN-ABC4EFGHIJK"
P5="P""C51234";        P4="P""C5123";         C="CL""K00099887"
X="Someone-X""MG-BOX9"; M="Someones-Mac""Book-Pro"
# The GENERALISED three-letter tag. Deliberately a prefix that is not any real
# one, and whose letters are not all hex -- an all-hex prefix is excluded on
# purpose, see the shape's comment.
#
# ⚠️ ROTATED 2026-09-06. The previous value is now in IGNORED, not gone: writing
# up the guard published the canary into a GitHub issue, so step 2b's scan of
# published content reported it on every run. A standing "known benign" hit is
# how a check earns the noise that gets it deleted -- but ALLOWLISTING a value
# that is still in CAUGHT makes the canary vacuous, and the selftest says so
# immediately (`MISSED:` in both casings). Rotating is what lets both hold: the
# retired value moves to the allowlist AND to IGNORED, so the exemption is
# itself locked, and this fresh value carries the catch assertion.
#
# 🔑 The cost is one canary value per write-up. If that recurs, do NOT keep
# rotating -- filter the dump at the step 2b boundary instead, which is where
# the noise actually is. See SKILL.md.
G="QV""Z77777"

# ⚠️⚠️ EVERY canary appears TWICE, upper and lower. A guard blind to lowercase
# is not a hypothetical: on 2026-09-04 a field log wrote two asset tags in
# lowercase, this guard reported "none found", CI went green, and the names
# reached a public repo. The uppercase canaries all passed that day -- which is
# exactly why the lowercase ones now exist. Do not "tidy" them away as
# duplicates; they are the regression test for the leak that happened.
d="desk""top-ab12xy9"; l="lap""top-qr34st78"; w="w""in-abc4efghijk"
p5="p""c51234";        p4="p""c5123";         c="cl""k00099887"
x="someone-x""mg-box9"; m="someones-mac""book-pro"
g="qv""z77777"

CAUGHT=("$D" "$L" "$W" "$P5" "$P4" "$C" "$X" "$M" "$G"
        "$d" "$l" "$w" "$p5" "$p4" "$c" "$x" "$m" "$g")

# Things the guard must NOT flag: our own alias forms (in both casings, since
# prose writes them either way), and ordinary hyphenated words that fit the
# DESKTOP- shape. Those words are all real strings in this repo, and flagging
# them is how a guard earns enough noise to get deleted -- the digit
# requirement in SHAPES is what keeps them out, and this locks it.
IGNORED=("WIN""HOST-A" "CORP""LAP-2" "Mac""Book-1" "ARGB""2101010" "XRGB""8888"
         "win""host-a" "corp""lap-2"
         "desk""top-classic" "desk""top-content" "desk""top-rebound"
         # The three-letter shape's real false-positive families, measured
         # across the whole history. The first two are hex — a GUID example
         # from msi_guid.rs and the tail of a codecov UUID — and are excluded
         # structurally by the shape's lookahead. The third is a spec number in
         # an X11 font string and is allowlisted. If any of these ever trips,
         # the shape has been loosened and a real GUID will start crying wolf.
         "DEF""012345678" "dbf""342009340" "iso""10646" "ISO""10646"
         # The RETIRED canary (rotated 2026-09-06). It is allowlisted because
         # writing up the guard published it, so it now appears only in
         # documentation -- and it lives HERE so that exemption is asserted
         # rather than assumed. Delete this pair and the allowlist entry
         # together, never one alone: the entry with no assertion is an
         # unguarded hole, and the assertion with no entry fails the selftest.
         "ZQ""X55555" "zq""x55555")

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

# --- the exit-code CONTRACT with .githooks/pre-commit -----------------------
#
# The hook blocks on EXIT_FOUND (1) and on nothing else, so "a name was found"
# and "the guard could not run" must never share a status. They did until
# 2026-09-04, and the hook consequently reported `COMMIT REFUSED: this change
# carries what looks like a real machine name` for a file of ordinary prose --
# twice, from two unrelated causes (a checkout predating `--staged`, and git
# exiting 128 inside a worktree). A guard that cries hostname when it means
# "I crashed" is one someone silences with --no-verify, which removes the layer.
#
# Both canaries below assert a NON-1 status. Asserting merely "non-zero" would
# pass on the exact bug this locks.

# Bad arguments must be argparse's 2, never 1.
python3 "$GUARD" --root . --no-such-flag >/dev/null 2>&1; rc=$?
[ "$rc" -eq 2 ] || { echo "bad args: expected exit 2, got $rc"; fail=1; }

# An internal failure must be EXIT_ERROR (20), never 1. Forcing one honestly:
# --staged shells out to git, so a git dir that cannot exist makes the guard
# raise exactly where a real worktree failure raised.
( export GIT_DIR="$PWD/.no-such-git-dir"
  python3 "$GUARD" --root . --staged >/dev/null 2>&1; exit $? ); rc=$?
[ "$rc" -eq 20 ] || { echo "internal error: expected exit 20, got $rc"; fail=1; }

# The hook's own decision, exercised in a THROWAWAY repo with a stub guard.
#
# Driving it against this repo would mean breaking the real check_shapes.py
# mid-test; a scratch repo runs the identical code path with none of that risk,
# and it is the only way to assert the branch honestly -- what matters is that a
# guard exiting 20 and a guard exiting 1 lead to opposite outcomes.
HOOK="$PWD/.githooks/pre-commit"

# ⚠️ A skip must ANNOUNCE itself. Guarding these canaries with `if [ -x ... ]`
# and nothing else reproduced, in this very file, the defect they exist to
# lock: with the hook absent the block vanished and the run still printed
# `selftest: ok` -- a pass that had asserted nothing about the hook. So the
# three states are now distinguished explicitly, and only one of them is quiet.
# ⚠️ `git ls-files` has THREE outcomes, not two: 0 tracked, 1 not tracked, and
# anything else "git could not answer". Treating the third as "not tracked" is
# the same conflation the hook was just fixed for, and it bites in exactly the
# same place -- running this from WSL inside a Windows worktree, where `.git` is
# a file holding a Windows path and git exits 128. Collapsing that into the
# untracked branch made this file report a confident, wrong FAILURE.
git ls-files --error-unmatch .githooks/pre-commit >/dev/null 2>&1; tracked=$?
case "$tracked" in
  0)
    # Tracked HERE, so it must be present and executable. Git ignores a
    # non-executable hook in silence: layer 1 disarmed with nothing to say so.
    [ -e "$HOOK" ] || { echo "hook: tracked but missing from the working tree"; fail=1; }
    [ ! -e "$HOOK" ] || [ -x "$HOOK" ] || {
      echo "hook: .githooks/pre-commit is not executable (git will skip it silently)"; fail=1; }
    ;;
  1)
    if [ -e "$HOOK" ]; then
      # Present but untracked -- the pre-2026-09-04 arrangement, where the file
      # lived in one clone and every worktree silently had no hook at all.
      echo "hook: present but UNTRACKED -- other worktrees of this clone have none"; fail=1
    else
      echo "hook: not in this checkout (branch predates it) -- hook canaries skipped"
    fi
    ;;
  *)
    # Not a verdict. Say so instead of inventing one; the canaries below still
    # run if the file is there, since they need no git of their own.
    echo "hook: git could not report tracking (exit $tracked) -- tracking check skipped"
    ;;
esac

if [ -x "$HOOK" ] && command -v python3 >/dev/null 2>&1; then
  hook_says() {   # $1 = status the stub guard exits with; echoes "<rc>|<output>"
    # ⚠️ The scratch repo must NOT inherit GIT_DIR/GIT_WORK_TREE. The hook
    # exports both when it crosses into WSL, so a selftest run from such a
    # context pointed every `git` here at the OUTER repo: `git add` failed, and
    # under `set -u` the unassigned `o` aborted the function with `unbound
    # variable` -- a canary that dies instead of reporting.
    ( unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
      t=$(mktemp -d) || exit 1
      git init -q "$t" || { rm -rf "$t"; exit 1; }
      mkdir -p "$t/.githooks" "$t/.claude/skills/sanitize-hostnames"
      cp "$HOOK" "$t/.githooks/pre-commit"; chmod +x "$t/.githooks/pre-commit"
      printf '#!/usr/bin/env python3\nimport sys\nsys.exit(%s)\n' "$1" \
        > "$t/.claude/skills/sanitize-hostnames/check_shapes.py"
      cd "$t" || exit 1
      echo ordinary-prose > a.txt
      git add a.txt || { rm -rf "$t"; exit 1; }
      # Assign unconditionally, then read $? from the assignment itself, so no
      # path through this function can leave the output variable unset.
      o=$(.githooks/pre-commit 2>&1); rc=$?
      printf '%s|%s' "$rc" "$o"
      cd / && rm -rf "$t" )
  }

  r=$(hook_says 20)
  [ "${r%%|*}" = "0" ]              || { echo "hook: a guard that CANNOT RUN must not block (got ${r%%|*})"; fail=1; }
  case "$r" in *"COULD NOT RUN"*) :;; *) echo "hook: missing the could-not-run warning"; fail=1;; esac
  case "$r" in *"COMMIT REFUSED"*) echo "hook: claimed a hostname when it merely failed"; fail=1;; esac

  r=$(hook_says 1)
  [ "${r%%|*}" = "1" ]              || { echo "hook: a real finding must block (got ${r%%|*})"; fail=1; }
  case "$r" in *"COMMIT REFUSED"*) :;; *) echo "hook: a finding must say COMMIT REFUSED"; fail=1;; esac

  r=$(hook_says 0)
  [ "${r%%|*}" = "0" ]              || { echo "hook: a clean guard must allow the commit (got ${r%%|*})"; fail=1; }
fi

if [ "$fail" -eq 0 ]; then echo "sanitize-hostnames selftest: ok"; else
  echo "sanitize-hostnames selftest: FAILED"; fi
exit "$fail"
