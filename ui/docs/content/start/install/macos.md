---
title: Install on macOS
description: Install the Roomler agent on macOS, grant the two privacy permissions it needs, and add the root half if you want the machine on the private mesh.
tags: [install, macos, getting-started, enrollment, permissions]
order: 11
---

macOS is the one platform where the agent runs as **two processes**, and it is
worth understanding why before you install — otherwise the second half looks
like a bug rather than a requirement.

## Why there are two halves

:::badges
- **The user half** icon:monitor — runs as you, inside your GUI login session. Does screen capture, input injection and clipboard.
- **The root half** icon:network — runs as root from boot. Does the overlay mesh and tunnels.
:::

Neither can do the other's job. A root process in session 0 has **no
WindowServer**, so capture and event injection simply do not work there; and
creating a `utun` interface and installing routes **requires root**. They also
cannot share one enrollment, because a second control connection for the same
device displaces the first.

:::warning A fully-featured Mac appears as TWO devices
The root half is its own enrollment, so it gets its own row — named
`<name>-daemon`. That is expected. If you only want screen sharing, install the
user half alone and you get one device row.
:::

## Install

Mint a token under **Devices → Enroll device**.

**Screen sharing only** — one token, one device row:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --server https://roomler.ai --name "$(hostname)"
```

**Screen sharing *and* the private mesh** — mint a **second** token and pass it
as `--daemon-token`:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --daemon-token <second-token> \
  --server https://roomler.ai --name "$(hostname)"
```

**Or download the wizard:**

[Roomler Setup for macOS](https://roomler.ai/api/setup/macos)

`--daemon-token` is what installs the root LaunchDaemon *and* turns the overlay
on — installing that half is itself the opt-in. Without it the Mac never joins
the mesh, and it is **absent from `roomler peers` entirely** rather than showing
as offline.

Running the one-liner under `sudo` is fine: the script resolves the console user
itself and enrolls the user half as them.

## Grant the two permissions — nothing works until you do

:::danger macOS reports no error when a grant is missing
The screen streams as **wallpaper only**, and injected keys and clicks are
**silently dropped**. There is no error message anywhere. If your first session
shows a desktop with no windows, this is why.
:::

Open **System Settings → Privacy & Security** and enable **Roomler Daemon**
under both:

:::steps
1. **Screen & System Audio Recording** — without it, capture returns the desktop picture and nothing else.
2. **Accessibility** — without it, your keyboard and mouse input is dropped on the floor.
:::

You do not have to hunt for these. The **Roomler menu-bar app** names whichever
grant is missing and gives you a button per permission that opens the right
pane. The agent also probes both at startup, logs what is missing, and reports
it to the server — so the device list shows *"No screen access"* / *"No input
access"* rather than letting you discover it by connecting to a blank desktop.

Then restart the user half:

```bash
launchctl kickstart -k gui/$(id -u)/com.roomler.agent
```

:::warning Grants are bound to the signed binary
macOS attributes a privacy grant to a specific bundle identity. An update that
changes that identity voids both grants, and the toggles need re-enabling. The
installer tells you when this has happened rather than leaving you to find out.
:::

## What gets installed

| Path | What it is |
|---|---|
| `/Applications/Roomler.app` | The **menu-bar companion** — status, routes, and the permissions panel. No Dock tile. The only Roomler icon you should see. |
| `/Library/Roomler/roomlerd.app` | The agent itself. A background service with nothing to launch, so deliberately not in `/Applications` — but still a bundle, because the two privacy grants attach to a bundle identity. |
| `/usr/local/bin/roomler` | The CLI. |
| `/usr/local/bin/roomlerd` | The agent on `PATH`, for `roomlerd enroll` and `--version`. A symlink into the bundle, not a second copy. |

Configuration lives at `/etc/roomler/config.toml` for the root half.

## Talking to the right half

:::tip "daemon not running" usually means you asked the wrong half
The two halves listen on **different local sockets**. `roomler status` and
`roomler peers` reach the **user** half; `sudo roomler …` reaches the **root**
half. If a command reports that the daemon is not running, try it with and
without `sudo` before assuming anything is broken.
:::

## Known limits on macOS

Being explicit rather than letting you discover these:

- **Video encoding is software-only on Apple Silicon.** The hardware-encoder dispatch covers NVIDIA, Intel and AMD; VideoToolbox is not wired up, so H.264 uses a software encoder.
- **Remote audio capture** is not available.
- **Per-org network adapters** (one Mac in several organizations, each with its own adapter) are not available.

## Uninstall

```bash
launchctl bootout "gui/$(id -u)/com.roomler.agent"
launchctl bootout "gui/$(id -u)/com.roomler.desktop"
sudo launchctl bootout system/com.roomler.daemon    # if the root half is installed
sudo launchctl bootout system/com.roomler.update    # the update helper
rm -f ~/Library/LaunchAgents/com.roomler.{agent,desktop}.plist
sudo rm -f /Library/LaunchDaemons/com.roomler.{daemon,update}.plist
sudo rm -rf /Library/Roomler /Applications/Roomler.app \
            /usr/local/bin/roomler /usr/local/bin/roomlerd /etc/roomler

# The privacy grants are bound to the binary you just deleted. Clearing them
# means a fresh install prompts again, instead of showing a toggle that is ON
# but attached to something no longer there.
sudo tccutil reset ScreenCapture com.roomler.agent
sudo tccutil reset Accessibility com.roomler.agent
```

Then remove the device row — or **both** rows, if you installed the root half —
in the dashboard under **Devices**. That revokes the credentials and releases
the mesh address.

## Troubleshooting

- **Screen is the wallpaper and nothing else** → the Screen Recording grant. See above.
- **Clicks and keys do nothing** → the Accessibility grant.
- **The Mac is missing from `roomler peers`** → you installed the user half only. Add the root half with `--daemon-token`.
- **More** → [Troubleshooting](/docs/troubleshooting/).
