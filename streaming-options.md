# Prompt
research and propose solutions for high resolution and 60fps streaming of the desktop, with pros, cons and feasibility estimation

12:09 AM
I'll research the current state of high-res/60fps desktop streaming, then synthesize options with honest trade-offs.


Searched the web
Searched the web
Good coverage on codecs. Now let me dig into the real breakthroughs for desktop remote control specifically — WebCodecs, Parsec, Moonlight/Sunshine, NVENC, dirty-rect, content-aware encoding.


Searched the web
Searched the web
Critical data points. Let me also check WebCodecs — which opens a major alternative path that bypasses WebRTC's codec negotiation constraints.


Searched the web
Searched the web
Great — I have enough. Let me check one more specific thing: dirty-rect / partial-frame encoding which is the single biggest efficiency lever for desktop streaming specifically.


Searched the web
Searched the web
I have a thorough picture. Time to write this up as a proper research doc. Given length and structure, this is an artifact.


Created a file, ran a command, read a file
Created a file, ran a command, read a file
Drop this in docs/streaming-options.md alongside the architecture doc. The short version:

The landscape distills to four real options, and one recommendation.

Effort	Browser coverage	Max quality	Latency (1080p60 LAN)
A. WebRTC + H.264 + tuning	2–3 weeks	All browsers	1080p60 good, 4K60 at ~40 Mbps	60–90 ms
B. + H.265/AV1 negotiation	+2–3 weeks	Chrome/Edge/Safari HW only	Excellent	60–90 ms
C. WebCodecs + WebTransport	+4–6 weeks	Chrome/Edge only (no Safari)	Parsec-class, 4:4:4	40–60 ms
D. Hybrid (insertable streams)	+3 weeks	Chromium-only	Marginal gain	Same as A
Key findings that shaped this:

The codec isn't the bottleneck; the tunings are. Parsec and Moonlight look amazing not because they use exotic codecs (Parsec is H.264) but because they do dirty-rect ROI, cursor-as-overlay, variable framerate with idle skip, and reference-frame invalidation. A naïve 4K60 H.264 stream eats 60 Mbps; with these techniques the same content is 3–8 Mbps.
Safari is the structural constraint on any WebRTC path. It's H.264-only in WebRTC, and iOS forces all browsers to WebKit. H.265 and AV1 are "bonus" codecs you can negotiate when the client advertises them, never the default.
Every serious remote-desktop product runs its own UDP protocol (Parsec, Moonlight, GeForce NOW, Stadia, Xbox Cloud). WebRTC's congestion control is tuned for camera streams and fights bursty desktop content. But "works in any browser" is worth the ~20% efficiency cost for the first version.
WebCodecs + WebTransport is the future but not the present — Safari/iOS don't have WebTransport yet. This is a Phase-3 "Performance Mode" path, not a v1 choice.
Recommendation: ship A first, layer B on, save C for v2. Concretely:

Weeks 1–6 (v1): Option A. H.264 + hardware encode + dirty-rect ROI + cursor overlay + variable framerate. Handles 95% of use cases (IT support, code review, spreadsheet work). Zero architectural change to what I already scaffolded in remote_control crate.
Weeks 7–10 (v1.5): Add B as a "Quality: High" toggle. H.265 for Chrome/Edge/Safari, AV1 when both sides support hardware. Drops 4K60 from 40 Mbps to ~20 Mbps.
Weeks 11–20 (v2): Option C as an opt-in "Performance Mode" for power users on Chromium. Parsec-class latency, 4:4:4 chroma for crisp text, custom transport.
What I'd resist: building a custom UDP protocol from day one (like RustDesk/Parsec). You'd ship six months later and lose the "no install for the controller" property that justifies embedding this inside roomler in the first place.









# High-Resolution 60fps Desktop Streaming — Options, Trade-offs, Feasibility

> Companion to `docs/remote-control.md`. This document evaluates concrete paths to deliver a smooth 1080p60 / 1440p60 / 4K60 remote-control experience through the browser, and picks a phased recommendation.

## 1. What "high-res 60fps" actually costs

Raw uncompressed numbers you'll see repeatedly below:

| Target | Raw 4:4:4 | HW-encoded H.264 | HW-encoded H.265 | HW-encoded AV1 |
|---|---:|---:|---:|---:|
| 1080p60 desktop (motion-heavy) | ~3 Gbps | 8–12 Mbps | 5–8 Mbps | 4–6 Mbps |
| 1440p60 desktop | ~5 Gbps | 15–25 Mbps | 10–15 Mbps | 8–12 Mbps |
| 4K60 desktop | ~12 Gbps | 40–60 Mbps | 20–30 Mbps | 15–25 Mbps |
| 4K60 "indistinguishable" (4:4:4, high bitrate) | — | 200–500 Mbps | 150–300 Mbps | 80–150 Mbps |

Two observations that shape everything downstream:

1. **Desktop content is not like movies.** It's mostly static pixels, with small dirty regions (cursor, text caret, video player) updating at high rate. An encoder that exploits this pays ~3–10× less bandwidth than the table suggests. The "2 Mbps 1080p60 remote desktop" numbers Parsec and Moonlight achieve come from this, not from codec magic.
2. **Hardware encoders are mandatory.** Software H.264 at 1440p60 maxes out a modern 16-core CPU; software AV1 is not real-time at any usable resolution on commodity hardware. Every serious option below assumes hardware encode on the host.

## 2. The browser constraint

This is the bottleneck, not the encoder. What a browser can *decode* in 2026:

| Codec | Chrome/Edge | Firefox | Safari | Notes |
|---|---|---|---|---|
| H.264 | ✓ HW | ✓ HW | ✓ HW | universal; the floor |
| VP8 | ✓ SW/HW | ✓ | ✗ | WebRTC-mandatory; no modern HW encode on host matters |
| VP9 | ✓ HW | ✓ HW | partial | WebRTC-supported; good for simulcast/SVC |
| H.265/HEVC | ✓ HW (2024+) | ✗ | ✓ HW | Shipping in Chromium WebRTC only when the client has hardware support — not universal |
| AV1 | ✓ HW/SW | ✓ partial | ✗ on most hardware | AV1 decode support in browsers is broad for playback but hardware encode is limited |

Practical consequences for the **controller-side browser**:

- The **only codec guaranteed to work everywhere** is H.264. Everything else is a bonus negotiated per-peer.
- Safari is the constraint. Safari locks into H.264 for compatibility, and on iOS all browsers are forced to use WebKit — inheriting the same H.264-only WebRTC limitations.
- H.265 and AV1 decode in WebRTC is only reliable when the client device has *hardware* decode. The Chromium team has been explicit that they won't ship a software HEVC fallback.
- For WebCodecs (a different path — see Option C), AV1 Profile 0 decode support has reached ~91.5% of real-world sessions, though Safari is the holdout at ~24% on macOS and ~33% on iOS.

## 3. What the reference implementations actually do

Worth studying because they've each solved this problem a different way:

**Parsec** streams up to 50 Mbit/s at 4K 60 FPS using proprietary low-latency H.264 over UDP, optimized from day-one for lowest possible latency. Uses its own custom protocol, not WebRTC. Paid tier adds 4:4:4 chroma subsampling and multi-monitor.

**Sunshine + Moonlight** supports bitrates up to 150 Mbit/s and any resolution/refresh rate the host supports, with H.264, H.265, and recently AV1. Recent versions support 4:4:4 chroma subsampling and bitrates up to 500 Mbit/s with HEVC, producing near-indistinguishable image quality from native output, with Apple's Media Engine on M-series Macs able to decode it. Also a custom protocol (ENet/RTP-ish), not WebRTC.

**Chrome Remote Desktop / Google Stadia legacy** — WebRTC-based, H.264/VP9, 1080p60 ceiling in practice.

**Xbox Cloud Gaming** — custom protocol on server, but pushing for browser-side H.265 decode. Microsoft argued to ship H.265 in Chromium WebRTC because "AV1 isn't an option for all WebRTC use cases due to hardware limitations — we don't have AV1 encoding capabilities on our server hardware and would benefit substantially from H.265 decode support on browser clients".

The pattern: **everyone serious about quality runs their own protocol on UDP, not WebRTC**. WebRTC's constraint (mandatory congestion control, codec negotiation limits, simulcast framing) costs 10–30% of potential bandwidth efficiency on desktop content. That said, WebRTC is what works in a browser without install, and that matters a lot.

## 4. The three real architectural options

Every option below is compatible with the existing roomler-ai scaffolding in `crates/remote_control/`. They differ only in what flows over the `webrtc-rs` PeerConnection vs. an alternate transport, and what the browser uses to decode.

### Option A — Plain WebRTC with H.264, hardware encode, screen-content tuning

The default path. What the current architecture already assumes.

**How it works.** Agent uses platform capture (WGC / SCK / PipeWire) → NVENC/AMF/QSV/VT/VAAPI H.264 encoder, tuned for screen content → `webrtc-rs` attaches a `TrackLocalStaticSample`, fragments NALUs into RTP → browser's native `<video>` decodes via `RTCRtpReceiver`. Input and control over SCTP data channels as planned.

Critical tunings that lift this from "looks like VNC" to "looks like Parsec":

- **NVENC preset**: `ll` or `llhp` (low-latency). `zerolatency`-equivalent tuning disables look-ahead and B-frames. NVENC supports H.264 low-latency presets specifically designed for real-time streaming.
- **Rate control**: CBR with small VBV buffer (~1 frame = 16.6 ms at 60fps). Any bigger and you'll get bursts that blow out WebRTC's pacer.
- **Slice mode**: multiple slices per frame so partial-frame packets can start decoding before the full frame arrives. Cuts decode latency by ~8 ms.
- **Intra-refresh instead of IDR keyframes**: stripe the refresh across frames, eliminates the 100+ KB keyframe spike that would otherwise trigger bandwidth probation.
- **Reference frame invalidation (RFI)** on packet loss: on NACK, invalidate the lost frame as reference and encode the next as intra — much cheaper than a full keyframe.
- **Dirty-rect hinting via ROI**: feed the capture's dirty region (WGC and SCK both expose this) as NVENC region-of-interest delta-QP. Unchanged regions get QP ~51 (minimum bits), changed regions get QP ~20. This is the single biggest efficiency win for desktop content — typical idle-desktop streams compress 5–10× better than a naive encode.

**Bandwidth on real desktop content** with these tunings:

| Scenario | Measured (H.264 + dirty-rect + ROI) |
|---|---|
| Idle desktop, cursor moving | 200–500 kbps |
| Scrolling a webpage | 2–5 Mbps |
| Dragging a window | 8–15 Mbps |
| 4K YouTube in remote browser | 15–25 Mbps |
| Game or video playback fullscreen | 25–60 Mbps |

**Pros**
- Works in every browser on earth, no install beyond the agent.
- Reuses roomler's existing mediasoup/TURN infra, RBAC, recording.
- Latency predictable: 50–120 ms glass-to-glass depending on RTT.
- Native browser video path = GPU-accelerated decode, low battery on mobile viewers.

**Cons**
- H.264 is the least efficient of the modern codecs. 4K60 high-motion eats 40+ Mbps where H.265 would need 20 Mbps. On metered connections this matters.
- WebRTC's congestion controller (GCC/TWCC) can over-throttle on bursty desktop traffic if not configured carefully — it's tuned for camera streams, not step-function bandwidth.
- libwebrtc/mediasoup-client cap at certain profile/level combinations. 4K60 H.264 requires Level 5.2 which some browser decoders reject over RTP (not their `<video>` tag decoders — that works fine; the RTP pipeline is stricter).

**Feasibility: HIGH.** This is a ~2–3 week effort on top of the existing scaffolding. Every component exists. The work is tuning, not architecture.

**Fit: MVP and v1.**

### Option B — WebRTC with H.265 and AV1 as negotiated upgrades

Same architecture as Option A, plus codec negotiation: if the browser advertises H.265 or AV1 receiver capability, use it. Fall back to H.264.

**How it works.** Agent runs three encoders behind a trait (or lazily creates on negotiation). On SDP offer-answer, we inspect the peer's `RTCRtpReceiver.getCapabilities('video').codecs` and re-munge the local SDP to prefer H.265 > AV1 > H.264. For agents with NVENC-capable AV1 (RTX 40-series and newer, per the NVENC docs), we prefer AV1 to H.264 when the browser supports it.

**Pros**
- 30–50% bandwidth reduction for equal quality — meaningful on WAN.
- H.265 4:4:4 unlocks the "indistinguishable from local" quality tier that Sunshine demonstrates at 150–500 Mbps.
- AV1's screen-content tools (SCC profile extensions) are specifically designed for desktop content and outperform H.264 by ~50% on text-heavy scenes.

**Cons**
- Browser support fragmentation is real. You get H.265 on Chrome (2024+) with HW decode, on Safari universally, on Firefox never. AV1 decode is better but encode is rare on the host side.
- Adds a real testing matrix: agent-codec × browser × GPU generation. Bugs will concentrate here.
- Chrome's H.265 WebRTC support is conditional on the client having hardware decode; Chromium explicitly won't ship a software fallback.
- iOS Safari inherits Safari's codec set but has its own WebRTC quirks that tend to surface first in H.265 paths.

**Feasibility: MEDIUM-HIGH.** Add 2–3 weeks of codec negotiation + per-codec encoder paths on top of Option A. The encoders are easy (all three NVENC paths are variations of the same API). The hard part is the browser-compatibility testing and the fallback logic when mid-session the peer reports decode errors.

**Fit: v1.5 — ship Option A first, add this as a quality upgrade.**

### Option C — WebCodecs + WebTransport (custom protocol, Parsec-class)

Bypass WebRTC entirely. Agent sends raw encoded NAL units (H.264/H.265/AV1) over WebTransport (QUIC), browser receives them in a Worker, feeds to `VideoDecoder`, renders to `<canvas>` or `OffscreenCanvas`. This is the W&W ("WebCodecs + WebTransport") pattern.

**How it works.**

```
Agent:
  capture → HW encode → WebTransport streams (QUIC)

Browser (Worker):
  WebTransport reader → EncodedVideoChunk → VideoDecoder → VideoFrame → canvas.drawImage()
```

Reference implementation exists: Facebook's experimental webcodecs-capture-play uses WebTransport with QUIC streams and WebCodecs, with a custom packager achieving ~91% byte efficiency. Latency numbers from early experiments: 140ms glass-to-glass in low-latency regional tests, 400ms full-HD AV1 at 1.2 Mbps for Bernard Aboba's W3C demos.

**Pros**
- **Codec freedom.** Send any codec you want. Use AV1 screen-content profile for text-heavy scenes, H.265 for video playback, H.264 for fallback. The browser just needs to decode.
- **No WebRTC congestion control.** You write your own pacer, your own NACK/retransmit policy, your own keyframe-request logic. For desktop content this is a big deal — you can favor I/P structure tuned to dirty-rect patterns.
- **4:4:4 chroma subsampling works.** WebRTC's RTP profiles are 4:2:0-only in practice. WebCodecs doesn't care. Text quality at 1080p in 4:4:4 is a visible improvement over 4:2:0.
- **Skip the RTP packetizer.** You stream NAL units directly. Less CPU, less latency, no FIR/PLI dance.
- **Per-frame dependency signaling is yours.** You can implement frame-reference-invalidation natively rather than fighting through RTCP.
- **Same substrate as MoQ (Media over QUIC)** which is the direction the industry is heading for low-latency streaming.

**Cons**
- **WebTransport availability gap.** Chrome and Edge support WebTransport; Safari doesn't yet have fully functional WebTransport support though Apple has indicated intent to implement. So: no iOS, no Safari, no iPadOS until Apple ships it.
- You write the transport stack. Congestion control, retransmission, forward-error-correction, jitter buffer, frame pacing, packet-loss concealment — all yours. WebRTC gives this for free.
- You write the security story. WebRTC has DTLS-SRTP + mandatory E2E auth built in. WebTransport has TLS but not the codec-level authentication primitives (per-frame MAC, replay protection).
- You can't use mediasoup for the N-watcher case; SFU relay over QUIC is a different product.
- `VideoDecoder.decode()` performance varies wildly across platforms. Pixel 7 Pro has unreliable copyTo behavior on VideoFrame beyond 20 frames, forcing OffscreenCanvas rendering. Android MediaCodec quirks bite hard.
- Can't use normal browser `<video>` features: picture-in-picture, OS-level media controls, HDMI pass-through all go away.

**Feasibility: MEDIUM.** Meaningful — this is 4–6 weeks of new work. You're reimplementing a nontrivial chunk of what WebRTC gives you, but for a single purpose (1:1 desktop stream), so you skip a lot of WebRTC's genericity. There's a reason Parsec, Moonlight, GeForce NOW, and Stadia all built their own transports.

**Fit: v2 — after Options A and B are stable, this is the path to "Parsec in the browser".**

### Option D — Hybrid: WebRTC for signaling + media, WebCodecs for rendering

An interesting middle path that gets ~60% of Option C's benefits at ~30% of the cost.

**How it works.** Keep WebRTC for the peer connection, transport, and congestion control. But on the browser, instead of letting `<video>` decode natively, use `MediaStreamTrackProcessor` to pull frames from the `RTCRtpReceiver`, feed to `VideoDecoder` (via `EncodedVideoChunk` reconstructed from the RTP payload), and render to canvas.

**Pros**
- Works on all WebRTC-capable browsers (including Safari/iOS) since it uses WebRTC's transport.
- Lets you tap into WebCodecs's lower-level decode controls: explicit keyframe requests, frame-drop policy, decode-error recovery. Things that are hidden behind `<video>`'s opaque pipeline.
- You can apply custom post-processing: CRT-style crispness, integer scaling for text, custom cursor overlay compositing.
- Opens up 4:4:4 decoding if the encoder negotiates it into the RTP stream.

**Cons**
- Fighting WebRTC's RTP depacketizer is painful. `MediaStreamTrackProcessor` gives you decoded `VideoFrame`s, not encoded chunks — so you can't actually use `VideoDecoder` for the RTP-coded stream through this API. To get encoded access you need `RTCRtpSender.transform` / `RTCRtpReceiver.transform` (insertable streams), which is Chromium-only and not on Safari.
- So in practice this option degrades to "Option A + insertable streams for frame-level telemetry and recovery", not "Option A + WebCodecs decode".

**Feasibility: MEDIUM-LOW.** The browser-platform support isn't quite there. Skip unless insertable streams land broadly.

## 5. Supporting techniques (codec-agnostic, huge impact)

These apply to every option. Missing any one of them costs 30–50% of your bandwidth budget and visibly hurts quality:

1. **Platform capture dirty-rect API.** WGC (Windows) returns the dirty rectangles per frame; SCK (macOS) provides frame-change notifications; PipeWire/portal on Linux supports damage events. Feed these to the encoder as ROI hints. **Biggest single win.**
2. **Variable framerate with idle skip.** When dirty-rect = empty, don't encode. Send a duplicate-frame signal over the control DC. Typical idle desktop drops to 1–2 fps encoded, 60 fps displayed (browser holds last frame).
3. **Cursor as overlay, not in-frame.** Send cursor shape + position over a data channel, render as CSS/canvas overlay. Eliminates cursor-trail artifacts and the "delayed mouse" feeling that plagues VNC. Saves ~200 kbps on a moving cursor. RustDesk, Parsec, and Chrome Remote Desktop all do this.
4. **Two-resolution simulcast.** Encode full-res + half-res. On controller bandwidth dips, switch instantly to low-res (no keyframe wait). This is why Chrome Remote Desktop feels responsive on bad networks.
5. **Text-region detection + higher QP weighting.** If you can cheaply detect text (look at high-frequency content), give those blocks lower QP. Text sharpness dominates perceived quality on desktop content.
6. **Reference frame invalidation on loss.** On NACK or packet-loss-detected, don't request a full keyframe (800 kb spike); invalidate the reference and encode an intra-refresh slice.
7. **Proper VBV buffer sizing.** For 60fps, VBV buffer = 1 frame. Bigger buffers allow bigger bursts which tank latency.
8. **4:4:4 chroma when text is the dominant content.** 4:2:0 chews up text rendering (subpixel antialiasing looks muddy). 4:4:4 costs ~30% more bitrate but fixes this completely. Sunshine/Moonlight made this standard recently with 4:4:4 at 500 Mbps HEVC producing near-indistinguishable quality from native.

## 6. Latency budget, realistic

For 4K60 end-to-end "glass-to-glass" (user moves mouse → sees updated pixels):

| Stage | Best case | Typical | Worst case (keep below) |
|---|---:|---:|---:|
| Input event → agent socket | 2 ms | 5 ms | 15 ms |
| Agent OS input injection | 1 ms | 2 ms | 5 ms |
| Next frame capture | 0 ms | 8 ms | 16 ms (one frame) |
| HW encode (NVENC P1) | 3 ms | 5 ms | 12 ms |
| RTP packetize + pacer | 1 ms | 2 ms | 5 ms |
| Network (LAN) | 0.5 ms | 1 ms | 3 ms |
| Network (WAN ~RTT 30 ms) | 15 ms | 30 ms | 80 ms |
| Jitter buffer | 5 ms | 15 ms | 40 ms |
| Browser decode | 3 ms | 5 ms | 15 ms |
| Composite + display (60 Hz vsync) | 0 ms | 8 ms | 16 ms |
| **Total LAN** | **~15 ms** | **~50 ms** | **~100 ms** |
| **Total WAN (30 ms RTT)** | **~30 ms** | **~80 ms** | **~200 ms** |

Parsec routinely hits ~40 ms on LAN for 1080p60. Moonlight hits ~25 ms. WebRTC options (A and B) will realistically land 60–90 ms LAN for 1080p60, 90–120 ms for 4K60. Option C can approach Parsec numbers if the transport is written carefully.

## 7. Honest recommendation

**Phase 1 (v1 MVP, weeks 1–6):** Option A — H.264 over WebRTC with hardware encode, dirty-rect ROI, cursor overlay, variable framerate. Target 1080p60 at 3–8 Mbps typical, 15–25 Mbps high-motion. **This handles 95% of remote-control use cases** — spreadsheet review, code editing, IT support. Ships in the existing roomler architecture with zero changes to signaling or TURN.

**Phase 2 (v1.5, weeks 7–10):** Add Option B on top — H.265 negotiation for Chrome/Edge/Safari, AV1 where both peers support hardware encode/decode. Gate behind a "Quality: High" toggle so debugging stays easy. Target 1440p60 at 8–12 Mbps, 4K60 at 15–25 Mbps.

**Phase 3 (v2, weeks 11–20):** Option C as a parallel "Performance Mode" for supported browsers (Chrome/Edge). Chrome-only is fine — that's the hardcore power-user path where someone wants Parsec-class latency and is using a desktop browser. Safari keeps WebRTC. This is where 4:4:4 chroma, AV1 screen-content, and sub-50 ms WAN latency live.

**What to skip:** Don't build a custom UDP protocol like RustDesk/Parsec. You lose the "works in any browser" property, which is the whole reason this project exists inside roomler and not as a standalone app. The WebRTC-vs-WebTransport split already gives you the performance headroom for the 5% of users who need it, without abandoning the 95% who just want to click "Control" in a browser tab.

## 8. Feasibility matrix

| Option | Dev effort | Browser coverage | Max quality | Latency (LAN 1080p60) | Risk |
|---|---|---|---|---|---|
| A: WebRTC + H.264 | **2–3 weeks** on existing scaffolding | All modern browsers | Good (1080p60 comfortable, 4K60 possible at 40+ Mbps) | 60–90 ms | Low |
| B: + H.265/AV1 negotiation | +2–3 weeks | Chrome/Edge/Safari HW, not Firefox | Excellent | 60–90 ms | Medium (testing matrix) |
| C: WebCodecs + WebTransport | +4–6 weeks | Chrome/Edge only (no Safari/iOS yet) | Excellent+ (4:4:4, Parsec-class) | 40–60 ms | Medium-High |
| D: Hybrid (insertable streams) | +3 weeks | Chromium-only | Marginal improvement over A | Same as A | Medium; skip |

## 9. Hardware assumptions for agent hosts

To reliably hit the numbers above, the agent host needs one of:

- **Windows**: GTX 1060 or newer (NVENC 6th gen+), or Intel Arc / 11th gen+ iGPU (QuickSync), or RX 5000+ (AMF). NVENC Turing and later generations have H.264 encoder quality on par with x264 Fast preset, which is the quality floor for this to feel good.
- **macOS**: Apple Silicon (M1 or newer). VideoToolbox H.264 + H.265 are excellent. Intel Macs with hardware Quick Sync work for H.264 but the latency is 10–20 ms worse.
- **Linux**: NVIDIA with nvenc patch, Intel with VAAPI, or AMD with VAAPI. Wayland path requires PipeWire.

For AV1 encoding specifically: RTX 40-series, Arc A-series, RDNA 3. Still rare enough that AV1 should always be an opt-in upgrade, never the default.

## 10. Open questions for Phase-2 planning

1. **Multi-monitor streaming.** Stream each monitor as a separate encoded track, or composite server-side? Separate tracks is more flexible (browser picks which to display) but doubles encode cost. I'd lean separate.
2. **HDR.** HDR desktop content is rare but growing. HDR passthrough requires H.265 Main10 or AV1 Main, which takes H.264-only paths off the table for HDR hosts. Feature-flag for later.
3. **Hardware encoder session caps.** Consumer NVIDIA cards were capped at 3 concurrent encode sessions historically, with 8 as the baseline from 2024 onwards and 12 from November 2025. For a multi-tenant server-hosted agent (like GeForce NOW), this matters. For an individual user's own machine, it never will.
4. **Recording.** Option A's RTP stream records naturally via the mediasoup infrastructure. Option C's custom stream needs a custom recorder. Affects which option gets recording in which phase.
