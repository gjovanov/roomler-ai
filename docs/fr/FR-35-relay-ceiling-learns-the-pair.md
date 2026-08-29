# FR-35: The constrained ceiling learns the pair — grow the relay cap on delivery evidence, remember it per peer

Status: **P0 measured; P1 + P2 implemented, not yet field-verified** (2026-08-29). Tracking issue: `FR-35` (#922).
Child of the RC-quality program. Follows FR-31 (every opening and repair number on an NVENC relay
session is proportional to `maxrate`, and nothing ffmpeg exposes changes that) and the parked
measured-rate line (#678: a sender cannot see capacity it is not using). Operator's directive
2026-08-29: the relay ceiling first; a vendored-ffmpeg patch only if this fails.

## What is in force today

- `relay_max_bps()` = **3 Mbps** (`agents/roomlerd/src/encode/mod.rs:454`, env `RELAY_MAX_KBPS`),
  applied as the encoder's `maxrate` cap on every constrained session
  (`encoder.rs` ↔ `rate_profile::ffmpeg_maxrate_bps_scaled`, `peer.rs:3029`), opened at 85 % =
  **2.55 Mbps**, and as the AIMD's ceiling (`governor.rs:254-273`); the send-queue budget derives
  from the same constant (`peer.rs:4518`).
- The AIMD (`encode/aimd.rs`) climbs additively (+ceiling/16 ≥ 150 kbps per 5 s of quiet) but
  **never above that ceiling**; it decreases ×0.85 on a full send channel, on a send-wait stall
  (#818) and on viewer-age excess (FR-15 P2). The governor's measured-ceiling clamp (stage 1) is
  **direct-only** and only ever lowers.

## The measurement (P0, 2026-08-29, the REAL pair `neo16 → PC55331`, DERP relay)

`ROOMLERD_FFMPEG_MAXRATE_KBPS=25500` on PC55331 (machine env + scheduled restart), viewer on neo16
with the FR-31 harness, one 20 s synthetic drag; PC55331's own heartbeats read afterwards.

| what | today (cap 2.55 Mbps) | cap 25.5 Mbps |
|---|---|---|
| opening | 5.6 KB max-QP keyframe, desktop repaired by inter frames over ~1.2 s | 83 B keyframe (black first capture) + **the whole desktop as one 680 KB frame at +0.3 s**, 1.06 MB in the first three frames |
| delivered rate during the drag | ≤ 2.55 Mbps | **3.3–10.6 Mbps per second**, no inter-message gap > 136 ms |
| agent `goodput` (stage-0 estimator, which never fired at 2.55 Mbps) | `None` | **3.6 → 5.8 → 7.5 → 10.9 Mbps** as the AIMD climbed 9.6 → 11.2 → 12.8 Mbps |
| `send_wait` | ~0.05 ms | ~0.05 ms up to ~11 Mbps; **one 7.9 s stall at 12.8 Mbps** (viewer age 6 986 ms, +556 backpressure skips) → AIMD cut to 4.1 Mbps; a second session opened at 18.8 Mbps and stalled 310 ms at once |

Reading: this path **sustains ~6–9 Mbps**, absorbs short bursts far above that, and chokes above
~12 Mbps. The nominal 3 Mbps is 2–3× too low for it; the 10× value that makes the opening keyframe
crisp is not sustainable. Other corp relays in the same fleet measured ~2 Mbps (pc50045, CLK,
2026-08). **A constant cannot be right for both — the cap must be learned per pair, and it may only
ever climb during a session** (FR-31: any NVENC rate change *down* forces a starved keyframe that
replaces the picture).

## Design

**P1 — growth above the nominal, gated by the signals that predicted the stall.** The constrained
AIMD ceiling becomes `clamp(nominal, hi)` with `hi = relay_max_hi_kbps` (default **8 000**;
`0` = nominal only = today). Above the nominal the additive increase additionally requires, over
the trailing 10 s: no send-wait stall (`send_stalls` unchanged), `send_wait_max` < 20 ms, and —
when the viewer reports age — age ≤ 1.5 × the learned floor. Steps stay +ceiling/16 (≥ 150 kbps)
per 5 s settle. Decreases are unchanged, plus one new rule from the measurement: a send wait
> 1 s is a **hard stall** and cuts to ×0.5 rather than ×0.85 (the 7.9 s stall was answered by
three ×0.85 steps over 4 s). All of it lives in the pure controller/governor (`aimd.rs`,
`governor.rs`, `rate_profile.rs`) — unit-testable on this box; no wire change.

**P2 — rate memory per peer.** The highest target that held ≥ 10 s with no decrease is the
session's *stable rate*. On session end the agent persists `{peer overlay ip → stable_bps, at}`
(`rate_memory.toml` in the data dir; entries expire after 7 days), and the next session on that
pair **opens at 0.85 × stable, clamped `[nominal, hi]`** — so the opening keyframe and the repair
speed scale with what the pair proved, not with a fleet constant. A pair never seen opens at the
nominal (today). Nothing here ever lowers the nominal.

Kill switches: `relay_ceiling_learn` (tribool, default on; off = today's fixed ceiling and no
memory) and `relay_max_hi_kbps` (config-surface + env). Direct sessions are untouched.

**Not done, deliberately:** raising the nominal 3 Mbps (the ~2 Mbps relays are real); an active
startup bandwidth probe (measures burst capacity, which this measurement shows is *not* the
sustainable rate — 18 Mbps instantaneous vs ~8 sustained — and costs bytes on exactly the thin
links); the goodput estimator as the learning input (its down-fast EWMA crashed on relay
lumpiness, governor.rs:246-252 — the AIMD's own stable target is the robust signal).

## Acceptance criteria

Measured with the FR-31 harness on `neo16 → PC55331`, N ≥ 5 sessions after ≥ 1 learning session,
shipped defaults:

- [ ] opening keyframe ≥ 2.5× today's bytes (≥ 14 KB at 1920×1200, today 5.6 KB) and first-light
      sharpness ≥ 70 % of the 8 s steady state (today 47 %)
- [ ] time-to-crisp ≤ 0.6 s (today 1.2 s)
- [ ] a 60 s drag: no send wait > 1 s, viewer-age p99 not worse than the FR-18 numbers, and the
      ceiling never above `hi`
- [ ] a ~2 Mbps relay (CLK, `av1_qsv`): `target_bps` never exceeds the nominal unless every growth
      gate passed — read from its heartbeats; the session must not get worse than today
- [ ] the learned rate survives a daemon restart and expires after 7 days (unit-tested)
- [ ] `relay_ceiling_learn=false` restores today's numbers byte-for-byte

## Open decisions

- `hi` default: 8 000 kbps from this one pair; the second corp pair (CLK) will say whether the
  gates keep it at the nominal there. Revisit with two pairs of data.
- Whether the learned rate should also seed the queue budget (`constrained_queue_budget_bytes`
  is derived from the nominal today). Probably yes, in P2.

## Out of scope

The carrier (FR-33 surfaces the capture); the ffmpeg-side keyframe budget (FR-31, refuted
levers); the direct path's measured clamp.

## Field log

| date | build | note |
|---|---|---|
| 2026-08-29 | agent 0.4.18 (PC55331), env `FFMPEG_MAXRATE_KBPS=25500`, web `v20260829-40e8fc071129` | P0: the table above. Two daemon restarts on PC55331 (apply, restore) via a scheduled task; env cleared afterwards. |
