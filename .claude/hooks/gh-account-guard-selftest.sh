#!/usr/bin/env bash
# Prove gh-account-guard.sh still blocks what it claims to.
#
# The reason this file exists at all is the rule its sibling guards paid for
# four times over: a guard nobody has watched FAIL is not evidence of anything.
# Every one of those regressions read as a confident pass, and this hook is the
# most likely of the set to rot silently -- it matches on `gh` subcommand
# spellings, which upstream is free to change, and its failure mode is not an
# error but a write that quietly goes out under the wrong name.
#
# ⚠️⚠️ The load-bearing half is the POSITIVE CONTROL in section 2. A run that
# only ever exercises the correct account proves the hook is quiet, not that it
# can speak: the hook returns 0 for "allowed" and 0 for "I could not tell who
# is active", and without a canary that must BLOCK, those are the same result.
# That is the same defect this repo found in an FR-68 stress cell, where a
# green multi-adapter assertion turned out to have run on a single-adapter host
# and could never have gone red.
set -u
cd "$(dirname "$0")/../.." || exit 1
HOOK=".claude/hooks/gh-account-guard.sh"
fail=0
bad() { echo "FAIL: $*"; fail=1; }

[ -f "$HOOK" ] || { echo "FAIL: $HOOK missing"; exit 1; }
if git ls-files --error-unmatch "$HOOK" >/dev/null 2>&1; then
  [ -x "$HOOK" ] || bad "$HOOK is tracked but not executable"
fi

export CLAUDE_PROJECT_DIR="$PWD"
tmp=$(mktemp -d) || exit 1
trap 'rm -rf "$tmp"' EXIT

# Two configs differing ONLY in the active account, so every assertion below
# isolates that one variable.
mkdir -p "$tmp/right" "$tmp/wrong" "$tmp/empty"
printf 'github.com:\n    git_protocol: https\n    user: gjovanov\n'     > "$tmp/right/hosts.yml"
printf 'github.com:\n    git_protocol: https\n    user: someone-else\n' > "$tmp/wrong/hosts.yml"

run() {  # $1 = config dir, $2 = command text -> exit status
  printf '{"tool_name":"Bash","tool_input":{"command":"%s"}}' "$2" \
    | GH_CONFIG_DIR="$1" bash "$HOOK" >/dev/null 2>&1
}

WRITES=(
  'gh issue create --title x'
  'gh issue comment 5 --body hi'
  'gh issue edit 5 --body hi'
  'gh pr create --fill'
  'gh pr comment 5 --body hi'
  'gh pr merge 5'
  'gh release create v1'
  'gh api -X POST repos/a/b/issues'
  'gh api --method PATCH repos/a/b/issues/1'
  'gh api -X DELETE repos/a/b/issues/comments/1'
  'gh api graphql -f query=mutation{deleteIssue}'
  'git push origin HEAD'
)
READS=(
  'gh issue view 123'
  'gh pr list'
  'gh api repos/a/b/issues'
  'gh api user --jq .login'
  'gh run watch 1'
  'cargo test'
)

# Commands that WRITE NOTHING but contain the text of a write verb. The guard
# treats them as writes, and that is the accepted trade rather than an
# oversight -- narrowing the match to let them through is how a substring guard
# acquires the false negatives it exists to avoid, since quoting can hide a
# command's structure from a parser but not its bytes from a match.
#
# ⚠️ Asserted explicitly, in BOTH directions, because an unasserted known
# false positive is indistinguishable from an unnoticed one: the next person to
# meet a blocked grep would "fix" it by loosening the match, and nothing here
# would object. This entry is what objects.
MENTIONS_ONLY=(
  'grep -rn gh issue create .'
  'echo see docs for gh pr create usage'
)

# ── prose that CITES these commands in backticks must always pass ──────────
#
# ⚠️ This is the regression test for a defect that shipped for about ninety
# seconds: the guard blocked its own introducing commit. That commit message
# explains why `gh auth switch` is refused, the matcher saw the bytes, and the
# `gh auth switch` block is unconditional -- so it fired regardless of account,
# on a commit that ran no gh command at all.
#
# In a repo whose convention is dense prose about its own guards, a guard that
# cannot be written about is one that gets switched off within a day. Backtick
# spans are therefore stripped before matching: a citation is not an invocation.
# Asserted against BOTH configs, because the auth-switch block ignores the
# active account and would otherwise regress unnoticed in the "right" case.
CITATIONS=(
  'git commit -m explains why `gh auth switch` is refused outright'
  'git commit -m the `gh issue comment` path posts as the active account'
  'echo docs mention `gh auth login` and `gh pr create` together'
)

# ── 1. the correct account: writes pass, reads pass ────────────────────────
for c in "${WRITES[@]}"; do
  run "$tmp/right" "$c" || bad "correct account: write was blocked -> $c"
done
for c in "${READS[@]}" "${MENTIONS_ONLY[@]}"; do
  run "$tmp/right" "$c" || bad "correct account: read was blocked -> $c"
done

# ── 2. POSITIVE CONTROL -- the wrong account: every write must BLOCK ───────
for c in "${WRITES[@]}"; do
  run "$tmp/wrong" "$c" && bad "WRONG account: write was ALLOWED -> $c"
done

# ...and reads must still pass. Auditing the other account's footprint is the
# legitimate case, and it is literally what found the leak this guard exists
# for -- a hook that blocked it would have prevented its own justification.
for c in "${READS[@]}"; do
  run "$tmp/wrong" "$c" || bad "WRONG account: read was blocked -> $c"
done

# The accepted false positives, asserted as such: with the wrong account these
# ARE blocked. If one of these ever starts passing, the write match has been
# narrowed and a real write spelling has probably gone with it.
for c in "${MENTIONS_ONLY[@]}"; do
  run "$tmp/wrong" "$c" \
    && bad "WRONG account: a command containing a write verb passed -> $c (the match has been narrowed)"
done

# ── 3. `gh auth switch` is refused whoever is active ───────────────────────
# It is global state: it re-identifies other live sessions. Blocked even from
# the correct account, because the harm is to the sessions it reaches, not to
# the one running it.
for cfg in right wrong; do
  run "$tmp/$cfg" 'gh auth switch --user other'  && bad "$cfg: gh auth switch was allowed"
  run "$tmp/$cfg" 'gh auth login'                && bad "$cfg: gh auth login was allowed"
  # ...and the citation form must pass, under BOTH configs.
  for c in "${CITATIONS[@]}"; do
    run "$tmp/$cfg" "$c" || bad "$cfg: a backticked CITATION was blocked -> $c"
  done
done

# ── 4. an explicit token override ──────────────────────────────────────────
# A write carrying its own credential cannot be attributed from the config, so
# it is the one case where "cannot tell" must mean refuse. A read carrying one
# is fine -- see section 2.
run "$tmp/right" 'GH_TOKEN=$T gh issue comment 5 --body hi' && bad "token-override WRITE was allowed"
run "$tmp/right" 'GH_TOKEN=$T gh api user'                  || bad "token-override READ was blocked"

# ── 5. cannot determine the account -> allow, but SAY SO ───────────────────
# The rule this repo has written down three times: a check that cannot
# distinguish "passed" from "never answered" is not a check. Allowing silently
# here would make an unreadable config indistinguishable from a correct one.
out=$(printf '{"tool_name":"Bash","tool_input":{"command":"gh issue create --title x"}}' \
        | GH_CONFIG_DIR="$tmp/empty" bash "$HOOK" 2>&1 >/dev/null); rc=$?
[ "$rc" -eq 0 ] || bad "no config: expected allow (0), got $rc"
case "$out" in (*"did not run"*) :;; (*) bad "no config: allowed SILENTLY -- must announce it did not run";; esac

# ── 6. a non-Bash tool is none of this hook's business ─────────────────────
printf '{"tool_name":"Read","tool_input":{"file_path":"gh issue create"}}' \
  | GH_CONFIG_DIR="$tmp/wrong" bash "$HOOK" >/dev/null 2>&1 \
  || bad "a Read tool call was blocked"

if [ "$fail" -eq 0 ]; then echo "gh-account-guard selftest: ok"; else
  echo "gh-account-guard selftest: FAILED"; fi
exit "$fail"
