#!/usr/bin/env bash
# Refuse a commit whose author or committer is not a known identity.
#
# The companion of check_shapes.py, one layer over. That guard asks "does this
# CONTENT name a real machine?"; this one asks "does this COMMIT name a real
# person we did not mean to publish?". Neither can see the other's surface:
# a commit identity is metadata, so it appears in no blob and no message, and
# the shape scan walks straight past it.
#
# Why it exists: on 2026-09-05 an audit of this repo found a second GitHub
# account -- a corp one, whose LOGIN alone named an employer -- had authored an
# issue and three comments on the public repo. It had authored no commits, and
# that was luck rather than design: nothing anywhere checked. The same account
# was one `gh auth switch` away from the git credential helper, and 520 commits
# already in this history carried a corp mailbox as their author address.
#
# ⚠️ This is the ONLY layer that can see the problem before publication.
# A commit identity cannot be edited afterwards. Rewriting it means rewriting
# every SHA at or above it, force-pushing ~700 branches and ~660 tags, and
# every other clone re-cloning -- and even then the pre-rewrite objects stay
# reachable by SHA through refs/pull/* until GitHub Support runs GC. There is
# no cheap fix downstream, only here.
#
# ── Exit codes ─────────────────────────────────────────────────────────────
# A CONTRACT with .githooks/pre-commit, .githooks/pre-push and CI, mirroring
# check_shapes.py's for exactly the reason recorded there: a check whose result
# cannot distinguish "passed" from "never answered" is not a check. A guard
# that says "foreign identity" when it means "I could not run" is one somebody
# silences with --no-verify, and that removes the layer permanently.
#
#   0   clean
#   1   a foreign identity was found   <- the ONLY blocking status
#   2   bad arguments (this script and its caller have drifted apart)
#   20  internal error (git failed, the allowlist is missing)
set -u

EXIT_CLEAN=0
EXIT_FOUND=1
EXIT_USAGE=2
EXIT_ERROR=20

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ALLOWFILE="$HERE/allowed-identities.txt"

usage() {
  cat >&2 <<'EOF'
usage: check-identity.sh [--require-commits] --pending
       check-identity.sh [--require-commits] --range <rev>..<rev>
       check-identity.sh [--require-commits] --commits <rev-list argument>...

  --pending  check what the NEXT commit would use. For a pre-commit hook:
             the commit does not exist yet, so there is nothing to read.
  --range    check every commit in a range. For pre-push and CI.
  --commits  check the commits named by arbitrary rev-list arguments.

  --require-commits
             fail (exit 20) if the range turned out to be EMPTY. See below.
EOF
}

# ⚠️ Why --require-commits exists. An empty range examines nothing and reports
# "all known", which is indistinguishable from a real pass -- the same failure
# this repo has already paid for in its integration lane, where `cargo test`
# with a filter matching no test exits 0 and a job that silently ran nothing
# looked green for months. A pre-push hook legitimately gets empty ranges, so
# it must not fail on one; CI computes its range from event metadata and an
# empty one there means the RANGE is wrong, not that the commits are clean.
# The caller that knows which case it is passes the flag.
REQUIRE_COMMITS=0
if [ "${1-}" = "--require-commits" ]; then REQUIRE_COMMITS=1; shift; fi

die_internal() {
  echo "commit-identity guard could not run: $*" >&2
  exit $EXIT_ERROR
}

# ── the allowlist ──────────────────────────────────────────────────────────
# Lowercased on read. Email addresses are case-insensitive in the half that
# matters here (the domain always, the local part by every practical mail
# system), and this file's sibling guard learned the hard way that a check
# which assumes a casing is not a check: shapes written uppercase-only reported
# "none found" while the leak that got through was written in lowercase.
[ -r "$ALLOWFILE" ] || die_internal "allowlist not readable at $ALLOWFILE"

ALLOWED=""
while IFS= read -r line || [ -n "$line" ]; do
  line="${line%%#*}"                       # strip comments
  line="$(printf '%s' "$line" | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')"
  [ -n "$line" ] && ALLOWED="$ALLOWED $line"
done < "$ALLOWFILE"
# An unreadable or empty allowlist must NEVER mean "everything is fine". That
# is the `Some([])` vs `None` distinction this codebase pays for elsewhere:
# "no policy" and "an empty policy" are different answers, and only one of them
# is safe to treat as a pass.
[ -n "${ALLOWED// /}" ] || die_internal "allowlist is empty -- refusing to pass everything"

is_allowed() {
  local needle
  needle="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  case " $ALLOWED " in (*" $needle "*) return 0 ;; esac
  return 1
}

FINDINGS=""
NFOUND=0
NCHECKED=0
report() {  # $1=where  $2=role  $3=identity
  NFOUND=$((NFOUND + 1))
  FINDINGS="$FINDINGS$(printf '  %-14s %-10s %s' "$1" "$2" "$3")
"
}

MODE="${1-}"
case "$MODE" in
  --pending)
    [ $# -eq 1 ] || { usage; exit $EXIT_USAGE; }
    # `git var`, NOT `git config user.email`. git var resolves the identity the
    # commit would ACTUALLY be made with -- which means it also sees the
    # GIT_AUTHOR_EMAIL / GIT_COMMITTER_EMAIL environment overrides, and sees
    # them separately. Reading user.email instead would miss both, and a guard
    # that can be stepped around with a one-word env prefix is decoration.
    NCHECKED=1
    for role in AUTHOR COMMITTER; do
      IDENT="$(git var "GIT_${role}_IDENT" 2>&1)" \
        || die_internal "git var GIT_${role}_IDENT failed: $IDENT"
      # "Name <email> <epoch> <tz>" -> email
      EMAIL="${IDENT#*<}"; EMAIL="${EMAIL%%>*}"
      [ -n "$EMAIL" ] || die_internal "could not parse GIT_${role}_IDENT"
      is_allowed "$EMAIL" \
        || report "(next commit)" "$(printf '%s' "$role" | tr '[:upper:]' '[:lower:]')" "$EMAIL"
    done
    ;;

  --range|--commits)
    shift
    [ $# -ge 1 ] || { usage; exit $EXIT_USAGE; }
    # %ae/%ce, NOT %aE/%cE: the capitalised forms apply .mailmap, which is a
    # DISPLAY rewrite. The raw address is what is stored in the object and what
    # gets published, so the raw address is what must be judged. (A .mailmap
    # would otherwise let the repo hide from its own guard.)
    LOG="$(git log --format='%H%x1f%ae%x1f%ce' "$@" 2>&1)" \
      || die_internal "git log failed: $LOG"
    while IFS=$'\x1f' read -r sha ae ce; do
      [ -n "${sha:-}" ] || continue
      NCHECKED=$((NCHECKED + 1))
      is_allowed "$ae" || report "${sha:0:12}" "author" "$ae"
      is_allowed "$ce" || report "${sha:0:12}" "committer" "$ce"
    done <<< "$LOG"
    if [ "$NCHECKED" -eq 0 ] && [ "$REQUIRE_COMMITS" -eq 1 ]; then
      die_internal "the range matched NO commits -- nothing was checked, so nothing was cleared"
    fi
    ;;

  ''|-h|--help) usage; exit $EXIT_USAGE ;;
  *) echo "unknown mode: $MODE" >&2; usage; exit $EXIT_USAGE ;;
esac

if [ "$NFOUND" -eq 0 ]; then
  # Report the COUNT, always. "all known" over zero commits and "all known"
  # over forty are the same sentence and opposite facts; printing the number is
  # what lets a reader of a green log tell them apart without re-deriving the
  # range by hand.
  echo "commit identities: all known ($NCHECKED checked)"
  exit $EXIT_CLEAN
fi

# Banner FIRST, then the findings. The obvious shape -- print each finding as
# it is discovered, then explain -- puts the explanation below a wall of
# addresses, where the reader has already decided what the tool is telling them.
{
  echo
  echo "REFUSED: a commit identity here is not one this repo publishes."
  echo
  printf '%s' "$FINDINGS"
  cat <<'EOF'

This repo is PUBLIC. An author/committer email is written into the commit
object itself -- it cannot be edited later, only rewritten, and a rewrite
renumbers every SHA above it and still leaves the old objects reachable
through refs/pull/*. The cheap moment is now.

Almost always the fix is your local git identity:

    git config user.email goran.jovanov@gmail.com
    git commit --amend --reset-author        # if a commit already has it

If the address really is one this repo should publish (a new contributor, a
bot that pushes commits, or GitHub's private-email address after you enable
that setting), add it to .githooks/allowed-identities.txt in its own commit --
deliberately, having decided you are content to publish it.

⚠️ Merges made through the GitHub web UI use the commit email set in your
GitHub ACCOUNT, which no local hook can see -- the commit is created on
GitHub, after every check has already passed. Fix that one in GitHub
Settings -> Emails.

A repository ruleset (commit_author_email_pattern) would BLOCK it, but the
whole metadata-rule family is organisation-and-paid-plan only and is refused
on a user-owned repo (measured 2026-09-06: HTTP 422 "Invalid rule", for
active enforcement too). What covers it here instead is the CI job's
push-to-master run, which scans the pushed range and goes red within a
minute -- detection, not prevention.
EOF
} >&2
exit $EXIT_FOUND
