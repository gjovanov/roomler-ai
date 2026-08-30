# FR-45 — Portal capture: Wayland where there is no scanout

**Issue:** [#1041](https://github.com/gjovanov/roomler-ai/issues/1041) · **Status:** design · **Owner:** agent / capture

## Goal

Capture a **Wayland desktop that has no DRM scanout** — WSL2, containers,
nested compositors, cloud VMs with no display controller — via
`xdg-desktop-portal`'s ScreenCast interface and PipeWire.

[FR-36](FR-36-wayland-capture.md) closed by capturing **below** the compositor
with DRM/KMS. That works and is the right answer for unattended access, but it
requires a real CRTC with live scanout. Where there is none, there is nothing
to read, and no amount of configuration creates one.

## Why now — the two halves are measured and they do not meet

| | Wayland desktop | DRM scanout | HW encode |
|---|---|---|---|
| `scw-m2-asahi` (Apple Silicon) | ✅ | ✅ `apple-drm`, 4096×2160 | ❌ **none** — Asahi has no video-encode driver (no `/dev/video*`, no `/dev/media*`, no AVD/encode module) |
| WSL2 on an RTX-class laptop | ✅ (nested GNOME under WSLg) | ❌ **`/dev/dri` does not exist** | ✅ `av1_nvenc` / `hevc_nvenc` / `h264_nvenc`, measured `avg_encode_ms=10.4` |

So today the only host that can *capture* Wayland cannot *encode* in hardware,
and the only host that can encode in hardware cannot be captured. **The portal
is the one path that joins them**, and it is the path FR-36 deliberately
demoted to a fallback and never built.

Measured on WSL2 2026-08-31 through the Xvfb virtual desktop, to show the
encode half already works there:

```
AV1 4:2:0 HW (av1_nvenc) · dec HW · 1920×1080 · 77 kbps · ~41 ms
avg_encode_ms=10.4     (Asahi software openh264: 15–21)
```

## ⚠️⚠️ This is the ATTENDED path. It must never be sold as unattended.

FR-36's P0 established this and it has not changed:

- ScreenCast needs an **interactive picker** and an **active user session**.
- `restore_token` only avoids re-prompting *after a human approved once*.
- **While the session is LOCKED, mutter refuses to create *or restore* a
  screencast**, and the failed attempt tends to **consume the saved token**.
- Token behaviour is **inconsistent across compositors**; reboot/logout
  invalidates it.

So this FR does **not** replace FR-36 and does not deliver greeter or
locked-screen capture. It serves the case FR-36 cannot reach: a logged-in user
on a machine with no scanout. The backend priority stays
**DRM/KMS → portal → X11**, with the portal picked only when DRM finds no CRTC.

## Design

| Phase | What | Kill switch |
|---|---|---|
| **P1** | Detect: is `org.freedesktop.portal.ScreenCast` actually on the session bus? Report it in `capture-smoke` so "why did this host pick X11" is answerable without a session. | n/a (read-only) |
| **P2** | ScreenCast session: `CreateSession` → `SelectSources` → `Start` → receive a PipeWire node id + fd. Handle the consent dialog and `persist_mode`/`restore_token`. | `ROOMLERD_PORTAL_CAPTURE=0` |
| **P3** | PipeWire consumer: attach to the node, negotiate a format, deliver `Frame`s through the existing `ScreenCapture` trait as a **sixth backend**. | same |
| **P4** | Input. The portal's **RemoteDesktop** interface can inject, and is the natural pair. ⚠️ First measure whether `/dev/uinput` works in WSL2 — if it does, FR-36's uinput backend already covers input and P4 is unnecessary. | separate flag |

### The seam is unchanged

`ScreenCapture` (`capture/mod.rs`), chosen in `open_default`. DRM is the fifth
backend; this is the sixth. No pump changes.

## ⚠️ The dependency question is the biggest design risk, and it is not theoretical

FR-36 P4b rejected `libxkbcommon` on exactly these grounds and the same
reasoning applies harder here: **linking `libpipewire` would put it in
`roomlerd`'s `DT_NEEDED` on every Linux build.** Headless fleet hosts —
cluster nodes, containers, the ones that will never run a portal — have no
reason to carry it, and a missing `.so` does not degrade a feature: **the
loader refuses to start the daemon at all.** This project has already paid for
that once, when vendored FFmpeg dylibs baked a Homebrew path into the macOS
agent and dyld killed it at launch on every end-user Mac.

Three options, in the order they should be evaluated:

1. **`dlopen` at runtime** behind the existing feature gate — the daemon starts
   everywhere and the backend simply reports unavailable where the library is
   absent. Most code, least deployment risk.
2. **A separate helper binary** that links PipeWire and streams frames over a
   socket, spawned via `current_exe()` — the `caps-probe` precedent
   (`main.rs`, `#[command(hide = true)]`), so no new packaged artifact. The
   daemon's own graph stays clean.
3. Link it directly and accept a hard runtime dependency. **Only if** the
   `.deb`/`.pkg` can be made to carry it, and even then it costs every headless
   host disk and risk for nothing.

⚠️ Whichever is chosen, **prove it by starting the daemon on a host without
PipeWire installed**, not by reasoning about it.

## Acceptance criteria

- [ ] A **nested GNOME Wayland session in WSL2** (per the WSLg + `gnome-shell
      --nested` recipe) is captured and rendered in the browser
- [ ] That session encodes with **`*_nvenc`**, and `avg_encode_ms` is within
      ~2× the 10.4 ms already measured on that host
- [ ] `roomlerd capture-smoke` reports **whether the portal is available and
      why not**, on a host where it is absent — FR-36 measured a host where
      `xdg-desktop-portal` was running yet exposed **neither ScreenCast nor
      RemoteDesktop**, so availability must be *detected*, never assumed
- [ ] **The daemon still starts on a host with no PipeWire library present.**
      Verified by running it, not by inspection
- [ ] Backend order holds: a host with a real CRTC still picks **DRM**, and the
      portal is chosen only when DRM finds none
- [ ] X11/Windows/macOS unchanged; the kill switch restores the current cascade
- [ ] Field-verified with the **before** state recorded beside the after
- [ ] The spec and the UI both say **attended-only** — no greeter, no locked
      screen

## Open decisions

- **Which of the three dependency shapes above.** This is the decision the FR
  turns on; everything else is ordinary work.
- ⚠️ **`/dev/uinput` WORKS in WSL2 — but that is not the question.** Measured
  2026-08-31: `CONFIG_INPUT_UINPUT=m`, the module loads, and FR-36's injector
  created a device and accepted a pointer move (`input: backend=uinput`,
  `has_permission=true`). So injection is not the obstacle.
  ⚠️⚠️ **`/dev/input/` is otherwise EMPTY on WSL2** — no evdev devices at all,
  because WSLg's input arrives over RDP rather than from libinput. A uinput
  device therefore publishes events that, on present evidence, **nothing in
  WSL2 consumes**. So P4 probably survives — not because uinput fails, but
  because the reader is missing.
  **Still unproven, and it is the thing to test first:** whether a *nested*
  compositor (`gnome-shell --nested` under WSLg) reads evdev. If it does, P4
  disappears; if it does not, the portal's RemoteDesktop interface is the only
  input path and P4 is mandatory. One nested session answers it.
- **Restore tokens: store or not.** They are per-compositor, invalidated by
  reboot/logout, and a failed locked-session attempt can consume one. Storing a
  token that is silently dead is worse than prompting.
- Multi-monitor selection: the portal's picker returns whichever outputs the
  user chose, not a stable index — the mapping to `DisplayInfo` needs thought.
- Whether the portal path should be offered on hosts that *do* have DRM (e.g.
  as an operator override when DRM capture misbehaves).

## Out of scope

- Replacing FR-36. DRM stays the unattended path and the default where a CRTC
  exists.
- Greeter and locked-screen capture. The portal structurally refuses both.
- macOS/Windows. This is a Linux desktop-portal concern.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-31 | 0.4.33, WSL2 | **`/dev/dri` absent** — DRM capture impossible. `/dev/dxg` + `libnvidia-encode.so` present; caps probe reports `ffmpeg-av1_nvenc`, `ffmpeg-hevc_nvenc`, `ffmpeg-h264_nvenc` on an RTX 5090 |
| 2026-08-31 | 0.4.33, WSL2 | Xvfb virtual desktop + `av1_nvenc`: 1920×1080, `avg_encode_ms=10.4`, 77 kbps idle, ~41 ms. Establishes the encode half works; only capture is missing |
| 2026-08-31 | 0.4.33, Asahi | Wayland captured via DRM at 1920×1080, but **software encode only** — no video-encode driver exists for Apple Silicon on Linux |
| 2026-08-31 | 0.4.33, WSL2 | **uinput measured, and it reframes P4.** `CONFIG_INPUT_UINPUT=m`, module loads, FR-36's injector created a device and accepted a move (`has_permission=true`) — injection is not the obstacle. But `/dev/input/` is **empty** (WSLg takes input over RDP, not evdev), so nothing appears to consume those events. P4 likely survives because the *reader* is missing, not the writer. Untested: whether a nested `gnome-shell` reads evdev |
