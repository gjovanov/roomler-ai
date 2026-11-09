#!/usr/bin/env bash
#
# bun-version-check.sh — bun's version has ONE record, `.bun-version`, and everything that
# installs bun for the build reads it or is checked against it (#3400).
#
# WHY THIS EXISTS
#
#   bun's version was written in four places and nothing made them agree: three workflows
#   pinned it exactly (the frontend checks, the hosted-image lane, the phone smoke), and the
#   server image's UI stage ran on `oven/bun:1`, whatever 1.x was newest on the day of the
#   build. That stage runs `bun install --frozen-lockfile` and `bun run build`, which
#   type-checks with vue-tsc, so a bun minor that changes the lockfile format or vue-tsc's
#   verdict broke the hosted build on master after the PR's checks had passed, and two
#   publishes of one commit could carry different bun versions.
#
#   So the record is `.bun-version`, which `oven-sh/setup-bun` reads directly
#   (`bun-version-file`), and the Dockerfile's `BUN_VERSION` build argument defaults to it.
#   This check fails when:
#     * a workflow step that sets up bun names a version of its own, or reads another file;
#     * the Dockerfile's `ARG BUN_VERSION=` default differs from the record, or a stage
#       builds on an `oven/bun` tag that is not `${BUN_VERSION}`.
#   `Dockerfile.e2e` (the Playwright image) is outside this record on purpose.
#
# Before it checks the repository it runs on planted files, and fails unless it refuses
# each planted drift: a gate that cannot see a drift is worse than none.
#
# Usage: bash scripts/bun-version-check.sh
# Needs yq (v4) and jq, both on GitHub's Ubuntu runners.
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in yq jq; do
  command -v "$tool" >/dev/null || { echo "::error::bun-version-check needs $tool"; exit 1; }
done

RECORD=.bun-version

# check <record-file> <dockerfile> <workflow...> — prints one line per problem.
check() {
  local record=$1 dockerfile=$2
  shift 2
  local want
  want=$(tr -d ' \r\n' < "$record")
  if ! [[ "$want" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "$record: '$want' is not an exact bun version (x.y.z)"
    return
  fi
  local wf
  for wf in "$@"; do
    yq -o=json '.' "$wf" | jq -r --arg file "$RECORD" --arg wf "$wf" '
      (.jobs // {}) | to_entries[] | .key as $job
      | (.value.steps // []) | to_entries[]
      | select((.value.uses // "") | startswith("oven-sh/setup-bun"))
      | .key as $i | (.value.with // {}) as $with
      | if $with["bun-version"] != null then
          "\($wf): job \($job), step \($i) (\(.value.name // "unnamed")): names bun \($with["bun-version"]); read the record instead: bun-version-file: \($file)"
        elif $with["bun-version-file"] != $file then
          "\($wf): job \($job), step \($i) (\(.value.name // "unnamed")): bun-version-file is \($with["bun-version-file"] // "unset"), not \($file)"
        else empty end'
    if grep -nE 'bun\.sh/install|npm (i|install) .*-g .*\bbun\b' "$wf"; then
      echo "$wf: installs bun outside oven-sh/setup-bun, so it cannot read $RECORD"
    fi
  done
  local default
  default=$(sed -nE 's/^ARG BUN_VERSION=([^[:space:]]+).*$/\1/p' "$dockerfile" | tr -d '\r')
  if [ -z "$default" ]; then
    echo "$dockerfile: no 'ARG BUN_VERSION=<default>' before the stages"
  elif [ "$default" != "$want" ]; then
    echo "$dockerfile: ARG BUN_VERSION defaults to $default, the record ($record) says $want"
  fi
  grep -nE '^FROM[[:space:]]+oven/bun' "$dockerfile" | grep -vE 'oven/bun:\$\{BUN_VERSION\}([[:space:]]|$)' \
    | sed "s|^|$dockerfile: a stage builds on another bun than \${BUN_VERSION}: |" || true
}

# Self-test on planted files: a clean set passes, and each drift is refused.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo 1.2.3 > "$tmp/record"
cat > "$tmp/Dockerfile" <<'DF'
ARG BUN_VERSION=1.2.3
FROM oven/bun:${BUN_VERSION} AS ui-builder
DF
cat > "$tmp/ok.yml" <<'YML'
jobs:
  a:
    steps:
      - uses: oven-sh/setup-bun@v2
        with:
          bun-version-file: .bun-version
YML
cat > "$tmp/named.yml" <<'YML'
jobs:
  a:
    steps:
      - name: Install bun
        uses: oven-sh/setup-bun@v2
        with:
          bun-version: 1.2.4
YML
cat > "$tmp/unset.yml" <<'YML'
jobs:
  a:
    steps:
      - uses: oven-sh/setup-bun@v2
YML
sed 's/BUN_VERSION=1.2.3/BUN_VERSION=1.2.4/' "$tmp/Dockerfile" > "$tmp/Dockerfile.default"
printf 'ARG BUN_VERSION=1.2.3\nFROM oven/bun:1 AS ui-builder\n' > "$tmp/Dockerfile.floating"

expect() {   # expect <what> <count> <check args...>
  local what=$1 n=$2
  shift 2
  local got
  got=$(check "$@" | grep -c . || true)
  if [ "$got" != "$n" ]; then
    echo "::error::bun-version-check self-test: $what: expected $n problem(s), found $got"
    check "$@" | sed 's/^/  /'
    exit 1
  fi
}
expect "a clean set" 0 "$tmp/record" "$tmp/Dockerfile" "$tmp/ok.yml"
expect "a step naming its own version" 1 "$tmp/record" "$tmp/Dockerfile" "$tmp/named.yml"
expect "a step with no version file" 1 "$tmp/record" "$tmp/Dockerfile" "$tmp/unset.yml"
expect "an image default off the record" 1 "$tmp/record" "$tmp/Dockerfile.default" "$tmp/ok.yml"
expect "a stage on a floating tag" 1 "$tmp/record" "$tmp/Dockerfile.floating" "$tmp/ok.yml"
echo "self-test: a clean planted set passes and each of four planted drifts is refused"

problems=$(check "$RECORD" Dockerfile .github/workflows/*.yml)
if [ -n "$problems" ]; then
  while IFS= read -r p; do echo "::error::$p"; done <<< "$problems"
  exit 1
fi
steps=$(cat .github/workflows/*.yml | grep -c 'oven-sh/setup-bun' || true)
echo "ok: bun $(tr -d ' \r\n' < "$RECORD") — $steps setup-bun step(s) read $RECORD, and the Dockerfile's BUN_VERSION defaults to it"
