#!/bin/sh
# roomler install.sh — terminal-driven install of the Roomler node stack
# (Linux + macOS), replicating the roomler-setup wizard's steps without a
# GUI: resolve via the roomler.ai proxy → download → sha256-verify →
# install → enroll → autostart.
#
# Usage (pipe from the API, or run a checked-out copy):
#
#   curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
#       --role daemon --token <enrollment-jwt> [--server https://roomler.ai] \
#       [--name "$(hostname)"]
#
#   Roles:
#     daemon  — the roomlerd daemon ("be accessed"): Linux .deb / macOS .pkg,
#               enrolls with an AGENT enrollment token (Admin → Agents),
#               enables the packaged autostart (systemd user unit / LaunchAgent).
#     tunnel  — the roomler CLI only ("reach others"): enrolls with a TUNNEL
#               enrollment token (Admin → Tunnels).
#
#   --download-only   resolve + download + verify, print what WOULD run, touch
#                     nothing else (safe on any box).
#   --no-enroll       install without enrolling (no token needed); prints the
#                     enroll command to run later.
#
# The desktop companion (roomler-desktop) ships for Windows only today — this
# script notes that and moves on.
#
# Conventions: POSIX sh (no bashisms), curl -fsSL, /tmp staging, sudo only
# where the target dir demands it. The enrollment token is single-use and is
# never echoed.

set -eu

SERVER="https://roomler.ai"
ROLE=""
TOKEN=""
NAME="$(hostname 2>/dev/null || echo roomler-device)"
DOWNLOAD_ONLY=0
NO_ENROLL=0

usage() {
    sed -n '2,30p' "$0" 2>/dev/null || true
    echo "usage: install.sh --role daemon|tunnel [--server URL] [--token JWT] [--name NAME] [--download-only] [--no-enroll]"
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --role) ROLE="$2"; shift 2 ;;
        --server) SERVER="$2"; shift 2 ;;
        --token) TOKEN="$2"; shift 2 ;;
        --name) NAME="$2"; shift 2 ;;
        --download-only) DOWNLOAD_ONLY=1; shift ;;
        --no-enroll) NO_ENROLL=1; shift ;;
        -h|--help) usage ;;
        *) echo "unknown flag: $1" >&2; usage ;;
    esac
done

case "$ROLE" in
    daemon|tunnel) ;;
    "") echo "error: --role daemon|tunnel is required" >&2; usage ;;
    *) echo "error: unknown role '$ROLE' (expected daemon|tunnel)" >&2; exit 1 ;;
esac

OS="$(uname -s)"
case "$OS" in
    Linux|Darwin) ;;
    *) echo "error: unsupported OS '$OS' — use install.ps1 on Windows" >&2; exit 1 ;;
esac

STAGE="$(mktemp -d /tmp/roomler-install.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }

# Verify a file against a "sha256:<hex>" digest (GitHub asset format).
# Soft-skips when the digest is empty (older releases lack it).
verify_sha256() {
    file="$1"; digest="$2"
    [ -n "$digest" ] || { warn "no sha256 digest published for $(basename "$file") — skipping verification"; return 0; }
    want="$(printf '%s' "$digest" | sed 's/^sha256://')"
    if command -v sha256sum >/dev/null 2>&1; then
        got="$(sha256sum "$file" | awk '{print $1}')"
    else
        got="$(shasum -a 256 "$file" | awk '{print $1}')"
    fi
    if [ "$got" != "$want" ]; then
        echo "error: sha256 mismatch for $file (got $got, want $want)" >&2
        exit 1
    fi
    say "sha256 verified: $(basename "$file")"
}

# Extract "browser_download_url" / "digest" for the first asset whose name
# matches a suffix pattern, from the compact JSON our API emits. grep-based
# so the script has NO jq/python dependency; the serde output has no
# whitespace inside objects, which keeps this reliable.
asset_field_for() {
    json_file="$1"; name_pattern="$2"; field="$3"
    # Objects are comma-separated; split them onto lines first so one
    # grep can anchor name + field within a single asset object.
    tr '{' '\n' < "$json_file" \
        | grep '"name":"[^"]*'"$name_pattern"'"' \
        | grep -o '"'"$field"'":"[^"]*"' \
        | head -n 1 \
        | sed 's/^"'"$field"'":"//; s/"$//'
}

download() {
    url="$1"; out="$2"
    say "downloading $(basename "$out")"
    curl -fsSL -o "$out" "$url"
}

# ─── daemon role ────────────────────────────────────────────────────────────

install_daemon_linux() {
    releases="$STAGE/releases.json"
    say "resolving latest agent release via $SERVER/api/agent/latest-release"
    curl -fsSL -o "$releases" "$SERVER/api/agent/latest-release"
    url="$(asset_field_for "$releases" 'x86_64-unknown-linux-gnu\.deb' browser_download_url)"
    digest="$(asset_field_for "$releases" 'x86_64-unknown-linux-gnu\.deb' digest)"
    [ -n "$url" ] || { echo "error: no linux .deb asset in the latest agent release" >&2; exit 1; }
    deb="$STAGE/roomler-agent.deb"
    download "$url" "$deb"
    verify_sha256 "$deb" "$digest"

    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        say "download-only: would run: sudo dpkg -i $deb"
        say "download-only: would run: roomlerd enroll --server $SERVER --token <token> --name $NAME"
        say "download-only: would run: systemctl --user enable --now roomler.service"
        return 0
    fi

    say "installing the roomlerd daemon (.deb — sudo required)"
    sudo dpkg -i "$deb" || sudo apt-get -f install -y

    enroll_daemon /usr/bin/roomlerd

    say "enabling the systemd user unit (autostart, this login session's user)"
    systemctl --user daemon-reload || true
    systemctl --user enable --now roomler.service
    say "daemon status: $(systemctl --user is-active roomler.service || true)"
    say "NOTE: on a headless host, run 'sudo loginctl enable-linger $USER' so the user unit runs without an open session."
}

install_daemon_macos() {
    releases="$STAGE/releases.json"
    say "resolving latest agent release via $SERVER/api/agent/latest-release"
    curl -fsSL -o "$releases" "$SERVER/api/agent/latest-release"
    url="$(asset_field_for "$releases" 'aarch64-apple-darwin[^"]*\.pkg' browser_download_url)"
    digest="$(asset_field_for "$releases" 'aarch64-apple-darwin[^"]*\.pkg' digest)"
    [ -n "$url" ] || { echo "error: no macOS .pkg asset in the latest agent release" >&2; exit 1; }
    pkg="$STAGE/roomler-agent.pkg"
    download "$url" "$pkg"
    verify_sha256 "$pkg" "$digest"

    daemon_bin="/Applications/roomler-agent.app/Contents/MacOS/roomlerd"
    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        say "download-only: would run: sudo installer -pkg $pkg -target /"
        say "download-only: would run: $daemon_bin enroll --server $SERVER --token <token> --name $NAME"
        return 0
    fi

    say "installing the roomlerd daemon (.pkg — sudo required; postinstall loads the LaunchAgent)"
    sudo installer -pkg "$pkg" -target /

    enroll_daemon "$daemon_bin"

    # postinstall already bootstrapped com.roomler.agent into the console
    # user's gui domain; kickstart restarts it so it picks up the fresh
    # enrollment config.
    say "restarting the LaunchAgent so it picks up the enrollment"
    launchctl kickstart -k "gui/$(id -u)/com.roomler.agent" 2>/dev/null \
        || warn "launchctl kickstart failed — the agent will pick up the config at next login"
}

enroll_daemon() {
    daemon_bin="$1"
    if [ "$NO_ENROLL" = 1 ] || [ -z "$TOKEN" ]; then
        [ "$NO_ENROLL" = 1 ] || warn "no --token given — skipping enrollment"
        say "enroll later with: $daemon_bin enroll --server $SERVER --token <agent-enrollment-jwt> --name \"$NAME\""
        return 0
    fi
    say "enrolling this machine as '$NAME' against $SERVER (token is single-use, never echoed)"
    "$daemon_bin" enroll --server "$SERVER" --token "$TOKEN" --name "$NAME"
}

# ─── tunnel role ────────────────────────────────────────────────────────────

install_tunnel_linux() {
    if command -v dpkg >/dev/null 2>&1; then
        deb="$STAGE/roomler-tunnel.deb"
        say "downloading the roomler CLI (.deb) via the proxy"
        download "$SERVER/api/tunnel/installer/linux-deb?version=latest" "$deb"
        if [ "$DOWNLOAD_ONLY" = 1 ]; then
            say "download-only: would run: sudo dpkg -i $deb"
            return 0
        fi
        sudo dpkg -i "$deb" || sudo apt-get -f install -y
    else
        tarball="$STAGE/roomler-tunnel.tar.gz"
        say "downloading the roomler CLI (tarball) via the proxy"
        download "$SERVER/api/tunnel/installer/linux-x86_64?version=latest" "$tarball"
        if [ "$DOWNLOAD_ONLY" = 1 ]; then
            say "download-only: would extract to /usr/local/bin"
            return 0
        fi
        install_tunnel_tarball "$tarball"
    fi
    enroll_tunnel
}

install_tunnel_macos() {
    tarball="$STAGE/roomler-tunnel.tar.gz"
    say "downloading the roomler CLI (universal tarball) via the proxy"
    download "$SERVER/api/tunnel/installer/macos?version=latest" "$tarball"
    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        say "download-only: would extract to /usr/local/bin"
        return 0
    fi
    install_tunnel_tarball "$tarball"
    enroll_tunnel
}

install_tunnel_tarball() {
    tarball="$1"
    xdir="$STAGE/tunnel"
    mkdir -p "$xdir"
    tar -xzf "$tarball" -C "$xdir"
    # Archives ship BOTH names since the P3d rename; prefer `roomler`.
    bin=""
    for name in roomler roomler-tunnel; do
        found="$(find "$xdir" -maxdepth 2 -type f -name "$name" | head -n 1)"
        [ -n "$found" ] && { bin="$found"; break; }
    done
    [ -n "$bin" ] || { echo "error: no roomler/roomler-tunnel binary in the archive" >&2; exit 1; }
    say "installing $(basename "$bin") to /usr/local/bin/roomler (sudo required)"
    sudo install -m 755 "$bin" /usr/local/bin/roomler
    # Legacy-name convenience symlink, matching the archives' compat alias.
    sudo ln -sf /usr/local/bin/roomler /usr/local/bin/roomler-tunnel
}

enroll_tunnel() {
    cli="$(command -v roomler || command -v roomler-tunnel || true)"
    [ -n "$cli" ] || cli=/usr/local/bin/roomler
    if [ "$NO_ENROLL" = 1 ] || [ -z "$TOKEN" ]; then
        [ "$NO_ENROLL" = 1 ] || warn "no --token given — skipping enrollment"
        say "enroll later with: $cli enroll --server $SERVER --token <tunnel-enrollment-jwt> --name \"$NAME\""
        return 0
    fi
    say "enrolling this tunnel client as '$NAME' against $SERVER (token is single-use, never echoed)"
    "$cli" enroll --server "$SERVER" --token "$TOKEN" --name "$NAME"
}

# ─── main ───────────────────────────────────────────────────────────────────

say "roomler install.sh — role=$ROLE os=$OS server=$SERVER"
say "note: the roomler-desktop companion ships for Windows only today — not installed here"

case "$ROLE/$OS" in
    daemon/Linux)  install_daemon_linux ;;
    daemon/Darwin) install_daemon_macos ;;
    tunnel/Linux)  install_tunnel_linux ;;
    tunnel/Darwin) install_tunnel_macos ;;
esac

say "done."
