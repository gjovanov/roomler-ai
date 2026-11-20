---
title: Install on macOS
description: Install the Roomler agent on macOS as one device, with screen sharing and the private mesh, and grant the two privacy permissions it needs.
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

:::note Two processes, one device
The one-line install makes a Mac that is not enrolled yet **one device row**:
the root half holds the enrollment and starts the user half in your login
session itself, so the user half needs none of its own. A Mac enrolled before
keeps its layout. Two rows, `<name>` and `<name>-daemon`, come from
`--daemon-token` and, for now, from the graphical installer: there each half is
its own enrollment, and the second row is expected.
:::

## Install

Mint a token under **Devices → Enroll device**.

**Screen sharing, input *and* the private mesh, as one device** — one token:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --server https://roomler.ai --name "$(hostname)"
```

**Screen sharing alone**, with no root service and no mesh: add `--per-user`.

**Two device rows**, the user half and the root half each its own — mint a
**second** token and pass it as `--daemon-token`:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --daemon-token <second-token> \
  --server https://roomler.ai --name "$(hostname)"
```

:::note Or use the graphical installer, with no token to copy
[Roomler Setup](https://roomler.ai/api/setup/macos) installs both halves behind
one administrator password, and for now the Mac joins as two devices, as with
`--daemon-token`. Safari
saves the download as a `.tar`: open it, open the folder it makes, then open
**Roomler Setup** and confirm macOS's question about an app from the internet.
**Be accessed — this Mac** is already selected; untick **Join the overlay network**
under Advanced options for screen sharing alone. Click **Set up in the browser**,
then at [roomler.ai/pair](https://roomler.ai/pair) sign in or create an account,
type the eight-letter code the wizard shows, check the device the page shows and
click **Approve**. Go back to the wizard, which may be behind your browser: it
names who approved the device and into which organization. Click **Install** and
answer the administrator prompt. A code lasts 15 minutes; when it runs out,
**Start again** gives you a new one.
:::

Installing the root LaunchDaemon is what turns the overlay on: the one-line
install and `--daemon-token` both do. With `--per-user` the Mac never joins the
mesh, and it is **absent from `roomler peers` entirely** rather than showing as
offline.

Running the one-liner under `sudo` is fine: the script resolves the console user
itself, and enrolls a user half as them.

## Grant the two permissions — nothing works until you do

:::danger macOS reports no error when a grant is missing
The screen streams as **wallpaper only**, and injected keys and clicks are
**silently dropped**. There is no error message anywhere. If your first session
shows a desktop with no windows, this is why.
:::

Open **System Settings → Privacy & Security** and enable **roomlerd**
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

Then restart the agent; the installer prints the line for your Mac. On one
device:

```bash
sudo launchctl kickstart -k system/com.roomler.daemon
```

With a user half of its own (`--per-user`, `--daemon-token`, the graphical
installer):

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
without `sudo` before assuming anything is broken. On one device, `roomler …`
without `sudo` is answered by the screen worker the root half starts for you,
and its note says which of your settings it changes and which need `sudo`.
:::

## Known limits on macOS

Being explicit rather than letting you discover these:

- **No AV1.** H.264 and HEVC encode in hardware, through VideoToolbox, but Apple ships no AV1 encoder, so a Mac does not offer AV1.
- **Remote audio capture** is not available.
- **Per-org network adapters** (one Mac in several organizations, each with its own adapter) are built but not yet verified on a Mac.

## Uninstall

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- --uninstall
```

It stops both halves through launchd **before** removing anything: the
per-user agent and the menu-bar app (for every account that has them), then the
root half and the update helper. That stop is the daemon's clean stop, so the
routes and DNS settings it added to the Mac go with it. Then it removes
`/Library/Roomler`, `/Applications/Roomler.app`, the CLI, the package receipt
and the config, which holds the credentials. It also resets the Screen Recording
and Accessibility grants, so a fresh install asks for them again rather than
showing a toggle that is on for a binary that is gone.

It prints `done.` only if every step worked; otherwise it lists what it could
not remove and exits non-zero.

| Flag | Effect |
|---|---|
| `--keep-config` | Keep the config and the two grants: a reinstall comes back as the same device |

Then remove the device row in the dashboard under **Devices** — or **both**
rows on a Mac installed as two. That revokes the credentials and releases the
mesh address.

## Troubleshooting

- **Screen is the wallpaper and nothing else** → the Screen Recording grant. See above.
- **Clicks and keys do nothing** → the Accessibility grant.
- **The Mac is missing from `roomler peers`** → it has the user half only (`--per-user`, or installed that way before one device became the default). For one device, run the install again with `--one-row` and a new token, then remove the old row under **Devices**. Or add the root half as a device of its own: `--daemon-token`, or enroll it (`sudo /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd --config /etc/roomler/config.toml enroll … --overlay`) and run `sudo roomlerd service install --as-service`.
- **More** → [Troubleshooting](/docs/troubleshooting/).
