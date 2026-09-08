# FR-79: One validity gate — a window is evidence about the pipe, or it is not

**Issue:** [#1524](https://github.com/gjovanov/roomler-ai/issues/1524) · **Status:** proposed 2026-09-08 ·
**Parent:** the consolidation FR-70's AC4 asks for (8 estimators → 1) and the rule
FR-71's three field events wrote by induction. Deletes rather than adds:
`transit_hold`, `transit_classify`, T2's quarantine and T2b's shadow all go.

## Goal

**One rule, consumed by every estimator, at every site that reads a window.** A
window is evidence about the pipe only if

1. the **agent was free** — no pump pass overran its budget inside it,
2. the **transport was not stalled**, and the window before it was not either,
3. the **carrier it will be attributed to is the carrier that produced it**.

Anything else is not a measurement, and no consumer may act on it: not the
goodput fold, not FR-59 P6's contradiction, not P1's floor relief, not FR-35's
hard halving, not P3's arrival-rate clamp, not FR-15's age loop, not FR-63's
ramp verdict, and not the pair memory's write-back.

## Why this FR exists — the rule of three

One day, three field events on the same host, three different inputs, one shape.
Each was answered with its own rule; the third made the pattern undeniable.

| when | the input that lied | what it cost | the rule it got |
|---|---|---|---|
| 2026-09-08 12:15 | a **blocked send** inside a pump pass that spent 2.9 s and 7.4 s in `other` | 6.60 → 0.68 M in 36 s, ~3 min back | FR-71 T2: quarantine the samples, defer the ×0.5 |
| 2026-09-08 14:52 | the **viewer's arrival-rate report** for the stall, landing one window later | 7.45 M → 834 k, the learned ceiling lost | FR-71 T2b: the stall's shadow gates the clamp |
| 2026-09-08 16:33–16:42 | the **opening burst**, read as capacity while it queued; and a stall-contaminated measurement written back as the pair's rate | the operator's own report: "starts very blurred, 5+ s to crystallize", openers swinging 1.26 ↔ 6.8 M | — (this FR) |

The third one is measured in §The opener, below. Writing a fourth per-input rule
is the wrong move: the inputs differ, the defect does not.

## Key design

### The gate (`agents/roomlerd/src/encode/evidence.rs`, new, pure)

```rust
pub enum Reason { AgentStalled, TransitStalled, StallShadow, CarrierChanged }

pub fn rejected(f: WindowFacts) -> Option<Reason>
```

`WindowFacts` is what the governor already holds at the fold: this window's
classifier verdict, the previous window's, whether a pump pass stalled inside
it, and whether the carrier moved. No new sensor, no threshold, no switch. The
verdict is taken **once per window, before any loop acts**, and every consumer
reads the same one.

⚠️ The classifier stops being a shadow: `transit_classify` is deleted and it
always runs, because the gate is built on its verdict. It has run fleet-wide
since 0.4.67 with the counters as its only effect.

⚠️ `agent_stalled` comes from the FFmpeg pump's own stall watch
(`encode::stall::PhaseAccum::is_stall`, already computed per pass). The VP9-444
pump keeps no send-side figures, so there the input reads false and the gate
falls back to conditions 2 and 3 — the same degradation `WindowSenderStats:
None` already documents.

### What each consumer does with it

| consumer | before | after |
|---|---|---|
| the goodput fold (`GoodputEstimator::observe_window`) | folds every window; T2 quarantined some | folds only a valid window; an invalid one's samples are **dropped**, not held |
| FR-35's hard halving (a send blocked ≥ 1 s) | applied inside `note_send_stall`; T2 deferred it | recorded for the window, applied at the fold **iff** the window is valid |
| FR-59 P6 contradiction, P1 floor relief | read the goodput estimate | unchanged — they see the last **valid** estimate, so no false evidence reaches them |
| FR-59 P3 arrival-rate clamp | `!hold`, then T2b's `!stall_shadow` | `valid` |
| FR-15 age loop | `!hold` (fire), always learns its floor | `valid` (fire), still always learns — a stall cannot lower a minimum |
| FR-63 ramp verdict | `!hold` | `valid` |
| FR-35 pair-memory write-back | the opener burst target and the session's stable rate | V2: the same gate, and the burst measured the way the goodput estimator measures |

### The opener (V2), measured 2026-09-08 16:33–16:42 on CORPLAP-1

`rate_memory::opener_growth_target_bps` divides the whole burst by the **longest
single frame's wait**:

```
bytes * 8 / wait_us * 75 %
```

That is only a rate if the last frame waited for the entire burst. In the
operator's six trials it was not:

| session | burst | longest wait | recorded target | the pipe, measured seconds later |
|---|---|---|---|---|
| 16:33:57 | 951 KB | 107 ms | 8.0 M (capped) | ~2.4 M after 438 gate skips |
| 16:38:52 | 1.19 MB | 440 ms | 8.0 M (capped) | **1.79 M** |
| 16:41:51 | 829 KB | 829 ms | 6.0 M | **3.41 M** |

`record_session` then keeps `max(stable, old, growth_target)`, so the memory
ratchets to the 8 M cap and stays there; the only thing that ever lowers it is a
P6 abandonment writing back the **stall-contaminated** number — which is where
the 16:39:35 session's seed of **1,255,327** came from, and FR-59 P8 opens the
encoder *at* the remembered rate: 1.26 Mbps for 1920×1200 is 0.018 bits per
pixel. That is the blur the operator sees, and the 6.8 M openers are the same
memory at the other extreme, over-driving the relay by 2–4× until P6 collapses
it about a second in.

Third element: the memory key is the nominated pair's remote address — the
**overlay** address — while the carrier under it flips between direct, relay and
DERP (six demote-follows onto DERP on 2026-09-08 alone). P6's own comment says
it: *"relay-keyed rate memory can carry a fast day onto a slow one"*. One key,
carriers with 4× different capacity.

So V2 is condition 3 plus the honest denominator:

- the opener's contribution comes from the **same estimator the session uses**
  (bytes over blocked time, byte-weighted), not from `bytes / max_single_wait`;
- the write-back runs only on evidence the gate accepted;
- the memory is keyed (or the seed clamped) by the **carrier in force**, which
  the pump already knows at session start and logs every 5 s.

## Phases

| phase | scope | kill switch | status |
|---|---|---|---|
| **V1** | the gate + every in-session consumer; **delete** `transit_hold`, `transit_classify`, T2's quarantine, T2b's shadow, and their four counters (one `evidence_rejected` triple replaces them) | **none — and none is the point**; the rule it replaces had three | proposed |
| **V2** | the write-back and the seed: the opener measured like every other window, the gate at `record_session`, the memory keyed/clamped by carrier (condition 3) | — | proposed |
| **V3** | one pipe estimator: goodput, the viewer's arrival rate and the prior behind ONE type with one accessor; delete the rest (FR-70 AC4's 8 → 1) | — | proposed |

## Acceptance criteria

- [ ] **AC1 — the deletion is the deliverable.** After V1 the tree contains no
      `transit_hold`, no `transit_classify`, no quarantine buffer, no
      `hard_stalls_paused` / `hard_stalls_confirmed` / `stall_shadowed` /
      `transit_holds`; `cargo clippy` and the suite are green, and the PR's diff
      is **net-negative** in `agents/roomlerd/src/encode/`.
- [ ] **AC2 — the three field events replay.** The 12:15, 14:52 and (V2) opener
      cells each fail on the pre-FR-79 laws and pass on the gate, as unit cells
      built from the logged numbers.
- [ ] **AC3 — one verdict, one counter.** The heartbeat reports
      `evidence_rejected=[agent, stalled, shadow]` and nothing else about
      stalls; a reader can attribute every rejected window to a reason.
- [ ] **AC4 (V2)** — on a pair remembered from one carrier, a session opened on
      a slower carrier does not open above what that carrier has measured, and
      an opening burst that queued does not raise the memory.
- [ ] **AC5 (V2)** — field: the operator's own repeat on CORPLAP-1 opens within
      a factor of the carrier's measured rate on five consecutive sessions, with
      no 1.26 M opener and no 6.8 M opener.
- [ ] **AC6 (V3)** — one estimator type; the count of distinct "how fast is the
      pipe" accessors in `encode/` is 1.
- [ ] **AC7** — docs updated with diagrams and a `docs/README.md` row
      (`docs/rate-control.md` gains the gate as its first section).

## Open decisions

- Whether a `ViewerLate` window is evidence about the pipe. Today it is (only
  `TransitStalled` rejects); the viewer being the limiter says nothing about the
  path's capacity, but the goodput estimator only counts blocked sends anyway.
- Whether the carrier should key the memory (a map per carrier) or clamp the
  seed (one entry, bounded by the live carrier's measurement). Keying remembers
  more; clamping cannot go stale.
- Whether `stall_seen` (FR-70 P1's prior verdict) belongs behind the gate too.

## Out of scope

- The plan handoff (FR-70 M3) and the single controller (M4). This FR makes
  those safe by removing the levers they would otherwise have to preserve.
- The viewer's own stall report (FR-71's first open decision).

## Field-verification log

| when | build | cell | result |
|---|---|---|---|
| 2026-09-08 12:15 / 14:52 / 16:33–16:42 | 0.4.87 / 0.4.90 | CORPLAP-1 on the Check Point VPN, relay | the three events this FR generalises; see FR-71's log for the first two and §The opener for the third |
