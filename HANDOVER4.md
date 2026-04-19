# Handover #4 — Streaming Options Phases 1 & 2

> Continuation of HANDOVER3 (which itself handed off the MF-HW
> encoder Phase 3 work). HANDOVER4 captures the state after the
> 2026-04-20 session that landed Phase 3 + most of streaming-options
> Phase 1 (Option A) + the Phase 2 (Option B) capability-reporting
> half. Documents the still-open work so the next session has
> uncomplicated scope.

## Current state — what shipped this session (16 commits ahead of HANDOVER3)

```
38a7837  feat(mf-encoder): probe-and-rollback cascade with async unlock      (1A.1)
c90ca15  feat(ui/remote-control): quality selector + live stats readout      (1G.1 browser)
775dbda  docs: streaming-options.md - phase 1+2 roadmap                       (docs)
039f3bc  feat(mf-encoder): auto -> MF-HW first on Windows + close HIGH issue (1A.3)
a575101  feat(mf-encoder): tight VBV + gradual intra refresh                 (1B.1)
baa4b3c  feat(agent): wire control DC to rc:quality bitrate clamp            (1G.2)
d2f94a2  feat(agent): REMB-driven adaptive bitrate + openh264 runtime control (1F.2)
22277b2  feat(agent): NACK-burst -> reference-frame invalidation hook        (1D.2)
a7218cc  feat(agent): VFR step-1 - bump idle keepalive to 1 fps              (1F.1)
3309f87  feat(agent): Frame::dirty_rects + set_roi_hints trait hook          (1C.2 + 1D.1)
b3a2642  feat(agent+ui): codec capability detection + AgentsSection chips    (2A.1 + 2A.2)
0e16518  feat(rc): browser codec caps inspection + wire                       (2B.1)
eea24f1  feat(rc): codec badge polish + HW HEVC capability enum               (2D.1 + caps)
<this>   chore: bump workspace version 0.1.25 → 0.1.26 + HANDOVER4           (this commit)
```

Status: **0.1.26 ready to tag**. Browser-side and capability-reporting
half of Phase 2 ships in the same release because they're harmless on
agents that don't yet do codec negotiation (the field defaults to
empty, agent ignores until 2B.2 lands).

| Plan task | Status | Notes |
|---|---|---|
| 1A.1 | ✅ landed 38a7837 | RTX 5090 + AMD 610M validated |
| 1A.2 | ⏳ deferred | Intel QSV async pipeline; needs an Intel-host validation box |
| 1A.3 | ✅ landed 039f3bc | Auto cascade now MF-HW first on Windows |
| 1B.1 | ✅ landed a575101 | Slice-count knob deferred (windows-rs 0.58 lacks the GUID) |
| 1C.1 | ⏳ deferred | WGC capture backend (~500 LoC of Windows.Graphics.Capture FFI) |
| 1C.2 | ✅ landed 3309f87 | Frame::dirty_rects field; populated by WGC when 1C.1 lands |
| 1C.3 | ⏳ deferred | GPU downscale via VideoProcessorMFT — depends on 1C.1 |
| 1D.1 | ✅ landed 3309f87 | trait + plumbing; backend overrides land with 2C.1 |
| 1D.2 | ✅ landed 22277b2 | NACK-burst → invalidation hook; collapses to keyframe today |
| 1E.1 | ⏳ deferred | Cursor capture (GetCursorInfo + GetIconInfo + GetDIBits) |
| 1E.2 | ⏳ deferred | Cursor data-channel wire format |
| 1E.3 | ⏳ deferred | Browser cursor canvas overlay |
| 1F.1 | ✅ partial a7218cc | Idle keepalive 500ms→1s; full VFR (encode-only-when-dirty) needs 1C.2 + dirty rects from WGC |
| 1F.2 | ✅ landed d2f94a2 | REMB-driven; TWCC BWE not exposed by webrtc-rs 0.12; openh264 set_bitrate fixed via raw FFI |
| 1G.1 | ✅ landed c90ca15 | UI complete |
| 1G.2 | ✅ landed baa4b3c | Agent control-DC + bitrate clamp |
| 2A.1 | ✅ landed b3a2642 | encode/caps.rs probes openh264 + MF H.264 + MF HEVC |
| 2A.2 | ✅ landed b3a2642 | AgentResponse + agents.ts + AgentsSection chips |
| 2B.1 | ✅ landed 0e16518 | inspectBrowserVideoCodecs + ClientMsg::SessionRequest browser_caps |
| 2B.2 | ⏳ deferred | SDP codec ordering + pre-encoded RTP track for HEVC/AV1 |
| 2C.1 | ⏳ deferred | MF HEVC backend (parallel to mf/sync_pipeline.rs + HEVC RTP packetizer) |
| 2C.2 | ⏳ deferred | VideoToolbox HEVC (macOS — untestable on this box) |
| 2C.3 | ⏳ deferred | AV1 path (best-effort, RTX 5090 supports it but probes-and-fails on most drivers today) |
| 2D.1 | ✅ landed eea24f1 | Codec badge + HW HEVC capability detection |
| Phase 1 milestone | ⏳ tag + deploy | After this commit: `git tag agent-v0.1.26 && git push --tags` to fire release-agent.yml |
| Phase 2 milestone | ⏳ tag + deploy | After 2B.2 + 2C.1 land |

## Verification on RTX 5090 Laptop + AMD Radeon 610M (this dev box)

- `cargo clippy --workspace -- -D warnings` clean on `--features full-hw`
- `cargo test -p roomler-agent --lib --features full-hw` → 32 passed
- `cargo test -p roomler-ai-remote-control --lib` → 23 passed (wire format locked)
- `cd ui && bun run test:unit` → 285 passed
- `cd ui && bunx vue-tsc --noEmit` → clean
- `cd ui && bun run build` → clean
- `roomler-agent.exe encoder-smoke --encoder hardware`:
  - cascade enumerates 2 adapters + 5 H.264 MFTs
  - winner: AMD Radeon 610M + H264 Encoder MFT
  - 1 keyframe + 9 P-frames, 4212 bytes total
- `roomler-agent.exe encoder-smoke --encoder auto` picks `mf-h264`
- `ROOMLER_AGENT_HW_AUTO=0 roomler-agent.exe encoder-smoke --encoder auto` falls back to `openh264`

## Remaining open work (for the next session)

### High-leverage: 2B.2 — SDP codec ordering + pre-encoded RTP track

The capability handshake is wired (agent advertises codecs, browser
advertises codecs, server passes both). What's missing is the agent
actually picking H.265/AV1 when both ends support it. The challenges:

1. webrtc-rs's `TrackLocalStaticSample` is hard-coded to H.264. For
   HEVC/AV1 you need `TrackLocalStaticRTP` and own the packetization
   yourself — RFC 7798 (HEVC) and draft-ietf-avtcore-rtp-av1-07
   (AV1) for NAL/OBU fragmentation.
2. `RTCRtpTransceiver::set_codec_preferences` exists in webrtc-rs
   but is fragile; the alternative is hand-munging the SDP offer
   string before sending. Both work in practice.
3. Negotiation flow: when `rc:session.request` arrives with
   `browser_caps`, intersect with `AgentCaps.codecs`, pick the
   highest-priority one (av1 > h265 > h264), build the matching
   encoder + RTP track, advertise only that codec in the SDP.

Estimated: 1-2 days. **Prerequisite for shipping HEVC.**

### High-leverage: 2C.1 — MF HEVC backend

Mostly copy `mf/sync_pipeline.rs` to `mf/hevc.rs` with these changes:
- output type GUID: `MFVideoFormat_HEVC` instead of `MFVideoFormat_H264`
- output mime: `"video/H265"` (or `"video/hevc"`)
- omit `CODECAPI_AVEncH264CABACEnable` (HEVC always uses CABAC)
- everything else — adapter cascade, async unlock, VBV + intra
  refresh — applies identically

The hard part is the HEVC RTP packetizer mentioned above (2B.2).
Estimated: 2-3 days including packetizer.

### High-leverage: 1C.1 — WGC capture backend

Replace `scrap` on Windows with Windows.Graphics.Capture so we
get per-frame dirty regions. Once landed, 1F.1 step-2 (encode-
only-when-dirty) and 1D.1 (real ROI delta-QP) light up
automatically — both are wired to consume `Frame::dirty_rects`.

Add `windows-capture` (third-party, maintained) or raw `windows`
crate bindings (Graphics_Capture, GraphicsCaptureSession,
Direct3D11CaptureFramePool, GraphicsCaptureItem::CreateFromMonitorHandle).
Cargo.toml workspace gains a `wgc-capture` feature gate folded into
`full-hw`. ~500 LoC. Estimated: 1-2 days.

### Medium-leverage: 1E.1-3 — Cursor overlay

Today the browser renders an initials badge as a synthetic cursor.
Real cursor capture lets controllers see arrow / text-cursor /
resize-cursor shapes from the host.

- Agent: GetCursorInfo + GetIconInfo + GetDIBits → CursorInfo
  struct with shape ARGB bitmap + hotspot. Cache by HCURSOR
  handle so we send the bitmap once per cursor change, not per
  frame.
- Wire: new `cursor` data channel (reliable+ordered) with
  CursorMsg::{Shape, Pos, Hide} JSON.
- Browser: useRemoteControl subscribes, RemoteControl.vue renders
  a `<canvas>` overlay (replacing the initials badge for
  single-controller; keep initials as a multi-controller marker).

Estimated: 1-2 days. Pure-quality polish, not a Phase 1 blocker.

### Medium-leverage: 1A.2 — Async pipeline for Intel QSV

Today async-only MFTs route to `MfInitError::AsyncRequired` and
the cascade falls through. Implementing the async pipeline:

- new `mf/async_pipeline.rs`
- `IMFAsyncCallback` via `#[implement(IMFAsyncCallback)]` macro
- event worker thread calling `IMFMediaEventGenerator::BeginGetEvent`
- mpsc bridge to the existing pinned worker
- handles `METransformNeedInput` / `METransformHaveOutput`

**Untestable on this box** (no Intel iGPU). Validation requires a
QSV-equipped Win11 host. Write the code defensively, validate via
`encoder-smoke --encoder hardware` once an Intel box is available.
Estimated: 1-2 days.

### Low-leverage: 2C.3 — AV1 path

`CLSID_MSAV1EncoderMFT` ships on Windows 11 24H2+ with compatible
GPUs. RTX 5090 has HW AV1 encode but driver match is finicky. The
plan said "best-effort: probes-and-fails cleanly on GTX 1650";
RTX 5090 may actually succeed on the right driver version. AV1
RTP packetization is a draft spec (draft-ietf-avtcore-rtp-av1-07)
that webrtc-rs doesn't ship. Big chunk of work for a small
audience until AV1 decode is widespread on receivers.

Estimated: 3-4 days including packetizer. Skip until 2B.2 + 2C.1
have baked.

### Low-leverage: 1C.3 — GPU-side downscale via VideoProcessorMFT

Replace the CPU-side 2× box filter with `CLSID_VideoProcessorMFT`
chained upstream of the encoder MFT. Saves ~15 ms / frame at 4K
on the capture stage. **Requires WGC backend (1C.1) first** —
the D3D11 texture path doesn't work with scrap's CPU-readback
buffers. Estimated: 1 day after 1C.1.

### Untestable: 2C.2 — VideoToolbox HEVC (macOS)

Requires a Mac dev box. Out of scope for this Win11 session.

### Operational: Phase 1 / Phase 2 milestone deploys

Tag + push to fire `release-agent.yml`:

```bash
git tag agent-v0.1.26
git push origin agent-v0.1.26
```

CI builds the signed MSI on `windows-latest`. Install via
`../remote-server.txt` recipe. End-to-end test against
`https://roomler.ai/tenant/.../agent/.../remote`.

Server-side signalling additions in this release:
- `AgentResponse.capabilities` (additive, no breaking change)
- `ClientMsg::SessionRequest.browser_caps` (additive, defaults empty)

Both are forward-compatible — old controllers + new server work,
new controllers + old server work, except the new agent's caps
won't be displayed until both server + UI are deployed. **Deploy
order: server → agent** (new server tolerates old agent payload;
new agent assumes new server).

To deploy server: `ssh mars && cd /home/gjovanov/roomler-ai-deploy
&& <ansible playbook>`.

## Things that surprised me this session (for future-self)

1. **MS SW MFT silently delegates to async HW** even when
   `MF_TRANSFORM_ASYNC` reads false. The blanket
   `MF_TRANSFORM_ASYNC_UNLOCK` (regardless of flag) was the fix
   — see commit 38a7837 + the inline comment. Bit me for ~30
   minutes during 1A.1 verification.
2. **NVIDIA NVENC ActivateObject still returns 0x8000FFFF** on
   Blackwell + current drivers + adapter-bound D3D device. The
   cascade correctly skips and lands on AMD; investigation
   deferred. Worth a fresh look during 1A.2 — async path may
   change activation semantics.
3. **windows-rs 0.58 doesn't export several CODECAPI GUIDs** the
   plan assumed (slice count, force-intra-refresh-period,
   per-frame ROI map). The boolean enables (`AVEncVideoROIEnabled`,
   `AVEncVideoGradualIntraRefresh`) are there; the per-frame
   setters are not. Re-evaluate when bumping windows-rs.
4. **VS Build Tools is a multi-attempt install:** Start-Process
   re-quoting of `--installPath` paths with spaces is broken.
   Use `--productId` + `--channelId` instead. Documented in
   memory `feedback_no_command_approval.md`.
5. **NVIDIA RTX 5090 Laptop, not GTX 1650:** HANDOVER3 was
   wrong about the dev box hardware. Saved to memory
   `user_hardware.md`.

## Reference material

- Plan: `~/.claude/plans/floating-splashing-nebula.md`
- Streaming options research: `streaming-options.md` (root)
- Phase 3 design: `docs/remote-control.md` §17
- Hardware notes: see `feedback_windows_mf_encoder_pitfalls.md`
  (on Linux dev box) for cumulative MFT lessons
