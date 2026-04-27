# VP9 4:4:4 over RTCDataChannel — implementation plan

Status: **active, 2026-04-26**.
Tracking: tasks #73-#76.
Supersedes the AV1 4:4:4 sketch from earlier discussion. Field-driven by
the 2026-04-24/-26 reports of blurry text + chroma artefacts on the
existing MF-H265 / MF-H264 path.

## Goal

Match RustDesk-class screenshare quality (sharp text, no chroma
fringing, smooth motion) **inside the existing roomler-agent →
browser session** without forking off into a custom protocol or
embedding Chromium.

Concretely: deliver VP9 profile 1 (8-bit 4:4:4) frames from the
agent to the browser, decode in-browser via WebCodecs, render to
canvas. No browser WebRTC video pipeline involvement — that path
enforces 4:2:0 in every codec it accepts.

## Why VP9 4:4:4 and not AV1 / HEVC

| Codec | Encoder | Browser decoder | Verdict |
|-------|---------|-----------------|---------|
| **VP9 profile 1** | libvpx via `vpx-encode` Rust FFI — mature since 2014, screen-content tuned | `VideoDecoder({codec:'vp09.01.10.08'})` — Chrome ships libvpx, decodes any profile reliably | ✅ chosen |
| HEVC Main 4:4:4 | MF HEVC encoders are 4:2:0 only; libx265 SW is heavy | Chrome's HEVC WebCodecs path is OS-platform-gated; 4:4:4 rarely supported by Apple VTB / Win HEVC Extension | ❌ |
| AV1 profile 1 | libaom 4:4:4 is ~50% more CPU than VP9 at same res | OK on Chrome via libdav1d | ✅ viable but heavier |

VP9 4:4:4 is what RustDesk uses. Production-validated for years,
royalty-free, and `cpu-used=8` keeps it real-time on a 12-core CPU
at 1080p 30fps.

## Why DataChannel and not WebRTC video track

Browser WebRTC video track pipeline enforces **4:2:0 across every
codec** (Chrome's RTP depayloaders strip or refuse profile 1 / Main
4:4:4 / High 4:4:4 frames). The only way to push 4:4:4 bytes to a
browser decoder is to bypass that pipeline.

`RTCDataChannel` is opaque-byte transport with full retransmission
(SCTP) and DTLS encryption, riding on the same ICE candidate pair we
already negotiate. The browser-side worker reads bytes off the
channel, wraps them in `EncodedVideoChunk`, and feeds `VideoDecoder`
— which accepts any profile because it's not in the WebRTC video
constraint set.

Trade-offs:

- **Lose**: TWCC video congestion control, NACK retransmission for
  video frames (DataChannel gets SCTP retransmission instead — different
  loss/latency profile, generally fine for sub-2 % loss)
- **Lose**: existing media-pump REMB hysteresis, `set_bitrate`
  feedback loop. Replace with a manual back-channel: viewer reports
  RTT + decode lateness; agent adjusts `cq-level` / target bitrate
- **Keep**: WebRTC for transport (ICE, DTLS, TURN, signalling, the
  other data channels — input, control, cursor, clipboard, files)
- **Keep**: existing session lifecycle, consent, audit, agent
  identity, all of `crates/remote_control`

## Architecture

```
┌─────────────────────────── Agent (roomler-agent, Rust) ────────────────────────────┐
│                                                                                    │
│   WGC capture ──► BGRA frame                                                       │
│                       │                                                            │
│                       ▼                                                            │
│           dcv_color_primitives BGRA→I444 (AVX2)                                    │
│                       │                                                            │
│                       ▼                                                            │
│           libvpx VP9 profile 1 (8-bit 4:4:4)                                       │
│           tune=screen-content, cpu-used=8, lag-in-frames=0                         │
│                       │                                                            │
│                       ▼                                                            │
│           Length-prefix framing  (4-byte LE size + payload)                        │
│                       │                                                            │
│                       ▼                                                            │
│   RTCDataChannel "video-bytes" (ordered, reliable)                                 │
│   bufferedAmountLowThreshold=64KiB, watch backpressure                             │
│                                                                                    │
└─────────────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ DTLS over SCTP over ICE
                                    ▼
┌──────────────────────────── Browser (worker) ──────────────────────────────────────┐
│                                                                                    │
│   onmessage(ArrayBuffer)                                                           │
│           │                                                                        │
│           ▼  parse 4-byte size header                                              │
│   FrameAssembler — concatenate fragments until full frame                          │
│           │                                                                        │
│           ▼                                                                        │
│   EncodedVideoChunk { type, timestamp, data }                                      │
│           │                                                                        │
│           ▼                                                                        │
│   VideoDecoder({ codec:'vp09.01.10.08', optimizeForLatency:true })                 │
│           │                                                                        │
│           ▼                                                                        │
│   VideoFrame ─► OffscreenCanvas.drawImage  (no pacing — paint on output)           │
│                                                                                    │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

## Frame format on the wire

We don't need IVF or EBML — the SCTP stream is reliable + ordered
so a minimal framing works:

```
struct Frame {
    u32  size_le;          // payload length, little-endian
    u8   flags;            // bit 0: keyframe, bit 1: end-of-sequence (unused for now)
    u64  timestamp_us;     // monotonic capture timestamp (for jitter detection only)
    [u8] payload;          // raw VP9 frame
}
```

13-byte header per frame. 30fps × ~50 KB avg payload = 1.5 MB/s
worst case data-channel bandwidth, well under SCTP capacity.

## Back-channel control

The existing `control` data channel carries new messages:

```jsonc
// browser → agent
{ "t": "rc:vp9.request_keyframe" }              // trigger IDR
{ "t": "rc:vp9.bandwidth", "rtt_ms": 45,
  "decode_lateness_ms": 12, "drops": 0 }        // every 2 s

// agent → browser (on rc:session.ok)
{ "t": "rc:vp9.config",
  "codec": "vp09.01.10.08",
  "width": 1920, "height": 1200,
  "framerate": 30 }                             // initial config
```

Replaces the lost TWCC + NACK feedback loops. Simpler, hand-tunable.

## Rust crate choices

- **VP9 encoder**: `vpx-encode` 0.6 — small wrapper around libvpx, via
  the `env-libvpx-sys` shim. Alternative: `libvpx-rs` (more
  idiomatic API but less maintained). Pick `vpx-encode`.
- **Colour convert**: `dcv-color-primitives` 0.6+ — AVX2 BGRA→I444
  with cleanly-allocated planes. Avoids hand-rolling SIMD.
- **Backpressure**: existing `bytes` crate, plus
  `RTCDataChannel::buffered_amount` watch.

Behind `vp9-444` Cargo feature so existing default builds don't pay
the libvpx compile cost (it's a C build, ~30 s on first compile).

### Build prerequisites for the `vp9-444` feature

The `env-libvpx-sys` shim links against system libvpx via pkg-config
rather than bundling. Install once per build host:

| Platform | Command |
|----------|---------|
| Linux (Debian/Ubuntu) | `sudo apt install -y libvpx-dev pkg-config` |
| macOS (Homebrew) | `brew install libvpx pkg-config` |
| Windows (vcpkg) | `vcpkg install libvpx:x64-windows-static-md && vcpkg integrate install` |
| Windows (msys2) | `pacman -S mingw-w64-ucrt-x86_64-libvpx pkg-config` |

CI release runners (`release-agent.yml`) must add these to their
install steps before building with `--features vp9-444`. Default
production builds still target `full-hw` and don't pull libvpx.

## Status (2026-04-27, post Y.runtime-encoder, 0.1.47)

| Phase | Plumbing | Tested | Field-usable |
|---|---|---|---|
| Y.1 — Encoder backend | ✅ | ✅ build, ✅ runtime | ✅ |
| Y.2 — Decoder worker | ✅ | ✅ | ✅ |
| Y.3 — Transport plumbing | ✅ both ends | ✅ wire, e2e, integration | ✅ |
| Y.4 — Caps + UI | ✅ | ✅ | ✅ caps-advertising |
| Y.5 — View canvas mount | ✅ | ✅ | ✅ |

**What works**: full pipeline. Wire format, signalling, browser
worker, DC plumbing, agent media-pump branch, view canvas mount,
toolbar toggle, e2e harness, **and** a working VP9 profile 1
(8-bit 4:4:4) libvpx encoder bound directly against
`env-libvpx-sys` (drop-in replacement for the `vpx-encode 0.6`
wrapper, which couldn't reach profile 1 — see Y.runtime-encoder
below for the rationale).

Caps probe re-enabled: an agent compiled with `--features
vp9-444` advertises `data-channel-vp9-444` in `rc:agent.hello`
when the libvpx probe activates at startup; browser unlocks the
"Crystal-clear (VP9 4:4:4)" toolbar toggle when the negotiated
session has the transport set.

### Y.runtime-encoder (LANDED 0.1.47)

Why it was needed: `vpx-encode 0.6` hardcoded `VPX_IMG_FMT_I420`
in its `encode()` and exposed no Config field for `g_profile`.
Result was `Vp9Encoder` would produce VP9 profile 0 (4:2:0) bytes
that the browser's profile-1 decoder rejects, and even the 4:2:0
fallback never emitted packets on first encode because the
default `g_lag_in_frames` buffered ~25 frames silently.

Rewrite shape (~250 LOC of unsafe FFI in
`agents/roomler-agent/src/encode/libvpx.rs`):

  - Drop `vpx-encode 0.6` dep, add `env-libvpx-sys = "5.1"` with
    the `generate` feature (bindgen against system libvpx headers
    so libvpx 1.14 on Ubuntu 24.04 works without the
    pre-generated bindings panic)
  - `vpx_codec_enc_config_default(vp9_iface, &cfg)` then override:
    `cfg.g_profile = 1`, `cfg.g_lag_in_frames = 0`,
    `cfg.g_bit_depth = VPX_BITS_8`, `cfg.g_input_bit_depth = 8`,
    `cfg.rc_end_usage = VPX_CBR`, `cfg.kf_max_dist = 240`
  - `vpx_codec_enc_init_ver(&ctx, iface, &cfg, 0, ABI_VERSION)`
  - Apply controls: `VP8E_SET_CPUUSED=8`,
    `VP9E_SET_TUNE_CONTENT=VP9E_CONTENT_SCREEN`,
    `VP9E_SET_AQ_MODE=0`, `VP9E_SET_TILE_COLUMNS=2`,
    `VP9E_SET_FRAME_PARALLEL_DECODING=1`,
    `VP8E_SET_STATIC_THRESHOLD=100`,
    `VP9E_SET_NOISE_SENSITIVITY=0`
  - Per-frame: build `vpx_image_t` manually with three plane
    pointers (`VPX_IMG_FMT_I444`, x_chroma_shift=0,
    y_chroma_shift=0, bps=24), pass `VPX_EFLAG_FORCE_KF` on first
    frame + on `request_keyframe`, drain with
    `vpx_codec_get_cx_data` until null pointer
  - `vpx_codec_destroy` in Drop
  - Runtime bitrate: mutate `cfg.rc_target_bitrate`, push back
    via `vpx_codec_enc_config_set` (the `vpx-encode 0.6` wrapper
    didn't expose this either — `set_bitrate` was a no-op)

Verification: lib test `first_frame_is_keyframe` (was
`#[ignore]`d under the old wrapper) now activates the encoder at
320×240, encodes one synthetic frame, asserts at least one
non-empty packet flagged keyframe. CI runs the full
`--features vp9-444 --lib` pass on Ubuntu with `libvpx-dev` apt-
installed; `detect_advertises_vp9_444_transport_when_encoder_works`
locks the agent.hello advertisement contract.

Field test: build with `--features full,vp9-444`, run on
controlled host, click toolbar toggle in browser, verify
`VP9-444 DC pump heartbeat … frames_sent=N` heartbeat in agent
log.

## Phasing

### Phase Y.1 — Encoder backend (1 week, task #73)

- Add `vpx-encode` + `dcv_color_primitives` deps behind `vp9-444` feature
- New `agents/roomler-agent/src/encode/libvpx.rs`
- Implement `VideoEncoder` trait: encode(BGRA frame) → Vec<EncodedPacket>
- BGRA→I444 conversion + libvpx VP9 profile 1 init with screen-content tuning
- `is_hardware()` returns `false` (it's pure SW, but with deliberate intent
  — the SW-demotion logic in caps.rs needs an exemption for "VP9 4:4:4 is
  the better SW path than HW H.264 4:2:0 when 4:4:4 is requested")
- Encoder smoke tests: feed synthetic BGRA, assert keyframe + correct profile

### Phase Y.2 — Browser decoder worker (3 days, task #74)

- New `ui/src/workers/rc-vp9-444-worker.ts`
- Receives ArrayBuffers from main thread (which forwards from DC)
- Reassembles fragments into complete frames
- Feeds `VideoDecoder` configured for vp09.01.10.08
- Paints to OffscreenCanvas via `transferControlToOffscreen`
- Telemetry messages back to main thread: first-frame, decode-error, heartbeat
- Pure DataChannel-fed; no `RTCRtpScriptTransform` (which is broken in current Chrome)

### Phase Y.3 — Transport plumbing (4-5 days, task #75)

- Agent: new path in `media_pump` that, when caps-negotiated, routes
  encoded frames into the `video-bytes` DC instead of the WebRTC track.
  WebRTC video track stays added (so SDP doesn't change) but we don't
  feed it.
- Browser: new transport mode in `useRemoteControl.ts`. View toggles
  between { classic-video, webcodecs-via-rtp, webcodecs-via-dc }
- Back-channel: rc:vp9.* messages on existing `control` DC
- PLI replacement: browser sends `rc:vp9.request_keyframe` on decode
  error or 5 s without IDR; agent forces next frame to keyframe
- Backpressure: agent monitors `bufferedAmount`; pauses encoder feed
  when over 1 MiB threshold (drops late frames rather than queuing forever)

### Phase Y.4 — UI cutover + caps (2 days, task #76)

- Caps probe: agent checks `vpx-encode` linkage + libvpx version,
  browser checks `VideoDecoder.isConfigSupported({codec:'vp09.01.10.08'})`
- New caps field: `transports: ["webrtc-video", "data-channel-vp9-444"]`
- Browser preference: when both sides advertise data-channel-vp9-444,
  default ON. Toolbar toggle "Crystal-clear (VP9 4:4:4)" lets the user
  fall back to the existing WebRTC path on demand.
- Telemetry comparison: side-by-side stats panel showing both paths'
  measured fps / bitrate / decode latency for 1 week of field hours
  before flipping default

### Phase 0 (still applicable, parallel)

The encoder cascade tunings from the earlier critique still matter
for hosts that don't have the CPU budget for VP9 4:4:4 SW encode
(low-end Atom-class devices). Stays as the default path; VP9 4:4:4
is opt-in for capable hosts.

- TWCC BWE surfaced from webrtc-rs (still useful for the legacy WebRTC
  video path)
- Real `set_roi_hints` MF override (legacy path)
- Screen-content tuning on MF cascade (legacy path)

## Risks

1. **libvpx 4:4:4 CPU on weak hosts**: Atom-class CPUs may not sustain
   1080p 30fps VP9 4:4:4 SW. Mitigation: caps-probe encode time on
   startup; if > 25 ms per frame, downgrade to 720p before negotiating.
2. **Chrome WebCodecs VP9 profile 1 regression**: low risk — libvpx
   ships with Chrome and decodes profile 1 robustly. Verify with
   `isConfigSupported` at session start, fall back to legacy path
   on unsupported.
3. **DataChannel throughput**: SCTP over DTLS over ICE caps around
   100 Mbps in practice on a typical TURN-relayed path. VP9 4:4:4
   at 1080p 30fps + screen content sits ~5-15 Mbps; fine.
4. **No TWCC = manual rate control**: replacing Chrome's GCC with our
   own RTT/lateness loop. Won't be as smooth on the first cut. Plan
   for two iterations of tuning.
5. **WebCodecs decoder stalls on packet loss**: if a frame is missing
   bytes, VideoDecoder errors out and we have to send a keyframe.
   With SCTP reliable mode this shouldn't happen; if we ever switch
   to unreliable for latency, we add a per-frame integrity check.
6. **Agent CPU**: VP9 4:4:4 SW at 1080p 30fps consumes ~30-50 % of
   one core on a modern x86_64. Acceptable for the 12-core orf box;
   marginal on quad-cores. Document the requirement.

## Out of scope

- Audio (we don't carry audio today and won't here)
- Multi-monitor: same as today's path — one display per session
- Hardware VP9 4:4:4 encode: rare HW support, complex MF/NVENC
  integration. SW is good enough on modern CPUs.
- Replacing WebRTC entirely: that's option Z from the prior plan
  (full RustDesk-style protocol). Don't go there.

## Decision points (gates)

- After Y.1: does the encoder hit 30 fps at 1080p 4:4:4 on the orf
  CPU? If yes, proceed. If no, drop to 720p 4:4:4 or 1080p 4:2:0 and
  document.
- After Y.2 + Y.3: side-by-side A/B test for one week. Subjective
  judgement: is text crisper than mediasoup screenshare? If yes,
  cut over default. If marginal, keep opt-in only.
- After Y.4 + 1 month field hours: any CPU/heat complaints? If yes,
  default-off and require operator opt-in.
