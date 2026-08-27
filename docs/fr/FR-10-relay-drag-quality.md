# FR-10: Relay drag quality — IDR thrift on constrained transports

**Status:** shipped in `agent-v0.4.5` (#785); field gate pending.
Tracking issue: `FR-10` in gjovanov/roomler-ai/issues. Child of FR-1 (#767), which
gated the direct-path rate machinery off relay and left goodput observe-only there.

## Goal

Window-drag over a RELAY session (corp-VPN hosts: CORPLAP-3, CORPLAP-2) should feel as smooth
as the pipe's physics allow — no multi-second "lumps" — while keeping the crisp-text
properties of the direct path. Field read (2026-08-27, neo16 → CORPLAP-3 on 0.4.x): direct
paths are "nice"; CORPLAP-3 via relay is "slightly bulky".

## Root cause (from CORPLAP-3's own heartbeats, session `6a9021e6…`, 11:39–11:40Z)

av1_qsv 1920×1200@30, relay RTT 88–105 ms, target ≈ pipe (~1.5–2 Mbps delivered):

- **Steady-state flow is HEALTHY**: `send_wait_avg_ms` 0.05–0.27, skips ~0.2/s,
  `frames_dropped_backpressure` 0. The relay carries the delta stream fine.
- **The "bulky" is single-frame IDR LUMPS**: `bytes_inflight` spikes 82→262→334 KB
  exactly at each `idle-settle keyframe` (one per drag-pause, ~every 6 s) and each
  `deferred bitrate applied at quiet` re-open (3 in 30 s, each a fresh IDR). One
  ~300 KB frame ≈ **1.2–1.5 s of pipe time**; everything behind it queues in SCTP.
  The constrained byte gate (450 ms budget = ~168 KB) cannot help: the lump is ONE
  frame, already encoded, larger than the whole budget.
- av1's HRD floor makes the lumps legal: `av1_*` is pinned at bufsize = 2×maxrate
  (rc.443 — Intel AV1 VDENC ERRORS on an over-reservoir IDR rather than clamping),
  so shrinking the reservoir is NOT an available lever for AV1.
- **Correction to FR-1's relay note**: the goodput v2 estimator doesn't "crash" on
  this relay — it **never samples** (`goodput_samples: "(0, 1)"`): SCTP's own buffer
  absorbs bursts instantly, so per-frame serialisation is sub-ms and no ≥10 ms
  blocked-send ever occurs. Measuring relay drainage would need SCTP-level feedback;
  out of scope here (and the ceiling was not the problem in this trace).
- Base RTT 88–105 ms is physics (DERP/TURN path) — not addressable by the encoder.

## Key design — `relay_idr_thrift` (default ON, one knob)

On a **reliable, ordered** DataChannel nothing is ever lost, so the idle-settle IDR
is a QUALITY refresh, not a correctness need — the correctness path is the
request-driven resync (browser backlog-shed → `rc:keyframe` → forced IDR), which
stays untouched. On a thin relay a full-frame refresh costs more in lag than it buys
in crispness; CQ deltas re-crystallise the static image progressively anyway (the
rc.234 argument).

Under `relay_idr_thrift` (env `ROOMLER_NODE_RELAY_IDR_THRIFT`, `0` restores):

1. **Settle-IDR suppression on constrained transports** — both DC pumps skip the
   `idle-settle keyframe` when the session is constrained; a heartbeat counter
   (`settle_kf_suppressed`) makes the suppression visible in the field.
2. **Deferred-apply spacing on constrained** — the quiet-flush of a deferred AIMD
   target (each flush = QSV/AMF re-open = another IDR) applies at most once per
   15 s unless the move is large (≥40 % relative), so rung-hopping (3 re-opens in
   the 30 s trace) collapses to ~1 while a genuine collapse still lands promptly.

Untouched: request-driven resyncs, lock-transition IDRs, dims-change rebuilds, the
direct path entirely, and every FR-1 mechanism.

## Acceptance criteria

- [ ] Relay drag session heartbeats: `bytes_inflight` spikes >150 KB only for
      requested resyncs; zero `idle-settle keyframe` lines while constrained
      (counter climbing instead); ≤1 `deferred bitrate applied` per 15 s
- [ ] Field: CORPLAP-3-from-neo16 drag no longer "bulky" (or clearly reduced); post-motion
      text still crystallises within ~1–2 s via deltas
- [ ] Direct sessions byte-identical in behaviour (settle IDRs still fire there)
- [ ] `relay_idr_thrift=0` restores the previous relay behaviour

## Open decisions

1. Relay-measured ceiling still needs an SCTP-level drainage signal
   (`buffered_amount` polling or DERP-side feedback) — separate follow-up if the
   post-thrift feel still lags the pipe's physics.
2. Whether the browser's adaptive-resolution ladder should weigh relay RTT.

## Out of scope

- The 88–105 ms base RTT (DERP/TURN path selection is the overlay's domain).
- AV1 HRD shrinking (blocked by the rc.443 VDENC error class).

## Field-verification log

| Release | Result |
|---|---|
| 0.4.5 (relay_idr_thrift) | shipped — field gate pending: CORPLAP-3 drag from neo16 (settle_kf_suppressed climbing, bytes_inflight lumps gone, bulky feel reduced) |
