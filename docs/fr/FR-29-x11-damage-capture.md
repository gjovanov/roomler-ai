# FR-29 — X11 capture reads the whole screen even when nothing changed

**Issue:** [#864](https://github.com/gjovanov/roomler-ai/issues/864) · **Status:** P1 field-verified; P2/P3 open · **Owner:** agent / capture

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
| **P1** | Damage-gated grab: no damage since the last delivered frame ⇒ return `Ok(None)` and skip the XShm readback entirely, reusing the existing idle-screen path. | `ROOMLERD_X11_DAMAGE=0` ⇒ today's behaviour byte-for-byte |
| **P2** | Emit `Damage::Tracked(rects)` instead of `Damage::Unknown` so ROI hints become real. | same flag; `Unknown` is the fallback, never a wrong rect list |
| **P3** | Partial readback — `GetImage` only the damaged bounding box into a persistent backbuffer, instead of the full screen. This is where the residual ~20 ms on *active* frames goes. | same flag; falls back to full-frame grab |

### The safety valve is load-bearing

A missed or coalesced damage event under P1 means a **frozen stream**, which is
far worse than a slow one. So P1 forces an unconditional full capture at least
every `ROOMLERD_X11_DAMAGE_MAX_SKIP_MS` (default **1000 ms**). Worst case
for a damage bug is therefore a 1 s stale tile, self-healing, not a dead
session. This bound is the reason P1 is safe to default on.

### `frames_unchanged` must NOT be folded into `frames_empty`

`frames_empty` today means *the pump was starved* — the documented diagnostic
is `frames_empty ≫ frames_encoded ⇒ frame-production-bound`. "Nothing changed"
is the opposite: a healthy, cheap idle. Folding them would silently destroy a
working diagnostic, so P1 adds a distinct counter and heartbeat field.

## Acceptance criteria

- [x] Idle 1080p XFCE: roomlerd CPU **45.8 % → 3.4 %** of a core. (The
      `avg_capture_ms < 2 ms` half is **superseded, not met**: idle ticks no
      longer capture at all, so there is no per-frame capture time to average.
      The criterion assumed a cheaper readback; P1 removes the readback.)
- [ ] **NOT met — and not addressable by P1.** Under sustained motion fps went
      26.4 → 27.6 and capture 22.1 → 20.6 ms, i.e. unchanged within noise.
      Every frame is genuinely damaged under continuous motion, so every frame
      is still read in full. This is what **P3** (partial readback) exists for;
      recording it as an honest partial rather than reframing the target.
- [x] Safety valve measured: on a genuinely static screen, captures ran at
      **1.0 /s** at `MAX_SKIP_MS=1000`. ⚠️ Re-measuring at 4000 ms still gave
      ~1.07 /s because a real XFCE desktop is **never** static — the panel
      clock repaints every second. `frames_unchanged` climbing ~28 /s beside
      `frames_captured` ~0.94 /s is the proof both halves work.
- [x] `frames_unchanged` reported separately — field-observed
      `frames_empty=2250 frames_unchanged=2250`, i.e. every empty was a
      *proven* no-change rather than a starved pump.
- [x] Windows and macOS untouched — the module and its call site are
      `#[cfg(all(target_os = "linux", feature = "scrap-capture"))]`.
- [x] `ROOMLERD_X11_DAMAGE=0` restores prior behaviour: idle went back to
      **50.6 %** CPU / 26.4 fps / 19.1 ms capture.
- [x] Field-verified on `scw-m2-asahi` as a same-session A/B — damage OFF
      50.6 %, damage ON 3.4 %, on the same host minutes apart.

## Open decisions

- ~~**Is root-window damage sufficient under a compositing WM?**~~ **SETTLED
  for XFCE/xfwm4, both directions** (2026-08-29). With `use_compositing=true`:
  motion still captured at **27.3 fps** (damage is not hidden by the
  redirection) *and* idle still cost **3.0 %** of a core (a compositor does not
  flood damage and defeat the skip). Both failure modes were plausible; neither
  occurs. ⚠️ **Plasma/KWin and GNOME/Mutter remain untested** — both are
  installed on the field host, so this is cheap to close and should be done
  before P1 defaults on for hosts running those sessions.
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
| 2026-08-29 | P1, source build on the host | **idle CPU 3.4 % (13×)**; motion unchanged (45.0 %, 20.6 ms, 27.6 fps); kill switch back to 50.6 % |
| 2026-08-29 | P1, xfwm4 **compositing ON** | motion 27.3 fps / 44.2 % / 20.4 ms; **idle 3.0 %** — a compositor neither hides damage nor floods it |

### What the field test caught that CI never would

1. **The media heartbeat goes silent exactly when P1 works.** It fires per 30
   *encoded* frames, so once captures are skipped there is nothing to key on —
   the idle host emitted **one heartbeat in two minutes** and `frames_unchanged`
   (added to make idle observable) was itself unobservable. Fixed by moving the
   periodic idle signal onto the already-rate-limited "idle screen" log.
2. **A headless remote-desktop host locks itself.** `xfce4-screensaver` blanked
   and then LOCKED the session mid-test; the viewer showed pure black at 1 fps.
   That black frame was faithful — capture was correct and the safety valve was
   ticking exactly as designed — but it cost a wasted measurement round, and on
   a real deployment an operator would connect to an unusable lock screen with
   no local keyboard. Screensaver, lock and DPMS are now disabled on the host.
   ⚠️ Worth considering whether the installer should do this for VD/headless
   roles; a host that autologins has little reason to lock.
3. **"Idle" desktops are not idle.** The panel clock repaints every second, so
   damage genuinely fires ~1 /s. Any future test of the safety valve must use a
   truly static screen or it will measure the clock instead.
4. ⚠️ **`roomler-encode-probe` reports nonsense across a session boundary** —
   counters reset, so `last - first` goes negative and printed `-10.8/s`. It
   also divides by a near-zero window when few frames encode, which is where a
   misleading `avg_capture_ms=36` came from. Read its output as suspect unless
   the heartbeat count is healthy and the deltas are positive.
