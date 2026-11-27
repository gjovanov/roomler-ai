# FR-38: A static screen costs 0.3–0.9 Mbps on a relay — the idle keepalive re-encodes an identical frame for 3–6.5 KB

Status: **P0 open — measured in August (2026-08-29), not yet bisected on the local rig; read across the fleet on 2026-11-22** ([#949 comment](https://github.com/gjovanov/roomler-ai/issues/949#issuecomment-7078976426)). On QSV the spend is the default open's constant-bitrate filler (`bit_rate` = `maxrate` = the cap), with no version boundary across 0.4.223 to 0.4.231. NVENC is at or near the goal. No box ticked by that read. Tracking issue: `FR-38` (#949).
Found during the FR-35 field run (#922) while explaining a "relay degraded" report on CORPLAP-3;
the report itself turned out to be a transient path condition, this did not.

## Goal

An identical-frame keepalive should cost what an identical frame costs — **≤ 1.5 KB at 1920×1200
on NVENC and QSV** — so a static remote desktop spends **≤ 0.2 Mbps** of a relay instead of a third
of it, and every motion burst (FR-31, FR-35) starts from a quiet pipe.

## What is in force today

- The FFmpeg DC pump re-encodes `last_good_frame` every `IDLE_KEEPALIVE` = 60 ms once the capture
  goes empty (`agents/roomlerd/src/peer.rs`, the `Ok(None)` arm: `last_capture_at.elapsed() >=
  IDLE_KEEPALIVE` ⇒ `f.clone()` of the same `Arc<Frame>`) — 16–17 encodes/s of bit-identical
  input, since rc.130. Its purpose is the "refinement" of a static picture after motion and a
  steady stream for the viewer's stall detector.
- The encoder is opened once per session with the constrained profile (`encode/ffmpeg/encoder.rs`,
  `encoder_options`): QSV `global_quality=22 low_power=1 maxrate=… bufsize=2×`, NVENC cq-driven VBR
  with `maxrate`; **byte-identical across rc.474 → 0.4.20** (checked in the device logs).
- The heartbeat reports `avg_qp=None max_qp=None` for both HW encoders — the pump cannot see whether
  the encoder is still refining or already emitting skip frames.

## Field evidence (2026-08-29, same host, same relay, same encoder options)

| host / build | idle spend (static screen) | per keepalive frame @ 16–17 fps |
|---|---|---|
| CORPLAP-3 `av1_qsv`, rc.474 (08-25) | 24–44 KB / 2 s | **0.7–1.3 KB** |
| CORPLAP-3, 0.4.10 (08-27) | 30–79 KB / 2 s | 1–2.4 KB |
| CORPLAP-3, 0.4.15 (08-28) | 78–125 KB / 2 s | 2.4–3.8 KB |
| CORPLAP-3, 0.4.17–0.4.20 (08-29) | 114–236 KB / 2 s | 3.5–**6.5 KB** |
| CORPLAP-2 `av1_nvenc`, 0.4.20 (08-29) | 74–86 KB / s | 5–12 KB, decaying from ~35 KB/frame after an IDR |

- **Not screen changes**: a 0.4.17 session with *zero* real captures during idle still cost
  215 KB / 2 s. Sessions with a 2 Hz caret blink cost the same as sessions without.
- **Content-dependent**: 114 vs 215 KB / 2 s five minutes apart on the same build; a later session on
  the same host cost 41 KB / s (3 KB/frame). So the cost scales with picture complexity — which is
  what an encoder that keeps *coding* the picture does, and what a skip-frame does not.
- **Both vendors**: the same signature on QSV (QVBR) and NVENC (cq VBR), so it is upstream of the
  encoder (what the pump submits) or in the encoder's steady state (a quality target it never
  reaches within `maxrate`, refining forever), not a vendor quirk.
- The rc.474 → 0.4.x growth is the only version-correlated signal; the pump changes in that span
  are FR-1 P1+P4+P7a (#750), P5 parallel convert / off-runtime encode (#781), FR-10 (#785), FR-15
  (#796/#804), the rebuild-storm fix (#817) and the send-stall signal (#818).

## Why it matters

On a 2–3 Mbps corp relay the idle stream permanently occupies a third of the pipe, the DERP TCP
stream is never quiet (so its congestion window never rests either), and every motion burst rides
on top of it. It also inflates the baseline FR-35's "carried" gate measures against.

## Plan

| phase | what | kill switch |
|---|---|---|
| **P0** | Bisect on the local rig: neo16's own daemon with `ROOMLERD_ICE_RELAY_TCP=1` (the FR-31 relay stand-in), the FR-31 viewer harness reading per-frame bytes, a held-constant static screen; builds rc.474 / 0.4.10 / 0.4.15 / 0.4.20, then the suspects' switches (`ROOMLERD_IDLE_REFINE=0`, the #781 off-runtime/parallel-convert keys, the #750 knobs). | — (measurement) |
| **P0b** | QP telemetry for HW encoders (packet side data / encoder stats) so "still refining" vs "skip" is visible in the heartbeat. | — (telemetry) |
| **P1** | Fix what P0 names. Fallback if it is the encoder's steady state: a slower keepalive cadence on constrained transports (60 → 250 ms halves the spend at zero code risk) and/or a bounded refinement window after motion. | config key (P1 decides) |

## Acceptance criteria

- [ ] idle keepalive ≤ 1.5 KB/frame on `av1_nvenc` and `av1_qsv` at 1920×1200, static screen,
      measured on CORPLAP-2 and CORPLAP-3 with the same harness as the table above
- [ ] idle spend ≤ 0.2 Mbps on both
- [ ] time-to-crisp after motion not worse than the FR-31 baseline (the refinement job still gets done)
- [~] `avg_qp` populated for HW encoders in the heartbeat. **Partial (2026-11-02):** set on
      every NVENC cell and on `h264_qsv`, on no window of `hevc_qsv`, `av1_qsv`, VideoToolbox
      or VAAPI (field log)

## Open decisions

- Whether the keepalive should exist at all on constrained transports once the picture is refined
  (the viewer's stall detector probes with `rc:keyframe` after 6 s anyway).

## Out of scope

The opening keyframe budget (FR-31), the relay ceiling (FR-35), Wayland (FR-36).

## Field log

| date | build | note |
|---|---|---|
| 2026-08-29 | 0.4.17–0.4.21 vs the same host's rc.474 logs | The table above; harness and method in `project_corplap3_relay_regression_2026_08_29` (operator memory) and the #922 field-run comment. |
| 2026-11-02 | 0.4.177 to 0.4.197, a week of `agent_logs` | **AC4 `[~]`.** Windows of the DC pump heartbeat with `avg_qp` set, by encoder. Set on all of them: `av1_nvenc` (10 838, median 14), `hevc_nvenc` (1 279, median 15), `h264_qsv` (510, median 12), `h264_nvenc` (83, median 14). Set on none of them: `hevc_qsv` (83 334), `av1_qsv` (8 948), `hevc_videotoolbox` (19 887), `h264_videotoolbox` (44), `h264_vaapi` (398), `hevc_vaapi` (241), `vp9_qsv` (36). `av1_qsv`, one of the two encoders this FR is about, reports nothing, and neither does the fleet's busiest cell. The reader takes `AV_PKT_DATA_QUALITY_STATS` side data (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:2395`) and treats a missing or zero value as `None` (`agents/roomlerd/src/encode/mod.rs:1269`). This read can't tell which of the two those cells do. The comment at `agents/roomlerd/src/encode/mod.rs:1265` says QSV fills it, which holds for `h264_qsv` only. The harness above hasn't been rerun since 08-29, so the first three criteria stay open. FR-135's fleet read gives the size of the second: sparse windows at 0.20 Mbit/s on `av1_nvenc`, at the goal, and 8.93 on `av1_qsv`, about 45 times over it (`docs/fr/FR-135-one-rc-media-pipeline.md:307`). |
| 2026-11-22 | 0.4.222 to 0.4.231, 7 days of `agent_logs` (2026-11-15 to 2026-11-22) | **A fleet read of what a still screen costs, per encoder cell; no box ticked.** The agents' own N3 counters (`window_ms`, `frames_keepalive`, `keepalive_bytes`), not a lab and not the harness above: 178 sessions on 10 devices, 197,460 heartbeat windows, 14 encoder cells ([#949 comment](https://github.com/gjovanov/roomler-ai/issues/949#issuecomment-7078976426)). **QSV pads every frame, a still one too, to `maxrate ÷ target_fps ÷ 8`:** 96.0 % of QSV windows (162,176 of 168,952) are within 5 % of that frame, on both codecs and both transports. `hevc_qsv` direct costs 52.10 KB a keepalive and 6.77 Mbit/s on a still screen, 35 and 34 times the goal, and the cost does not fall along a still run. `av1_qsv` direct is NOT MET at 1920×1200: 52.10 KB and 6.78 Mbit/s. The open sets `bit_rate` = `maxrate` = the cap, which FFmpeg reads as constant bitrate; the bytes follow the cap, not the picture. **NVENC is at the goal or near it:** `hevc_nvenc` direct costs 0.13 KB and 0.01 Mbit/s at 1920×1200 (33 windows, one session); `av1_nvenc` constrained costs 0.94 KB and 0.12 Mbit/s at the median over all sizes, but 4.13 KB at 1920×1200 (11 windows, one session, thin). **AC4 is unchanged:** `avg_qp` is set on every NVENC window (16,161 of 16,161) and on no QSV, VideoToolbox or VAAPI window (0 of 181,299); it stays `[~]`. **No version boundary on constant-bitrate QSV:** every device that runs it pads at 1.00 on every agent version from 0.4.223 to 0.4.231. What the read cannot do: it does not bisect before 0.4.223, it is not the two named hosts' harness (so AC1 and AC2 are informed and not ticked), and it carries no time-to-crisp (AC3). |
| 2026-11-22 | 0.4.225 to 0.4.230, the rows of the read above | **One device's QVBR windows in the 11-22 read, labelled; no box ticked.** One QSV device has run FR-148 P2's `qsv_rate_mode = qvbr` since 2026-11-18 (`docs/fr/FR-148-path-measured-plan-follows.md:850`; the switch, off by default, is described at `docs/fr/FR-148-path-measured-plan-follows.md:364-365`). The read above counts that device's windows. From 0.4.225 on it holds 324 QSV windows in 4 sessions, all `hevc_qsv` (104 direct, 220 constrained, at 1920×1200 and 1920×1080). None of them is within 5 % of the constant-bitrate frame: the median sits at 0.03 of it, 318 windows under 0.3 and 6 between 0.3 and 0.8, and each window sent every frame it encoded. They are 324 of the read's 6,776 off-frame windows. The comment's 96.0 % counts them. Left out, the default open is on the frame in 96.2 % of QSV windows (162,176 of 168,628). The same device's 8,516 windows on 0.4.223 and 0.4.224, versions that predate the switch, are on the frame in 99.8 % of them and stay in both shares. Counted again from the read's saved rows, with its filter and its chain rule; no new read. |
