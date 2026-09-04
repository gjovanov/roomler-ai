# FR-70 — The media pipeline leaves the runtime

**Issue:** [#1330](https://github.com/gjovanov/roomler-ai/issues/1330) · **Status:** proposed 2026-09-04 ·
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
5. 🚨🚨 **A remembered rate held a session at the 200 kbps floor for four
   minutes, overrode an explicit `Native` resolution choice, and reported
   success** — session `6a9abc30`, CORPLAP-1 → neo16 over the overlay pair
   (host↔host on the mesh, DERP underneath the corp VPN): `slow_link_floor_bps=
   Some(200000)` and `goodput_bps=None` in every window, zero send stalls, zero
   viewer-congested windows, paint age 55–108 ms, the target sawing
   `200k → 225k → 253k → 285k → 200k` on repeat, resolution forced to 1280×800
   at 15 fps. 0.013 bits/pixel; the operator sees blurred text. ⚠️ The plan's
   first reading of this line — `lc=170000`, a learned *ceiling* below the
   floor — was **wrong**: the FR-35 learner can only ever LIFT a ceiling above
   the plan's and was inert. The pin is the rate MEMORY, entering through
   three doors, the third of which seals the other two — see "P1 — as built".

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
threading work. **P1 landed 2026-09-04 in #1333** (kill switch `rate_prior_decay`) — see
"P1 — as built" below. ⚠️ **M5 is the acceptance criterion**, not an
afterthought — if the deletions do not happen this is one more lever and the
complaint stands.

**M0's last item — the age split — landed 2026-09-04.** The viewer's decode
workers now stamp every frame's ARRIVAL (last chunk in the worker) beside its
paint, and `rc:decodestat` carries the window's arrival age as an optional
`arr_ms` beside `age_ms` (same clock mapping, so it rides only alongside it;
floored at 1 because 0 is the agent's absent sentinel). The agent packs it into
the spare `u16` of the age word (`viewer_rate::pack_age_with_arrival`) and the
heartbeat prints one field:

```
age_split=Some(AgeSplit { sender_ms: Some(12.3), transit_ms: 4878, viewer_ms: 13 })
```

`viewer_ms` = paint − arrival (decode queue + decode + paint, inside the
browser); `sender_ms` = this window's send-queue wait (`send_wait_avg_ms`,
enqueue → wire-complete — `None` on the VP9-444 pump, which keeps no such
figure, so its `transit_ms` is an upper bound); `transit_ms` = arrival − sender,
everything between the wire and the worker, the relay included. Finding 4 reads
as `transit_ms ≈ 4.9 s` with `viewer_ms` and `sender_ms` in the tens of ms —
attributable without reading source, which was M0's gate. `None` from a pre-M0
viewer, or a window with no age report. ⚠️ Telemetry only: no loop reads it
yet — acting on it (a transit stall must not cut the rate) is T1, and the diag
HUD does not render the two new `HopWindow`s yet.

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
      *The attribution half is instrumented (M0's `age_split`, 2026-09-04);
      the "does not cut the rate" half is T1 and untouched.*
- [x] **AC6 (rate half)** — an unmeasured prior cannot hold a session at the
      floor. **Field-verified 2026-09-04 on 0.4.64** on the pair that failed:
      the prior decayed to `None` at 106 s and the target reached 3.9 Mbps with
      nothing measuring the pipe; the same build with `rate_prior_decay=false`
      reproduced the 200–285 kbps pin for three minutes. See the log.
- [ ] **AC6 (visible half)** — an overridden `user_target` is visible to the
      operator. On the wire since 0.4.64 (`cap_reason`/`cap_detail`); the
      on-screen label waits for the next web deploy.
- [ ] **AC7** — every phase carries a before/after from the same instrument, and
      each field test is shown to FAIL on the current deploy first.

## P1 — as built (2026-09-04, PR #1333)

### The mechanism, corrected

The rate memory (`rate_memory.json`, keyed on the nominated pair's REMOTE
address) held **200 kbps** for neo16's overlay address `100.65.4.2`, written
2026-09-03 07:43 UTC. The same laptop's sessions through the public relays the
same day were remembered at 2.5–5.3 Mbps. That one number entered the session
through three doors:

| door | what it did | code |
|---|---|---|
| the opener | the AIMD opened at 200 kbps | FR-59 P8, `open_seed_bps` |
| the floor relief | with nothing measured the seed **stood in** for a measurement, so the legibility floor was relieved to 200 kbps — which is also where the multiplicative decrease bottoms out | FR-59 P1, `measured_bps = g.or(rx).or(open_seed_bps)` |
| the queue budget | the FR-59 P2 byte budget is denominated in the measured pipe, i.e. in the seed: 450 ms × 200 kbps ⇒ the 16 KB minimum | `constrained_queue_reference_bps` |

The third door sealed the other two. Every drag frame over 16 KB tripped the
gate; every trip was an AIMD decrease (bottoming at the 200 kbps floor) that also
blocked the additive increase for 5 s; and because the gate never let a queue
form, the agent's sends never blocked (`goodput_bps=None`) and the viewer's queue
never grew (`link_stats=(0, …)`), so **nothing could ever measure the pipe and
contradict the memory**. The pipe's real rate is unknown to this day; the
session never once asked it.

🔑 **And the memory reproduces itself.** `record_session` took the LAST window's
applied rate whenever the session had seen a decrease — on a lumpy relay that is
wherever the last decrease left it, biased low by the ×0.85-at-once versus
+12.5 %-per-5 s asymmetry — so the memory drifted DOWN across sessions and
`slow_link_min_bitrate` (200 kbps) was an **attractor**, not a stale day.
The FR-59 P5 profile (1280×800 @ 15 fps) is resolved once at pump start from
the same memory, so it came along every time.

### What P1 changes

1. **`encode::prior::RatePrior`** — the remembered rate is a *prior*. While no
   live measurement exists, the value standing in for one climbs **×1.25 per 10
   clean windows** toward the nominal band (the AIMD's own slow-band slope);
   two consecutive pushed-back windows (a stall, an age excess, viewer queue
   growth, a drain — *never* a byte-budget skip, which is the pump's own
   throttle) walk it one step DOWN; a live measurement (blocked-send goodput
   or the viewer's arrival rate while its queue grows) becomes the new base at
   once and decays from there at a gentler **×1.1**; at the band the prior is
   simply *gone* and the session is byte-for-byte the unremembered one.
   ⚠️ The down-step is not optional: a floor 5–10 % above the pipe grows a
   queue too slowly for either measurement to latch, and the AIMD cannot
   decrease below the floor — the age LEVEL is the only sensor that sees it.
2. It is read **exactly where the seed stood in** (`measured_pipe_bps`,
   `pre_encode_tick`), so the floor relief and the queue budget follow it up.
   The opener is untouched — a prior may open a session.
3. **The write-back records what the session knows**: a live measurement, else
   the prior as it has decayed, else (as before) the applied rate. A
   misremembered fast pair records ≥ the band after one session and stops
   re-seeding slow; a genuinely slow pair records roughly the pipe.
4. **After the FR-59 P3 clamp releases, the floor stays at the last MEASURED
   rate** and decays from there, instead of snapping back to the 1.5 M
   constant — that snap forced the target 4× over a pipe just measured and
   re-created the queue the release had waited for (the three FR-59 release
   tests now assert this).
5. **Attribution** — `RungReason::SlowLinkCap` (`slow-link-cap`). The profile's
   cap rode the Priority dial's slot and logged as `priority-cap`; the viewer,
   told `relay-limited`, advised *Priority → Sharper*, which lifts a dial cap
   and does nothing against the profile.
6. **Visible override** — `rc:video-info` gains `cap_reason` + `cap_detail`
   (trailing optional keys, present only while the effective target differs
   from the operator's, re-sent whenever the plan changes). The resolution pill
   reads `1280×800 · slow link (remembered 200 kbps) · native 1920×1200`, and
   the Resolution setting says what caps the session and what lifts it.
7. Heartbeat `prior_bps` (read it against `goodput_bps` and
   `slow_link_floor_bps`); kill switch **`rate_prior_decay`** (default on;
   off = FR-59 P8 verbatim).

### What P1 deliberately does NOT change

- The FR-59 P5 resolution profile stays engaged for the session. A mid-session
  rung flip is the 865 ms blocking QSV rebuild P5 exists to avoid; it lifts on
  the next session once the memory no longer says slow, and **M2 is what makes
  it cheap mid-session**. P1 makes it *visible*.
- The FR-35 learner — untouched. It only ever lifts a ceiling and was never the
  pin (the open decision below is answered: P1 *bounds* the memory's use as a
  stand-in measurement; it does not replace the learner).
- The climb speed below the band (+12.5 % per 5 s, FR-59 P8) — FR-63's job.

### Simulation (B0, `encode::sim`, `cargo test -p roomlerd --lib p1_report -- --ignored --nocapture`)

| cell | `rate_prior_decay` | peak target | max age | memory at end |
|---|---|---|---|---|
| remembered 200 k, pipe **20 Mbps** (the field cell; pipe rate unknown, modelled fast) | off | 895 k — **never the band** in 180 s | 73 ms | the applied rate (re-seeds slow) |
| same | on | **2.55 M** (the relay ceiling) by 140 s | 117 ms | none below the band |
| remembered 200 k, pipe **300 kbps** (the pair the memory was right about) | off | 256 k | 753 ms | the applied rate |
| same | on | 260 k | 754 ms | 202 k |

⚠️ Three fidelity findings, recorded because they change what B0 can claim:
B0's `MeasureRule::EveryWindow` feeds the floor relief the delivered rate every
window — the shipped governor measures only on **push-back** — so B0 as shipped
*cannot* reproduce this pin and its "fast pair misremembered slow" fixture
passed for a reason that does not exist in production (FR-63 should re-run its
fixtures under `OnPushBack`); a byte-budget skip is a congestion sample but
**not** a blocked send (the first run of this cell "measured" a 20 Mbps pipe from
a gate skip); and the fast cell's hover point is model geometry (the burst size
at which a frame is still in flight when the next is due), not the field's
200–285 k sawtooth — the claim is only that the budget stays denominated in the
memory.

### Field-verification log

| when | build | cell | result |
|---|---|---|---|
| 2026-09-04 12:40 UTC | 0.4.61 | CORPLAP-1 → neo16, overlay pair (`100.65.4.28 ↔ 100.65.4.2`, `relay=false`, constrained), seed 200 k, `hevc_qsv` | **The FAIL** (AC7's baseline): 4 min at `target 200–285 k`, `slow_link_floor_bps=Some(200000)`, `goodput_bps=None`, `send_stalls=0`, `link_stats=(0,…)`, age 55–108 ms, 69 budget skips, `1280×800@15`, pill `relay-limited`. Memory unchanged at 200 k. |
| 2026-09-04 18:40 UTC | **0.4.64** (`rate_prior_decay` default on) | **the same cell**: `6a9b10b5`, the primary-org overlay pair (`100.65.4.28:61310 ↔ 100.65.4.2:59196`, `relay=false`, constrained), seed 200 k from the real memory, `hevc_qsv`, the profile engaged (`1280×800@15`), log `reason="slow-link-cap"` | **PASS.** `prior_bps` `Some(200000)` → 250 000 (18:41:06) → 312 500 → 390 625 → 488 281 → 610 351 → 762 938 → 953 672 → 1 192 090 → 1 490 112 → **`None` at 18:42:39 (106 s)**; `slow_link_floor_bps` followed at 0.85× and let go with it; the target rode the floor and its own AI (200 k → 648 k at 70 s → 1.5 M → 3.0 M at 130 s → 3.9 M at 180 s, the FR-35 learner lifting the ceiling to 4.6 M) — with `goodput_bps=None`, `send_stalls=0` and `link_stats=(0,…)` in **every** window, i.e. under exactly the conditions that pinned `6a9abc30` for four minutes. Age 58–115 ms throughout. Pill at the end: `5.7 Mbps · 16 fps · 1280×800`. Write-back: `peer="100.65.4.2" stable_bps=6083280 kept_bps=6083280` — the pair is freed (200 k → 6.08 M). Repeated on the jovanov-org pair (`6a9b11c9`, `100.65.0.6 ↔ 100.65.0.5`, hand-seeded to 200 k): `None` at 114 s, 3.45 M at 155 s. |
| 2026-09-04 18:51 UTC | **0.4.64 with `rate_prior_decay=false`** (the same-build FAIL control, one flag) | `6a9b1333`, the **same** primary-org overlay pair, memory hand-seeded back to 200 k | **FAIL, as it must**: three minutes of `prior_bps=Some(200000)` and `slow_link_floor_bps=Some(200000)`, the target sawing `200k → 225k → 253k → 285k → 200k` under budget skips (66 in 3 min), `goodput_bps=None`, zero stalls, zero congested windows, age 67–122 ms — the 12:40 session, reproduced on demand. Write-back: `stable_bps=253125` — **the attractor caught in the act** (the switch-on arm had just written 6 083 280 for the same pair). |
| 2026-09-04 | 0.4.64, web bundle **not yet deployed** | the viewer half | **PENDING**: the agent sends `cap_reason="slow-link-cap"` (the log line proves the attribution), but roomler.ai still serves the pre-P1 bundle, so the pill read `1280×800 · relay-limited (native 1920×1200)` throughout. Verify the label and the Resolution-setting hint after the next web deploy (master also carries FR-69 P0–P5c, which has its own prod-rollout gate — not this FR's call). |
| 2026-09-04 | 0.4.64 | the resolution half | as designed: the FR-59 P5 cap held `1280×800` for the whole session while the rate climbed to 5.7 Mbps — the cap lifts on the NEXT session (memory now 6.08 M ⇒ no profile). Making it lift mid-session is M2's job. |

⚠️ Two things the run taught that the plan did not know: ICE nominates a
**different pair on every reconnect** here (the two overlay host pairs and two
public relays, in five sessions), so a cell keyed on one pair's memory is not
reproducible on demand without seeding every constrained key; and a session
started six seconds after the update task fired ran on the OLD binary until the
installer restarted the service — read the version at the session, not at the
task.

**AC6 status**: the rate half is field-verified (above); the visible-override
half is verified on the wire (log + unit tests) and pending the web deploy for
the on-screen label.

## Open decisions

- Whether the media thread owns the scaler too, or scale stays with capture.
- Whether `PipeState` classification lives agent-side or needs a viewer-side
  report change (finding 4 needs arrival data the agent does not have today).
- ~~Whether P1's decay replaces the FR-35 learner or bounds it.~~ **Answered by
  P1: bounds.** The learner only ever lifts a ceiling and was never the pin; the
  pin was the memory's use as a stand-in measurement, and that is what decays.

## Out of scope

The encoder apply path itself (FR-62 — measured dead for QSV, and M2 makes it
irrelevant rather than fixing it); ICE path selection (FR-64).

## Related

FR-62 #1242, FR-63 #1243, FR-64 #1244, FR-65 #1255, FR-59 #1163, FR-1 #767.
