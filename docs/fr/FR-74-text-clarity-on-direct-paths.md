# FR-74 — Text clarity on direct paths: the bitrate ceiling follows the content, not a constant

**Issue:** [#1442](https://github.com/gjovanov/roomler-ai/issues/1442) · **Status:** proposed 2026-09-06 ·
**Parent:** the RC quality program (FR-17/16/14); rides on FR-59's measured pipe and FR-70's
pump instrumentation.

## Goal

Scrolling text over a direct, low-latency path is readable while it moves and crisp
within a second of stopping. The operator's own test that opened this FR: Notepad++
on CORPLAP-3 (av1_qsv, 1920×1200, direct, 4 ms RTT), scrolling through a day's
daemon log — "very blurred text (unreadable), 5–8 s to stabilize, and even when it
stabilizes the text is not crystal clear."

## Field evidence (2026-09-06, the operator's session `6a9db913`, 19:03 UTC)

Everything below is read from the pump's own heartbeat and the session's log lines;
nothing is inferred from source alone.

1. **The direct ceiling is a constant, and Sharper cannot exceed it.**
   `rate_profile::ffmpeg_maxrate_bps_scaled` derives the ceiling as
   `SCREEN_BPP_PER_SECOND (0.07) × width × height × fps`, clamped to [3, 12] Mbps:
   1920×1200 at 60 fps gives **9,676,800 bps**, and the session's `ceiling_bps` says
   exactly that. The Priority dial scales it 70 / 85 / 100 % (Smoother / Balanced /
   Sharper), so Sharper is the ceiling itself. The libvpx VP9-444 pump encodes the
   same content at **0.20 bpp** (24 Mbps at this geometry) — `encode::policy`'s own
   header names the divergence.

2. **Every scroll burst is read as congestion by a controller reacting to its own cap.**
   The encoder runs constant quality (ICQ `global_quality=22`) under `maxrate` = the
   AIMD target. Scrolling text at 60 fps overruns any 6–10 Mbps cap (the rate
   control raises QP: the blur while it moves), the send queue exceeds the direct
   budget (150 ms of the target = 181 KB — `direct byte-budget gate engaged
   inflight=272218 budget=181440` at 19:03:55), the gate skips frames
   (`frames_skipped_backpressure` 0 → 10 over the session), and the AIMD's
   occupancy signal fires: **three ×0.85 cuts in six seconds**, 9.68 → 8.23 → 6.99 →
   5.94 Mbps (19:03:56–19:04:02). The additive climb is target/16 per 5 s settle
   (+605 kbps): **~40 s back to the ceiling** (6.55 M at 19:04:06 … 9.68 M at
   19:04:44). A second scroll at 19:04:14 cut it to 6.08 M again, a third at
   19:06:12 to 8.23 M. The sent rate during a scroll window was 2–4.8 Mbps; at
   rest ~430 kbps. The path itself never showed congestion: viewer age 3–6 ms, no
   goodput estimate below the target.

3. **Each rate change on QSV is a rebuild with a forced IDR.** `coarsen_bitrate`
   snaps the target to the ladder (4.5 / 6 / 8 / 12 Mbps here), and every crossing
   rebuilds the encoder (`background-rebuilt encoder adopted (bitrate swap)`,
   `forced_idr=1`): **37 bitrate swaps and 35 idle-settle keyframes in 11
   minutes** — an IDR every ~9 s, each restarting quality from a keyframe encoded
   under the cap. That is the "5–8 s to stabilize": the picture pumps with the
   ladder.

4. **At native resolution nothing sharpens the still picture.** The P8a idle refine
   is gated on `capped_below_native` (`refine_eligible_now` in the pump), so at
   native the settled frame is whatever the last keyframe and its P-frames encoded
   at ICQ 22 on the VDENC (`low_power=1`) AV1 path, 4:2:0 — and 4:2:0 halves the
   chroma that ClearType text lives on. `cq_bias` is 0 at native (the sharpening
   bias exists for deep rungs only).

5. **Not yet measured**: the viewer's pixel chain. The pill reports the encode
   dims (1920×1200) and `FSR`, not the display scale; if the stage is not 1:1 with
   the frame, the browser's resample softens text no matter what the encoder does
   (rc.191's "Match remote display" exists for exactly that).

## Key design

The thesis: on a direct path the network is rarely the limiter for screen content,
and a controller that caps at a bits-per-pixel constant, then reacts to its own cap
as if it were congestion, produces exactly the symptom above. The remedy is the
same one every other lever in this program has taken: **measure, then follow** —
the ceiling follows the measured pipe and the content's demand; the queue budget is
denominated in the measured pipe; the still picture gets one sharp frame when
motion stops. No new controller, no new kill switch beyond the phase gate.

### P0 — A/B with the knobs that exist (no code)

On CORPLAP-3, the operator judging the same Notepad++ scroll, each cell first
reproducing the FAIL on the current settings, then one change at a time:

| cell | knob | from → to | what it isolates |
|---|---|---|---|
| A | `direct_queue_ms` (config) | 150 → 600 | the gate/AIMD reaction to a burst the wire drains |
| B | `ROOMLERD_FFMPEG_MAXRATE_KBPS` (env) | 9677 → 24000 | the ceiling (the VP9 pump's bpp at this geometry) |
| C | `ROOMLERD_FFMPEG_CQ` (env) | 22 → 16 | the still-text quality floor |

Read from the heartbeat: `target_bps` trajectory through a scroll, `frames_skipped`,
`swaps`, keyframes; from the operator: readable while moving, settle time, still
sharpness. A cell that helps names the phase; a cell that does not retires a guess.

### P1 — a content-following ceiling on direct paths

When the path is direct and the controller has no congestion evidence of its own
(viewer age flat, no measured goodput below the target — FR-59's `measured_pipe_bps`
and FR-70 M0's split are the instruments), the ceiling lifts toward the measured
pipe instead of the bpp constant, bounded by a sane maximum; the direct queue
budget is denominated in the measured pipe (FR-59 P2's `constrained_queue_measured`
shape, today constrained-only), so a burst the wire drains is not read as
congestion. A path that does show congestion keeps today's behaviour byte for byte
— the AIMD's cut and climb are the right response there.

### P2 — fewer keyframes on QSV

Hysteresis at the top of the bitrate ladder, so a target hovering near a rung
boundary does not cross it every few seconds; each crossing is a rebuild and an
IDR. Coarser rungs above 8 Mbps where the bits/quality slope is flat. Measured by
`swaps` and keyframes per minute in a scroll session.

### P3 — a still-text floor at native

Extend the idle refine to native: once motion settles, one re-encode of the settled
frame at a sharper CQ (a still frame costs nothing against the cap). Evaluate
4:4:4 where the host can — QSV on Iris Xe advertises HEVC/AV1 4:2:0 only, so on
CORPLAP-3 4:4:4 means libvpx VP9-444 software at ~12–25 fps. It is a side check for
the residual chroma softness of SETTLED text only — the scrolling blur and the
settle time are on the AV1 4:2:0 hardware path and are fixed there (P1, P2), not
by changing codec.

The P0 table therefore has three cells, all on the av1_qsv path the operator uses;
a 4:4:4 comparison, if ever run, is a P3 cell with its fps cost stated up front.

### P4 — the viewer's pixel chain

Show the display scale beside the encode dims in the pill (`shown at 0.9×`), verify
the 1:1 case, and point at "Match remote display" when the stage and the frame
disagree. FSR helps only when upscaling.

## Phases

| phase | scope | kill switch | status |
|---|---|---|---|
| P0 | A/B with existing knobs on CORPLAP-3 | — (settings only) | proposed |
| P1 | content-following direct ceiling + measured queue budget | `direct_ceiling_follows` (one release default off) | proposed |
| P2 | ladder hysteresis on QSV | — (pure policy, measured by `swaps`) | proposed |
| P3 | idle refine at native; 4:4:4 only as an optional, costed side check | `native_refine` | proposed |
| P4 | viewer display-scale pill + 1:1 guidance | — (UI) | proposed |

## Acceptance criteria

- [ ] **AC1** — a Notepad++ scroll on CORPLAP-3 stays readable while it moves (no
      unreadable phase), and the settled text matches a local screenshot of the
      same region (pixel comparison of a text block, not an impression).
- [ ] **AC2** — sharpness is stable within 1 s of the scroll ending; keyframes per
      minute in a scroll session drop from ~7 to ≤ 2.
- [ ] **AC3** — no regression on constrained hosts: CORPLAP-1/-2's `target_bps`,
      `frames_skipped`, `pipe_states` unchanged across the release (direct-only
      change).
- [ ] **AC4** — every phase carries a before/after from the same instrument (the
      heartbeat's `target_bps`, `frames_skipped`, `swaps`, keyframes) plus the
      operator's read of the same scroll.

## Open decisions

- Whether the direct ceiling should stay bpp-scaled at all (a higher constant is
  still a constant) or be purely measured — probe-and-follow.
- Whether a 4:4:4 "text" choice is worth offering at all on hosts whose hardware
  encodes 4:2:0 only (software VP9 at a fraction of the frame rate).
- Whether P1's ceiling lift needs the viewer's decode capacity as an input (a 24 Mbps
  AV1 stream on a laptop decoder).

## Out of scope

Constrained and relay paths (FR-59 / 62 / 63 own those); the session-start open
(FR-70's AC2 open half); the transport stall classification (FR-71).

## Related

FR-59 (queue budget, measured pipe) · FR-62 / FR-63 (rate control) · FR-70 (media
pipeline, the heartbeat instrument) · FR-71 (transport stalls) · the RC quality
program FR-17 / 16 / 14.

## Field-verification log

| when | build | host | what |
|---|---|---|---|
| 2026-09-06 19:03 UTC | 0.4.75 | CORPLAP-3, av1_qsv 1920×1200, direct, Sharper | The opening evidence (above): ceiling 9.68 Mbps, three ×0.85 cuts per scroll, ~40 s climb, 37 swaps + 35 settle keyframes in 11 min, no idle refine at native. Operator's read: unreadable while scrolling, 5–8 s to settle, not crystal clear after |
