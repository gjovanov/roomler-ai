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
#   --system          (daemon role, Linux only) install as a ROOT systemd
#                     SYSTEM service instead of the per-user unit. Pick this
#                     for a headless / server / WSL node: root is what makes
#                     unattended self-update work (installing a .deb needs it,
#                     and pkexec cannot authenticate with no polkit agent) and
#                     what lets the overlay bring up its TUN. You lose screen
#                     capture + input injection, which need a logged-in X
#                     session. Config goes to /etc/roomler/config.toml.
#
#   --download-only   resolve + download + verify, print what WOULD run, touch
#                     nothing else (safe on any box).
#   --no-enroll       install without enrolling (no token needed); prints the
#                     enroll command to run later.
#
# The desktop companion (roomler-desktop) rides IN the macOS .pkg. On Linux it
# is a SEPARATE .deb (FR-27) — installed automatically when this box has a
# graphical session, since the companion is the device's consent PROMPT
# surface and a device set to "Prompt on host" without one cannot ask anybody.
#   --desktop / --no-desktop   force that decision either way.
#
# Conventions: POSIX sh (no bashisms), curl -fsSL, /tmp staging, sudo only
# where the target dir demands it. The enrollment token is single-use and is
# never echoed.

set -eu

# ⚠️ FR-50 — this line is REWRITTEN at serve time by the server that hands you
# this script (`crates/api/src/routes/setup_release.rs`), so a script fetched
# from a self-hosted Roomler enrols against THAT server and not the hosted one.
# A piped script cannot see the URL it was fetched from — there is no $0 and no
# filename — so the substitution has to happen on the server side. The literal
# below is the default for a checkout, and it is also what a server serves
# UNCHANGED when its own `app.frontend_url` is not a plain scheme://host[:port].
# `--server` still overrides either.
#
# ⚠️ The API holds this exact text as a needle and a unit test asserts it
# appears exactly once, so editing it fails the build rather than silently
# disabling the substitution. Change both together.
SERVER="https://roomler.ai"
ROLE=""
TOKEN=""
NAME="$(hostname 2>/dev/null || echo roomler-device)"
DOWNLOAD_ONLY=0
SYSTEM=0
# Machine-global config for the --system (root) flow. The system unit starts
# at boot before any login, so a $HOME-derived path would be unreachable —
# and as root $HOME is /root, where the agent's appdirs resolver would look
# in the wrong tree entirely. Must match roomlerd.service's ROOMLERD_CONFIG.
SYSTEM_CONFIG="/etc/roomler/config.toml"
# macOS's privileged half has its OWN path, and it is NOT $SYSTEM_CONFIG above:
# com.roomler.daemon.plist passes it explicitly as `--config`, and the agent's
# root-aware config ladder is Linux-only. Reusing one variable for both would
# enroll into a file nothing reads.
# FR-46 P5a moved this off the retired name; the .pkg postinstall migrates an
# existing Mac and both gates it guards dual-read during the changeover.
MACOS_DAEMON_CONFIG="/etc/roomler/config.toml"
MACOS_DAEMON_MARKER="/etc/roomler/enable-daemon"
DAEMON_TOKEN=""
NO_ENROLL=0
# FR-27 — the Linux desktop companion. Empty = decide by probing for a
# graphical session; --desktop / --no-desktop force it either way.
DESKTOP=""

usage() {
    sed -n '2,30p' "$0" 2>/dev/null || true
    echo "usage: install.sh --role daemon|tunnel [--server URL] [--token JWT] [--name NAME]"
    echo "                  [--daemon-token JWT] [--system] [--download-only] [--no-enroll]"
    echo "                  [--desktop|--no-desktop]"
    echo
    echo "  --desktop       Linux only. Force the roomler-desktop companion on or off."
    echo "                  Default: installed when a graphical session is detected. It is"
    echo "                  the device's on-screen consent prompt, so a host set to"
    echo "                  'Prompt on host' without it cannot ask anybody."
    echo
    echo "  --daemon-token  macOS only. Installs the privileged half as well (overlay/mesh)."
    echo "                  macOS needs TWO processes: a per-user LaunchAgent for screen"
    echo "                  capture and input (they only work inside a GUI login session)"
    echo "                  and a root LaunchDaemon for the overlay (creating a utun needs"
    echo "                  root). They cannot share one enrollment — the hub keys on"
    echo "                  agent_id and the second connection displaces the first — so"
    echo "                  this takes a SECOND single-use enrollment token and the Mac"
    echo "                  appears as two devices."
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --role) ROLE="$2"; shift 2 ;;
        --server) SERVER="$2"; shift 2 ;;
        --token) TOKEN="$2"; shift 2 ;;
        --name) NAME="$2"; shift 2 ;;
        --daemon-token) DAEMON_TOKEN="$2"; shift 2 ;;
        --system) SYSTEM=1; shift ;;
        --download-only) DOWNLOAD_ONLY=1; shift ;;
        --no-enroll) NO_ENROLL=1; shift ;;
        --desktop) DESKTOP=1; shift ;;
        --no-desktop) DESKTOP=0; shift ;;
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

if [ "$SYSTEM" = 1 ] && { [ "$ROLE" != daemon ] || [ "$OS" != Linux ]; }; then
    echo "error: --system applies to '--role daemon' on Linux only (on macOS use --daemon-token for the privileged half; the tunnel role installs a CLI, not a service)" >&2
    exit 1
fi

if [ -n "$DAEMON_TOKEN" ] && { [ "$ROLE" != daemon ] || [ "$OS" != Darwin ]; }; then
    echo "error: --daemon-token applies to '--role daemon' on macOS only" >&2
    exit 1
fi

# ─── who is the desktop user? ───────────────────────────────────────────────
#
# On macOS the per-user half is a LaunchAgent in the console user's `gui/<uid>`
# domain, and its config comes from that user's HOME. So the enroll has to run
# AS that user — not as whoever invoked the script.
#
# This matters because running the whole one-liner under `sudo` is the natural
# thing to do (the script calls `sudo` internally for `installer`, so people
# reasonably front-load it), and it used to break the install silently:
# macOS sudoers carries `env_keep += "HOME"`, so the enroll wrote the config to
# the user's own path but owned root:wheel 0600 — and the uid-501 LaunchAgent
# then hit EACCES on every start, relaunched under KeepAlive, and logged to a
# file in /tmp nothing points at. With `sudo -i` it is worse but louder: the
# config lands in /var/root and the LaunchAgent's path simply does not exist.
# Either way the user sees "it didn't start as a service".
#
# `stat -f %Su /dev/console` is the same idiom the pkg postinstall uses to
# place the plist, so the two agree by construction.
CONSOLE_USER=""
CONSOLE_UID=""
if [ "$OS" = Darwin ]; then
    CONSOLE_USER="$(stat -f "%Su" /dev/console 2>/dev/null || echo "")"
    if [ -z "$CONSOLE_USER" ] || [ "$CONSOLE_USER" = root ]; then
        echo "error: no GUI session is logged in (console user is '${CONSOLE_USER:-unknown}')." >&2
        echo "       The macOS agent runs inside a login session — screen capture and input" >&2
        echo "       do not work from a LaunchDaemon. Log in on the Mac and re-run this." >&2
        exit 1
    fi
    CONSOLE_UID="$(id -u "$CONSOLE_USER")"
    if [ "$(id -u)" = 0 ]; then
        printf '==> %s\n' "running as root — the per-user half will be installed for '$CONSOLE_USER' (uid $CONSOLE_UID)"
    fi
fi

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

    # Two axes, same rules as the agent's own updater
    # (docs/linux-self-update.md): the arch we are running, and a format this
    # host can actually install. A .deb needs dpkg/apt — on Fedora / RHEL /
    # SUSE / Arch it downloads and then fails — so those hosts take the
    # self-contained tarball.
    case "$(uname -m)" in
        x86_64|amd64)   arch=x86_64 ;;
        aarch64|arm64)  arch=aarch64 ;;
        *) echo "error: unsupported Linux architecture $(uname -m)" >&2; exit 1 ;;
    esac
    if command -v dpkg >/dev/null 2>&1 || command -v apt-get >/dev/null 2>&1; then
        fmt=deb
    else
        fmt=tar.gz
    fi
    say "host: linux/$arch, installing the .$fmt"

    pattern="${arch}-unknown-linux-gnu\\.${fmt}"
    url="$(asset_field_for "$releases" "$pattern" browser_download_url)"
    digest="$(asset_field_for "$releases" "$pattern" digest)"
    if [ -z "$url" ] && [ "$fmt" = deb ]; then
        # A Debian host whose .deb is missing still installs via the tarball
        # rather than treating an absent artifact as a dead end.
        say "no .deb for ${arch} in this release — falling back to the tarball"
        fmt=tar.gz
        pattern="${arch}-unknown-linux-gnu\\.tar\\.gz"
        url="$(asset_field_for "$releases" "$pattern" browser_download_url)"
        digest="$(asset_field_for "$releases" "$pattern" digest)"
    fi
    [ -n "$url" ] || { echo "error: no linux/${arch} .${fmt} asset in the latest agent release" >&2; exit 1; }
    deb="$STAGE/roomlerd.${fmt}"
    download "$url" "$deb"
    verify_sha256 "$deb" "$digest"

    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        if [ "$fmt" = deb ]; then
            say "download-only: would run: sudo dpkg -i $deb"
        else
            say "download-only: would run: sudo tar -xzf $deb --strip-components=1 -C /"
        fi
        if [ "$SYSTEM" = 1 ]; then
            say "download-only: would run: sudo roomlerd --config $SYSTEM_CONFIG enroll --server $SERVER --token <token> --name $NAME"
            say "download-only: would run: sudo systemctl enable --now roomlerd.service"
        else
            say "download-only: would run: roomlerd enroll --server $SERVER --token <token> --name $NAME"
            say "download-only: would run: systemctl --user enable --now roomler.service"
        fi
        return 0
    fi

    if [ "$fmt" = deb ]; then
        say "installing the roomlerd daemon (.deb — sudo required)"
        sudo dpkg -i "$deb" || sudo apt-get -f install -y
    else
        say "installing the roomlerd daemon (tarball — sudo required)"
        # --strip-components=1 drops the versioned prefix dir so the payload
# RETIRED-NAME-ANCHOR: usr/lib/roomler-agent is baked into the binary as an
# RPATH — the directory name cannot move without a relink.
        # lands at /usr/bin, /usr/lib/roomler-agent, /usr/lib/systemd/…
        # (identical to what the .deb installs). On a FIRST install there are
        # no operator-edited units to protect, so unpacking wholesale is fine;
        # the agent's own updater is the path that preserves them on upgrade.
        sudo tar -xzf "$deb" --strip-components=1 -C /
        sudo test -x /usr/bin/roomlerd || { echo "error: tarball did not install /usr/bin/roomlerd" >&2; exit 1; }
    fi

    enroll_daemon /usr/bin/roomlerd

    if [ "$SYSTEM" = 1 ]; then
        say "enabling the systemd SYSTEM unit (root — self-update + overlay TUN work; no screen capture)"
        sudo systemctl daemon-reload || true
        sudo systemctl enable --now roomlerd.service
        say "daemon status: $(sudo systemctl is-active roomlerd.service || true)"
        say "logs: sudo journalctl -u roomlerd.service -f"
    else
        say "enabling the systemd user unit (autostart, this login session's user)"
        systemctl --user daemon-reload || true
        systemctl --user enable --now roomler.service
        say "daemon status: $(systemctl --user is-active roomler.service || true)"
        say "NOTE: on a headless host, run 'sudo loginctl enable-linger $USER' so the user unit runs without an open session."
        say "NOTE: a per-user daemon cannot install its own updates (writing /usr needs root, whichever format). Re-run with --system for unattended self-update."
    fi
}

install_daemon_macos() {
    releases="$STAGE/releases.json"
    say "resolving latest agent release via $SERVER/api/agent/latest-release"
    curl -fsSL -o "$releases" "$SERVER/api/agent/latest-release"
    url="$(asset_field_for "$releases" 'aarch64-apple-darwin[^"]*\.pkg' browser_download_url)"
    digest="$(asset_field_for "$releases" 'aarch64-apple-darwin[^"]*\.pkg' digest)"
    [ -n "$url" ] || { echo "error: no macOS .pkg asset in the latest agent release" >&2; exit 1; }
    pkg="$STAGE/roomlerd.pkg"
    download "$url" "$pkg"
    verify_sha256 "$pkg" "$digest"

    # The .app kept its legacy internal name through the roomlerd rename —
# RETIRED-NAME-ANCHOR(5): the macOS .app bundle name is FROZEN (FR-21 D5) —
# it keys the host's Screen Recording and Accessibility TCC grants, which a
# rename would silently void, leaving a black screen with no error.
    # CFBundleExecutable is `roomler-agent`, and CI asserts it, because
    # renaming would change the binary's TCC identity and force every existing
    # Mac to re-grant Screen Recording and Accessibility. The old probe led
    # with `roomlerd`, which never exists, and relied on the fallback.
    daemon_bin="/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd"

    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        say "download-only: would run: sudo installer -pkg $pkg -target /"
        say "download-only: would run: $daemon_bin enroll --server $SERVER --token <token> --name $NAME"
        [ -n "$DAEMON_TOKEN" ] && say "download-only: would also enroll the privileged half at $MACOS_DAEMON_CONFIG"
        return 0
    fi

    # The marker has to exist BEFORE the pkg runs: the postinstall reads it to
    # decide whether to install the root LaunchDaemon, and its ABSENCE actively
    # removes a previously-installed one. Nothing in the product ever created
    # this file, which is why the privileged half was unreachable through the
    # advertised one-liner despite shipping for months.
    if [ -n "$DAEMON_TOKEN" ]; then
        say "requesting the privileged half (root LaunchDaemon — the overlay needs root)"
        sudo mkdir -p "$(dirname "$MACOS_DAEMON_MARKER")"
        sudo touch "$MACOS_DAEMON_MARKER"
    fi

    say "installing the roomler agent (.pkg — sudo required)"
    sudo installer -pkg "$pkg" -target /

    [ -x "$daemon_bin" ] || {
        echo "error: the package did not install $daemon_bin" >&2
        exit 1
    }

    # ── the per-user half: capture + input, inside the GUI session ──
    enroll_daemon "$daemon_bin"

    # postinstall bootstrapped com.roomler.agent into the CONSOLE user's gui
    # domain, so kickstart it THERE. `gui/$(id -u)` was wrong under sudo — uid
    # 0 has no Aqua domain, the error went to /dev/null, and the warning that
    # replaced it ("will pick up the config at next login") was misleading.
    say "restarting the LaunchAgent so it picks up the enrollment"
    launchctl kickstart -k "gui/$CONSOLE_UID/com.roomler.agent" 2>/dev/null \
        || warn "launchctl kickstart failed — the agent will pick up the config at next login"

    # ── the privileged half: overlay/mesh, as root, its own enrollment ──
    if [ -n "$DAEMON_TOKEN" ]; then
        install_macos_privileged_half "$daemon_bin"
    else
        say "NOTE: this Mac has the per-user half only — screen sharing works, the overlay"
        say "      mesh does not (creating a utun needs root). Pass --daemon-token <a second"
        say "      enrollment token> to add the privileged half."
    fi

    macos_permissions_notice
}

# The root half. A SEPARATE enrollment by construction: the hub keys sessions
# on agent_id, so two processes sharing one identity would fight over the
# control WS — the second connection displaces the first.
install_macos_privileged_half() {
    daemon_bin="$1"

    if [ ! -f /Library/LaunchDaemons/com.roomler.daemon.plist ]; then
        warn "the package did not install the LaunchDaemon — skipping the privileged half"
        return 0
    fi

    if sudo test -f "$MACOS_DAEMON_CONFIG"; then
        say "privileged half already enrolled at $MACOS_DAEMON_CONFIG — leaving it alone"
    else
        # `--overlay` writes overlay_enabled with the rest of the enrolled
        # config. The overlay is the ONLY reason this half exists, and choosing
        # to install a root daemon IS the opt-in, so it would be perverse to
        # leave the operator hunting for a config key afterwards.
        # (`overlay_enabled` stays default-off everywhere else — unchanged.)
        say "enrolling the privileged half as '$NAME-daemon' (second single-use token, overlay on)"
        sudo "$daemon_bin" --config "$MACOS_DAEMON_CONFIG" \
            enroll --server "$SERVER" --token "$DAEMON_TOKEN" --name "$NAME-daemon" --overlay
    fi

    say "starting the privileged half"
    sudo launchctl kickstart -k system/com.roomler.daemon 2>/dev/null \
        || warn "launchctl kickstart failed for the daemon — it will start at next boot"
}

# macOS grants these two by hand, per binary, and never errors when they are
# missing: capture silently returns wallpaper-only frames and injected input is
# silently dropped. The agent logs both, but to a file under /tmp that nobody
# is told about — so say it here, where the person installing is still looking.
macos_permissions_notice() {
    say ""
    say "ONE MANUAL STEP REMAINS — macOS will not grant these without you:"
# RETIRED-NAME-ANCHOR(2): this is the string macOS SHOWS in the privacy pane;
# it comes from the frozen bundle, so it must match what the user will see.
    say "  System Settings → Privacy & Security → Screen Recording  → enable 'roomler-agent'"
    say "  System Settings → Privacy & Security → Accessibility     → enable 'roomler-agent'"
    say ""
    say "Without Screen Recording the remote screen is blank; without Accessibility the"
    say "remote keyboard and mouse do nothing. Neither reports an error. After granting:"
    say "  launchctl kickstart -k gui/$CONSOLE_UID/com.roomler.agent"
}

enroll_daemon() {
    daemon_bin="$1"
    # --system enrolls as root into the machine-global path the system unit
    # runs with; the per-user flow keeps the caller's own profile.
    if [ "$SYSTEM" = 1 ]; then
        enroll_pre="sudo $daemon_bin --config $SYSTEM_CONFIG"
    elif [ "$OS" = Darwin ] && [ "$(id -u)" = 0 ]; then
        # See `as_console_user`: the LaunchAgent runs as the console user and
        # reads that user's config, so the enroll has to write it AS them.
        # Running it as root here is what silently broke the whole install.
        enroll_pre="sudo -H -u $CONSOLE_USER $daemon_bin"
    else
        enroll_pre="$daemon_bin"
    fi
    if [ "$NO_ENROLL" = 1 ] || [ -z "$TOKEN" ]; then
        [ "$NO_ENROLL" = 1 ] || warn "no --token given — skipping enrollment"
        say "enroll later with: $enroll_pre enroll --server $SERVER --token <agent-enrollment-jwt> --name \"$NAME\""
        return 0
    fi
    say "enrolling this machine as '$NAME' against $SERVER (token is single-use, never echoed)"
    # Unquoted on purpose: $enroll_pre is a word list we built ourselves
    # (sudo + binary + --config + path), none of which contain whitespace.
    # shellcheck disable=SC2086
    $enroll_pre enroll --server "$SERVER" --token "$TOKEN" --name "$NAME"
}

# ─── tunnel role ────────────────────────────────────────────────────────────

install_tunnel_linux() {
    if command -v dpkg >/dev/null 2>&1; then
        deb="$STAGE/roomler-cli.deb"
        say "downloading the roomler CLI (.deb) via the proxy"
        download "$SERVER/api/tunnel/installer/linux-deb?version=latest" "$deb"
        if [ "$DOWNLOAD_ONLY" = 1 ]; then
            say "download-only: would run: sudo dpkg -i $deb"
            return 0
        fi
        sudo dpkg -i "$deb" || sudo apt-get -f install -y
    else
        tarball="$STAGE/roomler-cli.tar.gz"
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
    tarball="$STAGE/roomler-cli.tar.gz"
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
# RETIRED-NAME-ANCHOR-BEGIN
# The tunnel CLI is installed on hosts that predate the rename, so the archive
# may carry either name and an existing `roomler-tunnel` on PATH must keep
# working. The symlink below is that compatibility promise, not a leftover.
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
# RETIRED-NAME-ANCHOR-END
    [ -n "$cli" ] || cli=/usr/local/bin/roomler
    if [ "$NO_ENROLL" = 1 ] || [ -z "$TOKEN" ]; then
        [ "$NO_ENROLL" = 1 ] || warn "no --token given — skipping enrollment"
        say "enroll later with: $cli enroll --server $SERVER --token <tunnel-enrollment-jwt> --name \"$NAME\""
        return 0
    fi
    say "enrolling this tunnel client as '$NAME' against $SERVER (token is single-use, never echoed)"
    "$cli" enroll --server "$SERVER" --token "$TOKEN" --name "$NAME"
}

# FR-27 — the Linux desktop companion, from its OWN .deb.
#
# A SEPARATE package on purpose: the daemon .deb installs on headless nodes
# across the fleet, and folding webkit2gtk + GTK into it to ship a menu-bar app
# they cannot display would be a real regression for them.
#
# Installed by default when this box has a graphical session, because the
# companion IS the consent prompt surface — a device set to "Prompt on host"
# without one cannot ask anybody, and the operator only finds out when a
# session times out for no stated reason.
#
# Never fatal. A daemon with no companion is still a working daemon; it just
# has no on-screen way to answer a prompt, which this says out loud.
#
# Reads `arch` and `releases` from install_daemon_linux, which always runs
# first (the case dispatch below calls them in that order).
install_desktop_linux() {
    if [ "$DESKTOP" = 0 ]; then
        say "skipping the desktop companion (--no-desktop)"
        return 0
    fi
    if [ -z "$DESKTOP" ]; then
        # Probe, don't ask. A display or a login session is the honest signal
        # that someone could SEE a prompt on this box.
        if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ] &&
            ! { command -v loginctl >/dev/null 2>&1 &&
                loginctl list-sessions --no-legend 2>/dev/null | grep -q .; }; then
            say "no graphical session detected — skipping the desktop companion (--desktop forces it)"
            return 0
        fi
    fi
    if ! command -v dpkg >/dev/null 2>&1; then
        say "no dpkg here — the desktop companion ships as a .deb only; skipping"
        return 0
    fi

    desktop_pattern="roomler-desktop-.*${arch}-unknown-linux-gnu\\.deb"
    desktop_url="$(asset_field_for "$releases" "$desktop_pattern" browser_download_url)"
    if [ -z "$desktop_url" ]; then
        say "no roomler-desktop .deb for ${arch} in this release — skipping the companion"
        return 0
    fi
    desktop_digest="$(asset_field_for "$releases" "$desktop_pattern" digest)"
    desktop_pkg="$STAGE/roomler-desktop.deb"
    download "$desktop_url" "$desktop_pkg"
    verify_sha256 "$desktop_pkg" "$desktop_digest"

    if [ "$DOWNLOAD_ONLY" = 1 ]; then
        say "download-only: would run: sudo dpkg -i $desktop_pkg"
        return 0
    fi
    say "installing the roomler-desktop companion (.deb — sudo required)"
    # `apt-get -f install` pulls webkit2gtk / GTK / appindicator when dpkg
    # reports them missing, which on a fresh desktop it usually will.
    if sudo dpkg -i "$desktop_pkg" || sudo apt-get -f install -y; then
        say "desktop companion installed — starts at your next login, or now: roomler-desktop &"
    else
        warn "the desktop companion did not install; the daemon is unaffected."
        warn "  Without it this device has no on-screen consent prompt — use"
        warn "  'roomler consent --list' / '--approve', or set it to email/push consent."
    fi
}

# ─── main ───────────────────────────────────────────────────────────────────

say "roomler install.sh — role=$ROLE os=$OS server=$SERVER"

case "$ROLE/$OS" in
    # FR-27 — the companion AFTER the daemon: it depends on `roomlerd` (the
    # package literally Depends on it), and it reuses the release listing the
    # daemon step already fetched.
    daemon/Linux)  install_daemon_linux; install_desktop_linux ;;
    daemon/Darwin) install_daemon_macos ;;
    tunnel/Linux)  install_tunnel_linux ;;
    tunnel/Darwin) install_tunnel_macos ;;
esac

say "done."
