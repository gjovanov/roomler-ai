#!/usr/bin/env bash
# Refuse a GitHub WRITE made as the wrong account.
#
# ── The leak this exists for ────────────────────────────────────────────────
#
# `gh` on a developer box can hold several accounts, and `gh auth switch`
# mutates the ACTIVE one in a shared config file -- globally, for every process
# and every concurrent session, with no per-session scope of any kind. So a
# second account signed in for unrelated work is not passive: any session, at
# any later moment, silently starts speaking as it.
#
# Measured on this repo, 2026-09-05: a corp account -- whose LOGIN alone named
# an employer -- had authored one issue and three comments on the PUBLIC repo,
# across three separate days spanning eleven days. Nothing failed, nothing
# warned, and the mistake was invisible from inside the session that made it,
# because `gh issue comment` prints a URL and not an identity.
#
# ⚠️ No git hook can see this surface. `pre-commit` and `pre-push` guard commit
# metadata; an issue, a comment, a review and a release note never touch git at
# all. This is the only layer between an agent and a comment posted under the
# wrong name.
#
# ── What it does, and deliberately does not, block ─────────────────────────
#
# WRITES are gated; reads are not. `gh api` GETs, `gh issue view`, `gh run
# watch` and every other read-only call run untouched whatever the account --
# an audit of the wrong account's footprint is a legitimate and necessary thing
# to do, and it is exactly what found the leak above.
#
# The default for anything unrecognised is ALLOW. A hook that blocks what it
# does not understand is one somebody disables, and a disabled hook protects
# nothing; this repo has that written down three times about its sibling
# guards. The cost is that a novel write verb is unguarded until it is listed,
# which is the right trade for a layer that is one of four.
#
# Contract: exit 0 allows, exit 2 blocks and shows stderr to the model.
set -u

INPUT="$(cat)"

# The tool payload arrives as JSON. It is matched as raw TEXT rather than
# parsed, on purpose and not for want of a parser:
#
#   * there is no jq or python3 on this machine's Git-Bash PATH, so a parser
#     would mean a dependency the hook cannot rely on and would fail open;
#   * a substring test has strictly FEWER false negatives than a parse. Quoting
#     and escaping can hide a command's structure from a parser; they cannot
#     remove the bytes `gh issue comment` from the payload.
#
# The cost is false POSITIVES: a command that merely MENTIONS a write verb --
# `grep -rn 'gh issue create' .`, or a heredoc documenting one -- is treated as
# a write. While the active account is correct that costs one local file read
# and nothing else. While it is WRONG, such a command is blocked even though it
# would have written nothing.
#
# ⚠️ That is deliberate, and the selftest asserts it rather than papering over
# it. Narrowing the match to exclude a mention is precisely how a substring
# guard acquires false negatives -- quoting is what hides structure, and the
# whole reason this matches text is that quoting cannot hide bytes. A blocked
# grep tells you to fix the account, which is the correct next action anyway.
# (This paragraph replaces an earlier one claiming the cost was nil. It was
# written before the selftest existed, the selftest immediately disproved it,
# and the sentence is kept here as the reason not to reintroduce the claim.)
case "$INPUT" in
  (*'"Bash"'*) ;;
  (*) exit 0 ;;
esac

# ⚠️ Backtick-quoted spans are stripped before ANY matching below: in this
# repo's prose a command is always written `like this`, and a citation is not
# an invocation.
#
# This is not a nicety. Without it the guard blocked its own introducing
# commit, whose message explains at length why `gh auth switch` is refused --
# and it would go on blocking every doc edit, comment and commit message that
# discusses these commands, in a repo whose entire convention is dense prose
# about its own guards. A guard that cannot be written about is one that gets
# switched off.
#
# The exposure it accepts is an invocation deliberately wrapped in backticks,
# i.e. command substitution. That is an exotic and pointless way to run `gh
# auth switch`, and the account check further down still catches the WRITE that
# such a switch was performed in order to make -- which is the harm, and the
# thing this hook actually exists to stop.
INPUT="$(printf '%s' "$INPUT" | sed 's/`[^`]*`//g')"

# ── 1. `gh auth switch` is refused outright ────────────────────────────────
# Not because switching is wrong, but because it is GLOBAL: it reaches into
# other live sessions, including ones a human is watching, and changes who they
# are without telling them. There is a scoped way to do the same thing, and the
# message says so.
case "$INPUT" in
  (*'gh auth switch'*|*'gh auth login'*)
    cat >&2 <<'EOF'
BLOCKED: `gh auth switch` / `gh auth login` changes the ACTIVE GitHub account
globally -- in a config file shared by every process and every concurrent
session, including ones a human is watching. That is how this repo acquired an
issue and three comments authored by the wrong account.

If you need another identity for ONE command, scope it instead of switching:

    GH_CONFIG_DIR=/path/to/other-config gh <read-only command>

and if you need to change the default account, do it yourself in a terminal so
no session is re-identified underneath you.
EOF
    exit 2 ;;
esac

# ── 2. is this a GitHub write? ─────────────────────────────────────────────
# Listed verbs, not a catch-all: see the header on why the default is allow.
is_write=0
case "$INPUT" in
  (*'gh issue create'*|*'gh issue comment'*|*'gh issue edit'*|*'gh issue close'*\
  |*'gh issue reopen'*|*'gh issue delete'*|*'gh issue transfer'*|*'gh issue lock'*\
  |*'gh issue unlock'*|*'gh issue pin'*|*'gh issue unpin'*\
  |*'gh pr create'*|*'gh pr comment'*|*'gh pr edit'*|*'gh pr close'*\
  |*'gh pr reopen'*|*'gh pr merge'*|*'gh pr review'*|*'gh pr ready'*\
  |*'gh release create'*|*'gh release edit'*|*'gh release delete'*|*'gh release upload'*\
  |*'gh repo create'*|*'gh repo edit'*|*'gh repo delete'*|*'gh repo fork'*\
  |*'gh gist create'*|*'gh gist edit'*|*'gh gist delete'*\
  |*'gh workflow run'*|*'gh workflow enable'*|*'gh workflow disable'*\
  |*'gh run rerun'*|*'gh run cancel'*|*'gh cache delete'*\
  |*'gh secret set'*|*'gh variable set'*|*'gh label create'*|*'gh label edit'*\
  |*'git push'*)
    is_write=1 ;;
esac
# `gh api` is read-only by default and a write only when it names a method.
# graphql is the exception that a method check misses entirely: a mutation is a
# POST whichever way you look at it, and `deleteIssue` travels that way.
case "$INPUT" in
  (*'gh api'*)
    case "$INPUT" in
      (*'--method POST'*|*'--method PATCH'*|*'--method PUT'*|*'--method DELETE'*\
      |*'-X POST'*|*'-X PATCH'*|*'-X PUT'*|*'-X DELETE'*|*'mutation'*)
        is_write=1 ;;
    esac ;;
esac
[ "$is_write" -eq 1 ] || exit 0

# ── 3. who owns this repo? ─────────────────────────────────────────────────
# Derived from the remote rather than hard-coded, so the hook states an
# invariant ("write as the owner of the repo you are in") instead of a fact
# about one machine -- and so a clone of a fork is not silently wrong.
OWNER=""
URL="$(git -C "${CLAUDE_PROJECT_DIR:-.}" config --get remote.origin.url 2>/dev/null || true)"
case "$URL" in
  (*github.com[:/]*)
    OWNER="${URL#*github.com}"; OWNER="${OWNER#[:/]}"; OWNER="${OWNER%%/*}" ;;
esac
[ -n "$OWNER" ] || OWNER="gjovanov"     # public, and the answer when there is no remote

# ── 4. an explicit token override cannot be attributed ────────────────────
# The command carries its own credential, so the config below says nothing
# about who the write would be from. Refuse rather than guess: this is the one
# place where "unrecognised" must not mean "allow", because the override is
# precisely the way to route around everything else here.
case "$INPUT" in
  (*'GH_TOKEN='*|*'GITHUB_TOKEN='*|*'GH_ENTERPRISE_TOKEN='*)
    cat >&2 <<'EOF'
BLOCKED: this is a GitHub WRITE carrying an explicit token override, so the
account it would be attributed to cannot be determined from the config.

Reads with an overridden token are fine and are not blocked -- auditing another
account's footprint is a legitimate thing to do. A write is not: run it through
the normal credential, or if it genuinely must use another identity, run it
yourself in a terminal so the attribution is a decision somebody made.
EOF
    exit 2 ;;
esac

# ── 5. the active account ──────────────────────────────────────────────────
# Read from hosts.yml directly, not from `gh auth status`: that command makes a
# network round-trip to validate the token, and a hook on every Bash call must
# cost nothing. The file holds no secret -- tokens live in the OS keyring.
CFG="${GH_CONFIG_DIR:-}"
if [ -z "$CFG" ]; then
  for cand in "${XDG_CONFIG_HOME:-}/gh" "${APPDATA:-}/GitHub CLI" "$HOME/.config/gh"; do
    case "$cand" in (/gh|"/GitHub CLI") continue ;; esac
    [ -f "$cand/hosts.yml" ] && { CFG="$cand"; break; }
  done
fi

ACTIVE=""
if [ -n "$CFG" ] && [ -f "$CFG/hosts.yml" ]; then
  # The `user:` key inside the github.com block. Anchored to that block so a
  # GitHub Enterprise host's active user in the same file cannot be mistaken
  # for it.
  ACTIVE="$(awk '
    /^[^[:space:]]/ { inblock = ($0 ~ /^github\.com:/) }
    inblock && $1 == "user:" { print $2; exit }
  ' "$CFG/hosts.yml" 2>/dev/null || true)"
fi

if [ -z "$ACTIVE" ]; then
  # Could not determine it. Allow, and SAY SO -- the sibling guards' rule, paid
  # for twice: a check that cannot tell "passed" from "never answered" is not a
  # check, and one that blocks when it merely failed to run gets switched off.
  echo "NOTE: could not read the active gh account (no hosts.yml found); GitHub-write guard did not run." >&2
  exit 0
fi

[ "$ACTIVE" = "$OWNER" ] && exit 0

cat >&2 <<EOF
BLOCKED: this is a GitHub WRITE, and the active gh account is '$ACTIVE', not
'$OWNER' -- the owner of the repository this session is in.

This is the exact failure the guard exists for: on 2026-09-05 an audit found
one issue and three comments on this PUBLIC repo authored by a second account
signed in for unrelated work. Nothing failed at the time; the mistake was only
visible from outside the session.

An issue or a comment CANNOT be reattributed afterwards. The only remedy is to
delete and recreate it, which loses the thread's identity and dangles every
reference to its number.

Switch back in a terminal (\`gh auth switch --user $OWNER\`), or set
GH_CONFIG_DIR for this session to a config holding only that account -- see
scripts/gh-scoped-config.sh.
EOF
exit 2
