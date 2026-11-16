#!/usr/bin/env bash
#
# clippy-audited-crates.sh — the `--all-targets` clippy lane of ci.yml's rust-lint job,
# and of scripts/ci-local.sh: the server crates audited green under `--all-targets`, plus
# EVERY `roomler-ai-mod-*` workspace member.
#
# WHY THE MODULE LIST IS READ, NOT WRITTEN
#
#   The workspace clippy step before this one is not `--all-targets`, so it never compiles
#   a `#[cfg(test)]` body, an example or a bench. This lane is the only lint those get.
#   Until #3594 it named one module crate, conference, and the other modules' test code
#   was linted nowhere: fleet's `nudge.rs` and `socket.rs` each had a test module above
#   live items (`items_after_test_module`), and two PRs on one day met it and allowed the
#   lint locally. A hand-kept list holds only until the next module crate lands, so the
#   list is read from the workspace, and a new module crate is linted from its first PR.
#   For the same reason a module crate may not allow `items_after_test_module`: the fix is
#   to move the test module to the end of its file.
#
# Usage: bash scripts/clippy-audited-crates.sh
# Needs the rust-lint lane's Linux build deps (conference builds the mediasoup worker),
# and python3 to read `cargo metadata`.
set -euo pipefail
cd "$(dirname "$0")/.."

AUDITED=(roomler-ai-api roomler-ai-services roomler-ai-tests roomler-core)

if grep -rnE 'items_after_test_module' crates/modules Cargo.toml; then
  echo "::error::a module crate allows clippy::items_after_test_module — move its test module to the end of the file instead (#3594)" >&2
  exit 1
fi

modules=$(
  cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"].startswith("roomler-ai-mod-"):
        print(p["name"])' |
    sort
)
if [ -z "$modules" ]; then
  echo "::error::cargo metadata lists no roomler-ai-mod-* workspace member, so this lane would lint no module crate" >&2
  exit 1
fi

args=()
for crate in "${AUDITED[@]}" $modules; do
  args+=(-p "$crate")
done
echo "clippy --all-targets on: ${AUDITED[*]} $(echo $modules)"
exec cargo clippy "${args[@]}" --all-targets -- -D warnings
