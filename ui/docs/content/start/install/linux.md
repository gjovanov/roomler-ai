---
title: Install on Linux
description: Install the Roomler agent on Linux with one command — as a root systemd service or a per-user unit — including headless servers, containers and cluster nodes.
tags: [install, linux, getting-started, enrollment, headless]
order: 12
---

Linux is the most flexible of the three platforms and the one most often
installed on a machine with **no desktop session at all**. Both cases work; they
just want different flags.

## Pick a service mode first

| Mode | Flag | Runs as | Use it for |
|---|---|---|---|
| **System** | `--system` | root, systemd **system** unit | Servers, headless boxes, cluster nodes, containers |
| **Per user** | *(default)* | you, systemd **user** unit | Desktops where you want to share the session you are actually logged into |

:::tip Desktops want the per-user unit
Screen capture needs a logged-in session. On a desktop, the default per-user
unit is the right answer — a root system service has no session to capture
unless you also enable the headless display mode below.
:::

## Install

Mint a token under **Devices → Enroll device**, then:

**Headless server / cluster node** — root systemd service:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --system --token <token> --server https://roomler.ai --name "$(hostname)"
```

**Desktop** — per-user unit:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role daemon --token <token> --server https://roomler.ai --name "$(hostname)"
```

**Tunnels only** — just the CLI, no agent:

```bash
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
  --role tunnel --token <token> --server https://roomler.ai --name "My laptop"
```

The script installs the `.deb` on Debian and Ubuntu derivatives — **x86_64 and
arm64** — or a tarball elsewhere, verifies the SHA-256, enrolls the machine, and
enables the unit.

Useful flags:

| Flag | Effect |
|---|---|
| `--download-only` | Fetch and verify the artifact, install nothing |
| `--no-enroll` | Install and set up the service, but skip enrollment (do it later) |
| `--server <url>` | Point at a self-hosted server instead of `roomler.ai` |

## Check it came up

```bash
roomler status     # per-user install
sudo roomler status # system install
```

:::warning `systemctl is-active` can lie on a system install
A machine that was enrolled before the current config convention can end up
running a perfectly healthy daemon that systemd does not own. `systemctl
is-active` then reads **inactive** while the device is online and answering.
Check `pgrep -x roomlerd` and `roomler peers` before concluding anything is
wrong — and in particular, do not "restart to fix" on that basis.
:::

## Headless machines

A server with no display can still be reached as a **desktop**: the agent's
virtual-display mode gives the machine a screen, so *View screen* drops you into
a live console rather than failing. Enable it in the device's configuration
rather than by an environment variable, so the setting survives a reboot and
applies to a daemon started outside systemd.

Wayland and X11 desktops are both supported for capture and input, with
different backends underneath. Which one applies is detected at session time —
see [per-OS permissions](/docs/remote-desktop/per-os-permissions/).

## Where things live

| Item | Path |
|---|---|
| System config | `/etc/roomler/config.toml` |
| Per-user config | `~/.config/roomler/config.toml` |
| Binaries | `/usr/bin/roomlerd`, `/usr/bin/roomler` |
| Logs | `journalctl -u roomler` (system) or `journalctl --user -u roomler` (per user) |

:::danger The config file holds a credential
`config.toml` contains the agent token, and — if you enable SSH — the SSH host
private key. It is written `0600`. Treat it as a secret if you copy it anywhere.
:::

## Verify what you installed

Every release asset ships a SHA-256 sidecar, a **detached GPG signature**
(`.asc`, against a published release key) and **SLSA build provenance**:

```bash
gh attestation verify roomlerd-<version>-x86_64-unknown-linux-gnu.deb \
  --repo gjovanov/roomler-ai
```

The agent's own updater verifies the GPG signature against a key **pinned inside
the binary** before it will install anything, and refuses if the signature is
absent or does not match.

## Keeping it updated

The agent self-updates on a timer, verifying the signature before handing off to
`dpkg`. Admins can also push a specific version per device or fleet-wide.

:::warning `dpkg -i` alone does not restart the daemon
Installing a package by hand replaces the file on disk while the running process
keeps executing the **deleted inode** — so `roomler --version` reports the new
version while the daemon is still the old one. Restart the unit explicitly after
a manual install.
:::

## Uninstall

```bash
# per-user install
systemctl --user disable --now roomler.service

# system install
sudo systemctl disable --now roomler.service

sudo apt remove roomlerd     # or delete the tarball install
```

Then remove the device in the dashboard under **Devices** — that revokes its
credential and releases its mesh address.

## Troubleshooting

- **The device never appears.** The token is single-use; mint a fresh one.
- **Installed but shows offline.** Check the unit is actually enabled and that the machine has outbound HTTPS. See [device offline or stale](/docs/troubleshooting/device-offline/).
- **Screen is black on a desktop.** The per-user unit needs a logged-in session; a root system service does not have one. See [black screen](/docs/troubleshooting/black-screen/).
- **More** → [Troubleshooting](/docs/troubleshooting/).
