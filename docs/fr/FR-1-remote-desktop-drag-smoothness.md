# FR-1: RustDesk-parity remote-desktop drag smoothness

**Status:** phases P1–P4 shipped + field-verified; P2/P3 shipped, relay-gated after field
read; P5 shipped in 0.4.4 (field gate pending); P6 + P7-HUD remaining. Tracking issue: `FR-1` in gjovanov/roomler-ai/issues.

## Goal

Dragging a window on a remote screen through the browser viewer must feel as fast and
smooth as RustDesk on the same pair — and better where our hardware allows. Reference
field test (2026-08-26, neo16 → Rozalina, same LAN, direct): RustDesk H265 @ ~7 Mbps /
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
| P5 | Encode cost & cadence: parallel `convert_bgra` (row bands); `block_in_place` the encode; encode-pressure sheds **fps first** on HW (paced, even cadence) instead of the bitrate factor | `par_convert`, `fps_pace` | ✅ 0.4.4 (#781) — field gate pending |
| P6 | Pointer decoupling: `pointerrawupdate` / timer-coalesced `mouse_move` instead of rAF | web deploy | ⏳ |
| P7 | Latency telemetry: agent `send_wait_avg/max_ms` (✅ rc.483) + HUD end-to-end age (frame-header capture→send µs + control-DC RTT probe) | — | agent half ✅ · HUD half ⏳ |

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
      verified from neo16, pc50045 AND the MacBook viewing Rozalina)
- [x] Relay unaffected vs rc.483 (0.4.3 gating; regression caught same-day on CLK +
      PC55331 and reverted)
- [ ] P5: ≥45 fps at 2880×1800 on Iris-Xe-class with even cadence (WGC drop ratio no
      longer ~33 % random; `avg_encode_ms` ≤ ~17 at native)
- [ ] P6: pointer send rate independent of viewer rAF load
- [ ] P7: HUD shows end-to-end age; drag test becomes a number comparable to RustDesk's
      "Delay"
- [ ] Final A/B on the reference pair: subjective parity-or-better vs RustDesk on BOTH
      transports

## Open decisions

1. Relay-robust measured ceiling — estimator design (longer aggregation / stall-aware /
   median-of-windows) once enough relay heartbeat data accumulates.
2. The ~10 Mbps drain plateau: Wi-Fi leg vs webrtc-rs SCTP ceiling (`iperf3`
   neo16↔Rozalina decides; only matters for >10 Mbps ambitions).
3. Background dims/tier rebuilds (still synchronous; rare after P2, but each tier flip is
   a stall — P3's swap machinery could cover them).

## Out of scope

- macOS capture-side scaling (CG-side downscale) — separate arc.
- MoQ / transport replacement — SCTP stays; we just stopped overdriving it.

## Field-verification log

| Release | Result |
|---|---|
| rc.483 (P1+P4) | "definitively feels faster movement, but still bulky in several ms chunks/steps" — the byte gate converting queue-lag into skips against the still-unmeasured overrun |
| rc.484 (P2+P3) | ROZALINA direct = better; CLK + PC55331 over corp relay = WORSE than rc.483 (both mechanisms relay-hostile — see relay note) |
| 0.4.3 (relay gating) | Rozalina "much better now", verified from neo16, pc50045 and MacBook; relay reverted to rc.483 posture (re-test pending) |
| 0.4.4 (P5) | parallel convert + block_in_place + fps-first cadence pacing — field gate pending (`paced_fps` engaging in heartbeats, `avg_encode_ms` ≤ ~17 native, even cadence, WGC drops no longer ~33 % random) |
