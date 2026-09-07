# FR-74 — Text clarity on direct paths: the bitrate ceiling follows the content, not a constant

**Issue:** [#1442](https://github.com/gjovanov/roomler-ai/issues/1442) · **Status:** P0 done 2026-09-06;
P1 (0.4.77) + P1b (0.4.79) + P3 (0.4.80) released and **field-verified 2026-09-07 on all four
codecs** by the operator's read and the heartbeat; P2 retired by measurement (0 rate swaps);
AC3 ticked; open: AC1's pixel comparison, the thin-direct-path read of the q-cap, P4 (UI) ·
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

**Results (2026-09-06, the operator judging every cell on the same file).**

- **Baseline #2 (FAIL first)**, the operator's session `6a9dc448` at 19:51 UTC on
  0.4.76: one scroll took the target 9.68 → 8.23 → 6.99 → 5.05 → 3.65 → 3.10 →
  2.24 Mbps in 16 s (six ×0.85 cuts) while the gate skipped 49 frames, and it
  never recovered — 2.1–3.7 Mbps for minutes, every +605 kbps climb undone by the
  next small burst. The direct queue budget is 150 ms of the **applied** target,
  so at 2.5 Mbps it is ~47 KB and one text frame trips it: a self-reinforcing
  trap. The "stabilized but not crystal clear" picture was 1920×1200 AV1 at
  ~2.5 Mbps.
- **Cell A** (`direct_queue_ms` 600, restart 20:08): the trap is gone — one cut
  on the scroll (9.68 → 8.23), back at the ceiling in 28 s, 28 skipped frames
  instead of 81, 9 `set_bitrate` instead of 57, **0 rate swaps instead of 37**.
  Operator: still blurred while scrolling, especially the line-number gutter.
- **Cell B** (A + cap 24 Mbps, restart 20:15): a 40 s scroll at 24 Mbps with 0
  cuts, 0 skips, no gate, 12–25 Mbps sent, 25–40 fps. Operator: clear at first,
  blurred after 4–5 s of continuous scrolling (the cap's 2× VBV drains, then QP
  climbs to hold the cap), clears ~1 s after stopping (the idle-settle keyframe;
  was 3 s), and the second stop within 5 s stays blurred (the settle-keyframe
  gate's `SETTLE_KF_MIN_GAP`).
- **Cell B2** (A + cap 40 Mbps, restart 20:34), all four codecs tried: **"could
  not reproduce it with AV1, VP9 4:2:0 and H.264 — only with VP9 4:4:4 is it
  still happening."** The heartbeats agree — AV1 7 scroll windows, 0 cuts, 0
  skips, 15.6 Mbps, 31 fps; vp9_qsv 0 cuts, 15.7 Mbps, 26 fps; h264_qsv 0 cuts,
  23 Mbps, 32 fps; two more AV1 sessions at 22 Mbps, 32–43 fps, one 55 Mbps
  burst window absorbed with no cut.
- **What it decided.** On the FFmpeg hardware pump the ceiling and its budget
  were the whole scrolling problem: P1 is the build, cell C is not needed for
  the scroll. VP9 4:4:4 is the libvpx software pump and neither knob reaches it:
  it captured ~7–19 real frames per second at 1920×1200 4:4:4 (CPU-bound) while
  repeat-encoding 30 per second at `cpu_used=6` against a 20.7 Mbps target, so
  each real full-screen text delta got a 30-fps slot's bits — `avg_qp` 108 → 184
  of 255 in the scroll, 5 at rest. That is P3's item, with its own mechanism.
- Left live on CORPLAP-3 until P1 ships: `direct_queue_ms = 600` (config) and
  the machine environment `ROOMLERD_FFMPEG_MAXRATE_KBPS=40000`. Revert:
  `roomler config clear direct_queue_ms`, remove the variable, restart.

### P1 — a content-following ceiling on direct paths

When the path is direct and the controller has no congestion evidence of its own
(viewer age flat, no measured goodput below the target — FR-59's `measured_pipe_bps`
and FR-70 M0's split are the instruments), the ceiling lifts toward the measured
pipe instead of the bpp constant, bounded by a sane maximum; the direct queue
budget is denominated in the measured pipe (FR-59 P2's `constrained_queue_measured`
shape, today constrained-only), so a burst the wire drains is not read as
congestion. A path that does show congestion keeps today's behaviour byte for byte
— the AIMD's cut and climb are the right response there.

### P1 — as built (2026-09-06)

Two changes, both on the direct branch of the FFmpeg pump, relay paths untouched
byte for byte, no new switch:

1. **The direct ceiling is a content-generous bound.** `ffmpeg_maxrate_bps_scaled`
   uses 0.25 bpp/s on direct paths (34.6 Mbps at 1920×1200 @ 60), clamped to
   [3, 48] Mbps per codec factor (H.264's ×1.5 lands at 51.8 Mbps; a 4K panel at
   the 48 M top; small rungs stay on the 3 M floor). The constrained branch keeps
   0.07 / [3, 12] and the relay clamp, so every relay session sees exactly what it
   saw. The cap stays a ceiling, never a target: constant-quality rate control
   spends what the content demands and the AIMD follows the pipe below the cap on
   evidence (viewer age, the byte-budget gate).
2. **The direct send-queue budget is denominated in the path's ceiling.**
   `direct_queue_ms` (still 150 by default) is resolved against
   `last_ceiling_bps` instead of the AIMD's applied target. The applied-target
   reference was the self-reinforcing trap P0 measured: at 2.5 Mbps the budget
   was ~47 KB, one text frame tripped the gate, and every climb was cut again.
   A burst the wire drains now passes; a real backlog still trips the gate and
   the AIMD still cuts on that evidence.

Way back: `FFMPEG_MAXRATE_KBPS` (env) and `direct_queue_ms` (config) are the
operator's overrides in both directions, as before. What P1 does not do: it does
not measure the pipe on a direct path (there is no goodput estimate on an
uncongested link); the "follow" half of the design is the AIMD's existing
response to real congestion under a bound that no longer binds on screen
content. Whether a measured direct pipe should also lower the bound (a thin
Wi-Fi) is the open decision left to the field read.

**Field gate.** The P0 knobs on CORPLAP-3 are cleared before the release carrying
this rolls there, so the release's defaults are what is tested: the operator's
Notepad++ scroll on AV1, VP9 4:2:0 and H.264 stays readable while it moves
(AC1), the heartbeat shows no cuts and no gate skips through the scroll windows,
and the relay hosts' counters are unchanged (AC3).

### P1b — the gate judges the measured wait (2026-09-07)

The 0.4.77 gate read (session `6a9e5b03`, CORPLAP-3, av1_qsv 1920×1200 direct,
the release's defaults, P0 knobs cleared) showed both halves of P1 working —
ceiling 34.56 Mbps, scroll windows at 20–36 Mbps and 30–47 fps, viewer age
≤ 20 ms throughout — and one residual: the 150 ms budget (648 KB at that
ceiling) still tripped on the AV1 scroll burst. AV1's HRD window is floored at
200 % of maxrate (8.6 MB) because Intel's VDENC hangs on a forced IDR larger
than its reservoir (the rc.443 incident), so the encoder was *configured* to
burst far past a budget the controller then read as congestion: gate ×1, 54
skipped frames (~8 % of the scroll), two ×0.85 cuts 34.56 → 26.8 Mbps and
~10 s of additive climb back — while the wire drained every byte at ≤ 20 ms of
age. A controller cutting on a burst it had itself legalised.

P1b makes the direct gate the **measured wait's** call:

- **Measure.** The send task already records enqueue→wire-complete per frame
  (`send_wait_us_*`, the P7 telemetry). The pump now keeps an EMA (α = 0.3 per
  pass) of the per-pass average of those waits and — new — the *live age of the
  frame the send task is writing* (`send_head_enqueued_us`): a stalled pipe
  completes nothing, so a completion-based estimate reads stale-low exactly
  when the queue is growing. The gate's wait is the larger of the two.
- **Rule** (`rate_profile::direct_gate_trips`): bytes over the P1 budget gate
  only when the measured wait has also crossed `direct_queue_ms`; bytes alone
  gate at the hard ceiling `max(budget, reservoir)`, where the reservoir is the
  encoder's own HRD window in bytes (`direct_queue_hard_budget_bytes`, with the
  codec's effective `open_hrd_pct` — AV1's 200 % floor included). A LAN scroll
  (1.2 MB in flight at 20 ms) passes; the same bytes at 150 ms on a thin wire
  trip, the AIMD cuts on that evidence as before. Relay paths are untouched
  byte for byte; `0` still disables the gate; no new switch — `direct_queue_ms`
  keeps its meaning, which was always the lag bound.

What it deletes: the byte count as a *proxy* for lag on direct paths. What it
does not do: a standing lag the viewer feels stays the viewer age's call
(FR-15) — this gate bounds the sender-side queue only. The first-time gate log
line now carries `hard_budget` and `measured_wait_ms`, so a field read can tell
which arm tripped.

**Field gate.** The same Notepad++ scroll on the release's defaults: the
heartbeat's scroll windows show `gate: 0`, no `frames_skipped` growth and no
cuts, and the "direct byte-budget gate engaged" line is absent from the session.
### P2 — fewer keyframes on QSV

Hysteresis at the top of the bitrate ladder, so a target hovering near a rung
boundary does not cross it every few seconds; each crossing is a rebuild and an
IDR. Coarser rungs above 8 Mbps where the bits/quality slope is flat. Measured by
`swaps` and keyframes per minute in a scroll session.

### P3 — the libvpx pump: cap the worst quality on direct paths (2026-09-07)

**What the field showed after P1b.** With AV1, VP9 4:2:0 and H.264 clean by the
operator's read, VP9 4:4:4 still blurred on the same scroll. Its heartbeat (1 s
windows, session `6a9e8145`) had this shape: 30 encodes/s in motion, 15/s at idle
(the 60 ms keepalive re-encodes the same frame at ~6 kbps), and in the scroll
windows **8–13 Mbps spent of the 20.7 Mbps CBR target at avg q 113–192 / max
255** — under budget and at the worst quality at the same time. A steady short
scroll at the end converged to q 14–23 at 10 Mbps. So the hypothesis that opened
this phase (a 30-fps budget spread over ~10 real captures) did not fit: the
encoder was not out of bits, it was choosing not to spend them.

**Measured offline, not argued** — an ignored test in `encode/libvpx.rs`
(`fr74_p3_offline_scroll_rate_control`) feeds the real encoder a synthetic
1920×1200 text page: 20 warm frames, 3 s of idle keepalive duplicates (or none),
a 90-frame steady scroll at 24 px/frame, a stop, and a **wheel pattern** (a 54 px
notch, then four repeat frames at the keepalive cadence, ×12). Per-frame q and
bytes, per arm. (WSL: build the harness with
`cargo rustc -p roomlerd --lib --profile test --features ffmpeg-encoder,vp9-444 -- -A warnings`
and run the deps binary — a plain `cargo test` under `vp9-444` alone crashes
rustc's diagnostic renderer on a governor dead-code warning, not on the test.)

| round | arm | steady scroll | wheel notches | reading |
|---|---|---|---|---|
| 1 | as shipped (CBR, cpu-used 6, idle dups fed) | first frame **q 255**, then −7/frame to 0 by ~frame 40; 17.3 Mbps | — | a scene change resets q to the worst and libvpx walks it back over ~1 s |
| 1 | no idle duplicates | identical | — | the keepalive duplicates are innocent (the rate-factor theory refuted) |
| 1 | VBR / CQ modes | q 16 → **255 and pinned**, 2 Mbps | — | one-pass VBR/CQ go into "debt" and stay there — unusable here |
| 1 | cpu-used 5 / target ×2 | identical descent | — | neither speed nor budget enters into it |
| 2 | as shipped | — | **every notch q 255**, mean 231, **4 Mbps** | the field reproduced: worst quality at a fifth of the budget |
| 2 | content tune default / overshoot 100 / cyclic refresh | — | 255/193 alternating · no change · no change | no knob on the rate control fixes it |
| 2 | `rc_max_quantizer` 40 / 32 | descends from the cap | notches AT the cap (q-index 160 / 128) | the cap is the one lever that bounds the damage |
| 3 | constant quality (`VPX_Q`) cq 12–32 | q ≤ 48–128 by construction, **~9 Mbps** | ~8 Mbps | sharp, cheap — but no refine to lossless at idle, no rate bound at all |
| 4 | **CBR + `rc_max_quantizer` 16 / 20 / 24** | cap → 42 → 39 → 33 → … → **0 within ~1 s**, 14.0–14.7 Mbps | **all notches at 64 / 80 / 96**, 7.2–7.5 Mbps | notches readable, refine to lossless kept, the target still an average bound |

**Mechanism.** libvpx's one-pass CBR with the screen-content tune treats each
wheel notch (54 px = most pixels change) as a scene change: `high_source_sad` →
`calc_active_worst_quality_one_pass_cbr` returns `worst_quality` and the ambient
q is reset to it, after which q can only fall as fast as the ambient average
moves (~7 q-index per frame). A steady scroll gets there in a second; a wheel
scroll restarts the walk at every notch and is rendered at q 255 throughout —
while the encoder sits far below its target, because the reset is not a budget
decision at all.

**Built.** `Vp9Encoder::set_max_quantizer` (a runtime `vpx_codec_enc_config_set`,
no IDR) and `vp9_direct_max_q_from_env` (default **16** = q-index 64, env
`ROOMLERD_VP9_DIRECT_MAX_Q`, clamp 0–63, **63 = the pre-P3 behaviour** — the way
back). The pump applies the cap on a DIRECT transport at encoder open and
re-applies it on every transport flip (uncapped again on a relay, where the rate
cap is what matters). Nothing else changes: CBR, the target, the AIMD, the idle
keepalive, the settle keyframe, `cpu_used` — all as before. 16 rather than 20 or
24 because the three cost the same bytes on the synthetic scroll and 16 is the
sharpest; `avg_qp` / `max_qp` in the heartbeat are the instrument.

**Field gate.** The operator's Notepad++ wheel scroll on CORPLAP-3 with VP9
4:4:4: readable while it moves; the heartbeat's scroll windows show `max_qp` ≤ 64
with the bitrate inside the target (the pre-P3 shape was max 255 at 8–13 Mbps of
20.7); settled text still refines to q 0.

### P4 — the viewer's pixel chain

Show the display scale beside the encode dims in the pill (`shown at 0.9×`), verify
the 1:1 case, and point at "Match remote display" when the stage and the frame
disagree. FSR helps only when upscaling.

## Phases

| phase | scope | kill switch | status |
|---|---|---|---|
| P0 | A/B with existing knobs on CORPLAP-3 | — (settings only) | **done 2026-09-06** — A + B2 remove the blur on AV1, VP9 4:2:0 and H.264 by the operator's read; the queue budget denominated in the applied target was a self-reinforcing trap, the cap the limiter; VP9 4:4:4 (software) remains |
| P1 | direct ceiling 0.25 bpp / [3, 48] M; direct queue budget denominated in the ceiling | — (no switch: `FFMPEG_MAXRATE_KBPS` and `direct_queue_ms` are the way back) | **built 2026-09-06** (§"P1 — as built"); field gate on the release carrying it |
| P1b | the direct gate is the measured send wait's call below the encoder's HRD reservoir (EMA of completed waits ∨ live head-of-queue age); bytes alone gate only at the reservoir | — (no switch; `direct_queue_ms` keeps its meaning as the lag bound, `0` disables) | **field-verified 2026-09-07 on 0.4.79** — operator: "not seeing the blurring anymore" on AV1, VP9 4:2:0 and H.264; heartbeats of those sessions: 0 cuts, 0 gate skips, 0 gate lines. VP9 4:4:4 (the libvpx pump) still blurs ⇒ P3 |
| P2 | ladder hysteresis on QSV | — (pure policy, measured by `swaps`) | **retired by measurement 2026-09-07** — after P1 the rate ladder no longer fires on direct paths: 0 rate swaps in all 17 sessions on the three hosts today (QSV direct on CORPLAP-1/-3, nvenc relay on CORPLAP-2; the 2026-09-06 baseline had 37 in 11 min). Reopen only if a relay-path QSV session shows swaps |
| P3 | the libvpx pump: `rc_max_quantizer` 16 on DIRECT transports (63 on relay) — libvpx's scene-change reset to the worst quality on every wheel notch was the 4:4:4 blur, measured offline in four rounds (§P3) | `ROOMLERD_VP9_DIRECT_MAX_Q` (63 = pre-P3) | **built 2026-09-07, released in 0.4.80 (11:01 UTC on CORPLAP-3)** — offline: every notch frame at q 64 instead of 255, refine to lossless kept, 14.5 of 20.7 Mbps; field gate: **instrument PASS 13:25 UTC** (`max_qp` 64 in every scroll window, was 255; settles to q 0; 0 skips; 22–52 Mbps on the LAN) and **operator PASS** ("scrolling large texts seems much better") ⇒ **field-verified 2026-09-07** |
| P4 | viewer display-scale pill + 1:1 guidance | — (UI) | proposed |

## Acceptance criteria

- [ ] **AC1** — a Notepad++ scroll on CORPLAP-3 stays readable while it moves (no
      unreadable phase), and the settled text matches a local screenshot of the
      same region (pixel comparison of a text block, not an impression).
      *Operator half met on all four codecs 2026-09-07 (0.4.79: AV1, VP9 4:2:0,
      H.264 — "not seeing the blurring anymore"; 0.4.80: VP9 4:4:4 — "scrolling
      large texts seems much better"); the pixel comparison not yet done.*
- [ ] **AC2** — sharpness is stable within 1 s of the scroll ending; keyframes per
      minute in a scroll session drop from ~7 to ≤ 2.
      *Measured 2026-09-07 on 0.4.79/0.4.80 (CORPLAP-3, 11 sessions): the ~7/min of
      the baseline were the QSV rate ladder (37 swaps + 35 settle keyframes in
      11 min); rate swaps are now **0** in every session. What remains is the
      idle-settle keyframe, one per scroll stop at ≥ 5 s spacing — 6–12/min in a
      stop-and-go scroll, 0.3/min over a 2-hour session — and it is the mechanism
      that makes the settled text sharp within ~1 s (first half met). The "≤ 2/min"
      figure was a proxy for the ladder pumping and is superseded: the criterion
      that survives is "no keyframes from rate changes", which holds.*
- [x] **AC3** — no regression on constrained hosts: CORPLAP-1/-2's `target_bps`,
      `frames_skipped`, `pipe_states` unchanged across the release (direct-only
      change). *Read 2026-09-07 on 0.4.79 (field log): the relay sessions show
      the relay-clamped targets, a handful of skips per hour and the usual FR-71
      stall mix; no direct gate line can occur there and none did.*
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
| 2026-09-06 19:51 UTC | 0.4.76 | CORPLAP-3, av1_qsv, direct, Sharper, no P0 keys | **Baseline #2 (FAIL first)**: six ×0.85 cuts in 16 s (9.68 → 2.24 Mbps), 49 gate-skipped frames, then 2.1–3.7 Mbps for minutes — the queue budget (150 ms of the applied target, ~47 KB at 2.5 Mbps) tripping on every text frame |
| 2026-09-06 20:10–20:31 UTC | 0.4.76 | CORPLAP-3, av1_qsv, direct | **Cell A** (`direct_queue_ms` 600): one cut, back in 28 s, 0 rate swaps (was 37); operator: still blurred, especially the gutter. **Cell B** (+ cap 24 Mbps): 40 s scroll at 24 Mbps, 0 cuts / 0 skips, 12–25 Mbps, 25–40 fps; operator: clear, then blurred after 4–5 s (the 2× VBV draining), clears ~1 s after stopping, the second stop within 5 s stays blurred (settle-keyframe gap) |
| 2026-09-06 21:25–21:28 UTC | 0.4.76 | CORPLAP-3, direct, all four codecs | **Cell B2** (A + cap 40 Mbps): operator — "could not reproduce it with AV1, VP9 4:2:0 and H.264 — only with VP9 4:4:4". AV1 0 cuts / 0 skips / 15.6–22 Mbps / 31–43 fps (a 55 Mbps burst window absorbed); vp9_qsv 0 cuts / 15.7 Mbps / 26 fps; h264_qsv 0 cuts / 23 Mbps / 32 fps. VP9-444 (libvpx SW) `avg_qp` 108 → 184/255 in the scroll on ~10 real captures/s repeat-encoded at 30, target 20.7 Mbps — its own mechanism (P3) |
| 2026-09-07 06:34–06:40 UTC | **0.4.78** (the auto-updater had rolled it at 06:19; same pump code as 0.4.77) | CORPLAP-3, av1_qsv 1920×1200, direct, defaults (P0 knobs cleared 22:23 UTC the day before) | **P1 gate read**, session `6a9e5b03`: ceiling 34,560,000 confirmed; scroll windows 20–36 Mbps at 30–47 fps, viewer age ≤ 20 ms; **residual** — the 150 ms budget (648 KB) tripped once on the AV1 VBV burst (HRD floored at 200 % = 8.6 MB reservoir): 54 skips (~8 %), two ×0.85 cuts 34.56 → 29.38 → 26.8 Mbps, back within ~10 s; `set_bitrate: 7 swaps: 0 settle-KF: 10 gate: 1`. Operator's read of this session pending. ⇒ P1b |
| 2026-09-07 09:12–09:20 UTC | 0.4.79 | CORPLAP-3, direct, defaults, the operator judging | **P1b gate — PASS on the HW codecs, the 4:4:4 pump is P3.** Operator: "still only with VP9 4:4:4 … it gets blurred. In the other codecs like AV1, VP9 4:2:0 and H.264 I'm not seeing the blurring anymore." Heartbeats: av1_qsv `6a9e8017` (29.4 M opening climb → 34.56 M, 0 skips, 0 gate lines, 29 MB), vp9_qsv `6a9e806b` (43.2 M flat, 0 / 0, 22 MB), h264_qsv `6a9e8087` (44.1 → 50.5 M, 0 / 0, 44 MB). VP9 4:4:4 (libvpx SW) `6a9e8039` + `6a9e8145`: 20.7 M target flat, **8–13 Mbps actually spent in the scroll windows at avg QP 113–192 / max 255**, encodes at 30/s in motion and 15/s at idle (the 60 ms keepalive re-encodes the same frame at ~6 kbps for the whole idle), 1830 encodes for 481 captures; the last short scroll converged to QP 14–23 at 10 Mbps. |
| 2026-09-07 09:30–10:00 UTC | offline (libvpx 1.14, WSL) | the real encoder on a synthetic 1920×1200 text page | **P3 rounds 1–4** (table in §P3): as shipped, every wheel-notch frame is encoded at **q 255** while spending 4 Mbps of 20.7; idle duplicates innocent; VBR/CQ pinned at 255; content tune / overshoot / cyclic refresh / cpu-used / target ×2 change nothing; `rc_max_quantizer` 16 holds every notch at q 64 and keeps the refine to lossless (14.5 Mbps of 20.7 on the steady scroll); constant-quality mode is sharp and cheap (~9 Mbps) but loses the idle refine and the rate bound. ⇒ P3 = the cap. |
| 2026-09-07 09:20–10:44 UTC | 0.4.79 | CORPLAP-1 + CORPLAP-2, every session since the 07:29 restart | **AC3 read.** CORPLAP-2 relay (av1_nvenc, `constrained=true`): a 66-min session at 0 → 7.45 Mbps, 15 backpressure skips, 0 gate lines, `pipe_states` [1, 3761, 1, 53, 0] (1.4 % transit-stalled — the FR-71 gap mix of every previous read); a 7.8-min session 0.2 → 3.0 M, 4 skips; three short ones 0 skips. CORPLAP-1 ran **direct** today (hevc_qsv / vp9_qsv at 43.2 M, 0 skips, 0 gate lines, `pipe_states` all 0). The constrained branch is untouched by P1/P1b/P3 and the counters agree ⇒ AC3 holds. |
| 2026-09-07 10:59–11:04 UTC | 0.4.80 | release | P3 (#1463 → `64bee350`, bump #1464 → `ad3fc039`, 28 assets). CORPLAP-3 pid 7968 (11:01:47), CORPLAP-1 pid 5320 (11:02:30), CORPLAP-2 pid 15416 (11:03:33), each updated while idle. Field gate open: the operator's VP9 4:4:4 wheel scroll on CORPLAP-3. |
| 2026-09-07 13:25 UTC | 0.4.80 | CORPLAP-3, VP9 4:4:4 (libvpx), direct, the operator's scroll | **P3 gate — instrument PASS.** The session opened with `worst-quality cap applied max_q=16`; through the 17 s scroll every 1 s window had **`max_qp` = 64** (0.4.79: max 255, avg 113–192), avg 45–64 while moving, then 4 → 0 within ~2 s of stopping (refine to lossless intact); 0 skips; viewer age ≤ 39 ms. Bitrate 22–52 Mbps in the scroll windows — **above the 20.7 M target**: with q pinned at ≤ 64 the CBR target is a soft bound on content that needs more at that quality; on this direct path it cost nothing, and on a thinner one the DC buffered-bytes gate sheds frames rather than sharpness (the intended trade for a mode chosen for text). Operator's read pending. |
