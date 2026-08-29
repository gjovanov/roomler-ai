# FR-36 — Wayland capture: the desktops we cannot see at all

**Issue:** [#929](https://github.com/gjovanov/roomler-ai/issues/929) · **Status:** design · **Owner:** agent / capture

## Goal

Capture a **Wayland** desktop. Today the agent cannot — not slowly, not
partially: it produces a black or wallpaper-only stream. That is a growing
share of Linux hosts, and on Apple Silicon it is *every* accelerated desktop.

Secondary, and the reason this arrives now: it is the only remaining candidate
for lifting the ~27 fps ceiling FR-29 proved is unreachable on X11.

## Why now — FR-29 closed the other door

[FR-29](FR-29-x11-damage-capture.md) (#864) measured the X11 path to its floor:

- Capture is ~20 ms/frame at 1080p and **indifferent to how the framebuffer was
  drawn** — identical on Xvfb, on real KMS, and on a fully GPU-accelerated Xorg.
- It is the larger half of the frame budget next to a 14–18 ms encode, which is
  what pins the host at ~25–27 fps.
- P1 made an idle desktop nearly free (CPU 45.8 % → 2.8 % of a core), but under
  motion nothing moved.
- **P3 (partial readback) was ruled out by measurement**: a window drag reports
  **one damage rectangle covering the whole screen** (`avg_damage_rects=1`,
  union = bbox = 1000 ‰), with compositing on *and* off. There is no smaller
  region to read.

So the ceiling is not "X11 capture is unoptimised". It is *this* readback path.
A different source of pixels is the only lever left — and it is the same lever
that unlocks the desktops we currently cannot capture at all.

## The blocking problem is NOT the pixels

PipeWire delivers frames; that part is well-trodden. The hard part is **who is
allowed to ask, and from where** — and it breaks two assumptions the current
capture path is built on.

1. **The portal is a per-user session service.** `xdg-desktop-portal` lives on
   the user's D-Bus *session* bus. The agent runs as **root** (`User=root` in
   the unit) and reaches X today purely by reading a cookie
   (`XAUTHORITY=/run/lightdm/m1/xauthority`). There is no equivalent trick for
   the portal: a capturer must live *inside* the user's session, or be brokered
   by something that does.
2. **The grant is interactive by default.** The ScreenCast portal shows a
   picker. An unattended remote-desktop host has nobody to click it. The portal
   answer is `persist_mode` + a **restore token**, which makes the *first* grant
   interactive and later ones silent — which is a policy story, not just an API
   call, and it interacts with FR-27's consent work.

The repo already has a process that lives in the user session — the
`roomler-desktop` companion — so the shape of the answer exists; the decision
of what runs where does not.

## Design

The seam is clean and already used by four backends: `ScreenCapture`
(`capture/mod.rs:266`), chosen in `open_default` (`capture/mod.rs:413`), with
backends declared at `capture/mod.rs:16` (scrap), `:22` (x11_damage), `:25`
(wgc), `:34` (synthetic). A Wayland backend is a fifth — no pump changes.

| Phase | What | Kill switch |
|---|---|---|
| **P0** | **Decide the process model** — capturer in the session companion vs a session-scoped helper the daemon spawns. Blocking; everything else depends on it. | n/a (design) |
| **P1** | Portal `ScreenCast` → PipeWire stream → `Frame`, feature-gated, X11 remains the default. Interactive grant. | `ROOMLERD_WAYLAND_CAPTURE=0`; absent/failed ⇒ today's path |
| **P2** | Unattended: `persist_mode` + restore-token storage, so a rebooted host serves without a human. | same flag; no token ⇒ P1 behaviour |
| **P3** | DMA-BUF / zero-copy + PipeWire's own damage metadata — the actual fps lever. | same flag; falls back to the memcpy path |

### What this must not become

⚠️ **Not a second capture stack that only works on the author's desktop.** The
fleet spans GNOME, KDE and headless X11; the X11 path must stay the default and
stay correct until a Wayland host is *field-proven*, and the flag must restore
it exactly.

⚠️ **Not a silent screen-recording grant.** Making capture unattended is
precisely the property a consent design exists to control. This must land
*with* FR-27's consent surfaces, not around them.

## Acceptance criteria

- [ ] A GNOME **Wayland** session on `scw-m2-asahi` streams real pixels —
      currently it cannot be captured at all.
- [ ] `avg_capture_ms` **< 10 ms** at 1080p (X11 floor is ~20 ms; if Wayland
      cannot beat it, the fps half of this FR has failed and should be said so).
- [ ] Sustained-motion fps **≥ 29** at `target_fps=30` — the criterion FR-29
      could not meet.
- [ ] Survives a **reboot with nobody logged in interactively**: the host
      serves without a human clicking a portal dialog.
- [ ] X11 hosts byte-for-byte unchanged; `ROOMLERD_WAYLAND_CAPTURE=0` restores
      the current path exactly.
- [ ] Field-verified on a real Wayland session, with the **before** state
      (black/failed capture) recorded alongside the after.

## Open decisions

- **P0 — where does the capturer run?** In `roomler-desktop` (already
  per-user, already how the companion is placed) and frames shipped to the
  daemon? Or a small session-scoped helper the daemon spawns? The first reuses
  an existing lifecycle; the second keeps the media path in one process. This
  decides the IPC, the failure modes, and how much of the pump moves.
- **Which portal API** — `ScreenCast` alone, or `RemoteDesktop` (which also
  carries input injection and would eventually replace the enigo/XTest path on
  Wayland, where XTest does not reach native clients).
- **KDE vs GNOME divergence.** Both are installed on the field host, so this is
  cheap to answer early rather than discover at rollout.
- Whether the restore token lives in `config.toml` (inherits the atomic +
  0600 + `.prev` treatment) or in the session user's own store.

## Out of scope

- **Windows / macOS** — DXGI and CGDisplayStream are unaffected.
- **Replacing X11 capture.** X11 remains the default and the fallback; this
  adds a backend, it does not retire one.
- **The X11 fps ceiling itself** — FR-29 measured it as unreachable from that
  direction and is closed. This FR does not reopen it.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-29 | 0.4.20 (pre-FR baseline) | Wayland session: **cannot be captured at all**. X11 baseline on the same host: `avg_capture_ms` ~20 ms, ~27 fps at 1080p, idle 2.8 % of a core |

### Host readiness, measured 2026-08-29 (`scw-m2-asahi`)

The prerequisites are already present, so P1 is not blocked on provisioning:

- `pipewire` + `pipewire-libs` installed, **user service `active`**
- `xdg-desktop-portal`, `xdg-desktop-portal-gnome`, `xdg-desktop-portal-gtk`
  installed (`-wlr` absent, and not needed for GNOME/KDE)
- Wayland sessions available: `gnome-wayland`, `gnome-classic-wayland`,
  `plasma`
- The GPU works on this host since the 6.19.14-asahi kernel upgrade
  (`glamor` on `Apple M2 Pro (G14S B1)`), so an accelerated Wayland session is
  a real target rather than a hypothetical

⚠️ Neither `ashpd` nor `pipewire` is in `Cargo.lock` today — this is new
dependency surface on a binary that ships to every fleet host, and the graph
cost should be measured (P3e's size discipline) rather than assumed small.
