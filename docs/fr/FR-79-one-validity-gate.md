# FR-79: One validity gate — a window is evidence about the pipe, or it is not

**Issue:** [#1524](https://github.com/gjovanov/roomler-ai/issues/1524) · **Status:** proposed 2026-09-08; V1–V5a shipped (`agent-v0.4.93` → `0.4.97`), V3b in SHADOW (`0.4.98` → `0.4.100`), docs 2026-09-25; AC8 half-met, AC9 partly met 2026-10-31; **V5b proposed 2026-10-30** for the V5 loop FR-71's 2026-10-29 fleet read found, waiting on the operator ·
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

### V4 — the memory keeps what was measured, not the maximum (2026-09-08)

V2 made the opener's contribution a measurement. The rule that consumes it is
still `record_session`'s **maximum**:

```rust
let value = if had_decrease && stable_bps > 0 { stable_bps }
            else { stable_bps.max(old).max(growth_target_bps) };
```

Two measurements say that rule has outlived its reason.

**1. A carrier's capacity is not a constant, and max keeps the best minute of
it.** On `100.65.4.2|relay:derp/tcp`, five sessions inside six minutes measured
**1.44, 1.09, (unqueued), 3.32, 6.13 Mbps** (2026-09-08 19:12–19:18). The
maximum keeps 6.13, so the next session opens at 85 % of it — 5.2 M — into a
path that measured 1.09 M five minutes earlier. That is the over-drive this FR
exists to stop, rebuilt out of honest inputs.

**2. The unqueued branch still grows the memory on no evidence at all.** A burst
that never queued gets `maxrate × 150 %`, and with the maximum that is a
one-way ratchet to the `hi` cap. Measured over 2026-09-08: **every** opener on
CORPLAP-2 had `opener_wait_max_ms=0` (its SCTP buffer absorbs the burst), and its
memory sat at the 8 M cap all day; CORPLAP-1 recorded the cap in nine of
nineteen openers before V2. The branch's own comment says what it proves —
*"only 'not slower than this'"* — and the ratchet turns that into "this fast".

**The rule the gate makes possible.** The whole FR is one sentence: only
evidence moves a belief. The memory is a belief about a carrier, so:

> A session writes the memory **only if it measured the carrier**, and a
> measurement is allowed to move the number **in either direction**, damped —
> fast down, slow up, the asymmetry the goodput estimator already uses inside a
> session (`ALPHA_DOWN` 0.50, `ALPHA_UP` 0.10). A session that measured nothing
> leaves the entry exactly as it was.

What that deletes:

- the **maximum** composition, and with it `had_decrease` — the flag exists only
  to stop a non-measurement (an idle session's seed echoed back) from lowering
  the memory, and a session that measures nothing now writes nothing at all;
- the **unqueued growth step** and `OPENER_UNQUEUED_STEP_PCT`: a burst the
  socket absorbed is not a measurement. Convergence for a fast pair is already
  the learner's job — it steps the ceiling up when the pair *carries* it and its
  stable rate is written back, which is evidence rather than a guess;
- `record_session`'s three value parameters collapse to one: the session's
  evidence, or `None`.

**Why damped rather than last-wins.** One unlucky window would otherwise pin a
pair for a week (the TTL), and one lucky burst would re-create the over-drive.
Down-fast/up-slow biases the opener toward the safe side, because the two errors
do not cost the same: opening under costs an AIMD climb the learner shortens,
while opening over costs dropped frames, an abandoned ceiling and a visible
stall — which is what the operator reported and what tonight's five sessions
stopped doing.

### Extended by FR-119 P2b-8 (2026-10-23): every window, the opening, whole windows

[FR-119](FR-119-high-refresh-streaming.md) §4.10 found three gaps in what this gate
saw, when a freshly granted 120 fps was taken back one second into two of four
direct sessions on the MacBook:

- **The gate ran on constrained windows only.** A direct window was evidence by
  definition, and FR-119's grant, which only runs on a direct path, read capacity
  claims nothing had gated. It now runs on every window; on a direct one the
  classifier does not run, so only the agent's own stall and the opening apply.
- **A fifth reason, `Opening`.** A window any pass of which fell inside FR-35's
  opener window (now kept for every session) is not evidence: the first keyframe
  meeting a ramping transport blocked sends for 43–82 ms and folded as 14–22 Mbps
  of capacity against a 104 Mbps ceiling. Not congestion either. Its counter is
  appended to `evidence_rejected`, so the first four keep their positions.
- **The window was one pass long.** The pump hands its sender facts over on every
  pass and each call overwrote the last, so `AgentStalled` only ever saw a stall in
  a window's final pass. The governor now sums the passes
  (`WindowSenderStats::absorb`).

## Phases

| phase | scope | kill switch | status |
|---|---|---|---|
| **V1** | the gate + every in-session consumer; **delete** `transit_hold`, `transit_classify`, T2's quarantine, T2b's shadow, and their four counters (one `evidence_rejected` replaces them) | **none — and none is the point**; what it replaces had two | **built 2026-09-08**, shipped `agent-v0.4.93` (#1525); AC1–AC3 met |
| **V2** | the write-back and the seed: the opener measured like every other window (the estimator, not `bytes / max_single_wait`), and the memory keyed by CARRIER — `100.65.0.5|relay:derp/tcp` is not `100.65.0.5|direct` | — | **built 2026-09-08**, shipped `agent-v0.4.93` (#1529); AC4 met, AC5 field-verified |
| **V3a** | ONE belief (`pipe_bps`) composed in one place + one named source (`blocked_send_bps`); delete `remembered_candidate_bps`, the inlined third copy, and the `rate_prior_decay` switch | — | **built 2026-09-08**, shipped `agent-v0.4.95` (#1533); AC6 met |
| **V3b** | the three sources behind one `Pipe` type (`encode::pipe`): a demonstrated FLOOR (the viewer's arrival rate, every window on every carrier) and a pushed-back CAPACITY (an accepted goodput fold, or the arrival rate while the viewer's queue grows); `capacity_bps` answers "am I over?", `ceiling_anchor_bps` answers "what may the ceiling be?" | — | **built 2026-09-10, in SHADOW since `agent-v0.4.98`** (#1575): heartbeat `pipe_belief` / `pipe_belief_n` only, read by NO consumer. The shadow corrected the design three times in five days — V3b-1 (#1578, `0.4.99`): no single `believed_bps()`, the two questions are not interchangeable; V3b-2 (#1580, `0.4.99`): the floor is retired by evidence, never by time; V3b-3 (#1582, `0.4.100`): a capacity sample is also a delivery. "No behaviour change" still holds — nothing acts on it. Its authorising counter is `goodput_samples` (accepted) reading > 1 on a constrained session; the 2026-09-25 sweep found a three-hour relay session with `pipe_belief_n=(9912, 1)` |
| **V4** | the memory keeps what was MEASURED, damped down-fast/up-slow, and writes nothing when a session measured nothing; deletes the `max` rule, `had_decrease`, the opener arithmetic and its three constants | — | **built 2026-09-08** (net −159 lines), shipped `agent-v0.4.95` (#1542); AC8 half-met |
| **V5** | the gate answers *is this a MEASUREMENT*, which V1 let every consumer read as *does this window say anything at all*. A transit stall above the measured pipe still reaches the rate-limited MD, the ramp verdict, the age streak and the prior's push-back; everything that sets a NUMBER still reads `valid` | — | **built 2026-09-09**, shipped `agent-v0.4.96` (#1552); AC9 open |
| **V5a** | `stall_is_ours` compares the target against the composed `pipe_bps` (goodput → the viewer's arrival rate → the prior), not `blocked_send_bps`: the agent's sends block only when the queue is LOCAL, and on a relay it is downstream | — | **built 2026-09-09**, shipped `agent-v0.4.97` (#1566) — `0.4.96` was inert on CORPLAP-3 (`goodput_samples=(0, 5)`) |
| **V5b** | `stall_is_ours` compares only against a MEASURED source: the blocked-send goodput, or the viewer's arrival rate inside its 15 s TTL. Never against the prior's stand-in, and a stall that carries no number stops walking the prior. Test-first on the 2026-10-20 14:16 arc (§Open decisions, *The V5 loop*) | — | **proposed 2026-10-30**, waiting on the operator; agent code, so it rides the release after `0.4.191` |
| **Docs** | `docs/rate-control.md` opens with the gate: the verdict, every consumer, V5's exception, the write-back and the seed, and the V3b belief labelled as the shadow it is; `docs/README.md` row updated | — | **2026-09-25** (AC7), in the PR that carries these corrections |

## Acceptance criteria

- [x] **AC1 — the deletion is the deliverable.** V1 removes, with nothing put
      in their place: **2 kill switches** (`transit_hold`, `transit_classify`)
      with their config keys, env flags, `config_surface` entries, accessors and
      the simulator's copy; **4 heartbeat counters → 1**; **7 governor state
      fields → 2** (the hold's counter, T2's quarantine buffer, its pending flag
      and two counters, T2b's counter); **3 per-consumer rules → 1** shared
      predicate. Measured 2026-09-08: the edited files are **net −83 lines**
      (`governor.rs` −42, `encode/mod.rs` −16, the config surface −26,
      `peer.rs` +3); the new gate is 255 lines of which 118 are its own tests
      and 97 are comment, i.e. **~40 lines of logic** for what it replaced.
      `cargo test -p roomlerd --lib` 995 pass, `roomler-node-core` 175 pass,
      clippy clean. ⚠️ The overall line count is not negative and is not claimed
      to be: the inventory above is the claim.
- [x] **AC2 — the field events replay.** The 12:15 cell (a hard blocked send
      inside a window whose pump pass overran its budget) cuts nothing and is
      attributed to `agent-stalled`; its control (the same blocked send on a
      free loop) still halves at its own window, so FR-35's reaction is intact;
      the 14:52 cell's report in the stall's shadow does not arm the clamp; the
      B0 `finding_4` cell keeps its rate with the gate on and still cuts with
      the gate off (the control arm, which is no longer a daemon
      configuration). The opener cell is V2's.
- [x] **AC3 — one verdict, one counter.** The heartbeat reports
      `evidence_rejected=[agent-stalled, transit-stalled, stall-shadow,
      carrier-changed]` and nothing else about stalls; every rejected window is
      attributable to a reason.
- [x] **AC4 (V2)** — on a pair remembered from one carrier, a session opened on
      a slower carrier does not open above what that carrier has measured, and
      an opening burst that queued does not raise the memory.
      *Built 2026-09-08: `carrier_tag` puts the carrier in the key (`direct` /
      `tunnel` / `relay:<kind>/<transport>`, from the LocalAPI peer record's
      `relay_kind` and `relay_transport`, so DERP and a UDP relay are different
      memories), with `None` for a mid-churn carrier leaving the pre-FR-79 bare
      key; `opener_growth_target_bps` takes the goodput estimator's measurement
      for a burst that queued, and the bounded step is unchanged for one that
      did not. Cells: the 16:41:51 burst records 3.41 M (the measurement) where
      it recorded 6.0 M before, records nothing when the estimator has no
      confidence, and a direct memory does not seed a DERP session. ⚠️ Entries
      written before this are keyed by the bare address and stop matching; they
      age out with the 7-day TTL, at a cost of one session of learning per pair
      and carrier.*
- [x] **AC5 (V2)** — field: five consecutive sessions on CORPLAP-1 open within
      the carrier's measured range, with no 1.26 M opener and no 6.8 M opener.
      *Met on `agent-v0.4.93`, 2026-09-08 19:12–19:18 UTC (field log): openers
      2.55 / 2.55 / 1.44 / 2.55 / 2.82 M against a measured 1.09–6.13 M.*
- [x] **AC6 (V3a)** — ONE belief, composed in one place. `pipe_bps` is the
      session's answer to "how fast is the pipe" and the only place the sources
      are combined; `blocked_send_bps` is the one named SOURCE accessor, read
      by the opener's write-back alone. Deleted with it: `remembered_candidate_bps`
      (the same law under a second name — the two bodies were identical but for
      a flag), the THIRD copy inlined in `pre_encode_tick`, and the
      `rate_prior_decay` kill switch (flag, env accessor, config key, three
      config-surface entries, the enrollment default and its control test) — the
      prior always decays now, which FR-70 P1 field-verified on 0.4.64 with its
      own same-build FAIL control. Net −92 lines; 996 agent tests and 175
      agent-core tests pass. ⚠️ Not claimed: the three SOURCES (blocked-send
      goodput, the viewer's arrival rate, the decaying prior) are still three
      fields on the governor rather than one `Pipe` type. That move is
      mechanical and buys encapsulation, not behaviour; it is V3b, and this AC
      does not pretend to have done it.
- [x] **AC7** — docs updated with diagrams and a `docs/README.md` row
      (`docs/rate-control.md` gains the gate as its first section).
      *Done 2026-09-25: `docs/rate-control.md` opens with "The validity gate"
      — the verdict (mermaid), what every consumer does with it, V5's
      exception and its control, the write-back and the seed (mermaid, the
      four field reads of the write-back), and V3b labelled as the SHADOW it
      still is; the three deleted config keys are marked deleted, not off;
      history rows 0.4.96–0.4.100; the `docs/README.md` row names the gate.
      Every `file:line` anchor was printed from master on the day.*

- [~] **AC8 (V4)** — field, HALF met, and the second half observed in
      MECHANISM but not yet at the magnitude the criterion names. The memory
      MOVES DOWN on a carrier measured well below what is remembered, the next
      write lowers the entry (before V4 it could only rise), and the openers
      that follow stay inside the carrier's measured range. On a host whose
      opener never queues — CORPLAP-2, every opener on 2026-09-08 — the memory
      stops climbing to the `hi` cap, because an absorbed burst now writes
      nothing at all.
      *First half met on `agent-v0.4.95`, 2026-09-08 22:14–22:17 (field log): two
      idle sessions measured nothing and wrote nothing — value and timestamp
      untouched — where the pre-V4 rule would have recorded 3,825,000 from an
      absorbed burst, a 69 % rise on no evidence, twice.*
      *Second half, read 2026-09-25 from the hosts' own log files over Fleet RPC
      (field log): the down-move has happened once — CORPLAP-2, 2026-09-10
      08:39:41 UTC, `agent-v0.4.97`, `100.65.4.2|relay:derp/tcp`: a 61 s AV1
      session whose opener queued (133 ms worst wait,
      `opener_measured_bps=Some(3201927)`) and whose learner proved 3,187,500
      wrote `evidence_bps=Some(3187500) kept_bps=3210512` from a seed of
      3,233,525 — `damp` exactly (3,233,525 − 0.5 × 46,025), where the max rule
      would have kept 3,233,525. A move the old rule could not make, but by
      0.7 % on a carrier measured 1.4 % below the memory, not "well below", and
      the entry aged out (TTL) before the next relay session on 09-24, which
      adopted 3,386,718 outright — so no seeded opener followed. CORPLAP-1's
      6,131,302 did not move down: its next session (2026-09-10 12:51–14:03)
      measured the relay at 11.55–17.04 M, the learner reached the 8 M cap,
      and the write was `Some(8000000) → 6318171` — one damped step UP; that
      entry has since expired as well. The "well below" case needs a thin relay
      day, the same condition AC9 waits for.*
      *2026-10-30, classified: **needs a field pair.** The magnitude needs a
      carrier measured WELL below the memory, which is a thin relay, and a thin
      relay cannot be produced on demand without traffic shaping (§Open
      decisions, *The rig*).*
      *2026-10-31, read from the server's `agent_logs`: **the entry moves, but
      not on evidence, so this criterion cannot be proved from `kept < old`.**
      On CORPLAP-2's `100.65.4.2|relay:derp/tcp`, 2026-10-25 → 10-30, all 18
      writes with evidence are rungs of the session's own opener: 0.85 × the
      memory × (17/16)^k, six of them at k = 0. 17 of those 18 sessions held no
      live measurement in any heartbeat, and the session that held one for 60
      heartbeats wrote nothing. The entry fell 5,955,306 → 3,379,312 (−43 %)
      while those sessions delivered up to 10.2 M. So the sessions that
      measured nothing are the ones writing, the reverse of V4's rule. The
      2026-09-08 read above held because that opener sat at the nominal, where
      `stable_bps()` returns `None` (`ceiling_learn.rs:266`). A second path is
      field-seen too: at 2026-10-30 12:22:06 a session wrote the prior's
      stand-in as its evidence (§Open decisions, *The V5 loop*).*

- [~] **AC9 (V5)** — field: on a session whose transport stalls repeatedly while
      the target sits above the measured pipe, the target CONVERGES toward the
      measurement instead of climbing through every stall. Read `target_bps`
      against `goodput_bps` across a window drag on CORPLAP-2.
      *The failing run is on record (field log, 2026-09-09 07:41): target
      1,741,055 → 1,928,555 against a measurement of 1,058,145 that never moved
      all session, eleven `transit-stalled` windows, paint age 240 → 4,022 →
      7,784 ms, and a nine-second gap in a two-second heartbeat.*
      *Swept 2026-09-25 across the three relay laptops' daemon log files
      (`KEEP_DAYS = 14` ⇒ 2026-09-10 → 09-25; the 09-09 files are gone): 20
      worker logs, 24,013 heartbeats, 7,442 constrained windows in 8 relay
      sessions, 193 transit stalls — and at every one of the 193 the session
      held NO pipe estimate (`goodput_bps` live in 147 windows in total, never
      in a stalled one; `prior_bps` None in every window pulled), so the
      condition never arose (field log). The negative-control half held at
      scale: 117 of 118 stalls in a three-hour CORPLAP-2 session did not move
      the target. Still open — it needs a stall inside a MEASURED minute on a
      thin relay.*
      *2026-10-30, classified: **waits on an operator decision, then a field
      pair.** FR-71's 2026-10-29 read of the corporate laptops' logs
      (2026-10-15 → 10-29, `0.4.137` → `0.4.184`, 40,654 constrained windows)
      caught V5 cutting through stalls on thin afternoons, but toward the
      prior's stand-in rather than a measurement. It found 55 drops of exactly
      ×0.85 on gap or split windows with nothing sender-side, every one on a
      session whose prior stood at 0.02–0.8 M. On CORPLAP-2, 10-20
      13:53–14:19, the prior read 17.8–23.7 kbps against a 3.63 M target, so
      every stall read as "ours", and the target fell 3.63 → 0.94 M with 0 B
      in flight and a 3.1 s paint age. That is not this criterion: the
      criterion asks for convergence toward a MEASUREMENT, and what tells the
      two apart is a live `goodput_bps` in the stalled window.*
      *2026-10-31, **partly met**, read from the server's `agent_logs`: the
      first sustained run of stalls above a live measurement since V5 shipped.
      CORPLAP-2, 2026-10-30
      12:19:52–12:20:48, `agent-v0.4.187`, a constrained session on a genuinely
      bad path (27 transit stalls at ≤ 253 kbps, paint age to 6.1 s):
      `goodput_bps=180,484` live throughout, target 200,000–253,125, so
      1.11–1.40× the measurement. It did not climb through them. A stall
      landing above 1.2× the measurement took the target back to the 200 kbps
      floor (231,370 → 200,000; 225,000 → 200,000), and from 12:20:18 it left
      the floor for one step only. Compare 1.6–2.1× and rising at 07:41. The
      control held 15 minutes earlier: at 12:04:10–14 three stalls under a live
      709,460, with the target at 494,314, did not cut.
      ⚠️ Why this stays `[~]`: (a) the floor bounds the convergence, so it never
      reached the measurement; (b) this session's key is a bare public address,
      not a tagged carrier; (c) the same rule, the same morning, over DERP,
      converged toward a reading the path had already disproved: at 11:49:14 it
      cut 3.0 → 2.55 M against a live 223,794 taken from one 102 ms block, on a
      path that delivered 1.2 M before that and 5.39 M after. That is the open
      risk this issue recorded on 2026-09-09, now observed (§Open decisions,
      *The V5 loop*).*
      ⚠️ **The control is FR-71's finding 4**: a transit stall on a path the
      session is NOT overdriving must still not cut — that is what
      `stall_is_ours` gates on, and
      `finding_4_keeps_the_rate_with_the_validity_gate` must stay green.

**What V5 deletes: nothing.** It splits one predicate in two and removes a
blanket `valid` conjunct from three sites. Per the standing rule, the counter
that authorises the next deletion is **`goodput_samples` (accepted)**: it must
read greater than 1 on a constrained session before V3b collapses `prior`,
`goodput` and `link_rx_bps` into one `Pipe` and deletes two of the three. The
CORPLAP-2 session read `(1, 0)` — one accepted sample and, because the gate
intercepted upstream of the estimator, *zero rejected*: a counter blind to its
own starvation.

## Open decisions

- **The V5 loop: a stall is "ours" because the last stall said so.** FR-71's
  2026-10-29 fleet read found it. It was put to the operator on 2026-10-30 and
  is not answered yet. Anchors printed from master on 2026-10-31:
  - `stall_is_ours` (`governor.rs:1207`–`1216`) lets a `TransitStalled`
    window cut ×0.85 when `desired > 1.2 × pipe_bps`.
  - `pipe_bps` (`:634`) falls back to the prior's stand-in (`:648`) when
    neither the goodput nor the viewer's arrival rate is live. The arrival
    rate's TTL is `LINK_CEILING_TTL`, 15 s (`:415`).
  - The branch that cuts also calls `RatePrior::on_window(None, true)`
    (`:1434`–`1440`). That walks the prior one step down per two such windows
    (`prior.rs:85`), and it climbs back only one step per ten clean ones
    (`prior.rs:66`).

  So each cut lowers the number the next stall is judged against, and once the
  stand-in sits far below the target the comparison is true on every stall.
  Over 2026-10-15 → 10-29 (`0.4.137` → `0.4.184`) there were
  55 drops of exactly ×0.85 on clean gap or split windows. Every one was on a
  session whose prior stood at 0.02–0.8 M. The worst is CORPLAP-2, 10-20
  13:53–14:19: a prior of 17.8–23.7 kbps under a 3.63 M target, 24 gap
  windows, and 3.63 → 0.94 M with 0 B in flight and a 3.1 s paint age. That
  is the inverse of AC9's failing run: there nothing could lower the rate,
  and here nothing stops lowering it. The options put to the operator:

  | | change | what it rests on |
  |---|---|---|
  | **A** (recommended) | **V5b, here.** `stall_is_ours` compares only against a MEASURED source: the goodput, or the arrival rate inside its TTL. A stall that carries no number stops walking the prior. | The gate's own rule: "with nothing measured there is no claim to contradict". The smallest change that ends the loop and keeps the 07:41 fix. |
  | B | The same change, as FR-71 T3 | The same code. FR-79 still cannot close before it ships, because AC9 is V5's own criterion. |
  | C | Delete V5's cut, back to V1 | Ends the loop, but brings back 07:41: the rate climbed through 11 stalls to a 7.8 s paint age. |
  | D | Change nothing; baseline AC9 as debt; move the loop to a new FR | Leaves a ×0.85-per-gap ratchet on every relay session whose prior is low. |

  The fail-first cell is the 10-20 14:16 arc: prior 19,578, three gap windows,
  target 1.17 → 1.00 → 0.85 → 0.61 M. Two controls must stay green:
  `a_transit_stall_above_the_measured_pipe_cuts_the_rate` (`governor.rs:3160`,
  where a measurement stands and the stall still cuts) and
  `finding_4_keeps_the_rate_with_the_validity_gate` (`sim.rs:2452`).

  **The field A/B behind A.** CORPLAP-2, 2026-10-30 11:48–12:19,
  `agent-v0.4.187`, `relay:derp/tcp`, read from `agent_logs`. One session, one
  path, one build, sorted by what `pipe_bps` held at each stall:

  | while the session held | stalls | cut | not cut |
  |---|---|---|---|
  | a stand-in, nothing measured | 16 | **13**, ×0.85 each | 1 under the 1.2× line; 2 where an additive step landed in the same heartbeat |
  | nothing at all | 26 | **0** | 26, the target flat at 2.55–3.19 M |
  | a live measurement | 6 | 2 | 3 with the target under the measurement; 1 the heartbeat cannot explain |

  Around the first row's cuts the session read healthy, with a paint age of at
  most 107 ms bar one 203 ms and at most 43 KB in flight. The target still spent
  3 min 12 s under 1 M on a path that delivered 5.39 M.

  ⚠️ **A is necessary, not sufficient: a live measurement can be wrong.** At
  11:48:36 one 102 ms blocked send folded into `goodput_bps=223,794`, because a
  window needs only 60 ms of blocked time (`goodput.rs:82`), and it stood for
  its 60 s TTL (`:85`). V3b's delivered floor read 1.2 M eight seconds later
  and 5.39 M seven minutes later. While that reading stood the target fell
  3.0 → 0.68 M in 22 s at a paint age of 50–104 ms. V5 accounts for two of those
  cuts at most. The rest read `overproduced`, the classifier's verdict for the
  queue budget's gate, which is denominated in the same `pipe_bps`
  (`peer.rs:7713`): the door FR-70 P1 was written for, opened this time by a
  measurement rather than a seed. At 12:04:06 the shape repeated: one 106 ms
  block, 709,460, the arrival-rate clamp at 3.19 → 0.52 M, and 4.66 M delivered
  two seconds later. A does not touch this, because a blocked-send number is a
  measured source. What would is V3b's own rule, `believed = max(capacity,
  floor)`: a reading below what the path just delivered is the reading being
  wrong. That makes the floor a consumer for the first time, so it is a second
  decision, recorded here and not bundled into A.

  ⚠️ **The walk is not what made that prior small (worked out 2026-10-31 from
  FR-71's numbers).** The arithmetic is firm; the last step is an inference.
  - FR-71's arc has the stand-in at 23,688 → 21,535 → 19,578. Those values lie
    on the ×11/10 ladder (`prior.rs:80`), which `stand_in_bps` uses only when
    the prior holds a measured anchor (`prior.rs:167`).
  - The seed's ×5/4 ladder gives 24,472 from 19,578, and a walk below the base
    from 23,688 gives 21,534 and 19,576, so neither fits.
  - 17,799 → 19,578 → 21,535 → 23,688 reproduces the read's whole 17.8–23.7
    kbps range. The walk only moved the prior between four rungs above a
    measured base of at most 17,799 bps.
  - That session read `goodput_bps=None` throughout, which leaves the viewer's
    arrival rate as the likely source. A congested report stores it with no
    floor (`governor.rs:1343`), and the same window re-anchors the prior on it
    (`:1433`, `prior.rs:128`).
  - FR-71's second open decision, the stall's onset, is the likeliest way so
    low a rate gets reported. It is not proven for this arc: the read did not
    pull the report that set the anchor. Readings that low are field-seen,
    though: at 2026-10-30 12:19:52 `pipe_belief`'s capacity read 12,070 bps
    while `goodput_bps` read 180,484, so that number came from a congested
    report.

  So what cut the rate is the comparison reading the stand-in, and that is
  what V5b fixes. V5b does not remove the anchor. Until it climbs back, at
  ×1.1 per ten clean windows, `pipe_bps` still offers it to FR-59 P6's
  contradiction (`governor.rs:777`), the encoder plan (`peer.rs:7686`), the
  queue budget (`peer.rs:7713`) and the write-back. V5b also still reads the
  arrival rate itself for its 15 s TTL. That is why the onset decision may
  belong beside this one.

  ⚠️ **The stand-in reaches the pair memory: field-seen once.** When the learner
  holds no above-nominal stable rate, the write-back takes `pipe_bps` as the
  session's evidence (`peer.rs:8985`–`8994`), and `pipe_bps`'s last source is
  the stand-in (`governor.rs:648`). The 2026-10-30 12:19 session anchored at
  180,484, lost its goodput to the TTL at 12:20:50, and walked eight rungs down
  in 28 s: 180,484 → 164,076 → … → 84,195, the last one exact to the unit. It
  ended with `prior_bps=84195` and `goodput_bps=None`, and at 12:22:06 wrote
  `evidence=Some(84195) kept=84195` as a new entry. Nobody measured 84,195.
  FR-71's "next session 2.55 → 0.20 M" stays unconfirmed: its read did not pull
  the write-back's line (`peer.rs:2111`–`2118`). V5b's second half closes the
  stall's path into the memory.

  It does not settle an older tension, which is not part of this decision.
  FR-70 P1 writes the decayed prior on purpose, as its way out of a stale low
  memory, while V4's rule is that only evidence moves the memory. The
  write-back's first choice is the larger gap: `stable_bps()` is a held target,
  not a measurement, and AC8's 18 of 18 rungs are that (the question FR-35 AC1
  turns on; `docs/rate-control.md`, the write-back's field rows).
- **The rig AC8 and AC9 need (2026-10-30).** Both need a relay that is thin
  AND measured, on demand. The fleet's relays cannot be summoned thin: the
  same carrier measured 1.06 M at 07:41 and 4.41 M at 14:42 on 2026-09-09.
  Shaping is allowed only inside throwaway VMs, never on the dev box, a fleet
  device or a shared host. The cell:
  - Two throwaway Linux VMs, each running the released `roomlerd` package of
    the build under test, so nothing has to be built.
  - The host VM captures a virtual framebuffer (`scripts/dev-xvfb.sh`). The
    viewer VM runs the browser through `roomler.ai`.
  - The overlay pair is forced onto DERP by dropping UDP between the two VMs,
    inside the VMs. The carrier then tags itself `relay:derp/tcp`, as the
    CORPLAP cells did.
  - `tc` rate shaping (`tbf`/`netem`) runs on the host VM's own egress.

  The runs, in order:
  1. RED on a build without V5b, then GREEN on V5b's build (AC9).
  2. An unshaped session writes the memory; a shaped session measures well
     below it and the write-back lowers the entry; the next shaped session
     opens inside the measured range (AC8's magnitude half).

  It is not built. It costs two VMs and waits for a go.
- Whether a `ViewerLate` window is evidence about the pipe. Today it is (only
  `TransitStalled` rejects); the viewer being the limiter says nothing about the
  path's capacity, but the goodput estimator only counts blocked sends anyway.
- Whether the carrier should key the memory (a map per carrier) or clamp the
  seed (one entry, bounded by the live carrier's measurement). Keying remembers
  more; clamping cannot go stale.
- Whether `stall_seen` (FR-70 P1's prior verdict) belongs behind the gate too.
- Whether V5's `stall_is_ours` should read the V3b belief's `capacity_bps`
  once the shadow settles. The 2026-09-25 sweep found the blocked-send source
  live in 2 % of relay windows and a three-hour session with one push-back
  (`pipe_belief_n=(9912, 1)`): with the current sources the rule is inert on
  a healthy relay, which is safe, and on a thin one, which is the AC9 gap. The
  belief's capacity is fed by the same two sources today, so reading it would
  change nothing until a transit stall itself feeds the belief — and what a
  stall with no number attached should feed is exactly what is not settled.
  *2026-10-30: "inert on a thin one" held only while no prior stood. When a
  prior stands, the rule falls through to its stand-in and loops (the first
  decision above).*

## Out of scope

- The plan handoff (FR-70 M3) and the single controller (M4). This FR makes
  those safe by removing the levers they would otherwise have to preserve.
- The viewer's own stall report (FR-71's first open decision).

## Field-verification log

| when | build | cell | result |
|---|---|---|---|
| 2026-09-08 12:15 / 14:52 / 16:33–16:42 | 0.4.87 / 0.4.90 | CORPLAP-1 on the Check Point VPN, relay | the three events this FR generalises; see FR-71's log for the first two and §The opener for the third |
| 2026-09-08 19:12–19:18 UTC | **agent-v0.4.93** (V1 + V2), my view-only sessions | CORPLAP-1 on the Check Point VPN, HEVC over the relay — the carrier keyed itself `relay:derp/tcp` throughout | **AC5 — PASS, five consecutive sessions.** The pre-FR-79 memory (`100.65.0.5` = 8.0 M, bare key) was correctly ignored, so the first two sessions opened at the 2.55 M relay nominal instead of 6.8 M. Openers: **2.55 / 2.55 / 1.44 / 2.55 / 2.82 M** against a carrier the same sessions measured at **1.09–6.13 M** — every opener inside the measured range, no 6.8 M over-drive and no 1.26 M floor-opener. The write-back is the measurement now: `opener_measured_bps=Some(1440672) growth_target_bps=1440672` (653 KB, 179 ms worst wait — the old rule would have computed 21.9 M and recorded the 8 M cap) and `Some(1087230)` for the next (645 KB, 236 ms → 16.4 M, capped, before). The unqueued branch is untouched (`opener_measured_bps=None`, 256 KB, 0 ms → the ×1.5 step). The memory now holds `100.65.0.5\|relay:derp/tcp` and `100.65.4.2\|relay:derp/tcp` beside the stale bare keys, which age out. `evidence_rejected` per session: `[0,2,2,0] [0,1,1,0] [0,0,0,0] [0,1,1,0] [0,0,0,0]` — every rejection a transit stall and exactly one shadow each, none from an agent stall or a carrier change, and no session cut on one. ⚠️ Open, and honest: DERP's own rate moved 1.09 → 6.13 M inside six minutes, and `record_session` still keeps the MAXIMUM, so the memory ratchets toward the high measurement (session 5 opened at 2.82 M from a 3.32 M seed). It ratchets to something measured now rather than to arithmetic that could not be right, but "max of the measurements" is the next thing to question — a V3 candidate beside the single estimator. |
| 2026-09-08 22:14–22:17 UTC | **agent-v0.4.95** (V3a + V4) | CORPLAP-1 on the Check Point VPN, two view-only sessions over `relay:derp/tcp`, an idle desktop | **AC8, first half — PASS: a session that measured nothing wrote nothing.** Both openers were absorbed by the socket (203 KB and 330 KB, `opener_wait_max_ms=0`, `opener_measured_bps=None` ⇒ `growth_target_bps=0`), the goodput estimator held no confidence for the whole of either session (`goodput_bps=None`, send waits 0.08–0.15 ms, age 51–67 ms, `evidence_rejected=[0,0,0,0]`), and the write-back logged `evidence_bps=None kept_bps=2264993` — the entry kept its value **and its timestamp**. The counterfactual is exact: the pre-V4 rule would have taken the unqueued branch (`opener_maxrate 2,550,000 × 150 %`) and `max(old 2,264,993, 3,825,000)`, writing **3,825,000** — a 69 % rise recorded by a session that measured nothing, twice in three minutes. That is the ratchet this phase removed, caught in the act. **Second half still open**: the memory has not yet been observed MOVING DOWN, because neither session pushed back on the pipe (an idle 1.3–1.4 Mbps desktop over a carrier that carries it). It needs a session whose sends block — the ones that measured earlier this evening had 179–236 ms opener waits on a busier screen — and CORPLAP-1's `100.65.4.2|relay:derp/tcp` still holds **6,131,302** from the max rule as the standing target for it. |
| 2026-09-09 07:41 UTC | **agent-v0.4.95** (V1–V4), operator's own session | CORPLAP-2 (`100.65.4.4`) on `relay:derp/tcp`, AV1 on `av1_nvenc` at 1920×1200, `constrained=true`, an idle desktop with window drags | **AC9 — the FAILING RUN, and the defect V5 answers.** Operator report: *"dragging window jumping to over 140ms and more … now reached even over 1000ms, text blurred, freezing."* Heartbeats: `goodput_bps=Some(1058145)` **for the entire session, never once updated**, with `goodput_samples=(1, 0)` — ONE accepted sample and *zero rejected*, because the gate intercepts upstream of the estimator, so the counter is blind to its own starvation. Against that measurement `target_bps` ran **2,001,750 → 2,189,250 → 1,860,820 → 2,048,320 → 1,741,055 → 1,928,555**, i.e. 1.6–2.1× the measured pipe throughout. `viewer_age_ms` 240 → **4,022** → **7,784**, with a **nine-second gap between 07:41:22 and 07:41:31 in a two-second heartbeat** — the freeze. `pipe_states=[2,25,1,11,0]` and `evidence_rejected=[0,11,2,0]`: eleven transit stalls and two shadows, every one of them discarded, so `age_over`, `link_over` and the hard MD were all false and **the rate ENDED HIGHER than it started**. `bytes_inflight=0`, `send_wait_max_ms=0.3` — the local send queue never filled, which is why nothing sender-side could produce a decrease either. Cause: V1 replaced FR-35's quarantine-and-replay (`observe_window(&held)` + `apply_hard_md` once a stall was confirmed) with an unconditional DROP, and every loop that can lower the rate reads `valid`. ⚠️ Not a codec issue — AV1 and HEVC ride the same loop. ⚠️ The daemon restart the operator read as a crash was not one: no panic in the log and no Windows fault record; four `localapi: config key updated … (takes effect on restart)` writes at 07:30:53 (exec/ssh being enabled) restarted the worker one second later and killed the HEVC session that had started at 07:30:34. |
| 2026-09-09 14:41–14:43 UTC | **agent-v0.4.96** (V5), my own session | CORPLAP-2, same host and same cell as the failing run: AV1 4:2:0 HW (`av1_nvenc`) over `relay:derp/tcp`, `constrained=true`, four window drags | **AC9 NOT met — the cell did not reproduce, and the run is a NEGATIVE CONTROL instead.** DERP measured **4,410,833** this time against a target of **3,151,761**, i.e. the session was *under* its measured pipe, not 1.6–2.1× over it as at 07:41 (1,058,145 measured). `stall_is_ours` therefore never armed, and correctly so. What the run does prove is the half that must not regress: `viewer_age_ms` stayed **64–102 ms** across 80 windows with `bytes_inflight=0` throughout, and the one `transit-stalled` window that did occur (14:42:53, `pipe_states=[1,80,1,1,0]`, `evidence_rejected=[0,1,1,0]`) **did not cut** — the target held at 3,151,761. That is FR-71's finding-4 property holding in the field on the new law: a transit stall on a path the session is not overdriving still costs nothing. ⚠️ **The thin-pipe condition is not summonable on demand** — the same carrier measured 1.06 M at 07:41 and 4.41 M at 14:42, and this FR's own 2026-09-08 log already recorded DERP moving 1.09 → 6.13 M inside six minutes. AC9 needs a session caught while the relay is genuinely thin; the failing arc above is the standing baseline to compare it against. ⚠️ `goodput_bps` fell to `None` at 14:42:31 (the confidence TTL, no further blocked sends) — with nothing measured there is no claim to contradict, so a stall in that state cannot cut either, which is the intended conservative direction and another reason a thin-pipe cell needs live blocked sends to be meaningful. |
| 2026-09-10 08:38–08:39 UTC (read 2026-09-25) | **agent-v0.4.97** (V1–V5a) | CORPLAP-2, AV1 over `relay:derp/tcp`, a 61 s session; read from the host's own `roomlerd.log.2026-09-10` over Fleet RPC | **AC8, second half — the mechanism observed, the magnitude not.** Seeded from `100.65.4.2\|relay:derp/tcp` = **3,233,525**; the opener queued (440 KB, 133 ms worst wait, `opener_measured_bps=Some(3201927)`), the goodput held 3,201,927 for 30 windows, the learner proved 3,187,500, and the write-back logged `evidence_bps=Some(3187500) kept_bps=3210512`. That is `damp(3,233,525, 3,187,500)` to the unit (3,233,525 − 0.5 × 46,025 = 3,210,512.5 → 3,210,512), and the first field write that lowered an entry: the pre-V4 rule, `max(stable 3,187,500, old 3,233,525, growth 3,201,927)`, would have kept 3,233,525. 🔑 One line suffices as proof — `kept` lies between the old value and the evidence, so `kept > evidence` means the entry moved DOWN. ⚠️ Honest bound: a 0.7 % move on a carrier measured 1.4 % below the memory is the rule working, not the "measured well below" scenario the criterion describes, and no seeded opener followed inside the TTL — the entry expired on 09-17 and the next relay session (09-24 08:26) adopted its own 3,386,718 outright. |
| 2026-09-10 12:51–14:03 UTC (read 2026-09-25) | **agent-v0.4.97** | CORPLAP-1, HEVC over `relay:derp/tcp`, 72 min, 2,124 constrained windows | **The 6,131,302 question, answered: it moved UP, damped, on a measurement.** The session opened from the 6,131,302 seed at 5,211,606 (85 %), the opener queued 514 ms but the estimator had no confidence (`opener_measured_bps=None`), and then the relay was fast: goodput **11,550,209–17,044,136** in 88 windows, the learner at the 8 M `hi` cap, and the write-back `evidence_bps=Some(8000000) kept_bps=6318171` — one `ALPHA_UP` step (6,131,302 + 0.1 × 1,868,698 = 6,318,171.8). 69 transit stalls with the target (≤ 8 M) UNDER the measured pipe: no V5 cut is attributable and none should be; the cuts that did happen (12:57:48 8.0 → 5.78 M with `send_wait_max_ms=181`, age 3,435; 13:02:52 5.77 → 3.55 M with `send_wait_max_ms=504`, `bytes_inflight=229397`, age 2,132) are the local queue filling — the AIMD's own occupancy loop and the age loop, in windows the gate did not reject. No relay session has written that key since; it expired on 09-17. |
| 2026-09-25 (logs of 2026-09-10 → 09-25) | **agent-v0.4.97 → 0.4.101** (V5a + V3b shadow throughout) | AC9 sweep of the three relay laptops' daemon **log files** over Fleet RPC — not `roomler logs`, whose 64 KiB tail ages a finished session out; `KEEP_DAYS = 14` had already pruned the 09-09 files (V5's first day and the 14:41 control run) | **AC9 — no natural occurrence; coverage recorded so that "none" is a count.** 20 worker logs (`service-logs\roomlerd.log.<date>`), **24,013 heartbeats, 7,442 constrained windows in 8 relay sessions**: CORPLAP-1 5 sessions / 2,171 windows (09-10, 09-11), CORPLAP-2 3 sessions / 5,271 windows (09-10, 09-24), CORPLAP-3 0 constrained in 14,373 heartbeats (every session direct — `av1_qsv` at a 34.56 M target; the zero positive-controlled by counting `constrained=false`). Transit stalls in those sessions: 118 + 69 + 6 = **193**, and at every one of them the session held **no pipe estimate**: `goodput_bps` was live in 147 constrained windows in total (2 %: one accepted sample per long session, held for its 60 s TTL), never in a stalled window, and `prior_bps` was `None` in every window pulled (7,395 of the 7,442 — no seed in force, or a measurement at the band leaving nothing to stand in). The detector's verdict function (`analyse`, kept outside the repo because it names hosts; its selftest had been lost, so it was re-validated the same day: `NONE` on these rows, `did NOT converge — ratio 1.89x → 1.82x` on an arc shaped like the 07:41 failure, `CONVERGED` on one that steps ×0.85 per stall) reads **NONE**. What the sweep does show is the negative-control half at scale: the three-hour CORPLAP-2 session (`6ab4de94`, 09-24 08:26–11:21, AV1 over `relay:derp/tcp`, `0.4.100`) held its target at the learned 3,386,718 through **117 of 118** transit stalls — the exception, 10:33:30–34, is an 800 ms locally blocked send inside an `agent-stalled` window (the AIMD's own loop, not the gate) — at a paint age of p50 76 / p95 99 / max 988 ms, `bytes_inflight` ≤ 71 KB, and `pipe_belief_n=(9912, 1)`: one push-back with a number in three hours. ⚠️ The condition AC9 needs — a stall INSIDE a measured minute on a thin relay — did not happen in two weeks of fleet traffic, and nothing here can summon it; the 07:41 arc stays the baseline. |
| 2026-10-15 → 10-29 (read 2026-10-29 for FR-71; re-read against this FR 2026-10-30) | **agent-v0.4.137 → 0.4.184**; no line of the comparison or the prior's walk changed from `agent-v0.4.137` to master `dfca419fe` (tag diffs, 2026-10-31) | The three corporate laptops' worker logs over Fleet RPC: 67 constrained sessions, 40,654 constrained windows, 812 `transit-stalled` | **AC9: V5 caught converging, but toward the stand-in, not a measurement — the V5 loop, and not the criterion.** 55 drops of exactly ×0.85 on clean gap or split windows, every one on a session whose prior stood at 0.02–0.8 M. Worst: CORPLAP-2, 10-20 13:53–14:19, HEVC over `relay:derp/tcp`, a prior of 17.8–23.7 kbps under a 3.63 M target, 24 gap windows, 3.63 → 0.94 M, 0 B in flight, 3.1 s paint age; the next session 2.55 → 0.20 M. The half that must not regress held where nothing was measured: 433 of 482 clean stall episodes kept the target. Read on master the same day: the comparison falls through `pipe_bps` to the stand-in (`governor.rs:648`), and the cutting branch walks the prior down with no number (`:1439`). The stand-in's values show what set it: 23,688 → 21,535 → 19,578 lie on the measured ×11/10 ladder, above a base of at most 17,799 bps (§Open decisions). **AC8 was not read**: the write-back's line (`peer.rs:2111`–`2118`) was not part of the pull. Decision and rig: §Open decisions. |
| 2026-10-24 → 10-31 (read 2026-10-31) | **agent-v0.4.187** and **0.4.188** on the sessions traced | The server's `agent_logs`, not the hosts (exec is off-limits on the corporate laptops): CORPLAP-1 2,203 heartbeats, 312 constrained, no prior in any; CORPLAP-2 8,040 heartbeats, 7,766 constrained, 308 with a prior, 30 write-backs. Bounded: one agent and one day per query; Mongo's restart count stayed 1 | **AC9, partly met; the V5 loop on `0.4.187`; AC8's rule reversed.** All three are in §Acceptance criteria and §Open decisions with their numbers. The A/B: over one DERP session stalls cut 13 of 16 times with a stand-in and 0 of 26 with nothing. The measured minute: 2026-10-30 12:19:52–12:20:48, 27 stalls at a live 180,484, the target held at 1.11–1.40×. The write-backs: 18 of 18 on the pair are rungs of the session's own opener, 17 with nothing measured, −43 % in six days; one wrote the stand-in, 84,195. New on the same trace: a live goodput of 223,794 from one 102 ms block on a path that delivered 5.39 M, the open risk of 2026-09-09. ⚠️ Scope: two laptops of the three, a heartbeat spans two windows so a cut is attributed by its heartbeat's counters, `agent_logs` drops lines under load so every count is a lower bound, and the upload keeps 7 days, so these numbers are the record. |
