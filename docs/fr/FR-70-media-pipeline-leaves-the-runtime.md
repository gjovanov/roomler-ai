# FR-70 — The media pipeline leaves the runtime

**Issue:** [#TBD](https://github.com/gjovanov/roomler-ai/issues) · **Status:** proposed 2026-09-04 ·
**Plan:** [`media-pipeline-architecture`](../plans/media-pipeline-architecture.md)

## Goal

Make the remote-desktop media path a **dedicated thread per session** that reads
an immutable plan and never awaits, so that the 34 rate/quality heuristics, 8
estimators and 11 kill switches in `encode/` become unnecessary rather than
better-tuned — and delete them as the acceptance criterion.

Operator's framing, which is the requirement: *"no sync processes
(thread-offloaded) for relay-based connections, start and on-going, and reduce
the patch-work to minimum."*

## Why now — five field findings, five different causes, one symptom

Measured 2026-09-04 across CORPLAP-1/-2/-3. Full detail and log excerpts in the
plan; the short form:

1. **The encoder open blocks every session's first frames** — `open_ms` 292–957 ms,
   every session, every host. The session has *no encoder at all* while it runs.
2. **Five regions between the loop top and capture were untimed** — `other_ms`
   157–782 ms, including passes with capture *and* encode both zero. Named in
   #1327.
3. **CORPLAP-3 is encode-bound** — single frames at 221–502 ms.
4. 🚨 **The worst excursion is TRANSPORT** — a 4903 ms paint with a 1485-byte send
   queue, 28 ms iterations and a 5.6–8.5 Mbps link: a DERP/TCP head-of-line
   block, invisible to every pipeline instrument.
5. 🚨🚨 **A stale prior pinned a healthy link at 1/30th of capacity, overrode an
   explicit `Native` resolution choice, and reported success** — `lc=170000`
   with `gp=None`, target pinned at the 200 kbps floor, resolution forced to
   1280×800, `age=66 ms`. 0.013 bits/pixel; the operator sees blurred text.

🔑 **One symptom, five causes.** That is how the patch-work accumulated: every
excursion arrives looking like a pipeline problem because `viewer_age_ms` fuses
sender, transit and viewer latency into one number.

## Key design

Three planes, one owner each, no synchronous coupling — see the plan for the
full statement.

- **Media plane** — one dedicated OS thread per session owning capture → scale →
  encode. Never awaits. ⚠️ A thread, **not** per-frame `spawn_blocking`: MF needs
  per-thread COM/`MFStartup` and QSV sessions are thread-affine, so an arbitrary
  pool thread can break hardware encode outright. 🔑 Capture already has exactly
  this shape and is the one stage that measured clean.
- **Transport plane** — the send task plus arrival telemetry that **splits**
  produced-too-much / transit-stalled / viewer-side. Finding 4 is unattributable
  without it.
- **Control plane** — signalling, stats reads, policy, one controller. It
  *publishes* a plan; it never reaches into the media loop.

**The invariant:** the media thread never makes a rate or geometry decision, it
reads a plan. Adoption is cheap because a replacement encoder is built on that
same thread while the current one keeps producing (make-before-break), which is
impossible today because the open is an await point in the frame path.

**Two rules the control plane owes**, both broken by finding 5: a prior may open
a session but never pin one; an explicit operator choice is never silently
overridden.

## Phases

`M0` measurement → `M1` media thread → `M2` make-before-break → `M3` plan handoff
→ `M4` one controller (FR-63 B1/B2) → `M5` the deletions (FR-62 A4 + FR-63 B3) ·
`T1` transport classification · `P1` priors decay + visible overrides.

⚠️ **P1 first in value**: it is what the operator can see today and needs no
threading work. ⚠️ **M5 is the acceptance criterion**, not an afterthought — if
the deletions do not happen this is one more lever and the complaint stands.

## Acceptance criteria

- [ ] **AC1** — capture/scale/encode run on a dedicated thread; the async runtime
      shows no media-path blocking under a canary that records its own lateness.
- [ ] **AC2** — `open_ms` disappears from the frame path: first-frame latency
      drops by the measured open (0.29–0.96 s), before/after on the same host.
- [ ] **AC3** — no rate or geometry decision remains in the media loop.
- [ ] **AC4** — heuristics 34 → ≤ 10, kill switches 11 → ≤ 4, estimators 8 → 1,
      each retirement gated on a counter measured fleet-zero.
- [ ] **AC5** — a transit stall is classified as such and does **not** cut the
      rate; a repeat of finding 4 is attributable without reading source.
- [ ] **AC6** — an unmeasured prior cannot hold a session at the floor, and an
      overridden `user_target` is visible to the operator.
- [ ] **AC7** — every phase carries a before/after from the same instrument, and
      each field test is shown to FAIL on the current deploy first.

## Open decisions

- Whether the media thread owns the scaler too, or scale stays with capture.
- Whether `PipeState` classification lives agent-side or needs a viewer-side
  report change (finding 4 needs arrival data the agent does not have today).
- Whether P1's decay replaces the FR-35 learner or bounds it.

## Out of scope

The encoder apply path itself (FR-62 — measured dead for QSV, and M2 makes it
irrelevant rather than fixing it); ICE path selection (FR-64).

## Related

FR-62 #1242, FR-63 #1243, FR-64 #1244, FR-65 #1255, FR-59 #1163, FR-1 #767.
