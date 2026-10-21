#!/usr/bin/env bash
#
# mermaid-sequence-audit.sh — no `;` and no `#` in the text of a mermaid sequence diagram.
#
# WHY THIS EXISTS
#
#   Mermaid's sequence lexer reads a message's text as `":"…[^#\n;]*`, and takes `";"` as
#   a statement separator and `#` as the start of a comment
#   (packages/mermaid/src/diagrams/sequence/parser/sequenceDiagram.jison). So in
#   `D->>T: dial the address; pin the key`, the message ends at the `;`, the rest of the
#   line is parsed as a statement of its own, and GitHub draws "Unable to render rich
#   display: Parse error on line 13" where the diagram should be. A `#` is quieter: the
#   rest of the line becomes a comment, and `lease #user, #password` renders as
#   `lease `. Notes, `participant … as …` aliases and the labels of `alt`, `loop`, `opt`
#   and the other blocks read their text the same way.
#
#   Measured on 2026-10-21: 69 such lines on master, across 24 docs, the diagrams of the
#   `;` ones not drawn at all on GitHub. No build, test or link check notices.
#
#   Mermaid's own entity codes are decoded before the diagram is lexed, so they are the
#   way to write either character: `#59;` for `;`, `#35;` for `#`, `#lt;` for `<`. An
#   HTML entity (`&lt;`) holds a `;` and breaks the line like any other.
#
# USAGE
#   bash scripts/mermaid-sequence-audit.sh            # audit, exit non-zero on any fault
#   bash scripts/mermaid-sequence-audit.sh --summary  # report only, always exit 0
#
# ⚠️ Deliberately NO `set -o pipefail`: `grep` with no match exits 1, and "no match" is
#    the healthy answer here (fr-registry-audit.sh's note, learned the same way).

set -eu

cd "$(dirname "$0")/.."

MODE="${1:-}"

faults=$(
  git ls-files -z '*.md' \
    | xargs -0 awk '
        FNR == 1 { fence = 0; seq = 0 }
        /^[ \t]*```mermaid[ \t]*$/ { fence = 1; seq = 0; next }
        fence && /^[ \t]*```[ \t]*$/ { fence = 0; next }
        !fence { next }
        /^[ \t]*sequenceDiagram([ \t]|$)/ { seq = 1; next }
        !seq { next }
        {
          line = $0
          sub(/\r$/, "", line)
          text = ""
          if (line ~ /^[ \t]*%%/) next
          if (line ~ /^[ \t]*(participant|actor)[ \t]/ && match(line, /[ \t]as[ \t]/)) {
            text = substr(line, RSTART + RLENGTH)
          } else if (line ~ /^[ \t]*(alt|else|opt|loop|par|and|critical|option|break|box)([ \t]|$)/) {
            text = line
            sub(/^[ \t]*[a-z]+/, "", text)
          } else if (line ~ /^[ \t]*[Nn]ote[ \t]+(over|left of|right of)[ \t]/ || line ~ /(->|--|-x|-\))[^:]*:/) {
            i = index(line, ":")
            if (i == 0) next
            text = substr(line, i + 1)
          } else {
            next
          }
          # Mermaid decodes its own entity codes before the diagram is lexed.
          gsub(/#[A-Za-z0-9]+;/, "", text)
          if (text ~ /[;#]/) print FILENAME ":" FNR ": " line
        }
      ' 2>/dev/null
)

n=0
[ -n "$faults" ] && n=$(printf '%s\n' "$faults" | wc -l | tr -d ' ')

if [ "$n" -gt 0 ]; then
  echo "A ';' or '#' in the text of a mermaid sequence diagram (a message, a note, an alias"
  echo "or a block's label) ends it there: GitHub cannot draw a diagram with a ';' in one, and"
  echo "a '#' cuts the rest of its line as a comment. Write '#59;' for ';', '#35;' for '#'"
  echo "and '#lt;' / '#gt;' for '<' / '>', or say it another way:"
  printf '%s\n' "$faults" | sed 's/^/  ✗ /'
fi
echo "mermaid-sequence-audit: $n fault(s)"

if [ "$MODE" = "--summary" ]; then
  exit 0
fi
[ "$n" -eq 0 ]
