#!/usr/bin/env bash
#
# code-map-audit.sh — a code map names code that exists, and counts no lines.
#
# WHY THIS EXISTS
#
#   A doc's "Code map" is a table that says which file owns what: a file in the first
#   column, and in the second the functions and types a reader should look for. Those
#   tables used to give each name a line number. A number is right on the day it is
#   written and wrong after the next change above it, and nothing noticed: on
#   2026-11-11, 97 of the 171 numbers in docs/linux-capture.md's map had fallen behind
#   the code, by 3 to 854 lines, and they were re-pointed by hand. Measured the same
#   day, 179 of master's last 1,296 commits had moved at least one symbol that a code
#   map names: about four merges a day. A gate on the numbers would make each of them
#   edit the same table rows, and each row is one line, so two open PRs would conflict.
#
#   So a code map carries NO line numbers. It names its code, and a name survives a
#   move. What can still go wrong is a name that is no longer there, after a rename or
#   a delete, and that is always worth a red check: the doc sends its reader to
#   something that does not exist. A PR that only moves code needs no docs edit and
#   never fails here (#3686; the wider class is #3431).
#
# WHAT IT CHECKS
#
#   A code map is the table under a heading that says "Code map" whose first column is
#   headed `File`. In every row of such a table:
#
#     1. each file the first column names is in the repository. A short name is a
#        sibling of the row's first file, or else the one file in the repository whose
#        path ends with it. A pattern (`ui/public/<key>.txt`) is not checked;
#     2. each backticked name is in one of the row's files, as a whole word. The row's
#        files are the first column's, plus any source file the row itself names. For
#        a path such as `Broker::open`, each part must be there. Backticked text that
#        is not a plain name (`--flag`, `KEY=1`, `a-b`, `Notify*`) is not checked;
#     3. there is no line number: no `:N`, no `path.rs:N`, no `#LN` link.
#
#   A table under a "Code map" heading with another first column is not checked: those
#   maps lead with a description and are #3431's.
#
#   It is a presence check, on purpose. It does not parse Rust or TypeScript, so it
#   cannot tell a definition from a mention. A name that moved to another file while
#   its old file still mentions it passes.
#
# USAGE
#   bash scripts/code-map-audit.sh             # audit, exit non-zero on any fault
#   bash scripts/code-map-audit.sh --summary   # report only, always exit 0
#   bash scripts/code-map-audit.sh --list      # every name checked: doc:line, name, files
#   bash scripts/code-map-audit.sh --selftest  # plant each kind of fault in a throwaway
#                                              # repository; fail unless each is caught
#
# ⚠️ POSIX awk only: Ubuntu's `awk` is mawk. No `{n,m}` in a pattern, no gensub, no
#    three-argument match.

set -eu

SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
MODE="${1:-}"

# The audit itself, on the repository in the current directory. It prints tab-separated
# records: `F doc line message` for a fault, `L doc line name files` for a checked name,
# and one `T maps rows names files` with the totals.
records() {
  local tracked docs_list
  tracked="$(mktemp)"
  docs_list="$(mktemp)"
  git ls-files > "$tracked"
  # Only docs with such a heading are read. `|| true`: no match is an answer, not an error.
  git grep -l -i -E '^#+[[:space:]].*code map' -- '*.md' > "$docs_list" 2>/dev/null || true
  local docs=()
  while IFS= read -r d; do
    [ -n "$d" ] && docs+=("$d")
  done < "$docs_list"
  if [ "${#docs[@]}" -eq 0 ]; then
    printf 'T\t0\t0\t0\t0\n'
    rm -f "$tracked" "$docs_list"
    return 0
  fi
  awk -v TRACKED="$tracked" '
    BEGIN {
      while ((getline l < TRACKED) > 0) { sub(/\r$/, "", l); tracked[l] = 1; order[++ntracked] = l }
      close(TRACKED)
      # A source file is surely ours, so one that does not resolve is a fault. A config or
      # data name (config.toml, a.json) may be a file on a device, so it is only added to
      # the row when it resolves.
      SRC = "rs|ts|tsx|vue|js|mjs|cjs|py|sh|ps1|go|swift|c|h"
      DATA = "yml|yaml|toml|json|html|css|md"
      OFS = "\t"
    }

    function fault(msg) { print "F", FILENAME, FNR, msg }

    # The backticked spans of s, into span[1..n].
    function spans(s,    n, i, j) {
      n = 0
      while ((i = index(s, "`")) > 0) {
        s = substr(s, i + 1)
        j = index(s, "`")
        if (j == 0) break
        span[++n] = substr(s, 1, j - 1)
        s = substr(s, j + 1)
      }
      return n
    }

    # Where the first cell of a table line ends: the first pipe not escaped by a backslash.
    function cell_end(s,    i, off) {
      off = 0
      while ((i = index(s, "|")) > 0) {
        if (i > 1 && substr(s, i - 1, 1) == "\\") { off += i; s = substr(s, i + 1); continue }
        return off + i
      }
      return 0
    }

    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }

    # Every word of file f, once.
    function load(f,    l, k, j, w) {
      if (f in loaded) return
      loaded[f] = 1
      nloaded++
      while ((getline l < f) > 0) {
        gsub(/[^A-Za-z0-9_]+/, " ", l)
        k = split(l, w, " ")
        for (j = 1; j <= k; j++) words[f, w[j]] = 1
      }
      close(f)
    }

    function in_row_files(word,    i) {
      for (i = 1; i <= nfiles; i++) { load(files[i]); if ((files[i], word) in words) return 1 }
      return 0
    }

    function add_file(f,    i) {
      for (i = 1; i <= nfiles; i++) if (files[i] == f) return
      files[++nfiles] = f
    }

    # A path a row names: the tracked file it means, "" for none, "*" for more than one.
    # It is the path as written, else a sibling of the row first file, else the one
    # tracked file whose path ends with it (`promote.yml`, `capture/mod.rs`).
    function resolve(t,    i, hit, n, tail) {
      if (t in tracked) return t
      if (firstdir != "" && ((firstdir "/" t) in tracked)) return firstdir "/" t
      n = 0; tail = "/" t
      for (i = 1; i <= ntracked; i++)
        if (length(order[i]) > length(tail) && substr(order[i], length(order[i]) - length(tail) + 1) == tail) { hit = order[i]; n++ }
      if (n == 1) return hit
      return n ? "*" : ""
    }

    function row(line,    s, p, c1, rest, n, m, i, t, r, k, j, seg, parts, shown, where) {
      nfiles = 0; split("", files); firstdir = ""
      s = line; sub(/^[ \t]*\|/, "", s)
      p = cell_end(s)
      if (p == 0) return
      c1 = substr(s, 1, p - 1); rest = substr(s, p + 1)

      n = spans(c1)
      if (n == 0) fault("the row names no file in its first column")
      for (i = 1; i <= n; i++) {
        t = span[i]
        if (t ~ /[<>*{}$]/) continue      # a pattern (`ui/public/<key>.txt`), not one file
        r = resolve(t)
        if (r == "*") fault("`" t "` matches more than one file: write enough of the path to name one")
        else if (r == "") fault("`" t "` is not a file in the repository")
        else {
          add_file(r)
          if (firstdir == "") { firstdir = r; if (!sub(/\/[^\/]*$/, "", firstdir)) firstdir = "." }
        }
      }

      if (line ~ /\]\([^)]*#L[0-9]/) fault("a line number (a `#L` link): a code map names its code and counts no lines")

      m = spans(rest)
      # Pass 1: line numbers are faults, and a source file the row names joins its files.
      for (i = 1; i <= m; i++) {
        t = span[i]; kind[i] = ""
        if (t ~ /^:[0-9]+((-|–)[0-9]+)?$/ || t ~ ("\\.(" SRC "|" DATA ")(:|#L)[0-9]")) {
          fault("a line number (`" t "`): a code map names its code and counts no lines")
        } else if (t ~ ("^[A-Za-z0-9_][A-Za-z0-9_./-]*\\.(" SRC "|" DATA ")$")) {
          r = resolve(t)
          if (r == "*") fault("`" t "` matches more than one file: write enough of the path to name one")
          else if (r != "") add_file(r)
          else if (t ~ ("\\.(" SRC ")$")) fault("`" t "` is not a file in the repository")
        } else if (t ~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*(\(\))?!?$/) {
          kind[i] = "name"
        }
      }
      if (nfiles == 0) return
      # Pass 2: each name is in one of the row files.
      where = files[1]; for (i = 2; i <= nfiles; i++) where = where ", " files[i]
      for (i = 1; i <= m; i++) {
        if (kind[i] != "name") continue
        t = span[i]; shown = t
        sub(/!$/, "", t); sub(/\(\)$/, "", t)
        k = split(t, parts, "::")
        nnames++
        print "L", FILENAME, FNR, shown, where
        for (j = 1; j <= k; j++) {
          seg = parts[j]
          if (seg == "crate" || seg == "self" || seg == "super" || seg == "Self") continue
          if (!in_row_files(seg)) {
            fault("`" shown "` is not in " where (k > 1 ? " (no `" seg "` there)" : ""))
            break
          }
        }
      }
    }

    FNR == 1 { level = 0; intable = 0; filetable = 0; fenced = 0 }
    {
      line = $0; sub(/\r$/, "", line)
      if (line ~ /^[ \t]*(```|~~~)/) { fenced = !fenced; next }
      if (fenced) next
      if (line ~ /^#+[ \t]/) {
        n = 0; while (substr(line, n + 1, 1) == "#") n++
        if (level && n <= level) level = 0
        if (tolower(line) ~ /code map/) level = n
        intable = 0; filetable = 0
        next
      }
      if (!level) next
      if (line !~ /^[ \t]*\|/) { intable = 0; filetable = 0; next }
      if (!intable) {
        intable = 1
        s = line; sub(/^[ \t]*\|/, "", s)
        p = cell_end(s)
        filetable = (p > 0 && trim(substr(s, 1, p - 1)) == "File")
        if (filetable) nmaps++
        next
      }
      if (line ~ /^[ \t]*\|[ \t:|-]+$/) next
      if (!filetable) next
      nrows++
      row(line)
    }
    END { print "T", nmaps + 0, nrows + 0, nnames + 0, nloaded + 0 }
  ' "${docs[@]}"
  rm -f "$tracked" "$docs_list"
}

# Run the audit in the current directory and report. $1 is the mode.
report() {
  local mode="$1" out faults totals maps rows names files
  out="$(records)"
  totals="$(printf '%s\n' "$out" | awk -F'\t' '$1 == "T"')"
  maps="$(printf '%s' "$totals" | cut -f2)"
  rows="$(printf '%s' "$totals" | cut -f3)"
  names="$(printf '%s' "$totals" | cut -f4)"
  files="$(printf '%s' "$totals" | cut -f5)"

  if [ "$mode" = "--list" ]; then
    printf '%s\n' "$out" | awk -F'\t' '$1 == "L" { print $2 ":" $3 "\t" $4 "\t" $5 }'
    return 0
  fi

  faults="$(printf '%s\n' "$out" | awk -F'\t' '$1 == "F"' | wc -l | tr -d ' ')"
  if [ "$faults" -gt 0 ]; then
    echo "A code map names code that is not there, or counts lines:"
    printf '%s\n' "$out" | awk -F'\t' '$1 == "F" { print "  ✗ " $2 ":" $3 "  " $4 }'
    echo "  A name: call it what the code calls it now, or take it out of the row. If it lives"
    echo "  in another file, name that file in the row. A line number: remove it, the name is"
    echo "  the pointer (scripts/code-map-audit.sh says why)."
  fi
  # A check that looks at nothing must not read as green.
  if [ "${maps:-0}" -eq 0 ]; then
    echo "code-map-audit: no code map found. The audit is looking at nothing: has the heading"
    echo "  (\"Code map\") or the first column (\`File\`) changed?"
    faults=$((faults + 1))
  fi
  echo "code-map-audit: ${maps:-0} code map(s), ${rows:-0} row(s), ${names:-0} name(s) checked in ${files:-0} file(s), ${faults} fault(s)"

  if [ "$faults" -gt 0 ] && [ "$mode" != "--summary" ]; then
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------------------
# --selftest: a throwaway repository with one good code map, then one fault of each kind.
# Each case states how many faults it must raise and a text one of them must carry.
selftest() {
  local tmp fails=0
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  (
    cd "$tmp"
    git init -q .
    mkdir -p src/sub docs
    printf '%s\n' 'pub fn open_default() {}' 'pub struct Broker;' 'impl Broker {' '    pub fn open(&self) {}' '}' > src/a.rs
    printf '%s\n' 'fn helper() {}' > src/b.rs
    printf '%s\n' 'export function parseReply() {}' > src/sub/view.ts
  )

  # write_doc <name> <row>...: a doc whose code map has these rows.
  write_doc() {
    local name="$1"; shift
    {
      echo "# A page"
      echo
      echo "## 3. Code map"
      echo
      echo "| File | Owns |"
      echo "|---|---|"
      local r
      for r in "$@"; do printf '%s\n' "$r"; done
      echo
      echo "## 4. After"
      echo
      echo "| File | Owns |"
      echo "|---|---|"
      echo '| `src/nowhere.rs` | a table outside a code map is not checked (`:12`) |'
    } > "$tmp/docs/$name"
  }

  # expect <label> <fault count> <text a fault must carry, or ->
  expect() {
    local label="$1" want="$2" text="$3" out got
    out="$(cd "$tmp" && git add -A >/dev/null 2>&1 && records)"
    got="$(printf '%s\n' "$out" | awk -F'\t' '$1 == "F"' | wc -l | tr -d ' ')"
    if [ "$got" != "$want" ]; then
      echo "  ✗ $label: $got fault(s), expected $want"
      printf '%s\n' "$out" | awk -F'\t' '$1 == "F" { print "      " $2 ":" $3 "  " $4 }'
      fails=$((fails + 1))
      return 0
    fi
    if [ "$text" != "-" ] && ! printf '%s\n' "$out" | awk -F'\t' '$1 == "F"' | grep -qF -- "$text"; then
      echo "  ✗ $label: no fault carries \"$text\""
      printf '%s\n' "$out" | awk -F'\t' '$1 == "F" { print "      " $2 ":" $3 "  " $4 }'
      fails=$((fails + 1))
      return 0
    fi
    echo "  ✓ $label"
  }

  local good='| `src/a.rs` · `b.rs` | `open_default`, the handoff (`Broker::open`), `helper()`, the flag `--now`, the key `MODE=1` and the parser in `sub/view.ts` (`parseReply`) |'

  write_doc map.md "$good"
  expect "a good code map passes" 0 -
  local checked
  checked="$(cd "$tmp" && records | awk -F'\t' '$1 == "T" { print $2 "/" $3 "/" $4 }')"
  if [ "$checked" = "1/1/4" ]; then echo "  ✓ it read 1 code map, 1 row, 4 names"
  else echo "  ✗ it read $checked (code maps/rows/names), expected 1/1/4"; fails=$((fails + 1)); fi

  # The code moves and nothing is renamed: no docs edit is needed.
  ( cd "$tmp" && { for _ in 1 2 3 4 5 6 7 8 9 10; do echo '// a line above'; done; cat src/a.rs; } > src/a.new && mv src/a.new src/a.rs )
  expect "code that only moves passes with no docs change" 0 -

  # A rename: the doc still says the old name.
  ( cd "$tmp" && sed 's/open_default/open_first/' src/a.rs > src/a.new && mv src/a.new src/a.rs )
  expect "a renamed function fails, and the fault names the doc line" 1 'docs/map.md'
  expect "… and says which name and which files" 1 '`open_default` is not in src/a.rs, src/b.rs, src/sub/view.ts'
  ( cd "$tmp" && sed 's/open_first/open_default/' src/a.rs > src/a.new && mv src/a.new src/a.rs )

  write_doc map.md '| `src/a.rs` | `open_def` |'
  expect "part of a longer word is not the name" 1 '`open_def` is not in src/a.rs'

  write_doc map.md '| `src/a.rs` | `Broker::shut` |'
  expect "a path needs each of its parts" 1 'no `shut` there'

  write_doc map.md '| `src/gone.rs` | `open_default` |'
  expect "a file that is not in the repository fails" 1 '`src/gone.rs` is not a file in the repository'

  write_doc map.md '| `src/a.rs` · `view.ts` · `src/<key>.txt` | `open_default`, `parseReply` |'
  expect "a short name finds its one file, and a pattern is not checked" 0 -

  ( cd "$tmp" && mkdir -p other && cp src/sub/view.ts other/view.ts )
  write_doc map.md '| `src/a.rs` · `view.ts` | `open_default` |'
  expect "a short name that fits two files fails" 1 '`view.ts` matches more than one file'
  rm -rf "$tmp/other"

  write_doc map.md '| `src/a.rs` | `open_default`, and the parser in `sub/gone.ts` |'
  expect "a source file the row names must exist too" 1 '`sub/gone.ts` is not a file in the repository'

  write_doc map.md '| `src/a.rs` | `open_default` (`:1`) |'
  expect "a bare line number fails" 1 'a line number (`:1`)'

  write_doc map.md '| `src/a.rs` | `open_default`, the block `:3-5` |'
  expect "a bare range fails" 1 'a line number (`:3-5`)'

  write_doc map.md '| `src/a.rs` | `open_default` at `src/a.rs:1` |'
  expect "a path with a line number fails" 1 'a line number (`src/a.rs:1`)'

  write_doc map.md '| [`src/a.rs`](../src/a.rs#L1) | `open_default` |'
  expect "a #L link fails" 1 'a `#L` link'

  write_doc map.md '| the opener | `open_default` |'
  expect "a row with no file fails" 1 'names no file'

  # A map that leads with a description is not this audit to check.
  {
    echo "## Code map"
    echo
    echo "| piece | where |"
    echo "|---|---|"
    echo '| the opener | `src/a.rs:1`, `:3` |'
  } > "$tmp/docs/map.md"
  expect "a table with another first column is left alone" 0 -

  rm -rf "$tmp"
  trap - EXIT
  if [ "$fails" -gt 0 ]; then
    echo "code-map-audit --selftest: $fails case(s) FAILED: the audit does not catch what it says it does"
    return 1
  fi
  echo "code-map-audit --selftest: every case holds"
  return 0
}

case "$MODE" in
  --selftest)
    selftest
    ;;
  ""|--summary|--list)
    cd "$(dirname "$SELF")/.."
    report "$MODE"
    ;;
  *)
    echo "usage: bash scripts/code-map-audit.sh [--summary | --list | --selftest]" >&2
    exit 2
    ;;
esac
