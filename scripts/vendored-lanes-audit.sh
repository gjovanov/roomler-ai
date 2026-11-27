#!/usr/bin/env bash
#
# vendored-lanes-audit.sh — every vendored fork has an entry in ci.yml's vendored matrix,
# and every entry names a fork (#4503).
#
# WHY THIS EXISTS
#
#   A crate under crates/vendored/ is an upstream crate that carries roomler's patches, and
#   its own tests are what guard them. No fork is a workspace member, so no `cargo test` of
#   the workspace builds one: a fork's tests run in CI only through an entry of the
#   `vendored-tests` matrix in .github/workflows/ci.yml. Until #4503 two of the six forks
#   had a job and four had none, and a fork whose tests run in no lane can lose a patch
#   while CI stays green. A fork joins the tree by hand (its directory, and a line in each
#   of the root Cargo.toml's `exclude` and `[patch.crates-io]`), and nothing there asks for
#   a lane. This audit does.
#
# WHAT IT CHECKS
#
#   The forks are the directories crates/vendored/<name>/ that hold a Cargo.toml. The
#   .patch files beside them are text that rust-lint's drift checks read, not forks. The
#   entries are the items of `strategy.matrix.include` in the job `vendored-tests`, and an
#   entry names its fork with `crate: <name>`. It fails, naming each, on:
#
#     1. a fork with no entry;
#     2. an entry whose `crate:` names no fork;
#     3. an entry with no `crate:`;
#     4. a fork that two entries name, whose checks would share one name;
#     5. no `vendored-tests` job, or no entry in it that the audit can read.
#
#   It reads ci.yml as text, not as YAML, because it runs on the dev box too, which has no
#   YAML parser. The job is a key two spaces in under `jobs:`, its entries are the block
#   list under its `matrix:` key `include:`, and `crate:` is a key of an entry with its
#   value on the same line. An entry written another way is not seen, and its fork then
#   fails as a fork with no entry: the safe side.
#
# USAGE
#   bash scripts/vendored-lanes-audit.sh               # audit this repository; exit 1 on a fault
#   bash scripts/vendored-lanes-audit.sh --selftest    # plant each fault in a throwaway tree and
#                                                      # in a copy of the real ci.yml; fail unless
#                                                      # each is caught
#   bash scripts/vendored-lanes-audit.sh --root <dir>  # audit another tree laid out the same way
#
# ⚠️ POSIX awk only: Ubuntu's `awk` is mawk. No {n,m} in a pattern, no gensub, no
#    three-argument match.

set -eu

SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
REPO="$(cd "$(dirname "$SELF")/.." && pwd)"
JOB="vendored-tests"
WORKFLOW=".github/workflows/ci.yml"

# forks <root>: the name of every fork, sorted, one a line.
forks() {
  local m
  for m in "$1"/crates/vendored/*/Cargo.toml; do
    [ -f "$m" ] || continue
    m="${m%/Cargo.toml}"
    printf '%s\n' "${m##*/}"
  done | LC_ALL=C sort
}

# entries <workflow>: one line per entry of the job's matrix, in file order: the entry's
# number, a tab, and its crate (empty when it has no `crate:`). One line `none` when the
# file has no such job. A CR before each line end (a Windows checkout) is dropped first.
entries() {
  tr -d '\r' < "$1" | awk -v job="$JOB" -v sq="'" '
    function indent(s) { match(s, /^ */); return RLENGTH }
    function value(s) {
      sub(/^[ ]*(-[ ]+)?crate:[ ]*/, "", s)
      sub(/[ ]+#.*$/, "", s)
      sub(/[ ]+$/, "", s)
      gsub(/"/, "", s)
      gsub(sq, "", s)
      return s
    }
    function flush() {
      if (open) print n "\t" crate
      open = 0
    }
    BEGIN { matind = -1 }
    # Blank lines and comments say nothing about the structure.
    /^[ ]*$/ || /^[ ]*#/ { next }
    {
      i = indent($0)
      # A top-level key ends the job, and a key two spaces in under `jobs:` starts the next.
      if (i < 2 || (i == 2 && $0 ~ /^  [^ -]/)) {
        flush()
        inlist = 0
        injob = 0
        if (i == 0) injobs = ($0 ~ /^jobs:[ ]*$/)
        if (injobs && i == 2 && $0 ~ /^  [A-Za-z0-9_-]+:[ ]*$/) {
          name = $0
          sub(/^  /, "", name)
          sub(/:[ ]*$/, "", name)
          if (name == job) { injob = 1; seen = 1; listdone = 0; matind = -1 }
        }
        next
      }
      if (!injob) next
      if (!inlist) {
        # The entries are the list under the `matrix:` key `include:` of the job.
        if (matind >= 0 && i <= matind) matind = -1
        if ($0 ~ /^[ ]*matrix:[ ]*$/) { matind = i; next }
        if (!listdone && matind >= 0 && $0 ~ /^[ ]*include:[ ]*$/) { inlist = 1; listind = i; itemind = -1 }
        next
      }
      # A line no deeper than `include:` ends the list, and a job has one.
      if (i <= listind) { flush(); inlist = 0; listdone = 1; next }
      if ($0 ~ /^[ ]*-[ ]/ || $0 ~ /^[ ]*-$/) {
        if (itemind < 0) itemind = i
        if (i == itemind) {
          # A new entry. Its keys sit where the text after the dash starts.
          flush()
          n++
          open = 1
          crate = ""
          rest = substr($0, i + 1)
          if (match(rest, /^-[ ]+/) && length(rest) > RLENGTH) {
            keyind = i + RLENGTH
            if (substr(rest, RLENGTH + 1) ~ /^crate:/) crate = value($0)
          } else {
            keyind = -1
          }
        }
        # A dash deeper in is a list inside an entry, not an entry.
        next
      }
      if (open && i > itemind) {
        if (keyind < 0) keyind = i
        if (i == keyind && $0 ~ /^[ ]*crate:/) crate = value($0)
      }
    }
    END {
      flush()
      if (!seen) print "none"
    }
  '
}

# audit <root>: one `::error` line per fault, then a summary line. Returns 1 on any fault.
audit() {
  local root="$1" wf="$1/$WORKFLOW" faults=0 list fl names n c f count
  if [ ! -f "$wf" ]; then
    echo "::error::vendored-lanes-audit: there is no $WORKFLOW under $root"
    return 1
  fi
  list="$(entries "$wf")"
  if [ "$list" = "none" ]; then
    echo "::error file=$WORKFLOW::there is no job '$JOB' in $WORKFLOW, so no vendored fork's own tests run in CI (#4503, scripts/vendored-lanes-audit.sh)"
    return 1
  fi
  if [ -z "$list" ]; then
    echo "::error file=$WORKFLOW::job '$JOB' has no entry under strategy.matrix.include that this audit can read: write each as '- crate: <fork>' with its keys under it (scripts/vendored-lanes-audit.sh)"
    return 1
  fi
  fl="$(forks "$root")"
  names="$(printf '%s\n' "$list" | awk -F'\t' '$2 != "" { print $2 }')"

  while IFS=$'\t' read -r n c; do
    if [ -z "$c" ]; then
      echo "::error file=$WORKFLOW::entry $n of the $JOB matrix has no crate: key, so it names no fork (scripts/vendored-lanes-audit.sh)"
      faults=$((faults + 1))
    fi
  done <<EOF
$list
EOF

  for c in $(printf '%s\n' "$names" | LC_ALL=C sort -u); do
    if ! printf '%s\n' "$fl" | grep -qxF -- "$c"; then
      echo "::error file=$WORKFLOW::an entry of the $JOB matrix names '$c', and there is no crates/vendored/$c/Cargo.toml: remove the entry, or name the fork it tests (scripts/vendored-lanes-audit.sh)"
      faults=$((faults + 1))
    fi
    count="$(printf '%s\n' "$names" | grep -cxF -- "$c" || true)"
    if [ "$count" -gt 1 ]; then
      echo "::error file=$WORKFLOW::$count entries of the $JOB matrix name '$c', and each check is named after its fork: keep one (scripts/vendored-lanes-audit.sh)"
      faults=$((faults + 1))
    fi
  done

  for f in $fl; do
    if ! printf '%s\n' "$names" | grep -qxF -- "$f"; then
      echo "::error file=$WORKFLOW::crates/vendored/$f has a Cargo.toml and no entry in the $JOB matrix, so its own tests run in no lane: add '- crate: $f' with its runner, timeout and args (#4503, scripts/vendored-lanes-audit.sh)"
      faults=$((faults + 1))
    fi
  done

  local nf ne
  nf="$(printf '%s\n' "$fl" | grep -c . || true)"
  ne="$(printf '%s\n' "$list" | grep -c . || true)"
  if [ "$faults" -gt 0 ]; then
    echo "vendored-lanes-audit: $faults fault(s); $nf fork(s) under crates/vendored, $ne entr(ies) in the $JOB matrix"
    return 1
  fi
  echo "vendored-lanes-audit: $nf forks, $ne entries: every fork has one entry and every entry names a fork"
  return 0
}

# The planted faults. Each cell builds the fault in a throwaway tree, never in this checkout,
# and fails unless the audit catches it, with the right exit status and words.
selftest() {
  local tmp fails=0
  SELFTEST_TMP="$(mktemp -d)"
  trap 'rm -rf "${SELFTEST_TMP:-}"' EXIT
  tmp="$SELFTEST_TMP"

  # A fixture: two forks, a .patch file and a folder without a Cargo.toml (neither is a fork).
  local fx="$tmp/fixture"
  mkdir -p "$fx/crates/vendored/alpha" "$fx/crates/vendored/beta" "$fx/crates/vendored/notes" \
           "$fx/.github/workflows"
  : > "$fx/crates/vendored/alpha/Cargo.toml"
  : > "$fx/crates/vendored/beta/Cargo.toml"
  : > "$fx/crates/vendored/beta.patch"
  : > "$fx/crates/vendored/notes/README.md"

  # workflow <the include list's lines>: a workflow whose vendored job lists these entries,
  # between two other jobs, one of them with a matrix of its own.
  workflow() {
    {
      echo 'name: CI'
      echo 'on:'
      echo '  push:'
      echo '    branches: [master]'
      echo 'jobs:'
      echo '  before:'
      echo '    runs-on: ubuntu-latest'
      echo '    strategy:'
      echo '      matrix:'
      echo '        include:'
      echo '          - crate: gamma'
      echo '    steps: [{ run: "true" }]'
      echo ''
      echo '  vendored-tests:'
      echo '    # a comment at the top of the job'
      echo '    name: Vendored ${{ matrix.crate }} tests'
      echo '    runs-on: ${{ matrix.runner }}'
      echo '    strategy:'
      echo '      fail-fast: false'
      echo '      matrix:'
      echo '        include:'
      local l
      for l in "$@"; do printf '%s\n' "$l"; done
      echo '    steps:'
      echo '      - uses: actions/checkout@v4'
      echo '      - run: cargo test'
      echo ''
      echo '  after:'
      echo '    runs-on: ubuntu-latest'
      echo '    steps: [{ run: "true" }]'
    } > "$fx/$WORKFLOW"
  }

  # expect <label> <tree> <exit status> <::error count> <text a fault must carry, or ->
  expect() {
    local label="$1" tree="$2" want_rc="$3" want="$4" text="$5" out rc got
    out="$(audit "$tree" 2>&1)" && rc=0 || rc=$?
    got="$(printf '%s\n' "$out" | grep -c '^::error' || true)"
    if [ "$rc" != "$want_rc" ] || [ "$got" != "$want" ]; then
      echo "  ✗ $label: exit $rc with $got fault(s), expected exit $want_rc with $want"
      printf '%s\n' "$out" | sed 's/^/      /'
      fails=$((fails + 1))
      return 0
    fi
    if [ "$text" != "-" ] && ! printf '%s\n' "$out" | grep -qF -- "$text"; then
      echo "  ✗ $label: no line carries \"$text\""
      printf '%s\n' "$out" | sed 's/^/      /'
      fails=$((fails + 1))
      return 0
    fi
    echo "  ✓ $label"
  }

  local alpha='          - crate: alpha' beta='          - crate: beta'
  local keys='            runner: ubuntu-latest'

  workflow "$alpha" "$keys" "$beta" "$keys"
  expect "a tree whose every fork has one entry passes" "$fx" 0 0 "2 forks, 2 entries"

  workflow "$alpha" "$keys" "$beta" "$keys"
  printf 's/$/\r/\n' > "$tmp/crlf.sed"
  sed -f "$tmp/crlf.sed" "$fx/$WORKFLOW" > "$tmp/crlf.yml" && mv "$tmp/crlf.yml" "$fx/$WORKFLOW"
  expect "… and reads the same with CRLF line ends, as a Windows checkout has them" "$fx" 0 0 "2 forks, 2 entries"

  mkdir -p "$fx/crates/vendored/gamma" && : > "$fx/crates/vendored/gamma/Cargo.toml"
  workflow "$alpha" "$keys" "$beta" "$keys"
  expect "a fork with no entry fails and names the fork, though another job's matrix names it" "$fx" 1 1 "crates/vendored/gamma has a Cargo.toml and no entry"
  rm -rf "$fx/crates/vendored/gamma"

  workflow "$alpha" "$keys" "$beta" "$keys" '          - crate: delta' "$keys"
  expect "an entry that names no fork fails, and names it" "$fx" 1 1 "names 'delta', and there is no crates/vendored/delta/Cargo.toml"

  workflow "$alpha" "$keys" "$beta" "$keys" '          - runner: windows-latest' '            timeout: 15'
  expect "an entry with no crate: fails, by its number" "$fx" 1 1 "entry 3 of the $JOB matrix has no crate: key"

  workflow "$alpha" "$keys" "$beta" "$keys" "$alpha" "$keys"
  expect "a fork that two entries name fails" "$fx" 1 1 "2 entries of the $JOB matrix name 'alpha'"

  workflow "$alpha" "$keys" "          # - crate: beta" "$keys"
  expect "an entry commented out is no entry" "$fx" 1 1 "crates/vendored/beta has a Cargo.toml and no entry"

  workflow "$alpha" "$keys" '# a comment at the line start' '    # and one less indented than the list' "$beta" "$keys"
  expect "a comment less indented than the list does not end it" "$fx" 0 0 "2 forks, 2 entries"

  workflow "$alpha" "$keys" '          - runner: ubuntu-latest' '            crate: "beta"   # quoted, on a later line'
  expect "crate: on a later line of its entry, quoted, counts" "$fx" 0 0 "2 forks, 2 entries"

  workflow "$alpha" "$keys" '            skip:' '              - crate: beta' "$beta" "$keys"
  expect "a list inside an entry holds no entry" "$fx" 0 0 "2 forks, 2 entries"

  workflow "$alpha" "$keys" "$beta" '            matrix-ish:' '              crate: delta'
  expect "a crate: deeper in an entry is not the entry's" "$fx" 0 0 "2 forks, 2 entries"

  workflow "$alpha" "$keys" "$beta" "$keys"
  sed 's/^      matrix:/      not-a-matrix:/' "$fx/$WORKFLOW" > "$tmp/nm.yml" && mv "$tmp/nm.yml" "$fx/$WORKFLOW"
  expect "an include: list that is not the matrix's holds no entry" "$fx" 1 1 "has no entry under strategy.matrix.include"

  workflow "$alpha" "$keys" "$beta" "$keys"
  sed 's/^jobs:/env:/' "$fx/$WORKFLOW" > "$tmp/env.yml" && mv "$tmp/env.yml" "$fx/$WORKFLOW"
  expect "a vendored-tests key outside jobs: is no job" "$fx" 1 1 "there is no job '$JOB'"

  workflow
  expect "a job with no entry fails" "$fx" 1 1 "has no entry under strategy.matrix.include"

  sed 's/^  vendored-tests:/  vendored-gone:/' "$fx/$WORKFLOW" > "$tmp/gone.yml" && mv "$tmp/gone.yml" "$fx/$WORKFLOW"
  expect "a workflow with no vendored-tests job fails" "$fx" 1 1 "there is no job '$JOB'"

  # The real ci.yml, copied: what the parser reads on this runner is the file CI runs.
  local real="$tmp/real" f n
  mkdir -p "$real/.github/workflows"
  cp "$REPO/$WORKFLOW" "$real/$WORKFLOW"
  for f in $(forks "$REPO"); do
    mkdir -p "$real/crates/vendored/$f" && : > "$real/crates/vendored/$f/Cargo.toml"
  done
  n="$(forks "$REPO" | grep -c . || true)"
  expect "a copy of the real ci.yml, beside the real forks, passes" "$real" 0 0 "$n forks, $n entries"

  awk '!/^[ ]*- crate: scrap[ ]*\r?$/' "$REPO/$WORKFLOW" > "$real/$WORKFLOW"
  expect "… with its scrap entry's crate: line taken out, fails and names scrap" "$real" 1 1 "crates/vendored/scrap has a Cargo.toml and no entry"

  cp "$REPO/$WORKFLOW" "$real/$WORKFLOW"
  rm -rf "$real/crates/vendored/wintun-bindings"
  expect "… with no wintun-bindings fork beside it, fails and names the stray entry" "$real" 1 1 "names 'wintun-bindings', and there is no crates/vendored/wintun-bindings/Cargo.toml"

  rm -rf "$tmp"
  trap - EXIT
  if [ "$fails" -gt 0 ]; then
    echo "vendored-lanes-audit --selftest: $fails case(s) FAILED: the audit does not catch what it says it does"
    return 1
  fi
  echo "vendored-lanes-audit --selftest: every case holds"
  return 0
}

case "${1:-}" in
  --selftest)
    selftest
    ;;
  --root)
    [ -n "${2:-}" ] || { echo "usage: bash scripts/vendored-lanes-audit.sh --root <dir>" >&2; exit 2; }
    audit "$2"
    ;;
  "")
    audit "$REPO"
    ;;
  *)
    echo "usage: bash scripts/vendored-lanes-audit.sh [--selftest | --root <dir>]" >&2
    exit 2
    ;;
esac
