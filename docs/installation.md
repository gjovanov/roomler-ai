# Installation

Every way to put the Roomler node stack on a machine, on every supported platform.
"Daemon" means `roomlerd` — the full node: remote-desktop target, tunnel exit, and
overlay-mesh member. "Tunnel client" means just the `roomler` CLI for opening
forwards/SOCKS5 from the machine you sit at. *Re-checked against master on
2026-09-02 (0.4.47).*

## Which installer do I want?

```mermaid
flowchart TB
    START["I want to install…"] --> ROLE{"role?"}
    ROLE -->|"reach THIS machine remotely<br/>(desktop · tunnel exit · mesh)"| DAEMON["daemon (roomlerd)"]
    ROLE -->|"open tunnels FROM here only"| TC["tunnel client (roomler CLI)"]

    DAEMON --> OS1{"OS?"}
    OS1 -->|Windows| WGUI{"GUI or terminal?"}
    WGUI -->|GUI| WIZ["roomler-setup wizard<br/>role picker → MSI"]
    WGUI -->|terminal| PS1["irm roomler.ai/api/setup/install.ps1 | iex"]
    OS1 -->|Linux| SH["curl roomler.ai/api/setup/install.sh | sh<br/>--role daemon (.deb x86_64/arm64)"]
    OS1 -->|macOS| PKG["install.sh --role daemon (.pkg, arm64)<br/>or the wizard"]

    TC --> OS2{"OS?"}
    OS2 -->|Windows| TCW["wizard (tunnel-client role)<br/>or the zip"]
    OS2 -->|Linux/macOS| TCU["install.sh --role tunnel<br/>(tarball / .deb)"]
```

Everything needs an **enrollment token** — minted in the admin UI
(**Admin → Agents → Enroll** for daemons, **Tunnel clients → Enroll** for CLIs),
valid 10 minutes, single-use.

## Enrollment — what every path does

```mermaid
sequenceDiagram
    participant I as Installer (wizard / script / MSI)
    participant S as roomler.ai
    participant D as roomlerd

    I->>S: GET /api/agent/latest-release (or /api/setup, /api/tunnel)
    S-->>I: manifest (version, per-platform assets, sha256)
    I->>S: GET /api/agent/installer/{flavour}
    Note over I,S: artifact streams through roomler.ai —<br/>corporate AV allow-lists trust the origin
    I->>I: sha256 verify → install → register autostart
    I->>S: POST /api/agent/enroll {enrollment_token, machine_id, …}
    S-->>I: {agent_id, tenant_id, agent_token} → written to config.toml
    D->>S: /ws?role=agent — the machine appears in Devices
```

`machine_id` is a stable hardware hash and `(tenant_id, machine_id)` is unique —
re-running an installer on a known machine reuses its identity instead of
duplicating it.

## Windows

**Recommended: the `roomler-setup` wizard** (signed EXE, downloadable from the
admin UI or `https://roomler.ai/api/setup/windows-x86_64`). One wizard, four
roles:

| Wizard role | What it installs | Runs as |
|---|---|---|
| **Daemon — system** | perMachine MSI + SystemContext enabled | SCM service `Roomler` (LocalSystem) — controls lock screen, UAC, pre-logon |
| **Daemon — per user** | perUser MSI | Scheduled Task `Roomler` at logon (limited) |
| **Daemon — machine (attended)** | perMachine MSI | SCM service, no SystemContext |
| **Tunnel client** | CLI archive + PATH entry | on demand |

Details worth knowing:

- **Two MSI flavours** exist (`peruser`, `permachine`) — the wizard maps roles
  onto them and flips SystemContext separately (`roomlerd enable-system-context`).
- Daemon installs also place **`roomler-desktop`** (tray companion: status,
  tunnels pane, consent prompts) and **`roomler.exe`** — a small shim that
  re-execs `roomlerd cli`, so CLI and daemon can never version-skew.
- Terminal alternative: `irm https://roomler.ai/api/setup/install.ps1 | iex`
  (prompts for role/token; flags mirror the sh script).
- Binaries are Authenticode-signed; the MSI's payload EXEs are signed before
  packaging.

## Linux

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <enrollment-jwt> [--server https://roomler.ai] [--name lab-1]
```

- Installs the `.deb` (x86_64 **and** arm64) or tarball, verifies SHA-256,
  enrolls, and enables a **systemd user unit** (`roomler.service` →
  `systemctl --user enable --now`).
- `--role tunnel` installs just the CLI (tarball or `.deb`).
- Useful flags: `--download-only`, `--no-enroll`.
- Headless servers: the daemon's virtual-desktop mode gives the machine a
  display, so "Connect" drops you into a live console.
- Design notes for the tarball/self-update path: [linux-self-update.md](linux-self-update.md).

## macOS

Same `install.sh` one-liner, but macOS is the one platform that needs **two
processes**, and it is worth knowing why before you install:

<!-- RETIRED-NAME-ANCHOR(1): /etc/roomler-agent is now the LEGACY macOS daemon
     config dir, named only because the .pkg postinstall still reads it to
     MIGRATE off it (`migrate_legacy_dir /etc/roomler-agent /etc/roomler`,
     and it still honours a legacy `enable-daemon` marker there), so the
     path remains an input to code on user machines. FR-21 D5.
     ⚠️ This anchor's previous reason was FALSE and had been for some time:
     it claimed com.roomler.daemon.plist passes /etc/roomler-agent as its
     config. It does not — it passes /etc/roomler/config.toml. An anchor's
     stated reason is a CLAIM and ages like any other; re-read the code it
     names before trusting it (FR-46). -->
| Half | Runs as | Does | Why it cannot do the other's job |
|------|---------|------|----------------------------------|
| LaunchAgent `com.roomler.agent` | you, inside your GUI login session | screen capture, input, clipboard | a root LaunchDaemon in session 0 has no WindowServer — capture and `CGEvent` injection do not work there |
| LaunchDaemon `com.roomler.daemon` | root, from boot | overlay mesh, tunnels | creating a `utun` and installing routes require root |
| LaunchDaemon `com.roomler.update` | root, on a 6 h timer + on demand | self-update: check, verify, `installer -pkg … -target /` | `installer -target /` needs root, and neither agent half should ever exec its own replacement (the exit-to-update dance is what used to knock Macs offline). Installed by DEFAULT; opt out with `sudo touch /etc/roomler/disable-auto-update` + re-run the installer |

They cannot share one enrollment: the hub keys sessions on `agent_id`, so a
second control-WS connection displaces the first. **Each half is its own
enrollment, so a Mac appears as two devices** — the second named
`<name>-daemon`.

```bash
# Screen sharing only (one token, one device row):
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --server https://roomler.ai --name "$(hostname)"

# Screen sharing AND the overlay mesh — mint a SECOND enrollment token:
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --daemon-token <second-token> \
  --server https://roomler.ai --name "$(hostname)"
```

`--daemon-token` also turns the overlay on (`enroll --overlay`) — installing a
root daemon is itself the opt-in, and the overlay is the only reason that half
exists. Without it the Mac never sends `rc:overlay.join`, so it is **absent
from `roomler peers` entirely** rather than showing as offline.

Running the one-liner under `sudo` is fine: the script resolves the console
user itself and enrolls the per-user half as them.

### Grant the two permissions — nothing works until you do

macOS gates capture and input, and **it never reports an error when a grant is
missing**: the screen streams as wallpaper only, and injected keys and clicks
are silently dropped.

<!-- RETIRED-NAME-ANCHOR-BEGIN
     The whole macOS layout is FROZEN by FR-21 D5. The .app bundle name and
     path key the Screen Recording and Accessibility TCC grants; renaming
     either silently voids them, and the failure is a black screen with no
     error. The published .deb asset name is matched by the updater.
     Everything below names something a user can see today.
     ⚠️ This preamble also used to claim /etc/roomler-agent is what the
     LaunchDaemon plist passes as its config. It is not — the plist passes
     /etc/roomler/config.toml, and the old dir survives only as a migration
     source in the .pkg postinstall. Corrected 2026-09-02. -->
- System Settings → Privacy & Security → **Screen & System Audio Recording** → enable **Roomler Daemon**
- System Settings → Privacy & Security → **Accessibility** → enable **Roomler Daemon**

If a stale **Roomler Agent** entry is still listed from a pre-rename install,
remove it — macOS does not carry a grant across a bundle rename, and the `.pkg`
postinstall prints the same instruction when it detects one.

You do not have to hunt for this: the **Roomler menu-bar app** names whichever
grant is missing and gives you a button per permission that opens the right
pane. The agent also probes both at startup, says what is missing in its log,
and reports the state to the server — so the device list shows "No screen
access" / "No input access" instead of letting you discover it by connecting to
a black screen.

Then restart it: `launchctl kickstart -k gui/$(id -u)/com.roomler.agent`

⚠️ Grants are keyed to the binary's code signature. Until Apple notarisation is
finished the `.pkg` is unsigned, so **an agent update invalidates them** and
both toggles need re-enabling.

### What the package installs

| | |
|---|---|
| `/Applications/Roomler.app` | the **menu-bar companion** — status, routes, and the permissions panel. `LSUIElement`, so no Dock tile. The only Roomler icon you should see |
| `/Library/Roomler/roomlerd.app` | the daemon. A background service with nothing to launch, so it is deliberately NOT in `/Applications`; still a bundle, because TCC attributes the two permissions to a bundle identity |
| `/usr/local/bin/roomler` | the CLI (a small shim onto the daemon's own command surface) |
| `/usr/local/bin/roomlerd` | the daemon on PATH, for `roomlerd enroll` / `--version` — a symlink into the bundle, not a second copy |

Same three commands as Windows and Linux.

`roomler peers` / `status` reach the **per-user** half; `sudo roomler …` reaches
the **root** half, because the two listen on different LocalAPI sockets
(`$TMPDIR/roomler/roomler.sock` vs `/var/run/roomler/roomler.sock`). If a
command reports "daemon not running", you are probably asking the wrong half.

### Paths, logs, limits

| | Path |
|---|---|
| App bundle | `/Library/Roomler/roomlerd.app` (executable `roomlerd`). ⚠️ Renaming the bundle voids the TCC grants — FR-46 P5b did rename it, which is why the `.pkg` postinstall tells you to re-approve **Roomler Daemon** and delete a stale **Roomler Agent** entry |
| Per-user config | `~/Library/Application Support/live.roomler.roomler/config.toml` |
| Root config | `/etc/roomler/config.toml` |
| Per-user log | `/tmp/roomlerd.err.log` |
| Root log | `/var/log/roomler/daemon.log` |

Video is software-only on Apple Silicon (openh264 for H.264, libvpx for
VP9-4:4:4): the encoder dispatch tables contain only NVIDIA/Intel/AMD names, so
VideoToolbox is not wired up. Remote-audio capture and multi-org per-org
adapters are also unavailable on macOS today.

### Uninstall

```bash
launchctl bootout "gui/$(id -u)/com.roomler.agent"
launchctl bootout "gui/$(id -u)/com.roomler.desktop"
sudo launchctl bootout system/com.roomler.daemon          # if the root half is installed
sudo launchctl bootout system/com.roomler.update          # the update helper (installed by default)
rm -f ~/Library/LaunchAgents/com.roomler.{agent,desktop}.plist
sudo rm -f /Library/LaunchDaemons/com.roomler.{daemon,update}.plist
sudo rm -rf /Library/Roomler /Applications/Roomler.app /Applications/roomler-agent.app \
            /usr/local/bin/roomler /usr/local/bin/roomlerd \
            /etc/roomler /etc/roomler-agent
# ⚠️ /etc/roomler is the CURRENT root config dir and holds the agent token.
# This line used to remove only the legacy /etc/roomler-agent, so an uninstall
# left a live credential on disk.
# `sudo`, and both trees: an install done under sudo before rc.454 wrote the
# config into your home owned by root, so a plain rm cannot remove it.
# `live.roomler.roomler-agent` is the pre-rename path and may also be present.
sudo rm -rf ~/Library/Application\ Support/live.roomler.roomler \
            ~/Library/Application\ Support/live.roomler.roomler-agent

# The two privacy grants are bound to the binary you just deleted. Clearing
# them means the fresh install prompts again instead of showing a toggle that
# is ON but attached to something no longer there.
sudo tccutil reset ScreenCapture com.roomler.agent
sudo tccutil reset Accessibility com.roomler.agent
```

Remove the device row(s) in the admin UI too — deletion there releases the
overlay lease.

## Keeping it updated

| Component | Mechanism |
|---|---|
| `roomlerd` (daemon hosts) | Self-updater polls `/api/agent/latest-release` (24 h timer + startup cooldown), verifies SHA-256, hands off to MSI / `dpkg` / `installer` and restarts. Admins can force it fleet-wide or per device (`POST …/agent/update`). Crash-looping updates roll back to the last known-good version |
| `roomler` CLI (tunnel-only hosts) | `roomler self-update` (same proxy origin). On daemon hosts the MSI/.deb owns the binaries — the shim's `self-update` refuses by design |
| `roomler-desktop` / wizard | Refreshed by the daemon's MSI on Windows; wizard is fetch-latest by nature |

## Verifying what you install

Every release asset ships with a `.sha256` sidecar, a detached **GPG signature**
(`.asc`, key published as `roomler-release-pubkey.asc`), and **SLSA build
provenance**:

```bash
gh attestation verify roomlerd-<v>-x86_64-unknown-linux-gnu.deb --repo gjovanov/roomler-ai
```
<!-- RETIRED-NAME-ANCHOR-END -->

Windows binaries are Authenticode-signed; macOS artifacts are Developer-ID signed
and notarized where the format allows. [code-signing.md](code-signing.md) covers
the whole chain — who the publisher is, how CI signs without holding any key
material, and how to verify an artifact by hand. The installer proxies exist
precisely so corporate networks can allow-list `roomler.ai` instead of
`github.com`.

## Uninstall / cleanup

- **Windows**: uninstall the MSI (per-user or per-machine) from Apps; the agent's
  version sweep also removes older same-flavour MSIs after upgrades.
  `roomlerd service uninstall` removes a manually-installed service/task.
- **Linux**: `apt remove roomlerd` (or delete the tarball install) and
  `systemctl --user disable --now roomler.service`.
- **macOS**: see [Uninstall](#uninstall) above — there may be TWO halves to
  remove (the per-user LaunchAgent and, if installed, the root LaunchDaemon).
- Server side, delete the device (**Devices → remove**): this revokes its
  credential and releases its overlay address back to the pool.

The agent's `config.toml` (server URL, tokens, per-node settings like
`encoder_preference`, `overlay_*`, `tunnel_routes`) lives under
`%APPDATA%\roomler\roomler\` / `%PROGRAMDATA%\roomler\roomler\`
(machine-global) / `/etc/roomler/` (root) or `~/.config/roomler/` — remove it to fully forget an
enrollment.
