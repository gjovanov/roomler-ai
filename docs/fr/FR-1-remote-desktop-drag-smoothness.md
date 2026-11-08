# FR-1: RustDesk-parity remote-desktop drag smoothness

**Status:** phases P1–P4 shipped + field-verified; P2/P3 shipped, relay-gated after field
read; P5 shipped in 0.4.4; P6 + P7-HUD shipped in #793 (0.4.6 + web). **Field reads 2026-10-26 and 2026-11-02 (prod `agent_logs`):** P7's end-to-end age is on 96 % of the week's heartbeat windows, and the pill read in a real viewer matches the agent's `viewer_age_ms` (AC8, partial: held for #3566, the age a long-lived tab reads off by its page↔worker clock drift, until its fix #3568 is on production); P5's `>=45` fps at 2880x1800 Iris-Xe is reached with the frame on the GPU (`zero_copy`, 60 fps, encode 16.3 ms) and not on today's default path (38 fps; AC6, closes with FR-132 or the operator's call); P6's field read is owed (AC7); AC9 is the operator's A/B. Write-up: `docs/remote-desktop-latency.md`. Tracking issue: `FR-1` in gjovanov/roomler-ai/issues.

## Goal

Dragging a window on a remote screen through the browser viewer must feel as fast and
smooth as RustDesk on the same pair — and better where our hardware allows. Reference
field test (2026-08-26, neo16 → WINHOST-J, same LAN, direct): RustDesk H265 @ ~7 Mbps /
29 fps felt instant; roomler hevc_qsv 2880×1800 @ "10 Mbps / 40 fps" felt sluggish,
bulky, with occasional freezes.

## Root cause (field-evidenced from the agent's own 2 s heartbeats)

The network and the viewer were exonerated (DC drained ~9.8 Mbps; neo16 decoded 40 fps HW
with zero `viewer-rate` struggle reports). The drag feel was manufactured by the agent's
rate control:

1. Sessions (re)open at the nominal ceiling (15 Mbps at 2880×1800) on a ~10 Mbps pipe;
   nothing measured the pipe (`goodput_samples: "(0, N)"` all session — the stage-0
   busy-period rule was structurally unsatisfiable at per-frame granularity).
2. The AIMD collapsed 12.75→1.5 Mbps in ~10 s per burst, then ladder-climbed back — and on
   QSV **every rung crossing was a blocking encoder re-open + fresh IDR** (13 re-opens in
   one 13-minute session; 0.65–0.87 s each on Iris-Xe-class).
3. The rc.445 motion-defer held every rate DROP for the whole drag: the encoder ran a
   stale-high maxrate through the motion and congestion landed as **100–345 KB of standing
   send queue (~100–300 ms of felt lag) plus 3–7 production skips/s** (the "chunks").
4. `bufsize = 2×maxrate` legalised multi-second bursts; the flat 1.5 Mbps AIMD floor is
   0.006 bpp mush at 5.2 MPix; encode is ~19–32 ms/frame at native (single-threaded
   BGRA→NV12 inside `encode_sync`), capping ~40 fps with 30–36 % WGC drops.

## Design — phases (each with its own kill switch, per the config-surface rule)

| Phase | What | Key knobs | Status |
|---|---|---|---|
| P1 | Byte-budget gate on DIRECT (150 ms of the AIMD's live applied target); send depth 12→6 | `direct_queue_ms` | ✅ rc.483 |
| P2 | Measured-rate v2 (per-frame blocked-send ≥10 ms samples, window-aggregated ≥60 ms) + stage-1 `ceiling := min(nominal, 0.85×G)` | `measured_ceiling` | ✅ rc.484 · **direct-only since 0.4.3** |
| P3 | Background encoder rebuild: replacement opens on a blocking thread while the current encoder keeps producing; swap between frames + `send_epoch` bump | `bg_rebuild` | ✅ rc.484 · **direct-only since 0.4.3** |
| P4 | Direct HRD window 2×→1× (`av1_*` floored at 2× — rc.443 VDENC kill) + area-scaled AIMD floor (~3.1 M @ 2880×1800, cap 4 M, unconstrained only) | `direct_hrd_pct`, `area_min_bitrate` | ✅ rc.483 |
| P5 | Encode cost & cadence: parallel `convert_bgra` (row bands); `block_in_place` the encode; encode-pressure sheds **fps first** on HW (paced, even cadence) instead of the bitrate factor | `par_convert`, `fps_pace` | ✅ 0.4.4 (#781) — **field PASS** ("WINHOST-J works nicely", 2026-08-27) |
| P6 | Pointer cadence decoupled from rAF: immediate-send + 8 ms min-gap timer (latest-wins), `pointerrawupdate` sampling where supported | web deploy | ✅ #793 — field gate pending |
| P7 | Latency telemetry: agent `send_wait_avg/max_ms` (✅ rc.483) + HUD end-to-end age | — | ✅ #793 (agent 0.4.6 + web deploy) — field gate pending |

P7 design note (differs from the original sketch, deliberately): no frame-header change.
The 13-byte header already carries a µs timestamp; #793 makes BOTH pumps stamp it from one
process-wide epoch (`agent_epoch_us`) and adds an `rc:clock` control verb echoing that same
clock. The viewer keeps the lowest-RTT probe of 8 (fixed-origin clocks ⇒ the offset is a
constant; min-RTT bounds the asymmetry error to RTT/2 of the BEST probe) and the decode
workers measure age at paint: `age = epochNow + offset − wireTs`, covering encode-output →
send queue → network → decode → paint. Agent-side capture+encode (~10–15 ms) sits before
the stamp and is excluded — the pill's `~NN ms` reads ~15 ms low against a photographed
input-to-photon. Old agents never echo ⇒ the pill simply doesn't show an age.

Relay note (field 2026-08-27): P2 and P3 are **relay-hostile** — per-frame samples through
a lumpy TURN-TCP pipe read near-zero during stalls (down-fast EWMA crashes the derived
ceiling to its floor), and a swap's adoption IDR is a multi-second lump at ~2 Mbps. Both
are gated `!constrained` since 0.4.3; relay keeps the field-proven rc.483 posture (nominal
clamp + defer-at-quiet). Goodput stays observe-only on relay as the dataset for a future
lumpiness-robust estimator (its own follow-up FR when picked up).

## Acceptance criteria

- [x] Direct drag: standing send queue bounded (`bytes_inflight` no longer riding
      200–345 KB; `send_wait_avg_ms` low and even through drags)
- [x] Zero blocking QSV re-opens mid-drag on direct (`background-rebuilt encoder adopted`
      replaces `QSV/AMF encoder rebuilt` stalls)
- [x] AIMD tops out just under the measured pipe on direct (no more 15 M restarts /
      floor visits in steady state)
- [x] Field: direct drag "definitively faster" (rc.483) and "much better" (0.4.x,
      verified from neo16, CORPLAP-1 AND the MacBook viewing WINHOST-J)
- [x] Relay unaffected vs rc.483 (0.4.3 gating; regression caught same-day on CORPLAP-3 +
      CORPLAP-2 and reverted)
- [~] P5: ≥45 fps at 2880×1800 on Iris-Xe-class with even cadence (WGC drop ratio no
      longer ~33 % random; `avg_encode_ms` ≤ ~17 at native). *Met with `zero_copy` on
      (2026-10-30, FR-118 §9.10: 594 of 596 windows at ≥ 50 fps, median 60.0, encode
      16.34 ms). Not met on the default path: 0 of 1 701 windows at ≥ 45 in the week to
      2026-11-02, highest 42.8. WGC drop ratio median 2 % on the one WGC host, which is
      not Iris-Xe. Closes when FR-132 makes the GPU path the default, or on the
      operator's call that the switch-on proof is enough.*
- [ ] P6: pointer send rate independent of viewer rAF load. *Two real-pointer tries on
      2026-11-03, no result: the second stopped before its controls ran, and the first
      missed its 16 ms control. While input flowed, sends held 125-129/s from rAF 48 down
      to rAF 5. Owed: one run with its controls, on an idle host.*
- [~] P7: HUD shows end-to-end age; drag test becomes a number comparable to RustDesk's
      "Delay". *The number exists fleet-wide: 123 375 of 128 325 heartbeat windows in the
      week to 2026-11-02, direct median 15 ms, relay 42 ms. The pill was read in a real
      viewer beside the agent's `viewer_age_ms` on 2026-11-02: 63 of 66 heartbeats equal a
      value the pill showed within 5 s. The side-by-side with RustDesk's readout is AC9.
      Held 2026-11-08 for #3566: the decode worker applied a clock offset the page measured
      on the page's own clock, so a session that starts long after its page loaded shows an
      age off by the drift between the two (7 ms after 162 s on one host, near 160 ms after
      an hour), and the week's medians carry the same error. The 2026-11-02 read started
      each session right after loading its page, and the pill and the wire carry one
      number, so that read stands. The fix, #3568, merged on 2026-11-08 and waits for a
      promote; the box ticks once it is on production and its GREEN read is in.*
- [ ] Final A/B on the reference pair: subjective parity-or-better vs RustDesk on BOTH
      transports
- [x] Docs: `docs/remote-desktop-latency.md` created (P5/P6/P7 design + the 2026-10-26
      field evidence, mermaid pipeline, `file:line` anchors), linked from
      `docs/README.md`'s index

## Open decisions

1. Relay-robust measured ceiling — estimator design (longer aggregation / stall-aware /
   median-of-windows) once enough relay heartbeat data accumulates.
2. The ~10 Mbps drain plateau: Wi-Fi leg vs webrtc-rs SCTP ceiling (`iperf3`
   neo16↔WINHOST-J decides; only matters for >10 Mbps ambitions).
3. Background dims/tier rebuilds (still synchronous; rare after P2, but each tier flip is
   a stall — P3's swap machinery could cover them).

## Out of scope

- macOS capture-side scaling (CG-side downscale) — separate arc.
- MoQ / transport replacement — SCTP stays; we just stopped overdriving it.

## Field-verification log

| Release | Result |
|---|---|
| rc.483 (P1+P4) | "definitively feels faster movement, but still bulky in several ms chunks/steps" — the byte gate converting queue-lag into skips against the still-unmeasured overrun |
| rc.484 (P2+P3) | WINHOST-J direct = better; CORPLAP-3 + CORPLAP-2 over corp relay = WORSE than rc.483 (both mechanisms relay-hostile — see relay note) |
| 0.4.3 (relay gating) | WINHOST-J "much better now", verified from neo16, CORPLAP-1 and MacBook; relay reverted to rc.483 posture (re-test pending) |
| 0.4.4 (P5) | **field PASS 2026-08-27**: "WINHOST-J's screen works nicely now" (direct); relay pairs handled by FR-10 (0.4.5, its own field PASS) |
| 0.4.6 + web deploy (P6+P7, #793) | pending — expect: `~NN ms` age pill on the DC canvas paths (WINHOST-J ≈ 40–70 ms direct; CORPLAP-3-from-neo16 ≈ 120–180 ms relay; CORPLAP-1 pairs ≈ 200–300 ms — making FR-14's physics visible), drag cadence unchanged-or-better under heavy video |
| 0.4.167-0.4.172 (field read 2026-10-26, prod `agent_logs`, read-only via mars) | **AC6 partial** — at the named 2880x1800 Iris-Xe geometry (av1_qsv, 918 windows/8 sessions) delivered fps maxed 38.5, 0% of windows `>=45`, encode ~20 ms, pace engaged 100% @ 50 (`>=45` NOT reached: the pace keys on encode time, the 5.2 MPix pipeline delivers only ~38); at 1920x1200 Iris-Xe (hevc_qsv) motion-median 45 fps, encode ~10.5 ms (met at the lower geometry); WGC drop ratio median 2% on the one WGC host (was ~33%). **AC8 met (HUD half)** — end-to-end age a real value on 209709/259704 heartbeat windows, 294 sessions/10 hosts; direct median 15 ms, relay median 36 ms. **AC7** — the `setTimeout(8 ms)` + `pointerrawupdate` mechanism is confirmed in source; a foreground-browser field read is owed. Full write-up + anchors: `docs/remote-desktop-latency.md` §6 |
| 0.4.177-0.4.197 (field read 2026-11-02, prod `agent_logs`, read-only via mars) | **AC6:** every 2880x1800 heartbeat of the week, 2 831 windows, 17 sessions, one host (`av1_qsv`, direct). `input_path=cpu`, the default: 1 701 windows, motion median 37.1 fps, highest 42.8, none `>=45`, encode 17.7 ms. `input_path=gpu`, `zero_copy` on: 986 windows of one session (FR-118 §9.10's test pattern), median 60.0, 89 % `>=45`, encode 16.3 ms. WGC drop ratio median 2 %, p90 2, max 42, 2 571 samples, one NVENC host. **AC8:** age on 123 375 of 128 325 windows; direct 15 / 30 / 62 ms (med / p90 / p99, 411 sessions), relay 42 / 80 / 179 ms (150 sessions). **AC7:** still owed. **AC9:** the operator's. |
| 0.4.197 (viewer read 2026-11-02 22:19-22:22Z on the production viewer; `agent_logs` read 2026-11-03, read-only via mars) | **AC8: the pill and the wire agree** (held 2026-11-08, row below). Headed Chromium 151 under Xvfb with SwiftShader, page visible and focused, two sessions to a throwaway Linux device, VP9 4:4:4 software, `carrier=direct`. 164 pill samples at about 1 Hz, 36-2 865 ms, against 66 heartbeats carrying `viewer_age_ms`: 63 equal a pill value within 5 s (session 1: 50 of 52, session 2: 13 of 14), 28 of 29 at 300 ms or more. Chance control, the same test against the other session's pill values: 1 of 66. This shows the pill and the wire agree, and nothing about how low the number gets on real hardware. **AC7:** that run's driver limited it (`rawupdate_per_s` far under 125 in every arm, and the 16 ms control read 6.6 sends/s where it had to read 55-62), so it is void and AC7 stays owed. |
| 0.4.197 (viewer read 2026-11-03 00:59-01:11Z on the production viewer) | **AC7 no result.** A real X pointer through `xdotool`, 234 moves/s, reached the page at 153-187 raw events/s in the unloaded arms. **First try,** 1280×720 window, host load 4-20: A 78.8-96.8 sends/s at rAF 41-50, B 66.8-68.4 at rAF 17, B_heavy 61.3 at rAF 2.6, all gap p50 7.8-7.9 ms; D_busy40 14 sends/s, gap p50 86 ms; C_gap16 34.7 sends/s where it had to read 55-62, gap p50 16.0 ms, so that control failed and the try doesn't count. **Second try,** 1024×600 window, less motion, host load 2-10: A 93.0-99.2 sends/s at rAF 44-49, B 94.4-100.4 at rAF 30-32, B_heavy 90.1 at rAF 5.1. Leaving out gaps over 30 ms: 125.2-128.6 sends/s in all five arms. A host-pressure limit stopped it before D_busy40 and C_gap16 ran, so it doesn't count either. Stalls of 230-380 ms took about a quarter of every arm, loaded or not, cause unknown. 0 throttled CPU periods on the second try. |
| re-check 2026-11-08 (master `f777dd6e8`, live 0.4.206 and the 10:04Z promote; no new field run) | **AC8 back to partial, held for #3566.** That issue found the decode worker applying a clock offset the page measured on the page's own clock, so every age is off by the drift between the two since the page loaded: 7 ms after 162 s on one host, near 160 ms after an hour, low or high by host. Its fix, #3568, merged at 11:50Z and is not promoted yet. The 2026-11-02 read stands: it started each session right after loading its page, and the pill and `sendDecodeStat` still take one worker `stats` message on master. The fleet medians above carry the error, and a window whose ages all read negative sent none. AC8 ticks once #3568 is on production and its GREEN read is in. AC7 unchanged: no run since 2026-11-03. |
