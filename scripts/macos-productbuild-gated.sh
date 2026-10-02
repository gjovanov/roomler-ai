#!/usr/bin/env bash
#
# macos-productbuild-gated.sh — wrap a component .pkg into the product archive we
# ship, with a minimum macOS that `installer` itself enforces.
#
#   macos-productbuild-gated.sh <component.pkg> <out.pkg> <min-macos> [<installer-identity>]
#
# Why a gate, and why here. The .pkg bundles dylibs (Homebrew's libvpx, the
# vendored FFmpeg) that carry their own minimum macOS. When a runner image moves,
# that minimum moves with it, silently. The 2026-10 move off `macos-14` raised it
# to 15.0, because Homebrew no longer publishes a macOS 14 libvpx bottle. A Mac
# below the minimum must REFUSE the update and stay on the version it runs.
# Installing would leave a daemon that dies at launch on a missing symbol, which
# takes the operator's remote access to that Mac with it, and that Mac cannot
# pull its own fix.
#
# `productbuild --package` writes its Distribution implicitly, with no OS rule
# and no place to add one. So this asks productbuild for that very Distribution
# (`--synthesize`), adds Apple's <allowed-os-versions>, and builds from it. The
# update helper runs `installer -pkg … -target /`, which then fails, raises its
# operator notice ("Self-update to X failed … this Mac is still on Y") and
# changes nothing.
#
# Used by release-agent.yml (the shipped .pkg) and installer-smoke.yml (which must
# build its artifact exactly as the release does). The smoke also proves the gate
# with a negative control: a product gated at an impossible version must be
# REFUSED by the real `installer`.
set -euo pipefail

comp=${1:?component .pkg}
out=${2:?output .pkg}
min=${3:?minimum macOS, e.g. 15.0}
ident=${4:-}

[[ "$min" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] || { echo "::error::minimum macOS '$min' is not a version"; exit 1; }
[ -f "$comp" ] || { echo "::error::no component package at $comp"; exit 1; }

work=$(mktemp -d)
dist="$work/distribution.xml"
productbuild --synthesize --package "$comp" "$dist"

# The gate goes right after the root element's opening tag. Refuse to guess if
# that tag is not on one line: inserting inside it would produce invalid XML.
grep -Eq '^[[:space:]]*<installer-gui-script[^>]*>[[:space:]]*$' "$dist" \
  || { echo "::error::unexpected Distribution shape — the root tag is not on one line:"; cat "$dist"; exit 1; }
awk -v min="$min" '
  { print }
  /<installer-gui-script[^>]*>/ && !done {
    print "    <allowed-os-versions>"
    print "        <os-version min=\"" min "\"/>"
    print "    </allowed-os-versions>"
    done = 1
  }' "$dist" > "$dist.new"
mv "$dist.new" "$dist"
xmllint --noout "$dist"
echo "=== Distribution (gated at macOS $min) ==="
cat "$dist"

args=(--distribution "$dist" --package-path "$(dirname "$comp")")
[ -n "$ident" ] && args+=(--sign "$ident")
productbuild "${args[@]}" "$out"

# Read the gate back out of the BUILT product: what ships is what is checked.
pkgutil --expand "$out" "$work/expanded"
grep -q "<os-version min=\"$min\"/>" "$work/expanded/Distribution" \
  || { echo "::error::the built product does not carry the macOS $min gate"; exit 1; }
echo "gated: $out requires macOS $min or later"
rm -rf "$work"
