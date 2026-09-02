# FR-62 — Encoder rate changes without an IDR

**Issue:** [#1242](https://github.com/gjovanov/roomler-ai/issues/1242) · **Status:** proposed · **Plan:** `immutable-doodling-neumann` (approved 2026-09-02)

**Parent/siblings:** FR-59 (the regression that motivated the arc). This is one of three FRs from that plan (FR-62 encoder apply path, FR-63 the controller, FR-64 the data path).

## Goal

A remote-desktop encoder follows the rate controller's target **without an IDR and without a rebuild**. Today `FfmpegEncoder::set_bitrate` (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:1585–1671`) costs an IDR on every move: NVENC through FFmpeg's `reconfig_encoder` (n8.1.2 sets `resetEncoder=1; forceIDR=1` on a bitrate-only change), QSV/AMF/VideoToolbox through a full encoder rebuild under a comment that predates FFmpeg 6.0's runtime `update_bitrate()`. Nine of the 34 rate/quality heuristics in `encode/` exist only to ration that cost (coarsen ladder, deferred applies, 15-s thrift, background swap, settle-KF thrift, refine-vs-cap fight, opener grace, FlipTracker and kf_gate cooldowns); on LAN pairs the AIMD crosses a rung nearly every window — 44 swap IDRs in one day on one host (2026-09-02).

Also found while reading the sources: **our QSV sessions run CBR, not QVBR** (`select_rc_mode` tests `rc_max_rate == bit_rate` first and we set both to the cap), which is why every 0.4.49 frame on CORPLAP-1 was exactly `target ÷ fps ÷ 8` bytes; and the existing NVENC in-place write sizes `rc_buffer_size = target` while the open used `maxrate × hrd_pct` — every move silently resizes the HRD window.

## Key design

- **A0 — measure on silicon**: `roomlerd encoder-smoke --reconfigure-sweep` (moving-block synthetic content, 20 rate changes 6 M → 200 k → 6 M, per rung: key packets, `set_bitrate_ms`, burst, bytes-per-frame ratio). Pass = 0 key packets after the opener, apply < 5 ms, ratio ±25 % by frame 10, burst ≤ 2×. Hosts: Iris Xe (`hevc_qsv`, `low_power` 1/0), Arc (`av1_qsv`), RTX (`hevc/av1_nvenc`), Apple Silicon (`hevc_videotoolbox`); AMD recorded as unmeasured.
- **A1 — in-place applies, gated** (`encoder_inplace_rate`, default OFF in the PR, ON after A0 passes): `RateReconfig::{InPlace, Rebuild}` resolved at open; QSV writes `bit_rate + rc_max_rate + rc_buffer_size` together (CBR); the HRD sizing bug fixed; a 3 % dead-band; counters `rate_moves / rebuilds / idr_count` in the heartbeat; a startup probe per host/codec (a driver table is not evidence).
- **A2 — FFmpeg patches** carried in `.github/ffmpeg-patches/` and applied by all three vendor builders (Linux/macOS from source, Windows via the vcpkg overlay port's `PATCHES`), with a `ROOMLER-PATCHES.txt` hash gate in `release-agent.yml`: `0001-nvenc-no-idr-on-bitrate-reconfig`, `0002-videotoolbox-runtime-bitrate` (as WebRTC's VT encoder does), `0003-amf-runtime-bitrate` (disabled, unmeasured).
- **A3 — VideoToolbox/AMF** per A0; **A4 — delete** the nine rationing heuristics one shippable PR at a time (anchors in the spec).
- **A5 (later A/B)**: QSV to QVBR (`bit_rate = 0.9 × maxrate`, keeps `global_quality`) — stops CBR padding idle frames.

## Acceptance criteria

- [ ] AC1 — A0 report per backend on real hardware, attached here, with the pass/fail per codec.
- [ ] AC2 — On a constrained session the bytes-per-frame ratio is ≈ 1.0 within 2 s of every target change (FR-59's instrument).
- [ ] AC3 — A 5-minute constrained session's `idr_count` ≤ dims changes + viewer keyframe requests.
- [ ] AC4 — Every deleted heuristic's kill switch is retired through the registry table with its evidence.
- [ ] AC5 — `encoder_inplace_rate = false` restores today's behaviour byte-for-byte (unit test + field check).

## Open decisions

- VideoToolbox: patch 0002 vs. "rebuild at settle, ≥ 15 s apart" (A0 on a Mac decides).
- Whether to attach `mfxExtEncoderResetOption{StartNewSequence=OFF}` (patch 0004) if any QSV driver starts a new sequence on Reset.

## Out of scope

- The controller itself (FR-63); which path ICE selects (FR-64); the QVBR switch (A5, separate A/B).

## Related

FR-59 #1163 (the regression that exposed the ladder), FR-31 (opening keyframe budget — `max_frame_size` is runtime-updatable in qsvenc and is that lever), FR-10 (relay IDR thrift, to be retired), FR-63, FR-64.
