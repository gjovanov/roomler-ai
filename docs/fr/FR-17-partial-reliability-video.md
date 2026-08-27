# FR-17: Video rides a reliable + ordered DataChannel

Status: **proposed** (2026-08-27). Tracking issue: `FR-17` (#799). Sibling of FR-16
(#798, the harness that must gate it); the architectural half of the FR-1 program.

## The measurement

Prod `agent_logs`, 2026-08-27, both hosts on 0.4.9, 1920×1200@30 over a DERP relay:

| host | encoder | `send_wait_max_ms` | `bytes_inflight` max | `avg_encode_ms` |
|---|---|---|---|---|
| CORPLAP-1 | hevc_qsv | **10 263** | 38 011 | 8–10 |
| CORPLAP-1 | vp9_qsv | **4 740** | 71 517 | 12–18 |
| CORPLAP-1 | h264_qsv | 1 870 | 3 800 | 9–12 |
| CORPLAP-3 | av1_qsv | 907 | 43 170 | 12 |

`send_wait` is the time a SINGLE frame spent inside the DataChannel send call — not
queued in our channel, not waiting on the encoder. Ten seconds. Encode was 8–12 ms and
our own queue held tens of KB throughout: **the agent is healthy and simply cannot hand
bytes to the wire.**

The contrast that sizes the prize: the same CORPLAP-1 on a **direct** carrier the same
afternoon ran 1920×1200@**60** at 12 Mbps with a **7 ms** end-to-end age.

## Root cause

`video-bytes` is created `{ ordered: true }` — fully reliable, fully ordered SCTP
(`ui/src/composables/useRemoteControl.ts`, `VP9_444_DC_OPTIONS`). The comment there
states the rationale: the channel is ordered because dropping a P-frame "would force
the worker to wait for the next IDR — far worse than a few ms of retransmit latency."

That reasoning is correct on a LAN and **falsified on a lossy 90–210 ms relay**, where
retransmit latency is not a few ms but seconds. Reliable + ordered means:

- nothing can be dropped in flight — every byte queues until delivered;
- one lost chunk **head-of-line blocks every frame behind it**;
- under sustained loss the backlog has no bound, which is why a same-desk pair reaches
  20 s of age.

It also explains the non-linear degradation with drag speed the operator reported
(60 ms slow / 300 ms fast): more motion ⇒ more bits ⇒ more chunks ⇒ more loss exposure
⇒ longer HOL stalls, on a transport that answers loss with waiting rather than
dropping. This is the structural difference from RustDesk, which owns its transport and
drops frames instead of queueing them.

## Design directions (staged, each behind its own kill switch)

**A. Explicit frame framing, still ordered — zero behaviour change.** Add sequence +
chunk index/count to the wire header so the receiver can detect a gap at all. Landing
this first lets the assembler's gap handling be validated while the channel is still
reliable, so stage B flips one property instead of debugging two.

**B. Unordered / bounded-retransmit on CONSTRAINED transports only.** The receiver
discards an incomplete frame and requests an IDR — the `rc:keyframe` resync path
already exists and is min-gap clamped (500 ms). Direct carriers keep today's posture
until the harness says otherwise.

**C. Measure the retransmit budget rather than assume it.** `maxRetransmits: 0` versus
1–2 is an empirical question on these paths; FR-16 answers it per pair.

⚠️ **The risk to design against is an IDR storm.** On a lossy path, "drop the frame and
ask for a keyframe" can cost more than the stall it replaces — each IDR is the largest
frame on the thinnest pipe (FR-10 measured ~300 KB ≈ 1.2–1.5 s at 2 Mbps). The recovery
rate must be bounded, and this is the point where **intra-refresh (spread-I)** stops
being an optional nicety: it converts recovery from one huge frame into a spread cost.

⚠️ The 16 KiB chunking currently relies on ordered delivery for reassembly. Stage A is
what makes the reassembler independent of that assumption.

## Acceptance criteria

- [ ] Stage A ships with the assembler proving gap detection on a synthetic gap, and
      byte-identical behaviour on the wire otherwise.
- [ ] On the CORPLAP-1 ↔ CORPLAP-3 relay pair under FR-16's deterministic fast-motion profile:
      `send_wait` p99 drops below 250 ms (was: 10 263 ms max), and age p99 drops below
      300 ms.
- [ ] No IDR-storm regression: keyframe rate under sustained loss stays bounded, and
      delivered fps does not fall below the pre-change baseline.
- [ ] Direct transports are unchanged (verified by an FR-16 direct cell, not by
      inspection).
- [ ] A field A/B against RustDesk on the same pair, since that is the standard the
      program set itself.

## Out of scope

- Replacing SCTP/WebRTC wholesale (MoQ, a custom UDP protocol). This changes ONE
  property of the existing channel; the transport stays.
- The base path latency itself — two machines on one desk relaying at 90–210 ms is a
  carrier problem (FR-9), not a transport-reliability problem. Both need fixing and
  they are independent.

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.9 | Baseline measurements above; FR filed. |
