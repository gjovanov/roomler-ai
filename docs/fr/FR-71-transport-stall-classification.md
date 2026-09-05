# FR-71 — Transport stall classification: a transit stall is not an over-production signal

**Issue:** [#1362](https://github.com/gjovanov/roomler-ai/issues/1362) · **Status:** proposed 2026-09-05 ·
**Parent:** split out of FR-70 (#1330) on its own open question; FR-70's M0 is this FR's instrument.

## Goal

When the path between the wire and the viewer's decode worker stalls — a
DERP/TCP head-of-line block, a relay reconnect, a Wi-Fi roam — the rate
controller must recognise the window as **transit-stalled**, hold the rate and
let the backlog drain, instead of reading the paint age as "the encoder produced
too much" and cutting the rate into a link that was never the limiter. A repeat
of FR-70's finding 4 must be classified correctly **and** must not cut the rate.

## Why this is its own FR

FR-70's plan asked whether transport classification belonged in the media
pipeline FR or in its own, and answered its own question with the case for
splitting: it is the largest measured harm of the 2026-09-04 findings, it shares
nothing with the threading work except the instrument, and bundling would let
the pipeline FR claim credit for work that had not started. M0 has now delivered
the instrument, so the split is clean: FR-70 keeps the attribution half of its
AC5 (met), this FR owns the response.

## Field evidence

- **Finding 4** (CORPLAP-3, 2026-09-04, session `6a9abaa8`), the operator's
  4903 ms paint:

  ```
  12:33:56  age=None  inflight=5339   goodput=5.60M  iter_max=35.6  skips=1
  12:33:58  age=2851  inflight=2377   goodput=5.60M  iter_max=26.9  skips=1
  12:34:01  age=4903  inflight=1485   goodput=8.51M  iter_max=28.5  skips=24
  12:34:03  age=57    inflight=694    goodput=8.51M  iter_max=323   skips=24
  ```

  Frame age 4903 ms while the send queue held 1485 bytes, the worst pump
  iteration was 28 ms and the encoder averaged 14 ms. Nothing sender-side was
  wrong; the viewer sent no report for two windows; 23 frames were skipped to
  backpressure in the same window because the sender *wanted* to send and could
  not; then goodput jumped to 8.5 Mbps as the backlog drained at once. The AIMD
  and the FR-15 age loop cut the rate anyway, because `viewer_age_ms` fused the
  stall into one number that every loop reads as over-production.
- **The instrument is live** (FR-70 M0, `agent-v0.4.66`, field 2026-09-05): every
  heartbeat carries `age_split = AgeSplit { sender_ms, transit_ms, viewer_ms }`;
  a 45 ms relay age read as 44 ms transit + 1–2 ms viewer + 0.1 ms sender on a
  live pinned-relay path. A stall will read as `transit_ms` in the seconds with
  the other two flat.
- **The simulator already has the cell**: `encode::sim::fixtures::derp_with_stalls`
  (400 kbps with 120 kbps dips and 1–4 s stalls) and `fast_pipe_early_stall`,
  plus the shipped-rule harness (`MeasureRule::OnPushBack`, byte-budget gate,
  rebuild-bound encoder) from #1350 — the law can be verified before a field
  cell is even attempted.

## Key design

### `encode::pipe_state` — the classifier (pure)

One verdict per viewer window from signals the governor already holds:

| state | says | evidence |
|---|---|---|
| `Overproduced` | the sender is the limiter | `send_wait` rising, `inflight` at or over the byte budget, blocked sends (goodput samples accepted), budget-gate skips |
| `TransitStalled` | the path is the limiter | `transit_ms` rising or above its floor by more than the slack while `sender_ms` is flat, `inflight` is small and `viewer_ms` is flat; or a viewer report gap with a non-empty send queue |
| `ViewerLate` | the browser is the limiter | `viewer_ms` rising, decode queue deep, `struggling` |
| `Clear` | none of the above | |
| `Unknown` | no split reported (pre-M0 viewer, no age this window) | the loops behave exactly as today |

The floors are learned the way the FR-15 age loop learns its floor (window
minimum, bounded by the probe's half round trip), so a permanently slow path is
not a permanent stall. Pure and `Instant`-explicit, like `slow_start` and
`prior`, so it unit-tests on the default build and runs inside B0.

### The response

- On `TransitStalled`: the AIMD takes **no multiplicative decrease** for the
  window, the FR-15 age loop does not fire, and the FR-59 P3 arrival clamp is
  held rather than re-armed. The FR-59 P4 drain may still pause production —
  a pause is a drain, not a cut, and on a stalled path it is the right move.
  The target stays where it was; when the path recovers the backlog drains at
  the rate the link actually has (finding 4 drained at 8.5 Mbps).
- On `Overproduced`: exactly today's behaviour.
- On `ViewerLate`: today's viewer-rate cap (fps shedding), no bitrate cut.
- `Unknown`: today's behaviour, unchanged — an old viewer costs nothing.

### Kill switches

`transit_classify` (T1a, default on: shadow classification and counters only)
and `transit_hold` (T1b, default **off** for one release — a controller change
ships behind evidence, FR-63's rule).

## Phases

| Phase | What | Kill switch | Status |
|---|---|---|---|
| **T1a** | `encode::pipe_state` + heartbeat `pipe_state` + per-state counters; the B0 fixtures classified under the shipped-rule harness | `transit_classify` | proposed |
| **T1b** | the hold: no MD, no age-loop fire, clamp held on `TransitStalled` | `transit_hold` (default off) | proposed |
| **T1c** | the cells: `derp_with_stalls` in B0 (the law), then the corp-VPN DERP path (the field), each shown to FAIL with the hold off first | — | proposed |

## Acceptance criteria

- [ ] **AC1** — the classifier locks in unit tests and on the B0 stall fixtures
      under the shipped-rule harness: every stall window is `TransitStalled`,
      every budget-gate window on the thin pipe is `Overproduced`, a decode
      backlog is `ViewerLate`, a pre-M0 window is `Unknown`.
- [ ] **AC2** — one release of shadow classification across the fleet, reviewed
      from `agent_logs`: no constrained session classified `TransitStalled`
      while its send queue was over budget, and finding 4's shape classifies
      as `TransitStalled` in replay.
- [ ] **AC3** — with `transit_hold` on, a repeat of finding 4 shows **no rate
      cut** during the stall and recovery within the stall's own length; the
      same cell with the hold off still cuts — the FAIL recorded first.
- [ ] **AC4** — no regression on the LAN, direct and thin-pipe cells (peak
      paint, settle time, over-drive integral unchanged within noise).
- [ ] **AC5** — FR-70's AC5 closes here, and FR-63 B1's controller consumes
      `PipeState` rather than re-deriving it.

## Open decisions

- Whether the viewer should report stalls directly (a gap in arrivals with a
  non-empty decode queue) so the agent does not infer them from the split alone.
- Whether `TransitStalled` should also suppress the FR-35 learner's decrease
  follow-through (a stall is not evidence the pair cannot carry the ceiling).
- How long a hold may last before it is treated as a real capacity change
  (`MAX_LIFETIME`-style bound, so a path that never recovers still converges).

## Out of scope

The relay itself — why DERP/TCP head-of-line blocks and how to leave the relay
(FR-19 peer relays, FR-64 remote control off the overlay); the media thread
(FR-70 M1–M5); the diag HUD rendering of the split.

## Related

FR-70 #1330 (M0's split is this FR's instrument; its T1 row now points here),
FR-59 #1163 (P3 link loop, P4 drain), FR-15 (age loop), FR-63 #1243 (B1
consumes `PipeState`), FR-64 #1244, FR-19 #805.

## Field-verification log

| when | build | cell | result |
|---|---|---|---|
| 2026-09-04 12:34 UTC | 0.4.59 | CORPLAP-3 → neo16, DERP path, session `6a9abaa8` | **the FAIL on record**: 4903 ms paint with a 1485-byte send queue; the rate was cut into a link that was never the limiter |
