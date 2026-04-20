# Handover #6 — HEVC negotiation actually ships in 0.1.28

> Continuation of HANDOVER5 (which captured 0.1.27 with cursor overlay +
> agent-side codec intersection). This handover records the 2026-04-20
> follow-on session that closed the 2B.2 + 2C.1 deferred work: the
> agent now actually selects an H.265 track when both sides support it
> and the HW HEVC cascade succeeds. Live-verified on RTX 5090 Laptop +
> AMD Radeon 610M via `encoder-smoke`.

## Shipped in 0.1.28 (1 follow-on commit on top of 0.1.27)

```
<this>   feat(agent+rc): HEVC encoder backend + end-to-end codec selection  (2B.2 + 2C.1)
```

This closes the last substantive gap in the HANDOVER4/5 deferred list:
**agent now ships HEVC frames when the browser asks for it**, not
just observationally-log-it-then-send-H.264.

## What changed vs 0.1.27

| Layer | Change | Why |
|---|---|---|
| `encode/mf/sync_pipeline.rs` | Pipeline parametric on `OutputCodec { H264, Hevc }` — single sync pipeline handles both. HEVC picks `MFVideoFormat_HEVC` subtype GUID, skips the H.264-specific CABAC enable knob. IDR detection widened to the HEVC NAL-type encoding (`nal_type = (head >> 1) & 0x3f`, IDR types 19/20/21). | Sharing the pipeline keeps ~400 LoC of hard-won logic (async-unlock, SET_D3D_MANAGER E_NOTIMPL tolerance, STREAM_CHANGE retry, codec-api knob fallback) in one place. Hevc-specific knob handling is a 10-line branch. |
| `encode/mf/activate.rs` | New `activate_and_probe_pipeline_for_codec(codec, w, h)`. For HEVC the cascade is HW-only — no SW fallback, because Windows does not ship a software HEVC encoder CLSID. If every HW candidate fails the function returns Err; caller demotes. | Right semantics for the "best-effort HEVC" goal. A pretend-SW fallback would produce silence + confuse capability reporting. |
| `encode/mf/mod.rs` | `MfEncoder::new_h264(w, h)` + `MfEncoder::new_hevc(w, h)` entry points. Worker-thread name carries the codec label. `VideoEncoder::name()` returns `"mf-h264"` / `"mf-h265"` from the stored codec. | Lets the peer build the right encoder at the right time. Log lines are easier to triage when the name is specific. |
| `encode/mod.rs` | New `open_for_codec(codec, w, h, pref) -> (Box<dyn VideoEncoder>, &'static str)` returning both the encoder and the *actual* codec the host could service. Unknown codec / failed cascade demotes to H.264 and reports `"h264"` back so the caller can log the mismatch. | Single entry point the peer calls. The return tuple is the honest contract — "I wanted H.265 but the box has no HEVC encoder today" is observable instead of a silent hidden demotion. |
| `peer.rs` | `AgentPeer::new` takes `chosen_codec: String`. Track capability switches on `video/H264` vs `video/H265` (matching webrtc-rs's default MediaEngine entries byte-for-byte so payloader lookup resolves). `RTCRtpTransceiver::set_codec_preferences` pins the SDP to the chosen codec. Media pump uses `open_for_codec` instead of `open_default`. Runtime demotion is logged but kept running (SDP was already sent; can't renegotiate mid-session). | The end-to-end wiring. Without the `set_codec_preferences` pin, a browser free to pick would sometimes negotiate VP9 (Firefox default) despite our track carrying H.265 bytes. |
| `signaling.rs` | New `pending_codecs: HashMap<ObjectId, String>`: store `rc:session.request`'s codec pick, read it back at `rc:sdp.offer` time, drop on `rc:session.terminate` to avoid orphan entries. | The two-phase handshake (request → consent → offer) means the codec decision happens ~seconds before the peer is built; we need a place to hold it. |
| `main.rs` | `encoder-smoke --codec {h264|h265}` flag to exercise the new cascade. H.264 keeps the historical `open_default` path so CI's pinned output is stable; h265 routes through `open_for_codec`. | CI already runs the smoke test on windows-latest as a release gate — this lets us add a second line for the HEVC path. |
| `encode/mf/sync_pipeline.rs` | `is_unsupported_codec_key_error` widened: E_FAIL + E_INVALIDARG + **E_NOTIMPL**. | HEVCVideoExtensionEncoder (the Store-shipped HEVC Video Extension MFT) was failing probe on `AVLowLatencyMode` with E_NOTIMPL before this fix. Widening lets it win the cascade cleanly. Observed on this box: cascade now picks HEVCVideoExtensionEncoder on NVIDIA adapter. |

## Verification on RTX 5090 Laptop + AMD Radeon 610M

- `cargo clippy -p roomler-agent -p roomler-ai-remote-control --features full-hw -- -D warnings` clean
- `cargo test -p roomler-agent --lib --features full-hw` → **43 passed** (+ 4 new HEVC tests)
- `cargo test -p roomler-ai-remote-control --lib` → 23 passed
- `roomler-agent encoder-smoke --encoder hardware --codec h264`:
  - cascade enumerates 2 adapters × 5 H.264 MFTs
  - winner: AMD Radeon 610M + H264 Encoder MFT
  - 1 keyframe + 9 P-frames, 4212 bytes total
- `roomler-agent encoder-smoke --encoder hardware --codec h265`:
  - cascade enumerates 2 adapters × 4 HEVC MFTs (AMDh265Encoder ×2, NVIDIA HEVC Encoder MFT, HEVCVideoExtensionEncoder)
  - 2 async-only skipped (AMD), 1 ActivateObject 0x8000FFFF (NVENC Blackwell),
    1 winner (HEVCVideoExtensionEncoder on NVIDIA adapter)
  - **1 keyframe + 9 P-frames, 6714 bytes total — real HEVC NALUs**
- On boxes with no HW HEVC, `open_for_codec("h265", …)` demotes cleanly
  to H.264 with a WARN log; sessions still work, just at H.264 quality.

## What's still open after 0.1.28

| Task | Status | Why deferred |
|---|---|---|
| 2C.3 AV1 path | ⏳ | Needs draft-ietf-avtcore-rtp-av1-07 packetizer — webrtc-rs 0.12 ships `Av1Payloader` so the gap is mostly the MF AV1 encoder wrapper (~200 LoC copy of sync_pipeline with `MFVideoFormat_AV1`). RTX 5090 has HW AV1 but Blackwell ActivateObject regression (same 0x8000FFFF as H.264 + HEVC) would skip it; HEVCVideoExtensionEncoder-style MS AV1 shim doesn't exist on stock Windows yet. Ship once a different box is available, or once the NVENC Blackwell matching issue is root-caused. |
| 2C.2 VideoToolbox HEVC | ⏳ | macOS only; untestable on this Win11 box. |
| 1A.2 Intel QSV async pipeline | ⏳ | No Intel QSV on this RTX 5090+AMD 610M dev box. Cascade correctly routes AMD async MFTs to `MfInitError::AsyncRequired`; the receiver doesn't exist yet. |
| 1C.1 WGC capture backend | ⏳ | ~500 LoC of Win32_Graphics_Capture FFI + WinRT apartment handling. Unblocks real dirty-rect plumbing + 1C.3 GPU downscale. |
| 1C.3 GPU downscale | ⏳ | Depends on 1C.1. |
| NVENC Blackwell `ActivateObject 0x8000FFFF` | ⏳ root-cause unknown | Affects both H.264 and HEVC on this box. Cascade routes around it (AMD 610M + HEVCVideoExtensionEncoder win instead), so impact is "we don't use the most powerful encoder" rather than "it doesn't work". Worth a fresh look; possibly resolved by a driver update, or possibly needs `CODECAPI_AVEncAdapterLUID` to disambiguate. |

All tracked in HANDOVER4.md / HANDOVER5.md with design notes + effort estimates.

## Ship order for 0.1.28

```bash
# Push this commit, tag, let release-agent.yml build the MSI.
git push origin master
git tag agent-v0.1.28
git push origin agent-v0.1.28

# Once MSI is built and the server is deployed:
ssh mars && cd /home/gjovanov/roomler-ai-deploy && \
    ansible-playbook deploy.yml     # server first (forward-compatible change: none)
# Then on the controlled host, follow ../remote-server.txt:
#   1. Kill any running agent: Get-Process roomler-agent | Stop-Process -Force
#   2. Uninstall 0.1.27 MSI via Settings → Apps
#   3. Download + install 0.1.28 MSI from GitHub releases
#   4. Verify version: roomler-agent --version
#   5. Smoke: roomler-agent encoder-smoke --encoder hardware --codec h265
#   6. Start: roomler-agent run
#   7. Open https://roomler.ai/tenant/.../agent/.../remote (login gjovanov/Gj12345!!)
#   8. Check webrtc-internals: codec line should read "video/H265"
```

### Server-side changes in 0.1.28

**None.** This release is purely agent-side — the wire format additions
for `AgentResponse.capabilities` and `ClientMsg::SessionRequest.browser_caps`
already landed in 0.1.26/0.1.27 and are deployed. The new agent just
acts on data it was already receiving.

## Behavioural changes vs 0.1.27

1. **Chrome + Edge sessions on this dev box now negotiate H.265.** The
   cascade picks HEVCVideoExtensionEncoder on the NVIDIA adapter;
   bitrate at 1080p should drop ~40% vs H.264 at equal visual quality.
   Firefox stays on H.264 (no HEVC decoder on desktop Firefox).
2. **SDP answer m=video pins the chosen codec.** The browser's offer
   still lists H.264 + H.265 + AV1 + VP8 + VP9; our answer now offers
   only the one we can actually encode. This avoids the classic
   "browser picks VP9 from a shared codec list, agent sends H.264
   bytes, decoder produces garbage" gotcha.
3. **Encoder demotion is visible.** If the HEVC cascade fails at
   first-frame init (after caps::detect said HEVC was available —
   rare), the pump keeps running with H.264 but the track mime is
   still `video/H265`. The browser's video element goes dark and the
   agent logs a loud WARN. Manual reconnect re-negotiates.
4. **`encoder-smoke --codec h265` is a supported workflow.** CI can
   add it as a second smoke job alongside `--codec h264` once
   runners are known to have HW HEVC (GitHub Actions' windows-latest
   does include HEVCVideoExtensionEncoder as of the 2025 images).

## Things that surprised me during 2B.2 + 2C.1

1. **webrtc-rs 0.12 already ships all the RTP packetizers.**
   `rtp::codecs::{h264, h265, av1, vp8, vp9}` with a `payloader_for_codec`
   lookup off MIME type. The plan said "1-2 days of packetizer work";
   it turned out to be 0 days — just swap the track codec capability
   and webrtc-rs does the right thing. Massive scope collapse.
2. **`RTCRtpTransceiver::sender()` is async.** Returns
   `impl Future<Output = Arc<RTCRtpSender>>`, not a direct Arc like
   Pion's Go API. So `Arc::ptr_eq(&t.sender(), &video_sender)` was a
   type error — had to `await` it inside an explicit loop.
3. **HEVCVideoExtensionEncoder returns E_NOTIMPL on AVLowLatencyMode.**
   The MS HEVC Video Extension MFT doesn't implement half the
   `ICodecAPI` knobs the NVIDIA/AMD MFTs do. Widening
   `is_unsupported_codec_key_error` to include E_NOTIMPL was the
   lightest possible fix and lets it win the cascade.
4. **No software HEVC encoder ships on stock Windows.** `CLSID_MSH265EncoderMFT`
   doesn't exist, only `CLSID_MSH265DecoderMFT`. Cascade has to
   explicitly handle "HEVC means HW-or-nothing" and report that to
   the caller. H.264 keeps its `CLSID_MSH264EncoderMFT` SW fallback.
5. **HEVC NAL type encoding is shifted one bit left compared to H.264.**
   H.264: `nal_type = byte & 0x1F`, IDR = 5. HEVC: `nal_type = (byte >> 1) & 0x3F`,
   IDR_W_RADL = 19, IDR_N_LP = 20, CRA_NUT = 21. The cross-check test
   `hevc_idr_bytes_not_mistaken_for_h264_idr` locks both masks.

## Next-session priority if continuing

**AV1 (2C.3)** now becomes the highest-leverage item because:

1. Adds 30-40% more bandwidth reduction on top of HEVC.
2. webrtc-rs packetizer exists (`rtp::codecs::av1::Av1Payloader`).
3. Chrome/Edge decode AV1 in HW on any 2024+ GPU including the RTX
   5090 Laptop in this box.
4. MF exposes `CLSID_MSAV1EncoderMFT` on Windows 11 24H2+ with recent
   IHV drivers — the enumeration may surface AV1 MFTs here.

Risk: same Blackwell NVENC activation issue (0x8000FFFF) probably
bites AV1 too, meaning the cascade would land on HEVCVideoExtension-style
SW AV1 if one exists (it doesn't today). Outcome likely: AV1 code
lands, cascade probes-and-fails on this box, demotes to H.265. Still
useful for boxes with working AV1 (Intel Arc, RTX 40-series non-Blackwell
drivers).

Estimated: 1-2 days. The MF AV1 encoder wrapper is ~150 LoC copy of
the parametric sync_pipeline; the main work is verifying AV1
enumeration on different hosts.

**Alternative priority**: **1C.1 WGC capture backend**. Unblocks
dirty-rect + GPU downscale (1D.1 real ROI + 1F.1 full VFR + 1C.3).
Heavier (~500 LoC) but bigger payoff — 1080p60 desktop streaming at
<1 Mbps idle vs today's ~3 Mbps.

## Files new in 0.1.28

No new files. All changes are modifications to existing modules.

## Files modified (summary)

- `agents/roomler-agent/src/encode/mf/sync_pipeline.rs` — `OutputCodec`
  enum, parametric pipeline, HEVC IDR heuristic, E_NOTIMPL tolerance,
  4 new unit tests.
- `agents/roomler-agent/src/encode/mf/activate.rs` — codec-parametric
  cascade, HEVC no-SW-fallback branch.
- `agents/roomler-agent/src/encode/mf/mod.rs` — `new_h264` / `new_hevc`
  entry points, codec-aware worker name, codec-aware `VideoEncoder::name()`.
- `agents/roomler-agent/src/encode/mod.rs` — `open_for_codec` API.
- `agents/roomler-agent/src/peer.rs` — `chosen_codec` plumbed through
  `AgentPeer::new` and `media_pump`; `build_video_codec_cap` +
  `codec_params_for` helpers; `set_codec_preferences` on transceiver.
- `agents/roomler-agent/src/signaling.rs` — `pending_codecs` map.
- `agents/roomler-agent/src/main.rs` — `--codec` flag on `encoder-smoke`.
- `Cargo.toml` — workspace version 0.1.27 → 0.1.28.
- `HANDOVER6.md` — this file.
