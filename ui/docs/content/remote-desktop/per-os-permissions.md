---
title: Per-OS permissions
description: What Windows, macOS and Linux each require before screen capture and input injection work — and why a missing grant usually produces silence, not an error.
tags: [remote-desktop, permissions, windows, macos, linux, troubleshooting]
order: 6
---

Every operating system gates screen capture and synthetic input, and **none of
them reports a useful error when the grant is missing**. The symptom is a black
screen, a wallpaper-only desktop, or input that does nothing — never a message
saying what is wrong.

This page is the list of what each platform wants.

:::os
@windows
Windows needs no explicit user grant for capture or input. What it *does* gate
is **which desktop** the agent can see.

| Service mode | Can capture | Cannot capture |
|---|---|---|
| **Machine — system** | Everything, including the lock screen, UAC prompts and the pre-logon screen | — |
| **Machine — attended** | The logged-in desktop | The secure desktop (lock screen, UAC) |
| **Per user** | Your own session | Anything before you log in |

:::warning The classic Windows symptom
The session works fine until the remote machine shows a UAC prompt or locks —
and then the screen freezes on the last frame. That is the secure desktop, and
it means the agent is not running in system mode. Re-install with
`-Role daemon-system`.
:::

There is a second, rarer Windows case: a screen-capture handle bound during the
transition between locked and unlocked can end up returning empty frames
indefinitely. The agent detects a stream that has delivered nothing and rebuilds
it; if you ever see a permanently black screen that a reconnect fixes, that was
it.

@macos
macOS is the strictest of the three, and the least communicative. **Two grants
are required**, both under *System Settings → Privacy & Security*, both for
**Roomler Daemon**:

| Grant | Without it |
|---|---|
| **Screen & System Audio Recording** | The stream shows the desktop wallpaper and nothing else — no windows, no menu bar |
| **Accessibility** | Keys and clicks are silently dropped |

:::danger macOS never reports these as errors
There is no dialog, no log entry from the OS, and no failure — just a desktop
with nothing on it, or a cursor that will not click. If your first session looks
like an empty wallpaper, this is why, essentially every time.
:::

A third grant, **Microphone**, matters only for a screen recording that
includes the microphone (from agent 0.4.163). macOS asks the first time one
does. Unlike the two above, it does not fail silently: if it is turned off,
that recording is refused and names the setting. Where the **root half** runs
the screen (`macos_supervise_gui_worker`), macOS asks the same way from agent
0.4.168. Agents 0.4.166 and 0.4.167 refuse the microphone by name there,
because macOS never asked, rather than record silence.

You do not have to hunt: the **menu-bar app** names whichever grant is missing
and opens the right pane. The agent also probes both at startup and reports the
state to the server, so the device list shows *"No screen access"* / *"No input
access"* rather than letting you find out by connecting.

After granting, restart the user half:

```bash
launchctl kickstart -k gui/$(id -u)/com.roomler.agent
```

:::warning A grant is bound to the signed binary
macOS attaches the permission to a specific bundle identity. An update that
changes that identity voids both grants and the toggles need re-enabling — the
installer says so when it happens rather than leaving you to discover it.
:::

@linux
Linux depends on the display server, and the agent detects which one at session
time rather than caching an answer at startup.

| Session | Capture | Input |
|---|---|---|
| **X11** | Direct capture | Direct injection |
| **Wayland, GNOME, per-user install** | GNOME's own screen sharing, automatic; nobody at the screen is asked | Automatic |
| **Wayland, KDE Plasma, per-user install** | The desktop portal, automatic: the person at the screen is asked once, then remembered | Through the portal's remote control |
| **Wayland, GNOME or KDE, system install** | Automatic for the person's session on screen, read only while it is on screen and unlocked | Automatic, under the same conditions |
| **Wayland, wlroots compositors (sway and others)** | Through the desktop portal; less tested | Not available: the wlroots portal has no remote control |
| **Headless** | Virtual display mode | Supported |

A **system install** (a root service) switches its Wayland capture on by
itself only for a person's own session, and only where that person can be told:
the desktop companion shows a banner saying who is viewing, with Disconnect, or
a desktop notification says it. While that session is locked or not on screen,
or when only the login screen is showing, nothing is read and the viewer says
why. Where a Wayland desktop cannot be captured at all, the viewer names what is
missing instead of showing a black screen.

Two things regularly cause trouble:

:::warning The portal must actually be running
On KDE Plasma and other desktops that capture through the desktop portal, the
portal is started on demand, and if nothing has ever triggered it, the agent
finds no capture source at all. Starting it is not always enough on its own:
the portal front-end caches its backend choice at startup, so it may need
restarting after the backend becomes available.

The practical symptom: a machine with every package installed that still reports
no capture source.
:::

:::warning Keyboard input needs a detected layout
Synthetic key events are mapped through the active keyboard layout. A key that
is not on the detected layout **types nothing and reports success**, which looks
like input working for some characters and not others.
:::

A machine with no session on screen at all, such as a headless server, has
nothing to capture: enable virtual-display mode there.
:::

## Quick diagnosis

| Symptom | Almost always |
|---|---|
| Wallpaper only, no windows | macOS Screen Recording grant |
| Screen fine, clicks and keys do nothing | macOS Accessibility grant, or a Linux layout problem |
| Freezes at the lock screen or a UAC prompt | Windows not in system service mode |
| Completely black, fixed by reconnecting | A capture handle that came up empty; the agent rebuilds it |
| Nothing to capture on a Linux box | No logged-in session, or the portal is not running |

More in [black screen](/docs/troubleshooting/black-screen/).
