# FR-59 — Slow-link latency priority: make delay the controlled variable

**Issue:** [#1163](https://github.com/gjovanov/roomler-ai/issues/1163) · **Status:** P1/P2/P6 in progress ·
**Parent/siblings:** FR-1 (drag smoothness), FR-14 (direct jitter), FR-15 (age feedback),
FR-17 (partial-reliability video), FR-35 (relay ceiling learns the pair)

## Goal

A remote-desktop session over a genuinely slow link (mobile hotspot, congested hotel/airport
Wi-Fi, throttled VPN) must stay **usable at low latency** rather than degrading into a
seconds-behind slideshow. Concretely: **frame age must track the path floor, not the queue**,
and every rate/queue/resolution decision must be denominated in what the pipe is MEASURED to
carry — not in a nominal band.

Today every controller optimises a *rate* and lets latency fall out. On a slow link that is
backwards, and the result is not merely "no latency priority": the controller **climbs while
the viewer is seconds behind**.

## Root cause — field evidence

Measured 2026-09-01 14:05 UTC, live session `6a96dac3cc1d845926aec5a8`, CORPLAP-3 → neo16,
viewer on a phone hotspot at Sofia airport. `av1_qsv`, 1920×1200, `constrained=true`,
14 consecutive `FFmpeg DC pump heartbeat` windows:

| signal | value |
|---|---|
| **measured pipe** (`goodput_bps`) | **64,850 → 395,122 bps** |
| **encoder target** (`target_bps`) | **1,500,000 → 1,816,834 → 2,133,668** *(climbing)* |
| FR-35 learned ceiling | **5,069,353** (12.8× the pipe) |
| **viewer paint age** | 597 · 655 · 795 · **2,284** · **7,095** ms (floor 110 ms) |
| agent send queue | `bytes_inflight` 1–4 KB, `send_wait_max_ms` **0.09–0.2 ms** |
| shed counters | `frames_dropped_backpressure=0`, `frames_skipped_backpressure=88` (static) |
| encode resolution | 1920×1200 constant, `avg_scale_ms=0.0` — no downscale |
| DataChannel | `dc_ordered=Some(true)` — reliable **and** ordered |
| age reports | `None` in 8 of 14 windows; `viewer_age_implausible=60` |

The agent pushed **1.5–2.1 Mbps into a 0.4 Mbps pipe**, its own queue read empty, and it
therefore **increased**.

### The six mechanisms

1. **A hard 1.5 Mbps bitrate floor.** `MIN_BITRATE_BPS = 1_500_000`
   (`agents/roomlerd/src/encode/mod.rs:192`); on a constrained path `area_min_bitrate_bps()`
   (`:215`) returns exactly it, and `governor.rs:317` passes it as the AIMD's `floor_bps`.
   **The encoder cannot be told to emit less than 1.5 Mbps, ever** — 3.8× the measured pipe at
   best, 23× in the worst window. Every other lever is downstream of a target the encoder is
   not allowed to reach.

2. **The measured-goodput clamp is off on exactly the path that needs it.**
   `governor.rs:299` — `if self.measured_ceiling && !constrained` — is DIRECT-only, disabled
   for relay on 2026-08-27 because lumpy TURN-TCP samples crashed the down-fast EWMA. So the
   relay path has **no rate anchor at all**. And `derived_ceiling_bps` is itself
   `.max(1_000_000)` (`goodput.rs:107`), so even re-enabled it could not describe a 395 kbps
   link.

3. **The AIMD's congestion signal is structurally blind here.** It observes send-channel
   occupancy (`aimd.rs`, module docs). Field: 1–4 KB in flight and 0.1 ms max send-wait *in the
   windows the viewer is 2.3 s behind* — the bytes leave the agent instantly and queue in the
   TURN relay and the carrier. After `AI_SETTLE` (5 s) of "all clear" it additively increases.
   **The loop makes the latency worse.**

4. **The queue budget is denominated in the nominal ceiling.**
   `constrained_queue_budget_bytes(relay_max_bps())` (`rate_profile.rs:326`, called once at
   `peer.rs:4670`) = 3,000,000 × 450 ms / 8000 = **168,750 bytes**. At 395 kbps that "450 ms"
   budget is **3.4 seconds** of standing queue. The gate never fired.

5. **The only true latency signal is silent, rejected, or toothless.** `viewer_age_ms=None` in
   8/14 windows; `viewer_age_implausible=60` (FR-15 P2's plausibility bound discards real
   samples when a mobile probe RTT swings); `AGE_OVER_WINDOWS=2` (`viewer_rate.rs:316`) needs
   *consecutive* over-windows, so the `None` gaps keep resetting the streak. When it does fire,
   `age_over` cuts **fps** and issues an AIMD MD — which stops at the 1.5 Mbps floor.

6. **Video rides a reliable + ordered DataChannel.** FR-17 stage B exists but is behind a
   viewer `localStorage` opt-in (`useRemoteControl.ts:2878`). On a lossy mobile link every lost
   chunk head-of-line-blocks the stream *and* burns scarce capacity retransmitting a frame that
   is already stale. `peer.rs:2767` still records "a gap cannot occur".

Aside, and load-bearing for P6: FR-35's rate memory keys on `nominated_remote_ip`
(`peer.rs:1436`) — on a TURN relay that is the **relay's** address, so a fast office day writes
`5_069_353` into a key a slow airport session inherits for `TTL` (7 days).

## Key design

**Make delay the controlled variable.** Three ideas, in dependency order:

- **A floor is an assumption about the band, not physics.** The 1.5 Mbps legibility floor is
  right for a 2–9 Mbps relay and wrong for a 0.4 Mbps one. It descends **only on evidence** —
  a held goodput measurement below it — never on a guess, and never below an absolute hard
  minimum.
- **Budgets are denominated in the MEASURED rate.** A queue budget in milliseconds is a lie
  unless the bits-per-second it divides by is the pipe's. ⚠️ Consuming a lumpy under-estimate
  for the *queue budget* is safe in a way that consuming it for the *ceiling* is not: an
  under-estimate makes the budget smaller ⇒ more shedding ⇒ **lower latency**, which is the
  direction this FR wants. That asymmetry is why P2 may use the signal that #2 above
  (correctly) refused to use for the ceiling.
- **The signal must come from where the queue is.** The agent cannot see a queue that lives in
  the relay and the carrier. Only the viewer can — and the two viewer-side quantities that need
  neither clock sync nor a plausibility bound are **received bytes per window** (a local byte
  count over a local interval) and the **age *trend*** (a slope; the clock offset cancels in a
  difference). P3.

## Phases

| # | Phase | What | Kill switch | Status |
|---|---|---|---|---|
| **P1** | Evidence-gated floor relief | The AIMD floor descends toward the measured pipe on constrained paths, clamped at `slow_link_min_bitrate_bps`. Evidence = the agent's goodput estimate OR the viewer's arrival rate, whichever is lower | `ROOMLERD_SLOW_LINK_FLOOR=0` / `slow_link_floor` | **implemented** (#1169) |
| **P2** | Budgets in measured rate | `constrained_queue_budget_bytes` re-derived per iteration; a measurement may only ever LOWER the reference | `ROOMLERD_CONSTRAINED_QUEUE_MEASURED=0` | **implemented** (#1169) |
| **P6** | Abandon a contradicted seed | A held measurement more than 2× below `learned_bps` resets the FR-35 learner to nominal | `ROOMLERD_SEED_CONTRADICTION=0` | **implemented** (#1169) |
| **P3** | A signal that can't be fooled | Viewer reports `rx_bps` + queue **growth**, the latter as `Σ(Δarrival − Δwire)` — a difference of intervals, so the clock offset cancels and no probe is needed. Sustained growth caps fps, feeds the AIMD a congestion sample, and bounds the ceiling at 90 % of the arrival rate | `ROOMLERD_VIEWER_RATE_CLAMP=0` | **implemented** (#1169) |
| **P4** | Drain, don't wait | P3's drift INTEGRATED into a depth estimate; past `DRAIN_THRESHOLD_MS` the pump stops producing for a bounded sub-second pause | `ROOMLERD_QUEUE_DRAIN=0` | **implemented** (#1169) |
| **P5** | Slow-link profile, engaged once | Below a measured threshold: resolution + fps capped **at pump start** (never as a mid-session rung — that is why `PRIORITY_RES_CAP` is off by default: an 865 ms blocking QSV rebuild), with a viewer badge | `ROOMLERD_SLOW_LINK_PROFILE=0` | planned |
| **P7** | Unordered video — the reorder buffer | **FR-17 stage C**: chunks assemble into slots in ANY order and the frame emits whole; a frame overtaken by a newer completed one is abandoned with a gap (→ IDR); partials bounded at 3 | existing `roomler-rc-unordered-video` | **buffer implemented** (#1169); **default flip deliberately NOT taken — see below** |

## P7 — why the default flip is not in this PR

The reorder buffer (FR-17 stage C) is done, and it is the half that was
*blocking*: with the strict in-order assembler, turning `{ordered:false}` on made
things **worse**, because chunk 2 arriving before chunk 1 is the common case on an
unordered channel and every such frame read as a break — a keyframe request per
frame, on the thinnest pipe. So the existing opt-in was, in practice, unusable.
It now works, which is what makes a field test of the flip possible at all.

The flip itself is deliberately still opt-in
(`localStorage['roomler-rc-unordered-video'] = '1'`), for three reasons:

1. **It cannot be scoped to slow links.** A DataChannel's ordering is fixed at
   creation, before ICE nominates a pair, so neither side knows yet whether the
   path will be constrained — FR-17 already recorded "constrained-only" as
   un-implementable for exactly this reason. Flipping the default means flipping
   it for every session, direct included.
2. **This FR's own AC4 is still outstanding.** Changing the transport for the
   whole fleet on the strength of unit tests is the "CI green ≠ done" mistake this
   repo has a standing rule against.
3. **The failure mode is silent-ish.** An unordered channel that mis-assembles
   does not error; it produces a keyframe storm, which on a slow link looks like
   the very problem this FR is fixing.

**The flip is one line** — `useRemoteControl.ts`'s `storedUnorderedVideo()` default
— and the criterion for taking it: an airport-class session with the opt-in ON
showing `chunkStragglers` climbing while `chunkGaps` stays flat and fps holds
(stragglers rising with steady fps is the transport working; gaps rising is real
loss costing an IDR each — FR-18's lesson that a counter nothing reads is not
evidence is why both are already in the stats).

## Acceptance criteria

- [ ] **AC1** — On a link measured below 1 Mbps, `target_bps` converges **below** the old
      1.5 Mbps floor and within 2× of `goodput_bps`.
- [ ] **AC2** — `constrained` queue budget in bytes tracks the measured rate: on a 400 kbps
      pipe the byte gate engages (`frames_skipped_backpressure` climbs) instead of standing at 0.
- [ ] **AC3** — A session opening with an FR-35 seed ≥2× above the first measured window logs
      the abandonment and does not run at the seeded ceiling.
- [ ] **AC4** — Field: viewer paint age on the airport-class link falls from the measured
      2,284–7,095 ms band to **under 600 ms sustained**, with the before/after heartbeats recorded.
- [ ] **AC5** — No regression on a healthy relay (CORPLAP-3 from the office LAN) or a direct
      path: `target_bps`, age and `frames_skipped_backpressure` unchanged within noise.
- [ ] **AC6** — Every phase's kill switch restores the prior behaviour, verified by unit test.

## What building P1–P4 + P6 changed about the design

Four things the plan did not anticipate, all now load-bearing:

1. **P3 is inert without P1.** `AimdController::set_ceiling` raises any ceiling back up to
   `floor_bps`, so the arrival-rate clamp would have been silently undone at 1.5 Mbps — on
   exactly the links it exists for. The two ship together, and the coupling is asserted.

2. **The age *trend* became a queue-growth measurement instead.** The plan said "slope of
   frame age". What shipped is `Σ(Δarrival − Δwire)` — the difference between how fast
   frames were produced and how fast they landed. Same intent, but it needs no `frameAgeMs`
   at all, so it works when the clock probe never locks; and being a difference of two
   intervals, the offset cancels rather than being estimated and bounded.

3. **P1's evidence had to widen.** The goodput estimator needs the agent's own sends to
   BLOCK, and on this link they do not (`send_wait_max_ms` 0.1 ms) — the queue is
   downstream. The floor now takes the LOWER of the agent's estimate and the viewer's
   arrival rate; either alone leaves a real case uncovered.

4. **P4 could not discard anything.** The plan said "discard the queue via `send_epoch`".
   The agent-side queue on this path is 1–4 KB; the queue that matters is in the relay and
   the carrier, already sent and unrecallable. So P4 is a production PAUSE, and — because a
   pause loses no frames — it deliberately does NOT force a keyframe on resume, which at
   these rates would itself be seconds of transit. The kill switch is therefore
   `ROOMLERD_QUEUE_DRAIN`, not the planned `ROOMLERD_AGE_DRAIN`.

## Open decisions

- **Absolute hard minimum for P1.** 200 kbps is the working default. Below roughly this a
  1920×1200 frame cannot be legible at any QP and the honest answer is P5's resolution cap, not
  more bitrate shedding.
- **P3's transport for `received_bps`.** Piggy-backing `rc:decodestat` (as FR-15 did for age)
  keeps the cadence and costs no new message; the alternative is a dedicated report with its own
  interval. Leaning to the piggy-back.
- **P4's drain threshold and whether it is user-visible.** A deliberate 300 ms hitch that fixes
  a 2 s lag is a good trade, but it must not fire on a transient.

## Out of scope

- Corporate-VPN evasion or changing which carrier ICE picks (FR-33 territory).
- Server-side relay fan-out (named as future work in FR-44).
- Audio.

## Field-verification log

| date | version | host / path | result |
|---|---|---|---|
| 2026-09-01 | 0.4.45 | CORPLAP-3 → neo16, phone hotspot (Sofia airport) | **BEFORE**: goodput 65–395 kbps, `target_bps` 1.5–2.13 M climbing, age 597–7,095 ms, `bytes_inflight` 1–4 KB, `frames_dropped_backpressure=0`. The measurement this FR exists for. |
