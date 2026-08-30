# FR-36 — Wayland capture, and unattended access

**Issue:** [#929](https://github.com/gjovanov/roomler-ai/issues/929) · **Status:** **P1 landed behind `ROOMLERD_DRM_CAPTURE` (default OFF). Greeter AND locked-session capture field-verified — the two cases the portal refuses. 4K rate is the open problem** (2026-08-29) · **Owner:** agent / capture

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

## ✅ P1 go/no-go — MEASURED on `scw-m2-asahi`, 2026-08-29

The spec's stated first action was *"a linear-format grab that returns a sane
image, before any plumbing is written."* Done, with a ~120-line C probe
(`drmModeGetFB2` → `drmPrimeHandleToFD` → `mmap`). **It passes, and it resolves
the FR's largest risk.**

| question | answer |
|---|---|
| Does `apple-drm` scan out **tiled**? | **No — and it cannot.** The primary plane's `IN_FORMATS` blob advertises exactly **one** modifier: `DRM_FORMAT_MOD_LINEAR` |
| Live X11 (XFCE) grab | ✅ `fb 49, 1920×1080, XR24, modifier 0x0` — correct colours, geometry, text |
| Live **GNOME Wayland** grab | ✅ `fb 51, 4096×2160, **XR30**, modifier 0x0`, `allocated by = gnome-shell` |
| Is the grab **live** (not a stale buffer)? | ✅ opened a marked window, re-grabbed: **859,916 differing bytes**, new window present |
| Does it need DRM master? | **No.** Xorg (then mutter) held master throughout; `CAP_SYS_ADMIN` is what gates the GEM handle |
| Read cost, 1080p 8-bit (8.3 MB) | **1.58 ms** cached-map · 2.07 ms remapping every frame (5.2 GB/s) |
| Read cost, 4K 10-bit (35.4 MB) | **15.2 ms** cached-map · 16.7 ms remapping (2.3 GB/s) |

**Because the hardware plane accepts only LINEAR, no compositor on this platform
can hand it a tiled buffer.** That is a structural guarantee, not a lucky
sample — so **P2 (detiling) is not needed on Apple Silicon at all**. It stays in
the plan only for the tiled-scanout GPUs (Intel/AMD/Nvidia) that libdrmtap
exists to serve, and is no longer on the critical path for this host.

### ⚠️ Two findings that change the implementation

1. **GNOME scans out 10-bit `XR30` here, X11 scans out 8-bit `XR24`.** A backend
   written for `XRGB8888` alone produces a *structurally perfect, psychedelic*
   image — the probe did exactly that on its first Wayland run. P1 must branch on
   the fourcc: `XR30`/`AR30` unpack as one packed LE word `x:R:G:B 2:10:10:10`.
   The failure mode is silent and looks like a colour-space bug, not a format bug.
2. **The <10 ms bar is met at 1080p and MISSED at native 4K** (15.2 ms just to
   read, before any conversion or encode). Throughput also *halves* on the larger
   buffer (5.2 → 2.3 GB/s) — 35 MB does not stay in cache. So the fps half of
   this FR is resolution-dependent and must be reported that way, never as a
   single number.

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
| **P1** ✅ | DRM/KMS capture as a fifth `ScreenCapture` backend (`capture/drm_backend.rs`): enumerate CRTCs, `drmModeGetFB2` the active scanout, PRIME-export, mmap, deliver `Frame`s. Handles **`XR24` + `XR30`** (both measured in the field). | **`ROOMLERD_DRM_CAPTURE=1` to opt IN** — see below |
| **P2** | **Detiling** — EGL import + convert to linear. **NOT needed on Apple Silicon** (plane is LINEAR-only, measured); required for Intel `*_RC_CCS`, Nvidia block-linear. Deferred off the critical path. | same flag ⇒ fall back to portal/X11 |
| **P3** | **Privilege split.** A minimal helper holding `CAP_SYS_ADMIN`, passing **DMA-BUF fds over `SCM_RIGHTS`**; the daemon never needs the capability itself. | same flag |
| **P4** | Input on Wayland via **`uinput`** — XTest/enigo does not reach native Wayland clients, so capture without this is a read-only session. | separate flag |

**Backend priority: DRM/KMS → portal/PipeWire → X11**, but DRM is **opt-in**
(`ROOMLERD_DRM_CAPTURE=1`) rather than the Linux default — the inverse of this
repo's usual kill-switch shape, and deliberately so.

Measured on one host, one session, back to back at 1080p: scrap delivered **1 of
30** frames because FR-29's damage tracking proved the other 29 unchanged; DRM
delivered **30 of 30**, because DRM reports no damage and cannot know. Defaulting
it on would silently undo the FR-29 win (45.8 % → 2.8 % of a core, idle) on every
X11 host in the fleet. A Wayland or headless host opts in; everyone else keeps
the existing cascade byte-for-byte. The portal keeps its place as the *attended*
path (a logged-in user who consents once).

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
| Wayland sessions installed | `gnome-wayland`, `gnome`, `gnome-classic-wayland`, `plasma` (+ mutter 48.8) |

⚠️ **DRM card numbering SHIFTED after the kernel upgrade** — the display
controller is now `card2` (it was `card0`), and `card1` is the `asahi` render
node. **Enumerate by driver/CRTC, never by a hard-coded card index.** The probe
selects the node whose `drmModeGetResources` reports `count_crtcs > 0 &&
count_connectors > 0`, which is the render-vs-display discriminator.

⚠️ **Screen blanking must stay off**, because DRM capture requires active
scanout — a blanked screen is not merely dark, it stops the thing we read.
[#921](https://github.com/gjovanov/roomler-ai/pull/921) already disables the X
server's built-in screen saver in VD mode for an unrelated reason; that is now a
*prerequisite*, and the DE-level screensaver/lock question it deliberately left
out of scope becomes in-scope here.

## Acceptance criteria

- [x] **A GNOME Wayland session on `scw-m2-asahi` yields real pixels.** Proven
      twice: by direct DRM probe, then through the shipped
      `capture::open_default` cascade. The **before** state is recorded beside
      it — the current path logs `scrap capture unavailable → NoopCapture (no
      primary display: connection refused)` and delivers **zero frames**
- [x] The P1 backend is **wired into the capture cascade** and delivers
      `Frame`s through the `ScreenCapture` trait (`roomlerd capture-smoke`)
- [ ] A **browser-visible remote-control session** against that Wayland
      desktop — capture is proven, encode/transport end-to-end is not
- [x] Streams **while the session is locked**, and **at the login greeter** —
      the cases the portal structurally refuses. Greeter: **20/20 frames with
      nobody logged in**. Locked: the XFCE unlock dialog captured.
      ⚠️ **A locked-and-IDLE screen is genuinely BLACK in scanout.** DRM
      reports it faithfully, but a viewer connecting to an idle locked host
      sees black until something wakes the display — do not diagnose that as a
      capture failure; it is FR-34 territory
- [ ] Survives a reboot with nobody logged in interactively
- [x] `avg_capture_ms` **< 10 ms** at **1080p** — measured **5.84 ms** for the
      whole backend (1.58 ms of that is the framebuffer read; the rest is the
      BGRA repack). Same host/session, scrap measured 5.11 ms
- [ ] **`avg_capture_ms` at native 4K: 42.9 ms — FAILS the bar by 4×**
      (52.9 ms with the production `Auto` downscale). ⚠️ Recorded as a failure,
      not reframed: see the open decision below
- [ ] Sustained-motion fps **≥ 29** at `target_fps=30`. At 1080p the headroom
      is there; at 4K the measured ceiling is **~19 fps**
- [ ] Input reaches native Wayland clients (uinput), not only Xwayland ones
- [x] X11/Windows/macOS unchanged — the backend is Linux-only, feature-gated,
      AND env-gated off; with the flag unset the same host still selects
      `backend=scrap` with X11 damage tracking active
- [x] Field-verified with the **before** state recorded beside the after

## Open decisions / risks

- ~~⚠️ **Asahi detiling is UNPROVEN — the single biggest technical unknown.**~~
  **RESOLVED 2026-08-29: no detiling needed.** The `apple-drm` primary plane's
  `IN_FORMATS` advertises `DRM_FORMAT_MOD_LINEAR` and nothing else, so a tiled
  scanout buffer is not representable on this hardware. Verified live under both
  Xorg and mutter.
- ⚠️ **4K is the open problem, and it is memory bandwidth, not the loop.** The
  whole backend costs **42.9 ms/frame** at 4096×2160 10-bit (~19 fps) against
  5.84 ms at 1080p. Reading is 15.2 ms of that; the BGRA repack is most of the
  rest, and `Auto` downscale ADDS 10 ms because it is a second full pass.
  ⚠️ A vectorisable-loop rewrite (paired `chunks_exact`) was tried and
  **REFUTED** — 43.8 ms vs 42.4 ms, i.e. no change — so the cost is the ~70 MB/
  frame of traffic, not instruction count. **The identified fix is to fuse the
  downscale INTO the repack**: today the `Auto` path is read 35 MB → write
  35 MB → read 35 MB → write 8.8 MB; fused it becomes read 35 MB → write
  8.8 MB. Predicted ~2.5×, unmeasured. Alternatives remain: cap the advertised
  mode, or accept a lower fps at 4K.
- Vendor `libdrmtap` (MIT, C + meson) vs reimplement the narrow slice in Rust.
  **The P1 probe is ~120 lines of libdrm calls with no detiling**, which shifts
  this decision toward reimplementing: vendoring adds a C build dep to every
  Linux agent build plus a drift-gate obligation, and its main value (detiling)
  is exactly the part Apple Silicon does not need. Re-open for tiled GPUs.
- Multi-monitor: per-CRTC capture with physical origin/scale mapping.
- Cursor: the hardware cursor plane is captured separately; hotspot is
  approximate on bare metal. (Measured: `plane-1` sits unbound at `fb=0` while
  `plane-0` carries the desktop.)
- ⚠️⚠️ **The policy hazard is no longer hypothetical — it is demonstrated.** The
  greeter capture contains lightdm's **password field**, and the locked capture
  contains XFCE's **unlock dialog**. That is precisely the capability unattended
  access needs and precisely what must never be quietly enabled: gate it, audit
  it, and make the viewer indicator (FR-27) reachable on those surfaces before
  this leaves opt-in.
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
| 2026-08-29 | **P1 probe, X11 (XFCE)** | ✅ `card2`/`apple-drm`, `fb 49 1920×1080 XR24 modifier=0x0`. Correct image. Liveness confirmed (new window ⇒ 859,916 differing bytes). Read **1.58 ms** cached / 2.07 ms remap-per-frame |
| 2026-08-29 | **P1 probe, GNOME Wayland** | ✅ session switched to `gnome-wayland` (mutter 48.8). `fb 51 4096×2160 **XR30** modifier=0x0`, `allocated by = gnome-shell`. First decode was psychedelic — probe assumed 8-bit; with `XR30` unpacking the image is correct. Read **15.2 ms** cached (35.4 MB). Host reverted to XFCE; `roomlerd` PID unchanged across this switch pair |
| 2026-08-29 | **P1 backend, X11 (XFCE), 1080p** | ✅ `open_default` picks `backend=drm` under `ROOMLERD_DRM_CAPTURE=1`; correct desktop image. **5.84 ms/frame, 30/30 delivered.** Same host, same session, scrap: **5.11 ms** but **1/30 delivered, 29 proven unchanged** — FR-29 damage tracking working, and the reason DRM must not be the default |
| 2026-08-29 | **P1 backend, GNOME Wayland, 4K** | ✅ **before:** the shipping path logs `scrap capture unavailable — falling back to NoopCapture (no primary display: connection refused)` — **zero frames**. **after:** `backend=drm`, **30/30 at 4096×2160 XR30**, correct image. ⚠️ **42.9 ms/frame raw, 52.9 ms with `Auto` downscale** ⇒ ~19 fps. Memory-bandwidth bound (~70 MB/frame). A vectorisable-loop rewrite was tried and **REFUTED** (43.8 vs 42.4 ms — no change) |
| 2026-08-29 | daemon impact | `roomlerd` restarted **once** during the whole session (`NRestarts=1`, 18:04), not coincident with either lightdm restart; ended `active/running`. The FR-19 relay it also hosts stayed reachable throughout (every measurement above arrived over its own control WS) |
| 2026-08-29 | **P1 at the LOGIN GREETER** | ✅ autologin disabled, lightdm restarted, **nobody logged in** (`loginctl`: only a `greeter` session for user `lightdm`). Shipping path: `NoopCapture`, zero frames. DRM: **20/20 at 1920×1080, 6.85 ms**, image is the lightdm greeter — user dropdown, password field, Log In button. This is the case the portal structurally refuses (no session ⇒ no portal) |
| 2026-08-29 | **P1 on a LOCKED session** | ✅ with a real locker running (`xfce4-screensaver`, `/lock/enabled=true`) DRM captured the **XFCE unlock dialog** — user `m1`, password field, Switch User / Cancel / Unlock. ⚠️ **The first attempt was a false negative twice over**, see below |
| 2026-08-29 | ⚠️ two false results caught | (1) `loginctl lock-session` reported success but `LockedHint` stayed `no` and no locker process existed — the screen was never locked, and the capture was an ordinary desktop. Claiming it would have been a **vacuous pass**. (2) With the locker genuinely active but idle, the capture was **pure black with every counter green** (15/15 frames, 6.71 ms). Waking the dialog with harmless input (mouse move + shift; no password typed) produced the lock screen — so the black was a **genuinely black screen**, faithfully reported |
