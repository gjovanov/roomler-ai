# FR-36 — Wayland capture, and unattended access

**Issue:** [#929](https://github.com/gjovanov/roomler-ai/issues/929) · **Status:** design — **P0 decided (2026-08-29): DRM/KMS first, portal demoted to fallback** · **Owner:** agent / capture

## Goal

Capture a **Wayland** desktop, **unattended** — including while the session is
locked and at the login greeter. Today the agent cannot capture Wayland at all:
it produces a black or wallpaper-only stream. That is a growing share of Linux
hosts, and on Apple Silicon it is *every* accelerated desktop.

Secondary: it is the only remaining lever on the ~27 fps ceiling, after
[FR-29](FR-29-x11-damage-capture.md) measured the X11 path to its floor
(~20 ms/frame at 1080p, indifferent to how the framebuffer was drawn, and
partial readback ruled out because a window drag reports ONE full-screen damage
rectangle).

## ⚠️ P0 DECIDED — and it reverses this spec's first draft

The original plan was portal-first (`xdg-desktop-portal` ScreenCast → PipeWire),
with a session-resident broker to satisfy the portal's per-user-session
requirement and `persist_mode` restore tokens for unattended use.

**That plan cannot deliver unattended access.** RustDesk shipped the portal
path, hit the wall, and went around it. Their findings, which we should not
re-derive the expensive way:

- ScreenCast needs an **interactive consent dialog + monitor picker**, and the
  screencast exists only **inside an active user session**.
- `restore_token` only avoids re-prompting *after a human approved once* — it is
  not a headless grant.
- **While the session is LOCKED, mutter refuses to create or restore any portal
  screencast** (`Session creation inhibited`), and each locked attempt tends to
  **consume the saved restore token**, dropping back to the picker.
- Token behaviour is **inconsistent across compositors**; reboot/logout
  invalidates it, and other screencast apps (OBS) can interfere.

So the portal is structurally an *attended* API. A locked host — the normal
state of an unattended machine — is exactly where it refuses.

**Decision: capture below the compositor via DRM/KMS.** Read the scanout
framebuffer from the kernel, beneath both the compositor and its permission
prompt. This is compositor-independent (one backend for GNOME, KDE, XFCE, X11
and *no session at all*), works at the greeter, and dissolves the process-model
dilemma P0 was originally about — no session-resident broker is needed, because
we never talk to the session.

⚠️ The earlier recommendation (session broker + portal fd handoff over
`SCM_RIGHTS`) is **superseded**. It solved the wrong problem: it made the portal
reachable, when the portal is the thing that refuses.

## Prior art — read before building

- RustDesk blog: *Unattended Remote Access on Wayland* —
  <https://rustdesk.com/blog/unattended-remote-access-wayland/>
- Design discussion (the technical source):
  <https://github.com/rustdesk/rustdesk/discussions/15417>
- Restore-token failure modes:
  <https://github.com/rustdesk/rustdesk/discussions/10216>
- **`libdrmtap`** — <https://github.com/fxd0h/libdrmtap>

### ⚠️ Licensing — the distinction is load-bearing

- **`libdrmtap` is MIT** (C, with Rust bindings on crates.io). MIT is compatible
  with our MPL-2.0 agent side (FR-24).
- **RustDesk itself is AGPL-3.0.** Do **not** copy or adapt RustDesk source into
  this tree. Read their *discussion* for the design; take code only from the MIT
  library. FR-24's CI check asserts no AGPL crate reaches a shipped agent binary
  — a careless copy-paste is a licence violation, not merely a CI failure.

## Design

| Phase | What | Kill switch |
|---|---|---|
| **P1** | DRM/KMS capture as a fifth `ScreenCapture` backend: enumerate CRTCs, `drmModeGetFB2` the active scanout, deliver `Frame`s. Linear formats first. | `ROOMLERD_DRM_CAPTURE=0` ⇒ today's path exactly |
| **P2** | **Detiling.** Modern GPUs scan out tiled/compressed (Intel `INTEL_4_TILED_MTL_RC_CCS_CC`, Nvidia block-linear). EGL import + convert to linear. Without this, tiled hosts produce garbage — not a slow picture, a wrong one. | same flag ⇒ fall back to portal/X11 |
| **P3** | **Privilege split.** A minimal helper holding `CAP_SYS_ADMIN`, passing **DMA-BUF fds over `SCM_RIGHTS`**; the daemon never needs the capability itself. | same flag |
| **P4** | Input on Wayland via **`uinput`** — XTest/enigo does not reach native Wayland clients, so capture without this is a read-only session. | separate flag |

**Backend priority: DRM/KMS → portal/PipeWire → X11.** The portal keeps its
place as the *attended* path (a logged-in user who consents once); X11 stays the
default until DRM is field-proven.

### Privilege model — do not simply run capture as root

`roomlerd` already runs as root, so P1 *can* open `/dev/dri/*` directly and P3
is tempting to skip. Do not skip it permanently. RustDesk's shape is a separate
`drmtap-helper` with `cap_sys_admin+ep`, `/dev/dri/`-only device access,
`PR_SET_NO_NEW_PRIVS`, a seccomp allowlist, and an 8-byte protocol whose only
attacker-controllable field is a CRTC-id equality filter. That is the right
target for a binary shipping to every fleet host — and it maps onto this repo's
existing self-spawn precedent (`roomlerd caps-probe`,
`#[command(hide = true)]` at `main.rs:149`, spawned via `current_exe()` at
`encode/caps.rs:79`), so it needs **no new packaged artifact**.

### The seam is unchanged

`ScreenCapture` (`capture/mod.rs:266`), chosen in `open_default`
(`capture/mod.rs:413`); backends at `:16` scrap, `:22` x11_damage, `:25` wgc,
`:34` synthetic. DRM is a fifth — no pump changes.

## Host readiness — measured on `scw-m2-asahi`, 2026-08-29

Every DRM prerequisite is already satisfied:

| requirement | state |
|---|---|
| **Active CRTC** (DRM capture needs live scanout) | `card2-HDMI-A-1 status=connected enabled=enabled`, mode 4096x2160 |
| Root / `CAP_SYS_ADMIN` | `roomlerd` runs as **root** |
| `/dev/uinput` for input | present; `CONFIG_INPUT_UINPUT=m` |
| `vkms` (virtual CRTC, if ever genuinely headless) | `CONFIG_DRM_VKMS=m` |
| Kernel ≥ 4.20 for `GETFB2` | **6.19.14-asahi** |
| GPU/EGL for detiling | works since the kernel upgrade (`glamor` on `Apple M2 Pro (G14S B1)`) |

⚠️ **DRM card numbering SHIFTED after the kernel upgrade** — the display
controller is now `card2` (it was `card0`). **Enumerate by driver/CRTC, never by
a hard-coded card index.**

⚠️ **Screen blanking must stay off**, because DRM capture requires active
scanout — a blanked screen is not merely dark, it stops the thing we read.
[#921](https://github.com/gjovanov/roomler-ai/pull/921) already disables the X
server's built-in screen saver in VD mode for an unrelated reason; that is now a
*prerequisite*, and the DE-level screensaver/lock question it deliberately left
out of scope becomes in-scope here.

## Acceptance criteria

- [ ] A GNOME **Wayland** session on `scw-m2-asahi` streams real pixels (today:
      uncapturable at all)
- [ ] Streams **while the session is locked**, and **at the login greeter** —
      the cases the portal structurally refuses
- [ ] Survives a reboot with nobody logged in interactively
- [ ] `avg_capture_ms` **< 10 ms** at 1080p. ⚠️ If DRM cannot beat the ~20 ms
      X11 floor, the **fps half of this FR has failed** and must be recorded as
      such, not reframed — FR-29's discipline
- [ ] Sustained-motion fps **≥ 29** at `target_fps=30`
- [ ] Input reaches native Wayland clients (uinput), not only Xwayland ones
- [ ] X11/Windows/macOS byte-for-byte unchanged; `ROOMLERD_DRM_CAPTURE=0`
      restores the current path exactly
- [ ] Field-verified with the **before** state (black/failed capture) recorded
      beside the after

## Open decisions / risks

- ⚠️ **Asahi detiling is UNPROVEN — the single biggest technical unknown.**
  libdrmtap lists Intel, AMD, Nvidia Jetson (aarch64) and virtio-gpu as
  verified; its Apple mention is a **T2 Intel Mac**, not Apple Silicon.
  `apple-drm` scanout may use a tiled/compressed modifier nobody has exercised.
  **P1's first test should be a linear-format grab that returns a sane image**,
  before any plumbing is written.
- Vendor `libdrmtap` (MIT, C + meson) vs reimplement the narrow slice in Rust.
  Vendoring adds a C build dep to every Linux agent build plus a drift-gate
  obligation (`scripts/revendor-*.sh` precedent); reimplementing risks getting
  detiling subtly wrong. Measure the graph/size cost either way (P3e).
- Multi-monitor: per-CRTC capture with physical origin/scale mapping.
- Cursor: the hardware cursor plane is captured separately; hotspot is
  approximate on bare metal.
- Interaction with FR-27 (consent) and FR-34 (locked host): DRM capture makes
  the agent able to see a **locked** screen. That is the point for unattended
  access and simultaneously a policy question — it must be gated and audited,
  not quietly enabled.

## Out of scope

- Replacing X11 or Windows/macOS capture. This adds a backend.
- The X11 fps ceiling itself — FR-29 measured it unreachable from that
  direction and is closed.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-29 | 0.4.20 (pre-FR baseline) | Wayland: **uncapturable**. X11 baseline same host: `avg_capture_ms` ~20 ms, ~27 fps at 1080p, idle 2.8 % of a core |
| 2026-08-29 | portal probe | `xdg-desktop-portal` reachable (4 portal names on the session bus) but **exposes neither `ScreenCast` nor `RemoteDesktop`** in an XFCE/**X11** session — those come from a compositor-matching backend. `-gnome` + `-gtk` installed; **`-kde` is NOT**. The agent must therefore *detect* ScreenCast, never assume it |
