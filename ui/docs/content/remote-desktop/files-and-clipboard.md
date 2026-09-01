---
title: Files and clipboard
description: Copy text between your machine and a remote one, and transfer files in both directions over the same encrypted peer-to-peer session.
tags: [remote-desktop, files, clipboard, sessions]
order: 5
---

A remote session carries more than pixels. Clipboard contents and file transfers
travel on their own channels **inside the same encrypted peer-to-peer
connection** — they do not take a detour through the server.

## Clipboard

Copy on one side, paste on the other. Text is synchronised in both directions
while the session is open.

:::warning The clipboard is shared while the session lasts
Anything you copy on either machine becomes available to the other. Be aware of
it before copying a password on the machine you are controlling *from*.
:::

Clipboard sync can be turned off per session if you would rather it did not
follow you.

## File transfer

Drag a file onto the session window to send it to the remote machine, or use the
session toolbar's transfer panel to pull one back.

:::badges
- **Resumable** icon:download — a transfer that is interrupted picks up where it stopped rather than starting again.
- **Direct** icon:network — bytes ride the same peer-to-peer channel as the video, so the server never holds the file.
- **Both directions** icon:copy — send and retrieve within one session.
:::

Progress is shown per transfer, and a transfer continues while you keep working
in the session.

## What the far end sees

Files land in the remote machine's download location for the account the agent
runs as.

:::tip The agent's identity is what matters, not yours
On a machine where the agent runs as a system service, transferred files arrive
owned by that service account rather than by a logged-in user. If a file appears
to be "missing", it is usually in the service account's location rather than the
desktop user's.
:::

## When you want a file path rather than a session

For scripted or repeated movement of files, a session is the wrong tool. Two
better options:

:::cards
- **[SFTP over Roomler SSH](/docs/network/ssh/)** icon:terminal — `scp` and `sftp` work against a node with no `sshd` running. Transfers run as the session's own account, not as the daemon.
- **[Tunnels](/docs/network/tunnels/)** icon:network — forward a file service's port and use whatever client you already have.
:::

## Limits worth knowing

- **Clipboard sync is text.** Copying an image or a file object through the clipboard is not the transfer path — drag the file instead.
- **On macOS**, remote *audio* capture is unavailable, which occasionally surprises people expecting a full session mirror.
- **SFTP against a Windows target** works when the SSH session runs as the daemon's own account; running it as the logged-in console user is not yet supported there.
