---
title: Your first remote session
description: Open a machine's screen, switch monitors, control quality, send keyboard shortcuts, and read the connection indicator that tells you how you are connected.
tags: [remote-desktop, sessions, getting-started, performance]
order: 1
---

Open **Devices**, pick a machine, click **View screen**. What follows is the
session surface and how to read it.

## The connection indicator

The single most useful thing on screen. It tells you **how** the two ends are
talking, which explains almost every performance question before you ask it.

| Reads | Meaning | Expect |
|---|---|---|
| **LAN** | Same local network | Best case — a few milliseconds |
| **Direct** | Straight across the internet | Very good; latency is the network's |
| **Relay** | No direct path; traffic is bounced through a relay | Higher latency, capped bandwidth |

:::tip Relay is not a verdict, it is a stage
A pair that lands on a relay keeps trying to upgrade to a direct path in the
background — it does not give up and stay there. If it *stays* relayed, that is
worth investigating: see [cannot connect](/docs/troubleshooting/cannot-connect/).
:::

## Multiple monitors

A machine with several displays exposes each one; switch between them from the
session toolbar. Each switch renegotiates the stream, so expect a brief blur as
the first full frame arrives.

## Quality and performance

The session adapts on its own: it measures what the path can carry and spends
its bits accordingly, dropping detail before it drops responsiveness.

You can steer it with a **priority** control:

| Priority | Trades | Good for |
|---|---|---|
| **Responsiveness** | Sharpness while things move | Dragging windows, scrolling, anything interactive |
| **Balanced** | The default | Almost everything |
| **Clarity** | Latency for detail | Reading small text, design work, still screens |

:::tip A still screen sharpens itself
When motion stops, the encoder spends its budget on detail rather than frame
rate, so a screen you stop touching gets crisper after a moment. That is
expected behaviour, not a glitch.
:::

## Keyboard and shortcuts

Keystrokes are injected on the far end, so the remote machine's own keyboard
layout applies. Two things regularly surprise people:

:::warning Some shortcuts are captured by *your* machine, not sent
`Ctrl+Alt+Del`, and on macOS most `Cmd` combinations, are intercepted by the
operating system you are sitting at, before the browser ever sees them. Use the
session toolbar's key-send control for those.
:::

Mac-to-Windows and Windows-to-Mac sessions remap the modifier keys so the
combination you press means what you expect on the far side.

## Ending a session

Close the tab or click **Disconnect**. The remote machine is not logged out and
nothing is left running.

Every session start, end and refusal is recorded — who connected, when, from
where. [Consent and audit](/docs/security/consent-and-audit/) covers what is
kept.

:::danger The pixel stream is never recorded
Session *metadata* is audited. The screen contents deliberately are not, and
there is no server-side recording of a remote session. Anything else would make
the server a data path, which is the one thing the design does not permit.
:::

## If the screen is black

Almost always a capture-permission problem on the target, not a network problem
— and the operating system usually reports no error at all. See [black
screen](/docs/troubleshooting/black-screen/) and [per-OS
permissions](/docs/remote-desktop/per-os-permissions/).
