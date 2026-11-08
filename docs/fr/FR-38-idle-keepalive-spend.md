# FR-38: A static screen costs 0.3–0.9 Mbps on a relay — the idle keepalive re-encodes an identical frame for 3–6.5 KB

Status: **P0 open — measured, not yet bisected** (2026-08-29). Tracking issue: `FR-38` (#949).
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
