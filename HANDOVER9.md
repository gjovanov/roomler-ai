# Handover #9 — RustDesk-parity Tier A + Viewer controls (0.1.33 → 0.1.35)

> Continuation of HANDOVER8 (which landed honest caps probe in 0.1.30 and
> closed the AV1 false-advertising gap). This session shipped three
> groups of features against the RustDesk-parity plan at
> `~/.claude/plans/floating-splashing-nebula.md`:
>
> 1. RustDesk-parity Tier A (60 fps HW + native-res + higher bitrate +
>    codec override + browser buffering-to-zero).
> 2. Viewer controls (Scale modes + Fullscreen) and Remote-Resolution
>    override with agent-side CPU downscale.
> 3. In-field bug fixes surfaced by user testing: keyboard VK path,
>    clipboard worker lifetime, WGC + pipeline telemetry.

## Shipped in 0.1.33 → 0.1.35 (one tag per milestone)

```
902cb44  feat(rc): Phase 1 — Scale modes + Fullscreen toggle      [0.1.35]
ae447b5  feat(rc): Phase 2 — remote resolution control (rc:resolution DC)
7bc1b47  fix(rc): scale-mode bugs — cursor math, Original sizing, flex min-0
---------- 0.1.35 tag ----------
cf02eb3  chore: bump workspace 0.1.32 -> 0.1.33                   [0.1.33]
4beccf3  perf(rc): Tier A1+A2+A3+A5 — 60 fps HW, native-res, richer bitrate, codec override
ef4cf5f  perf(rc): Tier A6 — browser buffering to zero + rVFC stall recovery
---------- 0.1.33 tag ----------
6175fb5  fix(agent): Windows keyboard combos + clipboard worker lifetime
8b37ca7  perf(agent): pipeline timing diagnostics + WGC drop counter + bump 0.1.34
---------- 0.1.34 tag ----------
```

Plus supporting plumbing from the same session: hotkey interception
(Ctrl+A/C/V/X/Z/Y/F/S/P/R gated on pointer-over-video), Ctrl+Alt+Del
toolbar button, viewer-indicator overlay on Windows, file-transfer DC
handler, and the full build-and-push deploy pipeline promoted into the
`roomler-ai-deploy` repo.

## Why each lands

### Tier A — bang for buck, all browser-visible

- **A1 60 fps HW + A2 native-resolution capture** (`peer.rs:47`,
  `downscale_for`): `TARGET_FPS_SW=30 / TARGET_FPS_HW=60`, and
  `DownscalePolicy::Never` when MF-HW is compiled in. The user
  reported pre-0.1.33 streams at 30 fps + 1707×1067 (a 2× CPU box
  filter applied to 4K) — now at 60 fps + native.
- **A3 bitrate ceilings** (`encode/mod.rs`): `DESKTOP_BPP_PER_SECOND`
  0.10 → 0.15, `MAX_BITRATE_BPS` 15 → 25 Mbps, `quality::MAX_HIGH_BPS`
  20 → 30 Mbps. Measured against RustDesk's ~0.14 bpp/s default.
- **A5 codec override** (`useRemoteControl.ts` + `RcPreferredCodec`):
  new toolbar dropdown forces H.264 / H.265 / AV1 / VP9 via
  `filterCapsByPreference` on the `browser_caps` list sent in
  `rc:session.request`; localStorage-persisted.
- **A6 browser buffering** (`useRemoteControl.ts:pc.ontrack`):
  `jitterBufferTarget = 0`, `playoutDelayHint = 0`,
  `contentHint = 'motion'` on the receiver; `requestVideoFrameCallback`
  loop keeps the tab hot + `play()` kicker rescues idle-optimizer
  pauses.
- **A4 deferred** — webrtc-rs 0.12 `RTCRtpSender` has no
  `set_parameters` and `RTCRtpCodingParameters` has no
  `max_bitrate/max_framerate` fields. Encoder-level `set_bitrate`
  via REMB continues to cover the intent. Revisit when bumping
  webrtc-rs.

### Viewer controls

- **Scale** (`RcScaleMode` = `adaptive` / `original` / `custom`,
  persisted per browser): purely client-side CSS + input-coord
  mapping. New `directVideoNormalise` pure helper sits alongside
  `letterboxedNormalise` for the non-adaptive branches. Cursor
  overlay + synthetic badge are now scale-aware via the new
  `cursorMapping()` function (was `letterboxScale()` — only valid in
  Adaptive).
- **Fullscreen** toggle: `requestFullscreen` on the stage element;
  `fullscreenchange` listener flips the icon; ESC exits natively.
- **Remote Resolution** (`RcResolutionSetting`, persisted per
  `agentId`): new `rc:resolution` control-DC message, agent plumbs
  `TargetResolution` atomic shared between the DC handler and media
  pump, `apply_target_resolution` downscales via a CPU box filter
  (`downscale_bgra_box` — ~30 ms on 4K→1080p). **No SDP
  renegotiation needed** — SPS/VPS in the first frame after the
  rebuild carries the new size on the existing RTP track. Fit mode
  uses `ResizeObserver` on the stage, debounced 250 ms.

### In-field fixes from user testing

- **Ctrl+C → `©`, Backspace → `^H`** in pwsh (`input/enigo_backend.rs`):
  `hid_to_key` mapped letters/digits to `Key::Unicode(c)`; enigo
  routes that through `KEYEVENTF_SCANCODE` on Windows, which is
  layout-sensitive and drops modifier composition. Switched to
  `Key::Other(VK_A..VK_Z / VK_0..VK_9)` on Windows. Non-Windows
  unchanged (XTest/CGEventPost combine modifiers with Unicode fine).
- **"Clipboard worker gone"** on second clipboard:read
  (`clipboard.rs`): `Clipboard` is `Clone` (cheap Sender) but had
  a `Drop` impl that sent `Shutdown` on every clone drop — first
  closure-captured clone dropping killed the worker. Removed the
  Drop impl; Sender drop-to-refcount-0 ends `rx.recv()` cleanly.
- **Pipeline timing diagnostics** (`peer.rs:media_pump`): heartbeat
  log now reports `avg_capture_ms` / `avg_encode_ms` per 30-frame
  window, reset per window so transient stalls don't smear.
  WGC `SharedSlot` tracks `arrived_total` + `dropped_stale` and logs
  `capture cadence` every ~120 arrivals with a `drop_ratio_pct`.

## In-field 7-fps case (agent 69e3460311b4234e5f450ab5)

User A/B'd against RustDesk and reported 7-8 fps at 4K HEVC HW on the
RTX 5090 Laptop + Intel UHD 630 box. Diagnosis path added in 0.1.34;
confirmed cause: NVENC Blackwell H.264/HEVC/AV1 all fail `ActivateObject`
with `0x8000FFFF` (known, see 2026-04-20 Known Issue), cascade lands
on the Intel UHD 630 HEVC MFT which can't sustain 4K@30 let alone 60.

**Workaround shipped in 0.1.35**: user picks Remote Resolution = Fit
(1920×1080 or similar), agent CPU-downscales before encode, Intel UHD
630 HEVC holds 30-60 fps comfortably at that size. Typed as Known
Issue HIGH on 2026-04-22 with the proper fix deferred to Tier 1C.3
(GPU-side scale via `VideoProcessorMFT`) — see the current plan file.

## Live URLs + deploy

- Live: `https://roomler.ai/` — last K8s rollout at 2026-04-22 10:58
  CEST, UI includes all of the above.
- Agent MSI: https://github.com/gjovanov/roomler-ai/releases/tag/agent-v0.1.35
  (unsigned; install via the recipe in `remote-server.txt`).
- Test URLs used during verification:
  - `https://roomler.ai/tenant/69a1dbbad2000f26adc875ce/agent/69e2a1ee7af054f8a14e84c6/remote` — primary test agent
  - `https://roomler.ai/tenant/69a1dbbad2000f26adc875ce/agent/69e3460311b4234e5f450ab5/remote` — the 7-fps box (5090 + UHD 630)

## Next session priorities (ordered)

1. **Tier B7 — WebCodecs + canvas render path.** Highest-leverage
   remaining item. Use `RTCRtpScriptTransform` → `VideoDecoder` →
   offscreen canvas with `requestVideoFrameCallback` pacing. Bypasses
   Chrome's ~80 ms `<video>` jitter-buffer floor entirely. ~3 days
   prototype. If this works, Tauri-style native companion (Tier C) is
   optional.
2. **Intel UHD 630 fallback**. When the cascade lands on an iGPU
   HEVC MFT that can't sustain 4K, automatically downscale on the
   agent side (adaptive Resolution=Fit default) rather than waiting
   for the operator to notice and pick. Proper fix: `VideoProcessorMFT`
   for GPU-side scale. Temporary MVP: keep the per-session CPU box
   filter but default to half-resolution when the winning MFT matches
   an iGPU vendor.
3. **Adaptive Resolution driven by REMB**. If bandwidth estimate
   drops below a threshold, auto-step the agent-side capture
   resolution down (and back up when the link recovers). Couples into
   the existing `remb_bps` atomic.
4. **Dirty-rect ROI → encoder QP map** (Tier B11). `Frame::dirty_rects`
   already populated by WGC; plumb to MF `ICodecAPI_AVEncVideoROIEnabled`
   + `CODECAPI_AVEncVideoEncodeFrameTypeQP` per-MB delta-QP (dirty
   regions QP≈20, unchanged ≈42). Idle desktops should drop to <500
   kbps at visually-identical quality.
5. **Tier B10 — TWCC BWE decoded locally**. webrtc-rs 0.12 receives
   TWCC but doesn't expose the estimate. Decode
   `rtcp::transport_feedbacks::TransportFeedback` + a Google-CC-lite
   estimator. ~1 week of control-theory plumbing, but replaces the
   REMB-only path (Chrome's phasing REMB out).
6. **Multi-monitor capture selection**. `Frame::monitor` exists but
   the capture backends hardcode the primary. WGC supports per-monitor
   sessions (`CreateForMonitor(HMONITOR)` is what we already call);
   the control-DC needs an `rc:monitor` message + UI dropdown (the
   agent already advertises all monitors in `AgentHello.displays`).
7. **Agent auto-start on boot / login**. Windows: register a Scheduled
   Task at MSI install time ("At log on of any user") that launches
   `roomler-agent.exe run`. Linux: a systemd user unit in
   `~/.config/systemd/user/roomler-agent.service` + `loginctl enable-linger`.
   macOS: `LaunchAgent` plist in `~/Library/LaunchAgents/`. Agent
   binary should also gain a `service install / uninstall / status`
   CLI for manual setup.
8. **Agent version checker + auto-updater**. On startup and every
   ~6 h: GET `https://api.github.com/repos/gjovanov/roomler-ai/releases/latest`,
   parse tag, compare to `env!("CARGO_PKG_VERSION")`. If newer,
   download the platform-appropriate artifact (MSI / .deb / .pkg)
   to a temp dir, verify size, then spawn the installer detached
   (`msiexec /i path /qn /norestart` → schedules the replace; agent
   exits so the MSI can overwrite the binary, Scheduled Task relaunches
   the new build). Use the `self_update` crate for the cross-platform
   scaffolding. Surface "update available" in the admin UI via a new
   `agent_version` field on `AgentHello`.

## Resumption recipe

```powershell
# Rebuild agent locally for fast iteration:
Get-Process roomler-agent -ErrorAction SilentlyContinue | Stop-Process -Force
cargo build -p roomler-agent --release --features full-hw
$env:RUST_LOG = "roomler_agent=info,webrtc=warn"
.\target\release\roomler-agent.exe run
```

Deploy UI to prod (browser-side changes):

```bash
# In a tmux session attached to mars (/tmp/mars pattern from
# prior sessions — the ssh mars inside tmux). Then:
cd /home/gjovanov/roomler-ai && git pull && \
  cd /home/gjovanov/roomler-ai-deploy && \
  scripts/build-and-push-image.sh 2>&1 | tee /tmp/deploy.log
```

Cut an agent MSI release (Windows/Linux/macOS artifacts):

```bash
# Bump workspace.package.version in Cargo.toml, commit, then:
git tag agent-v0.1.36
git push origin master
git push origin agent-v0.1.36
# release-agent.yml builds + publishes automatically.
```

Find the winning MFT quickly when debugging fps regressions:

```powershell
$env:RUST_LOG = "roomler_agent=info,webrtc=warn"
.\target\release\roomler-agent.exe run 2>&1 | Select-String `
  "cascade winner|capture cadence|media pump heartbeat|media pump starting"
```

## Plan file

`~/.claude/plans/floating-splashing-nebula.md` is still live, tracking
the RustDesk-parity tiers. Tier A is now fully shipped; Tier B has
two items partially addressed (diagnostics in 0.1.34, per-frame ROI
plumbing on `Frame::dirty_rects` since 1C.2); Tier C (native companion
app) remains optional pending Tier B7 outcome.
