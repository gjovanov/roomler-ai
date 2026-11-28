#!/usr/bin/env bash
# The vendored webrtc crate's own tests (crates/vendored/webrtc), against the
# workspace's pinned versions and the other forks the crate is written
# against. It is not a workspace member, so no `cargo test` at the root builds
# it, and a plain `cargo test --manifest-path crates/vendored/webrtc/Cargo.toml`
# does not compile: the fork calls what only the vendored webrtc-ice and
# webrtc-sctp have (the `--config` patches below). The workspace's Cargo.lock
# seeds the build, as each entry of ci.yml's vendored-tests matrix does.
# Arguments go to `cargo test`; libtest's go after a `--`.
#
#   bash scripts/vendored-webrtc-test.sh               # build and run
#   bash scripts/vendored-webrtc-test.sh --no-run      # build only
#   bash scripts/vendored-webrtc-test.sh -- --ignored  # the ignored tests
#
# #4500 — its tests bind loopback only on Windows: see docs/testing.md, "How
# tests bind". CI runs this on Windows under the bind audit
# (.github/workflows/test-binds.yml). On Linux, ci.yml's vendored-tests matrix
# runs the same tests in full, from its webrtc entry (#4503), whose `patches`
# are the three below: change both together.
set -eu
cd "$(dirname "$0")/.."
cp Cargo.lock crates/vendored/webrtc/Cargo.lock
exec cargo test --manifest-path crates/vendored/webrtc/Cargo.toml --lib \
  --config 'patch.crates-io.rtp.path="crates/vendored/rtp"' \
  --config 'patch.crates-io.webrtc-ice.path="crates/vendored/webrtc-ice"' \
  --config 'patch.crates-io.webrtc-sctp.path="crates/vendored/webrtc-sctp"' \
  "$@"
