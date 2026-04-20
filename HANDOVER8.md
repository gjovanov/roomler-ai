# Handover #8 — Honest caps probe in 0.1.30

> Continuation of HANDOVER7 (which landed AV1 cascade + fail-closed
> semantics in 0.1.29). This session closes the "enumerate but
> fail-to-activate" false-advertising gap that made AV1 negotiation
> a session-breaker on the RTX 5090 Blackwell dev box.

## Shipped in 0.1.30 (1 follow-on commit on top of 0.1.29)

```
<this>   feat(agent): probe-at-startup for honest AgentCaps
```

## Why this matters

In 0.1.29, `caps::detect` advertised a codec whenever `MFTEnumEx`
found at least one matching MFT. But enumeration isn't the same as
activation — on RTX 5090 Blackwell the NVIDIA AV1 Encoder MFT
enumerates cleanly and then `ActivateObject` returns `0x8000FFFF`
on every call. Downstream consequences:

1. Agent advertises `av1` in `rc:agent.hello`.
2. Browser advertises AV1 in its `RTCRtpReceiver.getCapabilities`.
3. `pick_best_codec` picks AV1 (highest priority).
4. `AgentPeer::new` binds the track to `video/AV1`, pins
   `set_codec_preferences` to AV1, sends the SDP answer.
5. Media pump calls `open_for_codec("av1", …)` — cascade fails.
6. Fail-closed NoopEncoder: browser shows black video.

Correct behaviour is for the agent to NOT advertise AV1 in step 1,
so the intersection lands on HEVC instead, and the session works.

## What changed vs 0.1.29

| Layer | Change | Why |
|---|---|---|
| `encode/caps.rs` | `detect()` now returns cached `AgentCaps` from a `OnceLock`. First call runs `compute_caps`, which for HEVC + AV1 actually spins up `MfEncoder::new_hevc`/`new_av1` at 480×270 and only advertises the codec if activation succeeds. H.264 path unchanged — enumeration is sufficient because `open_default`'s triple-fallback (MF → openh264 → Noop) means H.264 always works as long as any encoder compiles in. | Closes the enumerate-but-fail-to-activate false-advertising gap for the two codecs where bitstream demotion is unsafe. Runtime cost: ~300-500ms per probe at first `rc:agent.hello`, cached for the rest of the process lifetime. |
| `main.rs` | New `caps` subcommand prints the probed `AgentCaps` and exits. Operators can run `roomler-agent caps` to verify what the agent will advertise on their host. | Debugging aid; also useful output for release-CI when adding codec-specific smoke gates. |
| `CLAUDE.md` | Updated remote-control status line to describe the codec-negotiation pipeline. Closed the TWCC/REMB adaptive-bitrate Known Issue (landed in 0.1.26, was still marked OPEN). Added new LOW Known Issue for the NVENC Blackwell 0x8000FFFF regression with "workaround shipped" status. | Honest documentation. |

## Verification on RTX 5090 Laptop + AMD Radeon 610M

- `cargo clippy -p roomler-agent -p roomler-ai-remote-control --features full-hw -- -D warnings` clean
- `cargo test -p roomler-agent --lib --features full-hw` → 54 passed
- `roomler-agent caps` on this box (full probe timing):
  - HEVC probe: activates via HEVCVideoExtensionEncoder in ~461ms → advertised
  - AV1 probe: all HW candidates fail `ActivateObject` in ~168ms → NOT advertised
  - **Result:** `codecs: ["h264", "h265"]`, `hw_encoders: ["openh264-sw", "mf-h264-hw", "mf-h265-hw"]`
- `encoder-smoke --codec h264` / `--codec h265` / `--codec av1` unchanged in behaviour from 0.1.29.

## Behavioural changes vs 0.1.29

1. **RTX 5090 Blackwell agents no longer advertise AV1.** On this
   dev box the caps probe now runs at first `rc:agent.hello`, finds
   AV1 can't activate, and returns `codecs: ["h264", "h265"]`
   instead of `["h264", "h265", "av1"]`. Browsers negotiate HEVC —
   sessions work where they previously failed closed.
2. **First `rc:agent.hello` takes ~300-500ms longer.** The cost is
   the HEVC + AV1 activation probes. Subsequent `rc:agent.hello`
   calls read from the cache and are instant. On hosts with no HW
   HEVC/AV1 (e.g. WARP VM) the probes skip entirely (enumeration
   returns 0 so no activation attempted).
3. **New `caps` subcommand.** Operators can run
   `roomler-agent caps` to print what the agent will advertise.
   Typical output on this box:
   ```
   codecs: ["h264", "h265"]
   hw_encoders: ["openh264-sw", "mf-h264-hw", "mf-h265-hw"]
   has_input_permission: true
   supports_clipboard: false
   supports_file_transfer: false
   max_simultaneous_sessions: 1
   ```

## What's still open after 0.1.30

| Task | Status | Why deferred |
|---|---|---|
| 2C.2 VideoToolbox HEVC | ⏳ | macOS only; untestable here. |
| 1A.2 Intel QSV async pipeline | ⏳ | No Intel QSV on this dev box. |
| 1C.1 WGC capture backend | ⏳ | ~500 LoC of Win32_Graphics_Capture FFI + WinRT apartment handling. The highest-leverage remaining Phase 1 item — unblocks dirty-rect + GPU downscale + full VFR. |
| 1C.3 GPU downscale | ⏳ | Depends on 1C.1. |
| NVENC Blackwell ActivateObject 0x8000FFFF | ⏳ | Workaround shipped (caps probe filters AV1, cascade routes around it for H.264/HEVC). Root-cause investigation deferred; worth a driver-update experiment or `CODECAPI_AVEncAdapterLUID` probe. |

## Ship order for 0.1.30

```bash
git push origin master
git tag agent-v0.1.30
git push origin agent-v0.1.30

# Once MSI is built (per ../remote-server.txt):
ssh mars && cd /home/gjovanov/roomler-ai-deploy && \
    ansible-playbook deploy.yml       # no server-side wire format changes
# On each controlled host:
#   1. Get-Process roomler-agent | Stop-Process -Force
#   2. Uninstall 0.1.29 MSI via Settings → Apps
#   3. Download + install 0.1.30 MSI
#   4. Smoke: roomler-agent caps     (new! prints what gets advertised)
#   5. Smoke: roomler-agent encoder-smoke --encoder hardware --codec h265
#   6. Start: roomler-agent run
```

## Files modified (summary)

- `agents/roomler-agent/src/encode/caps.rs` — `OnceLock` cache,
  probe-at-startup `activates()` helper, HEVC/AV1 gated on probe.
- `agents/roomler-agent/src/main.rs` — new `caps` subcommand.
- `Cargo.toml` — workspace version 0.1.29 → 0.1.30.
- `CLAUDE.md` — status line update, Known Issues refresh.
- `HANDOVER8.md` — this file.

## Next-session priority if continuing

**1C.1 WGC capture backend.** Still the biggest remaining Phase 1
item; unblocks dirty-rect plumbing (already wired through
`Frame::dirty_rects` + `set_roi_hints`) and GPU downscale.
Estimated ~500 LoC, 1-2 days. Requires:

- `windows-capture` crate or raw `windows` bindings for
  `Graphics_Capture` + `Graphics_DirectX_Direct3D11`
- `GraphicsCaptureItem::CreateFromMonitorHandle` + framepool + session
- WinRT apartment handling (init via `RoInitialize` on the capture thread)
- New `capture/wgc_backend.rs` implementing `ScreenCapture` trait
- `Frame::dirty_rects` populated from
  `Direct3D11CaptureFrame::DirtyRegion()` (Windows 11 22000+)

Alternative smaller targets:
- **Clipboard + file-transfer DC handlers** (Known Issue, MEDIUM).
  Today both channels are accepted but log-only. Implementing a
  real clipboard pipe would close a real user-facing gap for the
  remote-control feature.
- **NVENC Blackwell root-cause probe** via `CODECAPI_AVEncAdapterLUID`.
  Experimental; if it works, unblocks the RTX 5090's dGPU encoders
  for all three codecs (including AV1) on this box.
