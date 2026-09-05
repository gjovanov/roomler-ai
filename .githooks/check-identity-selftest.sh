#!/usr/bin/env bash
# Prove check-identity.sh still catches what it claims to.
#
# A guard nobody has watched FAIL is not evidence of anything. Its sibling
# check_shapes.py has silently self-disarmed four times and every symptom was a
# confident "none found" -- most recently by matching uppercase only, while the
# leak that got through was written in lowercase. This file exists so the same
# class cannot happen one layer over, where the consequence is worse: a content
# leak can be swept, a published commit identity can only be rewritten.
#
# ⚠️ Every canary address is under `.invalid` (RFC 2606, reserved and
# unresolvable by definition). Never plant a real one: this file is committed
# to a PUBLIC repo, and a canary naming a real mailbox publishes exactly what
# the guard exists to keep out -- the mistake the sibling selftest documents
# making twice, in its own explanatory prose.
#
# ⚠️⚠️ HERMETIC. Every assertion below is about the GUARD, never about the
# repository this happens to be running in. The first version was not, and CI
# found two ways that fails, both on the first run:
#
#   * a CI runner has NO git identity, so `git var GIT_AUTHOR_IDENT` errors and
#     every --pending canary got exit 20 ("could not run") instead of 1;
#   * on a pull_request, actions/checkout checks out GitHub's ephemeral MERGE
#     commit, which is authored with the email set on the GitHub ACCOUNT -- so
#     a HEAD-relative assertion judged a commit that never lands on master, and
#     failed for a reason unrelated to whether the guard works.
#
# Whether the real history is clean is the CI STEP's question, answered with a
# properly computed range right after this script runs. Mixing the two made a
# guard that is fine look broken, which is how a guard gets switched off.
set -u
cd "$(dirname "$0")/.." || exit 1
GUARD=".githooks/check-identity.sh"
ALLOW=".githooks/allowed-identities.txt"
fail=0
bad() { echo "FAIL: $*"; fail=1; }

[ -f "$GUARD" ] || { echo "FAIL: $GUARD missing"; exit 1; }
[ -f "$ALLOW" ] || { echo "FAIL: $ALLOW missing"; exit 1; }

OK_ADDR=$(grep -vE '^\s*(#|$)' "$ALLOW" | head -1 | tr -d '[:space:]')
[ -n "$OK_ADDR" ] || { echo "FAIL: allowlist has no entries"; exit 1; }

# A known-good identity for the whole script, so `git var` always resolves and
# each canary can override exactly one field. Without this the canaries depend
# on the ambient git config -- present on a developer box, absent on a runner.
export GIT_AUTHOR_NAME="Identity Selftest"    GIT_COMMITTER_NAME="Identity Selftest"
export GIT_AUTHOR_EMAIL="$OK_ADDR"            GIT_COMMITTER_EMAIL="$OK_ADDR"

# ⚠️⚠️ Git skips a non-executable hook in SILENCE -- the layer disarmed with
# nothing anywhere to say so.
#
# The mode that decides is the one in the INDEX, not the one on disk, and on
# Windows those routinely disagree: `core.filemode` is false there, so `chmod
# +x` changes the filesystem bit and git records 100644 anyway. Measured while
# writing this file -- every new hook staged as 644 while `test -x` reported
# them all executable, so an earlier version of this very check passed on a
# pre-push hook git would have skipped.
#
# `git ls-files -s` reads the index, which is what git actually consults, and
# is the same answer on every platform. Fix with:
#     git update-index --add --chmod=+x <file>
for f in "$GUARD" ".githooks/pre-commit" ".githooks/pre-push" \
         ".githooks/check-identity-selftest.sh" \
         ".claude/hooks/gh-account-guard.sh" \
         ".claude/hooks/gh-account-guard-selftest.sh"; do
  mode=$(git ls-files -s -- "$f" 2>/dev/null | awk '{print $1}')
  case "${mode:-}" in
    ('')       : ;;                      # not tracked here -- nothing to assert
    (100755)   : ;;
    (*)        bad "$f is tracked mode $mode, not 100755 -- git will skip it SILENTLY" ;;
  esac
done

# ── 1. the clean path ──────────────────────────────────────────────────────
bash "$GUARD" --pending >/dev/null 2>&1 \
  || bad "an allowlisted identity was refused by --pending"

# The developer's OWN configured identity, checked only where there is one. On
# a runner there is not, and asserting it there is what broke the first run.
if cfg_addr=$(git config user.email 2>/dev/null) && [ -n "$cfg_addr" ]; then
  GIT_AUTHOR_EMAIL="$cfg_addr" GIT_COMMITTER_EMAIL="$cfg_addr" \
    bash "$GUARD" --pending >/dev/null 2>&1 \
    || bad "this clone's configured git identity ($cfg_addr) is not on the allowlist"
fi

# ── 2. a foreign identity is CAUGHT, in BOTH casings ───────────────────────
#
# ⚠️⚠️ The lowercase/uppercase pair is not a tidy-up-able duplicate. It is the
# regression test for the exact leak that got past the sibling guard on
# 2026-09-04: the uppercase canaries all passed that day. Email domains are
# case-insensitive and people paste whatever their terminal produced, so a
# guard that assumes a casing is not a guard.
for canary in 'nobody@example.invalid' 'NOBODY@EXAMPLE.INVALID'; do
  out=$(GIT_AUTHOR_EMAIL="$canary" bash "$GUARD" --pending 2>&1); rc=$?
  [ "$rc" -eq 1 ] || bad "author canary $canary: expected exit 1, got $rc"
  case "$out" in (*REFUSED*) :;; (*) bad "author canary $canary: no REFUSED banner";; esac
done

# The COMMITTER is a separate field and a separate leak. git lets them differ,
# and a rebase or a web-flow merge routinely makes them differ -- a guard that
# only reads the author passes a commit whose committer is the wrong person.
out=$(GIT_COMMITTER_EMAIL='nobody@example.invalid' bash "$GUARD" --pending 2>&1); rc=$?
[ "$rc" -eq 1 ] || bad "committer canary: expected exit 1, got $rc"
case "$out" in (*committer*) :;; (*) bad "committer canary: role not reported as committer";; esac

# ── 3. an ALLOWED identity is still allowed when written in another case ───
# The other half of case-insensitivity, and the one that turns the guard into
# noise if it regresses: refusing the owner's own address because it arrived
# uppercased is how a hook gets switched off within a week.
upper=$(printf '%s' "$OK_ADDR" | tr '[:lower:]' '[:upper:]')
GIT_AUTHOR_EMAIL="$upper" GIT_COMMITTER_EMAIL="$upper" \
  bash "$GUARD" --pending >/dev/null 2>&1 \
  || bad "an allowlisted address written UPPERCASE was refused"

# ── 4. the allowlist may not be loosened into a pattern ────────────────────
# `*@example.com` or a bare domain admits every address anyone ever mistypes,
# which is precisely the property the allowlist exists to deny. It reads like a
# harmless simplification in review, so it is asserted here instead.
if grep -qE '^\s*[^#]*[*?]' "$ALLOW"; then
  bad "allowed-identities.txt contains a wildcard -- an allowlist of patterns admits the class it excludes"
fi
if grep -qE '^\s*@' "$ALLOW"; then
  bad "allowed-identities.txt contains a bare domain entry -- entries must be whole addresses"
fi

# ── 5. the EXIT-CODE CONTRACT with the hooks ───────────────────────────────
#
# The hooks block on 1 and on nothing else, so "a foreign identity" and "the
# guard could not run" must never share a status. They did in the sibling guard
# until 2026-09-04, and it consequently told authors they had committed a
# hostname when it had merely crashed -- twice, from two unrelated causes.
#
# ⚠️ Each assertion below tests for a SPECIFIC status. Asserting merely
# "non-zero" would pass on the exact bug this locks.
bash "$GUARD" --no-such-flag >/dev/null 2>&1; rc=$?
[ "$rc" -eq 2 ] || bad "bad args: expected exit 2, got $rc"

bash "$GUARD" >/dev/null 2>&1; rc=$?
[ "$rc" -eq 2 ] || bad "no args: expected exit 2, got $rc"

# An internal failure must be 20, never 1. Forced honestly: a copy of the guard
# whose allowlist is absent, and one whose allowlist is present but empty.
#
# ⚠️ The EMPTY case is the one worth having. "No allowlist" and "an empty
# allowlist" are different answers and only one of them may be treated as a
# pass -- the same Some([]) vs None distinction this codebase pays for in its
# overlay ACLs, where collapsing them turns a deny into a grant. Here,
# collapsing them would make an unreadable allowlist mean "everything is fine".
scratch=$(mktemp -d) || exit 1
r=$(mktemp -d) || exit 1
p=$(mktemp -d) || exit 1
trap 'rm -rf "$scratch" "$r" "$p"' EXIT

mkdir -p "$scratch/g"
cp "$GUARD" "$scratch/g/check-identity.sh"
( cd "$scratch/g" && bash ./check-identity.sh --pending >/dev/null 2>&1 ); rc=$?
[ "$rc" -eq 20 ] || bad "missing allowlist: expected exit 20, got $rc"

printf '# only comments and blanks\n\n' > "$scratch/g/allowed-identities.txt"
( cd "$scratch/g" && bash ./check-identity.sh --pending >/dev/null 2>&1 ); rc=$?
[ "$rc" -eq 20 ] || bad "EMPTY allowlist: expected exit 20 (never a pass), got $rc"

# ── 6. the commit-reading modes, in a repo built for the purpose ───────────
#
# ⚠️ A throwaway repo, NOT this one. A guard that quietly answered from `git
# config` in every mode would pass every canary above and be worthless in CI,
# so the modes have to be exercised against real commits -- but against
# commits whose identities this file CHOSE, not whichever ones the checkout
# happened to produce.
GUARD_ABS="$PWD/$GUARD"; ALLOW_ABS="$PWD/$ALLOW"
(
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
  git init -q "$r" && cd "$r" || exit 1
  git config core.autocrlf false
  mkdir -p .githooks
  cp "$GUARD_ABS" .githooks/check-identity.sh
  cp "$ALLOW_ABS" .githooks/allowed-identities.txt
  G=.githooks/check-identity.sh

  # A clean commit, then a foreign one, so both verdicts are exercised against
  # commits rather than config.
  echo a > a.txt && git add a.txt
  git commit -qm "clean commit" --no-verify || exit 70
  bash "$G" --commits HEAD -1 >/dev/null 2>&1 || exit 71

  # The count must be REPORTED. "all known" over zero commits and over forty
  # are the same sentence and opposite facts.
  out=$(bash "$G" --commits HEAD -1 2>&1)
  case "$out" in (*"(1 checked)"*) :;; (*) exit 72;; esac

  echo b > b.txt && git add b.txt
  GIT_AUTHOR_EMAIL='nobody@example.invalid' \
    git commit -qm "foreign commit" --no-verify || exit 73
  bash "$G" --commits HEAD -1 >/dev/null 2>&1 && exit 74
  bash "$G" --range HEAD~1..HEAD >/dev/null 2>&1 && exit 75

  # ⚠️ An EMPTY range must not read as a pass when the caller says so. This
  # canary exists because writing this file FOUND the bug: the first draft
  # asserted against `HEAD~0..HEAD`, which is empty, and the guard happily
  # answered "all known" -- a green verdict over zero commits, the same defect
  # as a `cargo test` filter matching no test. A pre-push hook legitimately
  # sees empty ranges, so silence stays the default; CI passes
  # --require-commits, where an empty range means the range expression is wrong.
  bash "$G" --range 'HEAD..HEAD' >/dev/null 2>&1 || exit 76
  bash "$G" --require-commits --range 'HEAD..HEAD' >/dev/null 2>&1; [ $? -eq 20 ] || exit 77
  exit 0
); rc=$?
case "$rc" in
  0)  ;;
  70|73) bad "scratch repo: could not create a commit (exit $rc)" ;;
  71) bad "--commits over a CLEAN commit was refused" ;;
  72) bad "the clean line does not report how many commits were checked" ;;
  74) bad "--commits over a commit with a foreign identity did not refuse" ;;
  75) bad "--range over a commit with a foreign identity did not refuse" ;;
  76) bad "an empty range without --require-commits should be quiet" ;;
  77) bad "an empty range WITH --require-commits must be exit 20" ;;
  *)  bad "scratch-repo canaries could not run (exit $rc)" ;;
esac

# ── 7. pre-push REFUSES a real push, end to end ────────────────────────────
#
# The layer that matters most, and the only one asserted here against a real
# `git push` rather than against the guard directly. pre-commit can be stepped
# around with --no-verify and is absent entirely in a clone where nobody set
# core.hooksPath; pre-push is the last thing between a foreign identity and a
# public remote, so "the hook is wired correctly" has to be tested, not assumed.
HOOKS_ABS="$PWD/.githooks"
(
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
  [ -f "$HOOKS_ABS/pre-push" ] || exit 0        # branch predates the hook
  git init -q --bare "$p/remote.git" || exit 1
  git init -q "$p/work" && cd "$p/work" || exit 1
  git config core.autocrlf false
  git config core.hooksPath .githooks
  git remote add origin "$p/remote.git"
  mkdir -p .githooks
  for f in pre-push check-identity.sh allowed-identities.txt; do
    cp "$HOOKS_ABS/$f" ".githooks/$f"
  done
  chmod +x .githooks/pre-push .githooks/check-identity.sh

  # A commit whose identity is foreign, made with --no-verify so pre-commit is
  # explicitly bypassed -- the exact scenario pre-push exists to catch.
  echo x > a.txt && git add a.txt
  GIT_AUTHOR_EMAIL='nobody@example.invalid' GIT_COMMITTER_EMAIL='nobody@example.invalid' \
    git commit -qm "bypassed pre-commit" --no-verify || exit 1
  if git push -q origin HEAD:refs/heads/main >/dev/null 2>&1; then
    exit 90            # pushed anyway: the hook did not fire
  fi

  # And the same push must SUCCEED once the identity is right, or the hook is
  # simply broken rather than protective. Amend rather than a fresh commit, so
  # what changes between the two halves is only the identity.
  git commit -q --amend --reset-author --no-edit --no-verify || exit 1
  git push -q origin HEAD:refs/heads/main >/dev/null 2>&1 || exit 91
  exit 0
); rc=$?
case "$rc" in
  0)  ;;
  90) bad "pre-push let a foreign-identity commit through to a remote" ;;
  91) bad "pre-push refused a push whose identity IS allowlisted" ;;
  *)  bad "pre-push end-to-end canary could not run (exit $rc)" ;;
esac

# NOTE: there is deliberately NO assertion here that this repository's own
# history is clean. That is the CI step's question, asked immediately after
# this script with a range computed from the event -- and it is a question with
# a different answer on a developer box, on a PR merge commit, and mid-way
# through a history rewrite. An earlier version asserted it and failed on the
# first CI run for a reason that had nothing to do with the guard.

if [ "$fail" -eq 0 ]; then echo "commit-identity selftest: ok"; else
  echo "commit-identity selftest: FAILED"; fi
exit "$fail"
