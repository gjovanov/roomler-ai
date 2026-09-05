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
set -u
cd "$(dirname "$0")/.." || exit 1
GUARD=".githooks/check-identity.sh"
ALLOW=".githooks/allowed-identities.txt"
fail=0

note() { echo "  $*"; }
bad()  { echo "FAIL: $*"; fail=1; }

[ -f "$GUARD" ] || { echo "FAIL: $GUARD missing"; exit 1; }

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
  || bad "the repo's own configured identity is refused by its own allowlist"

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
ok_addr=$(grep -vE '^\s*(#|$)' "$ALLOW" | head -1 | tr -d '[:space:]')
[ -n "$ok_addr" ] || bad "allowlist has no entries"
if [ -n "$ok_addr" ]; then
  upper=$(printf '%s' "$ok_addr" | tr '[:lower:]' '[:upper:]')
  GIT_AUTHOR_EMAIL="$upper" GIT_COMMITTER_EMAIL="$upper" \
    bash "$GUARD" --pending >/dev/null 2>&1 \
    || bad "an allowlisted address written UPPERCASE was refused"
fi

# ── 4. the allowlist may not be loosened into a pattern ────────────────────
# `*@gmail.com` or a bare domain admits every address anyone ever mistypes,
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
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/g"
cp "$GUARD" "$scratch/g/check-identity.sh"
( cd "$scratch/g" && bash ./check-identity.sh --pending >/dev/null 2>&1 ); rc=$?
[ "$rc" -eq 20 ] || bad "missing allowlist: expected exit 20, got $rc"

printf '# only comments and blanks\n\n' > "$scratch/g/allowed-identities.txt"
( cd "$scratch/g" && bash ./check-identity.sh --pending >/dev/null 2>&1 ); rc=$?
[ "$rc" -eq 20 ] || bad "EMPTY allowlist: expected exit 20 (never a pass), got $rc"

# ── 6. --range actually reads commits, not the config ──────────────────────
# A guard that quietly answered from `git config` in every mode would pass this
# repo's whole history and every canary above, and would be worthless in CI --
# where the config identity is the runner's, not the commit's.
r=$(mktemp -d) || exit 1
trap 'rm -rf "$scratch" "$r"' EXIT
(
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
  git init -q "$r" && cd "$r" || exit 1
  git config core.autocrlf false
  git config user.name  "Canary"
  git config user.email "nobody@example.invalid"
  mkdir -p .githooks
  cp "$OLDPWD/$GUARD" .githooks/check-identity.sh
  cp "$OLDPWD/$ALLOW" .githooks/allowed-identities.txt
  echo x > a.txt && git add a.txt
  git commit -qm "canary commit with a foreign identity" --no-verify
  bash .githooks/check-identity.sh --commits HEAD -1 >/dev/null 2>&1
  exit $?
); rc=$?
[ "$rc" -eq 1 ] || bad "a commit with a foreign identity: expected exit 1, got $rc"

# ── 6b. an EMPTY range must not read as a pass when the caller says so ─────
#
# ⚠️ This canary is here because writing this file FOUND the bug. The first
# draft asserted against `HEAD~0..HEAD`, which is empty, and the guard happily
# answered "all known" -- a green verdict over zero commits. That is the same
# defect as a `cargo test` filter matching no test: a job that silently ran
# nothing is indistinguishable from a passing one, and this repo has already
# been bitten by exactly that in its integration lane.
#
# A pre-push hook legitimately sees empty ranges, so silence stays the default;
# CI passes --require-commits, because an empty range THERE means the range
# expression is wrong and the check examined nothing.
bash "$GUARD" --range 'HEAD..HEAD' >/dev/null 2>&1; rc=$?
[ "$rc" -eq 0 ] || bad "an empty range without --require-commits should be quiet, got $rc"

bash "$GUARD" --require-commits --range 'HEAD..HEAD' >/dev/null 2>&1; rc=$?
[ "$rc" -eq 20 ] || bad "an empty range WITH --require-commits must be exit 20, got $rc"

# The count must actually be reported, or a reader of a green log cannot tell
# "checked forty" from "checked none".
out=$(bash "$GUARD" --commits HEAD -1 2>&1)
case "$out" in (*"(1 checked)"*) :;; (*) bad "the clean line does not report how many commits were checked: $out";; esac

# ── 7. pre-push REFUSES a real push, end to end ────────────────────────────
#
# The layer that matters most, and the only one asserted here against a real
# `git push` rather than against the guard directly. pre-commit can be stepped
# around with --no-verify and is absent entirely in a clone where nobody set
# core.hooksPath; pre-push is the last thing between a foreign identity and a
# public remote, so "the hook is wired correctly" has to be tested, not assumed.
p=$(mktemp -d) || exit 1
trap 'rm -rf "$scratch" "$r" "$p"' EXIT
(
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
  git init -q --bare "$p/remote.git" || exit 1
  git init -q "$p/work" && cd "$p/work" || exit 1
  git config core.autocrlf false
  git config core.hooksPath .githooks
  git config user.name "Canary"
  git remote add origin "$p/remote.git"
  mkdir -p .githooks
  for f in pre-push check-identity.sh allowed-identities.txt; do
    cp "$OLDPWD/.githooks/$f" ".githooks/$f"
  done
  chmod +x .githooks/pre-push .githooks/check-identity.sh

  # A commit whose identity is foreign, made with --no-verify so pre-commit is
  # explicitly bypassed -- the exact scenario pre-push exists to catch.
  git config user.email "nobody@example.invalid"
  echo x > a.txt && git add a.txt
  git commit -qm "bypassed pre-commit" --no-verify || exit 1
  if git push -q origin HEAD:refs/heads/main >/dev/null 2>&1; then
    exit 90            # pushed anyway: the hook did not fire
  fi

  # And the same push must SUCCEED once the identity is right, or the hook is
  # simply broken rather than protective. (Amend rather than a fresh commit, so
  # what changes between the two halves is only the identity.)
  git config user.email "$(grep -vE '^\s*(#|$)' .githooks/allowed-identities.txt | head -1 | tr -d '[:space:]')"
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

# ── 8. the guard runs green over what is actually about to be pushed ───────
# Not decoration: it is the assertion that the allowlist and this repo's real
# history agree, so a green CI run means something. Scoped to commits not yet
# on origin, because the historical corp-address commits below that point are
# the subject of a separate rewrite and would make this permanently red.
if git rev-parse --verify -q origin/master >/dev/null; then
  bash "$GUARD" --commits HEAD --not --remotes=origin >/dev/null 2>&1 \
    || bad "commits on this branch but not on origin carry an unknown identity"
fi

if [ "$fail" -eq 0 ]; then echo "commit-identity selftest: ok"; else
  echo "commit-identity selftest: FAILED"; fi
exit "$fail"
