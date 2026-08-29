# FR-29 — X11 capture reads the whole screen even when nothing changed

**Issue:** [#864](https://github.com/gjovanov/roomler-ai/issues/864) · **Status:** design → P1 · **Owner:** agent / capture

## Goal

Break the ~20 ms-per-frame X11 capture floor that pins every Linux
remote-desktop host at ~25–27 fps at 1080p, and make an *idle* desktop
approximately free to serve instead of costing half a CPU core.

## Field evidence (2026-08-28/29, `scw-m2-asahi`, Fedora Asahi 42, M2 Pro)

`avg_capture_ms` measured on the same host across three progressively more
"real" display stacks, H.264-SW, XFCE, 1920×1080:

| display stack | idle capture | scroll capture | idle CPU | fps |
|---|---|---|---|---|
| Xvfb (virtual framebuffer) | 19.3 ms | 21.9 ms | 49.5 % of a core | 26.4 |
| real KMS Xorg, software render | 21.2 ms | 20.9 ms | 49.8 % | 25.2 |
| real KMS Xorg, **GPU-accelerated** | 22.2 ms | 22.1 ms | 45.8 % | 25.2 |

Three conclusions the matrix forces:

1. **Capture cost is indifferent to how the framebuffer was drawn.** Adding a
   real display controller changed nothing; adding full GPU acceleration
   (`glamor` on `Apple M2 Pro (G14S B1)`, glxgears 3221 fps) changed nothing.
2. **Capture, not encode, is the bottleneck.** `avg_encode_ms` was 14–18 ms
   against a 33 ms budget at 30 fps; capture ate the larger half.
3. **Idle costs the same as active.** A completely static desktop paid the
   full ~20 ms readback 25 times a second — ~50 % of a core to send 0.2 Mbps.

This is not Asahi-specific and not a GPU problem. It is a property of the X11
capture path and therefore applies to **every Linux desktop host in the fleet**.

## Root cause

`scrap` on Linux is XShm. `Capturer::frame()` performs a full-screen
`GetImage` on every call and has no "nothing changed" signal — unlike Windows
DXGI Desktop Duplication, which returns `WouldBlock` when the desktop is
unchanged. Two consequences in
[`capture/scrap_backend.rs`](../../agents/roomlerd/src/capture/scrap_backend.rs):

- `scrap_backend.rs:375` — the `WouldBlock` arm is the "no new frame" path.
  **On Linux it is unreachable**: XShm always yields a full buffer.
- `scrap_backend.rs:366` — every delivered frame is stamped
  `damage: Damage::Unknown`, so the encoder must treat all pixels as dirty and
  `peer.rs:2376`'s `set_roi_hints` receives an empty rect list.

The pump already has the fast path this needs. `peer.rs:2052` logs
`"capture produced no frame (idle screen)"` on a `None` from `next_frame` and
counts `frames_empty`. **That path exists, is named, and is simply
unreachable on Linux.** P1 is about making it reachable, not about inventing it.

`Damage` (`capture/mod.rs:76`) likewise already distinguishes the two empty
states — `Unknown` ("no information, assume all dirty") vs `Tracked(vec![])`
("provably nothing changed"). Only the producer is missing.

## Design

A Linux-only damage tracker in the capture worker, on its own X11 connection
(`x11rb` 0.13.2, already in `Cargo.lock` transitively via enigo/arboard; needs
its `damage` feature enabled). The worker registers `XDamage` on the root
window and drains events before each grab.

| Phase | What | Kill switch |
|---|---|---|
| **P1** | Damage-gated grab: no damage since the last delivered frame ⇒ return `Ok(None)` and skip the XShm readback entirely, reusing the existing idle-screen path. | `ROOMLER_AGENT_X11_DAMAGE=0` ⇒ today's behaviour byte-for-byte |
| **P2** | Emit `Damage::Tracked(rects)` instead of `Damage::Unknown` so ROI hints become real. | same flag; `Unknown` is the fallback, never a wrong rect list |
| **P3** | Partial readback — `GetImage` only the damaged bounding box into a persistent backbuffer, instead of the full screen. This is where the residual ~20 ms on *active* frames goes. | same flag; falls back to full-frame grab |

### The safety valve is load-bearing

A missed or coalesced damage event under P1 means a **frozen stream**, which is
far worse than a slow one. So P1 forces an unconditional full capture at least
every `ROOMLER_AGENT_X11_DAMAGE_MAX_SKIP_MS` (default **1000 ms**). Worst case
for a damage bug is therefore a 1 s stale tile, self-healing, not a dead
session. This bound is the reason P1 is safe to default on.

### `frames_unchanged` must NOT be folded into `frames_empty`

`frames_empty` today means *the pump was starved* — the documented diagnostic
is `frames_empty ≫ frames_encoded ⇒ frame-production-bound`. "Nothing changed"
is the opposite: a healthy, cheap idle. Folding them would silently destroy a
working diagnostic, so P1 adds a distinct counter and heartbeat field.

## Acceptance criteria

- [ ] Idle 1080p XFCE: `avg_capture_ms` ~20 ms → **< 2 ms**, roomlerd CPU
      ~50 % of a core → **< 10 %**.
- [ ] Under sustained motion at 1080p, fps rises above the ~27 fps ceiling
      (**≥ 29** at `target_fps=30`).
- [ ] A suppressed damage event self-heals within **1 s** (verified by forcing
      the tracker to drop events, not by argument).
- [ ] `frames_unchanged` is reported separately from `frames_empty`.
- [ ] Windows and macOS behaviour byte-for-byte unchanged (Linux-gated).
- [ ] `ROOMLER_AGENT_X11_DAMAGE=0` restores current behaviour exactly.
- [ ] Field-verified on `scw-m2-asahi`, with the **before** run recorded
      failing and the **after** run passing — CI green is not a result.

## Open decisions

- **Is root-window damage sufficient under a compositing WM?** With a
  compositor, clients are redirected offscreen and the compositor paints the
  root; root damage should cover it, but XFCE-with-compositing, Plasma/KWin and
  GNOME/Mutter must each be checked — all three are installed on the field host.
- Whether P3's partial readback is worth its complexity, or whether P1 alone
  (idle free, active unchanged) already buys the operator experience we want.

## Out of scope

- **Wayland / PipeWire capture.** Required for accelerated Wayland desktops
  (and the only way to capture an Asahi GPU session), but it is a new backend,
  not a change to this one. Separate FR.
- **Windows DXGI** — already has the equivalent via `WouldBlock`.
- **Hardware video encode** — measured impossible on Apple Silicon under Linux:
  no `/dev/video*`, and `vulkaninfo` reports 0 `VK_KHR_video_encode*`
  extensions. Not a lever on this hardware.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-29 | 0.4.15 (pre-change baseline) | idle capture 22.2 ms, CPU 45.8 %, 25.2 fps — the numbers P1 must move |
