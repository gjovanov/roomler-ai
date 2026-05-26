# HEVC over DataChannel — Option B Plan

Design and phased rollout for HEVC + HW VP9 encoder support in the Roomler agent
via `ffmpeg-next` (Option B). Replaces the Windows Media Foundation HEVC path
that fails on RTX 5090 Blackwell and Iris Xe Tiger Lake.

## Why

Two MF regressions block HEVC for our two known field hosts:

1. **RTX 5090 Blackwell**: `IMFActivate::ActivateObject` returns `0x8000FFFF`
   (`E_UNEXPECTED`) for HEVC, H.264, AND AV1 MFTs regardless of adapter
   binding. NVENC itself works — the MS Media Foundation wrapper is broken.
2. **Intel Iris Xe Tiger Lake**: HEVC MFT is async-only and our MF cascade
   doesn't handle the async path correctly. Unverified for HEVC specifically;
   the symptom on VP9 SW is CPU-bound 8–12 fps at 1920×1200.

FFmpeg dispatches to NVENC / Intel QSV (libmfx) / AMD AMF via the SAME C-level
SDKs, but exposes a unified `avcodec_send_frame` / `avcodec_receive_packet`
interface that bypasses MF entirely. RustDesk does this same thing (verified
2026-05-26 — see their `cpp/ffmpeg_vram/ffmpeg_vram_encode.cpp` + `cpp/nv/`).

Bonus: FFmpeg has `vp9_qsv` for Intel HW VP9 encode. Roomler currently has no
HW VP9 path (NVENC and AMF never added VP9 encode); Iris Xe gets to use its
iGPU encoder for VP9 4:2:0 too.

## What the browser sees

HEVC bytes ride the existing `data-channel-vp9-444` DataChannel transport
pattern — Annex-B framed NAL units behind a 13-byte length-prefix header,
decoded via `VideoDecoder({codec: 'hev1.1.6.L93.B0'})` with no description.

The pre-flight spike (2026-05-26, `docs/hevc-webcodecs-spike.html`) locked
the design:
- 30/30 frames round-tripped in Chrome (RTX 5090) with NO description
- Edge decoder supports HEVC (encoder unsupported but irrelevant — agent encodes)
- 4-byte start codes (`00 00 00 01`)
- IDR NAL order: AUD → VPS → SPS → PPS → IDR_N_LP

## Phased rollout (8–13 RCs over ~6 weeks)

Honest budget per critique #9 (1.6× historical multiplier). Each RC ships
independently with its own back-out window.

### rc.64 — Cargo feature flag + vcpkg port files (header-only)
Files: this RC declares `ffmpeg-encoder` in `agents/roomler-agent/Cargo.toml`,
adds the stub `src/encode/ffmpeg/mod.rs` (`available()` returns false), and
checks in port files at `vcpkg-ports/ffmpeg-roomler/`. The portfile emits
`FATAL_ERROR` if vcpkg tries to build it — CI doesn't reference it yet.
Back-out = revert one commit.

### rc.65 — CI plumbing + `ffmpeg-next` version verification
Files: `.github/workflows/release-agent.yml` gains a vcpkg install step that
exercises the port. `ffmpeg-next` + `ffmpeg-sys-next` added to Cargo.toml as
optional deps gated on `ffmpeg-encoder`. Build verifies `hevc_qsv` + `vp9_qsv`
symbols are reachable through the binding. Cache key composition:
`hash(vcpkg.json) + hash(portfile.cmake) + libmfx-version-pin`. MSI size delta
measured. **HARD STOP if >+8 MB compressed.**

### Gate 0 probe (no RC tag, debug build)
Operator runs `target/debug/roomler-agent encoder-smoke --backend ffmpeg --codec hevc`
on PC50045 (Iris Xe) and RTX 5090 host. Verify both produce output bytes.
**STOP and re-plan if either fails.**

### rc.66 — FFmpeg encoder backend + D3D11VA zero-copy
New `src/encode/ffmpeg/encoder.rs` implements existing `VideoEncoder` trait.
Encoder dispatch: hevc_nvenc → hevc_qsv → hevc_amf for HEVC, similar for
H.264/VP9. **D3D11VA zero-copy from day 1** (critique #6: do NOT defer this
to a later RC — late zero-copy refactors are the worst regression class).
BGRA→NV12 CPU SIMD via `dcv_color_primitives` as fallback for non-D3D11
capture sources. Behind `ROOMLER_AGENT_USE_FFMPEG=1` env var; MF cascade
remains default. MF unchanged.

### rc.67 — Caps probe + HEVC DC framer + anti-IDR-storm guard
`caps::detect` advertises `data-channel-hevc` when HEVC probe succeeds at
both 480×270 (startup) AND actual capture resolution (session-bind). New
`hevc_dc_framer.rs` reuses 13-byte header from VP9-444 path. IDR prepends
VPS+SPS+PPS in the Annex-B order the spike confirmed. VPS/SPS/PPS extraction
reuses logic from `crates/vendored/rtp/src/codecs/h265/mod.rs`. **Anti-IDR-
storm coalescer in this RC** (critique #11: do NOT defer — known regression
class from rc.33–35).

### rc.68 — Browser HEVC worker
Clone `ui/src/workers/rc-vp9-444-worker.ts` → `rc-hevc-worker.ts`. Codec
string `hev1.1.6.L93.B0`, no description (per spike). `useRemoteControl.ts`
gains `isHevcDecodeSupported()` + fallback chain HEVC-DC → VP9-444-DC.

### rc.69 — VP9 HW via `vp9_qsv` (Intel iGPU only)
Encoder dispatch adds `vp9_qsv` → libvpx fallback for codec=vp9. Intel iGPU
auto-engages HW path. chroma=4:4:4 stays libvpx SW (vp9_qsv is 4:2:0-only).
**Quality head-to-head test required before commit** (critique #13):
libvpx@1.5 Mbps vs vp9_qsv@2 Mbps on screen-content. Browser worker unchanged.

### rc.70 — Single codec-selector dropdown
Collapse chroma + codec selection into one dropdown. Options:
`Auto / HEVC HW / VP9 4:2:0 / VP9 4:4:4 / H.264 RTP`. `chroma_pref` wire
field stays accepted forever — no deprecation collision (critique #4).
Add `codec_pref` field alongside.

### rc.71 — AIMD tuning (single concern)
Codec-aware `MIN_BITRATE_BPS` floors. IDR cadence per codec. AIMD constants
tuned for HEVC's per-bit quality elbow.

### rc.72, rc.73 — AIMD field hotfixes (expected)
Per 1.6× historical multiplier (rc.36 → rc.40 took 5 RCs for VP9 AIMD).
Specific failure modes likely: bitrate oscillation, REMB ramp-up on HEVC
keyframes, vp9_qsv quality cliff.

### rc.74 — Linux VAAPI support
Extend `ffmpeg-encoder` feature to non-Windows. Add `hevc_vaapi` + `vp9_vaapi`
to dispatch. vcpkg port adds `--enable-libdrm --enable-vaapi` for Linux
triplet. Critique #8 fix: closes Option B's "cross-platform" selling point.

### rc.75 — macOS VideoToolbox support
Add `hevc_videotoolbox` to dispatch. vcpkg port adds `--enable-videotoolbox`
for darwin triplet.

## What if Gate 0 fails?

If MF HEVC also fails via FFmpeg's QSV path on Iris Xe (i.e., the underlying
libmfx driver itself is the bug, not just MF), Option B's value for Iris Xe
collapses. Counter-recommendation kicks in: ship a 300-LOC direct-NVENC
patch for Blackwell only (`nvEncodeAPI64.dll` runtime load), no FFmpeg, no
MSI growth. Iris Xe stays on libvpx VP9 SW. We re-plan based on actual
field demand.

## Distribution model

Same as RustDesk's pattern:

- **FFmpeg**: statically linked into `roomler-agent.exe` (no DLL ships)
- **libmfx** (Intel oneVPL): statically linked (~3–5 MB)
- **NVENC** (`nvEncodeAPI64.dll`): runtime-loaded from NVIDIA driver
- **AMF** (`amfrt64.dll`): runtime-loaded from AMD driver

Net MSI growth target: ~6 MB compressed (8 MB → ~14 MB agent MSI).

## References

- Pre-flight HEVC WebCodecs spike: [`docs/hevc-webcodecs-spike.html`](./hevc-webcodecs-spike.html)
- Custom vcpkg port: [`agents/roomler-agent/vcpkg-ports/ffmpeg-roomler/`](../agents/roomler-agent/vcpkg-ports/ffmpeg-roomler/)
- VP9 4:4:4 sibling design: [`docs/vp9-444-plan.md`](./vp9-444-plan.md)
- Remote-control architecture: [`docs/remote-control.md`](./remote-control.md)
- RustDesk's equivalent (private repo `rustdesk-org/hwcodec`):
  https://github.com/rustdesk-org/hwcodec
- Known Issues (MF Blackwell regression): `CLAUDE.md` § Known Issues
