#!/usr/bin/env bash
# Put a published release's Linux binaries where `cargo deb` reads them, so a
# packaging change can go through the release lane's own `cargo deb --no-build
# --no-strip` without compiling the agent. The payload is the release's; the
# control file is this checkout's. The lane that gates a PR on it is
# .github/workflows/deb-bare-image.yml.
#
# Usage: scripts/deb-stage-release.sh <target dir> ROOMLERD.deb [ROOMLER-DESKTOP.deb]
# Run from the repo root.
set -euo pipefail

target="${1:?target dir, e.g. target}"
daemon="${2:?the released roomlerd .deb}"
companion="${3:-}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

dpkg-deb -x "$daemon" "$work/daemon"
mkdir -p "$target/release" agents/roomlerd/vendor-ffmpeg
cp "$work/daemon/usr/bin/roomlerd" "$target/release/roomlerd"
# The asset list installs the shim under the user-facing name.
cp "$work/daemon/usr/bin/roomler" "$target/release/roomler-shim"
find "$work/daemon/usr/lib" -name '*.so.*' -exec cp {} agents/roomlerd/vendor-ffmpeg/ \;

if [ -n "$companion" ]; then
    dpkg-deb -x "$companion" "$work/companion"
    cp "$work/companion/usr/bin/roomler-desktop" "$target/release/roomler-desktop"
fi
