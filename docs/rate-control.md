# Rate control — how the remote-desktop stream spends its bits

How a session decides **bitrate, quality, frame rate, and resolution**, and why
(since rc.445) it never changes resolution mid-motion. Companion to
[encoders.md](encoders.md) (which encoder runs) — this doc is about what that
encoder is told to do. History and field evidence at the bottom. Since
`agent-v0.4.93` every loop here reads **one validity gate** before it acts,
so that is the first section: everything after it consumes its verdict.

## The validity gate (FR-79) — a window is evidence about the pipe, or it is not

Every loop in this document estimates the same quantity — what the path will
carry — from the same 1–2 s viewer window. Until `agent-v0.4.93` each loop
decided for itself whether a window could be trusted, and on 2026-09-08 one
host produced three field events through three different inputs with one
shape: a number taken while the pipe was *not free to be measured* was read as
the pipe (FR-71 T2 and T2b, then the opener — the record is in
[`fr/FR-79-one-validity-gate.md`](fr/FR-79-one-validity-gate.md)). FR-79
replaced the three per-input rules, their two kill switches and their four
counters with **one verdict per window, taken before any loop acts**:

> A window is evidence about the pipe only if **(1)** the agent's own loop was
> free — no pump pass overran its stall budget inside it, **(2)** the transport
> was past its opening, **(3)** the transport did not stall in it, and did not
> stall in the window before it, and **(4)** the carrier it will be attributed
> to is the carrier that produced it.

`encode::evidence::rejected(WindowFacts) -> Option<Reason>`
(`agents/roomlerd/src/encode/evidence.rs:194`) is pure — no clock, no I/O, no
configuration and **no switch**. Its inputs (`WindowFacts`, `evidence.rs:172`)
are things the governor already holds at the window boundary: this window's
pipe-state verdict, the previous window's, whether a pump pass stalled inside
it (`encode::stall`, counted per pass), whether the transport was still in its
opening, and whether the carrier moved.

**FR-119 P2b-8 — every window, and the whole window.** Three corrections, all
found when a freshly granted 120 fps was taken back one second into two of
four direct sessions on the MacBook:

- **The gate runs on every window.** It used to run on constrained windows
  only, and a direct window was evidence by definition, so FR-119's grant
  (which only runs on a direct path) read capacity claims nothing had gated.
  On a direct window the classifier does not run, so its state is `Unknown`
  with no predecessor, and only the agent's own stall and the opening apply
  (`governor.rs:1429`).
- **`Reason::Opening`.** The opening keyframe meeting a transport that is
  still ramping blocked sends for 43–82 ms, and the drain rate folded as a
  capacity of 14–22 Mbps against a 104 Mbps ceiling, which held for its 60 s.
  A window any pass of which falls inside FR-35's opener window (at least 2 s
  from the first keyframe and until the send ledger drains, at most 3 s, now
  kept for every session) is not evidence. FR-35 already declined to cut for
  that burst, so it is not congestion either.
- **The window is the sum of its passes.** The pump hands its sender facts
  over on every pass, as deltas since the previous one, and each call
  overwrote the last, so the "window" the classifier and the gate read was one
  pass long: a stalled pass anywhere but in the window's last iteration never
  reached `AgentStalled`, and `frames_sent` / `gate_skips` read 0–2 where the
  window held 30–120. `WindowSenderStats::absorb` now folds the passes: counts
  add, the longest wait wins, the average is weighted by frames sent, `opening`
  ORs, and the levels are the latest. ⚠️ On constrained sessions this also
  corrects the classifier's inputs: a gate skip anywhere in a window now makes
  it `overproduced`, and the silent-viewer gap rule sees real frame counts.

**FR-119 P2b-13 — a keyframe's ramp, later in the session.** The opening's
defect was not confined to the opening. SCTP grows its congestion window only
while data is queued (`on_cumulative_tsn_ack_point_advanced` in the vendored
`webrtc-sctp`). So on a fast path the deltas leave it about one delta's size,
and a keyframe many times larger spends its send growing the window back. On
the first Windows run, one ~625 KB keyframe on a mesh path blocked 71.6 ms and
folded as 69.9 Mbit/s of capacity, while the path carried 311 Mbit/s over TCP.
The measured-ceiling clamp cut the rate to 59 M, and FR-119's grant handed a
granted 120 fps back on `pipe`. Each blocked send now says whether it was a
keyframe (`BlockedSend::keyframe`, read by the send task from the frame's own
header), and on a **direct** window whose blocked sends include a keyframe the
gate answers `Reason::KeyframeBurst`. Constrained windows keep the old rule:
there a keyframe's drain is dominated by the path, and it may be the only
sample a session gets.

**P2b-14 widened it from "only keyframes" to "a keyframe at all".** P2b-13
still read a window when a delta blocked beside the keyframe, on the theory
that a narrow pipe shows in the deltas behind it. But at 120 fps eight deltas
queue behind a 66 ms keyframe send, and they block on the keyframe's own
backlog, so they measure the same ramp. On the second Windows run the
encoder's idle-settle keyframe after a motion burst did exactly that: the
window folded as 60.7 Mbit/s against the 88.8 Mbit/s ceiling at 120, on a LAN
path, and 120 went back after 6 s. A pipe really narrower than the stream
still shows: its deltas block in the keyframe-free windows either side, which
are read as before.

```mermaid
flowchart TB
    W["viewer window closes<br/>governor.rs:1325 — every window;<br/>the classifier on constrained ones only"] --> A{"a pump pass overran<br/>its stall budget?"}
    A -- yes --> RA["Reason::AgentStalled"]
    A -- no --> O{"inside the transport's<br/>opening?"}
    O -- yes --> RO["Reason::Opening"]
    O -- no --> K{"direct, and a blocked<br/>keyframe in the window?"}
    K -- yes --> RK["Reason::KeyframeBurst"]
    K -- no --> T{"classifier says<br/>transit-stalled?"}
    T -- yes --> RT["Reason::TransitStalled"]
    T -- no --> S{"the previous window<br/>was transit-stalled?"}
    S -- yes --> RS["Reason::StallShadow"]
    S -- no --> C{"carrier changed<br/>under the session?"}
    C -- yes --> RC["Reason::CarrierChanged"]
    C -- no --> OK["None — this window<br/>MEASURES the pipe"]
    RA & RO & RK & RT & RS & RC --> REJ["evidence_rejected[reason] += 1<br/>the window's numbers are DROPPED, not held"]
    RT --> V5{"V5b: desired ><br/>1.2 × measured_bps?"}
    V5 -- yes --> OURS["stall_is_ours — the rate's doing:<br/>×0.85 rate-limited · ramp ENDS ·<br/>age streak kept"]
    V5 -- "no, or nothing measured" --> HOLD["the transport's doing<br/>(FR-71 finding 4): nothing cuts"]
```

The heartbeat carries the tally as `evidence_rejected=[agent-stalled,
transit-stalled, stall-shadow, carrier-changed, opening, keyframe-burst]` (`Rejections`,
`evidence.rs:219`; printed at `peer.rs:10207` for the FFmpeg pump and `:5357` for
VP9's) — one field where T1b, T2 and
T2b each had their own counter. `opening` was appended (P2b-8) and then
`keyframe-burst` (P2b-13), so the earlier positions keep their meaning for a
reader that parses them.

### The classifier the gate reads (FR-71 T1a) — which plane is the limiter

Every loop below used to read one number, the viewer's paint age, and every one
of them read an excursion as "the encoder produced too much". On 2026-09-04
(CORPLAP-3 over DERP, session `6a9abaa8`) a head-of-line block held frames in
transit for **4.9 s** while the sender's queue held **1,485 bytes**, the worst
pump pass was 28 ms and the encoder averaged 14 ms — nothing sender-side was
wrong, and the AIMD and the FR-15 age loop cut the rate into an 8 Mbps link
(FR-71's finding 4, the FAIL on record in
[`fr/FR-71-transport-stall-classification.md`](fr/FR-71-transport-stall-classification.md)).
FR-70 M0 split the age into the three planes that can own it
(`viewer_rate::split_age`, `viewer_rate.rs:314`: sender wait, transit, the
browser's own share), and FR-71 T1a turns the split plus what the governor
already knows about its own send queue into **one verdict per constrained
viewer window** — `encode::pipe_state::PipeClassifier::classify`
(`pipe_state.rs:159`). The gate reads that verdict, and the previous window's,
as two of its facts (`WindowFacts::state` / `prev_state`, `evidence.rs:172`);
the classifier itself acts on nothing. Pure, like the gate: no clock, no I/O,
no switch — it unit-tests on the default build and runs inside the B0 simulator.

```mermaid
flowchart TB
    W["a constrained viewer window closes<br/>governor.rs:1355 — WindowSignals from the<br/>summed pass stats and the viewer's reports"] --> O{"the sender pushing back?<br/>inflight ≥ budget · a byte-gate skip ·<br/>a blocked send · a wait ≥ 100 ms"}
    O -- yes --> OP["Overproduced<br/>the sender is the limiter"]
    O -- no --> SP{"an age split<br/>this window?"}
    SP -- "no report, frames sent,<br/>and the viewer has reported before" --> GAP["TransitStalled (gap)<br/>pipe_gap_stalls += 1"]
    SP -- "no split, otherwise" --> UNK["Unknown<br/>a pre-M0 viewer · the first report<br/>still on its way · an idle sender"]
    SP -- yes --> FL["learn the floors — the smallest transit<br/>and viewer share seen (a stall never lowers them)"]
    FL --> X{"transit ≥ floor + 200 ms?<br/>viewer ≥ floor + 100 ms, or struggling?"}
    X -- "transit only, or both<br/>and transit grew more" --> TS["TransitStalled (split)<br/>the path is the limiter"]
    X -- "viewer only, or both<br/>and viewer grew more" --> VL["ViewerLate<br/>the browser is the limiter"]
    X -- neither --> CL["Clear"]
    OP & GAP & UNK & TS & VL & CL --> HB["pipe_state · pipe_states[5] · pipe_gap_stalls<br/>and the gate's facts: state + prev_state (evidence.rs:172)"]
```

| verdict | says | the rule (`decide`, `pipe_state.rs:169`) | what the gate makes of it |
|---|---|---|---|
| `Overproduced` | the sender is the limiter | checked **first**: `inflight_bytes ≥ budget_bytes`, or a byte-budget skip, or a blocked send the goodput sink accepted, or `send_wait_max_ms ≥ SENDER_WAIT_MS` (100 ms, `:67`) — whatever the path is doing, a full queue is real back-pressure, the one case the loops were always right about | evidence: the window measures the pipe |
| `TransitStalled` | the path is the limiter | the transit share sits `TRANSIT_SLACK_MS` (200 ms, `:63`) or more over its learned floor while the browser's share is under its own slack (both over: the larger excess names the limiter); **or a report gap** — no report this window, frames were sent, and the viewer has reported at least once this session (`:193`) | rejected (`Reason::TransitStalled`), and the window after it too (`StallShadow`); under V5 it is also the transport pushing back — the rate-limited ×0.85 fires only while `desired > 1.2 × measured_bps` |
| `ViewerLate` | the browser is the limiter | the viewer share sits `VIEWER_SLACK_MS` (100 ms, `:65`) or more over its floor, or the viewer reports `struggling` (`rc:decodestat`) | evidence (an open decision in FR-79); the viewer-rate cap sheds frames, nothing cuts the bitrate |
| `Clear` | none of the above | | evidence |
| `Unknown` | nothing to classify | no split and no gap: a pre-M0 viewer, a report without an age, silence **before** the first report, or a window the sender was idle in — and every direct window, because the classifier runs on constrained windows only (`governor.rs:1390`) | evidence; the loops behave as they did before FR-71 |

Its inputs are `WindowSignals` (`pipe_state.rs:112`), assembled once at the
window fold (`governor.rs:1378`):

| input | where it comes from | note |
|---|---|---|
| `split` — sender / transit / viewer ms | `split_age` over the window's average age, the viewer's arrival stamp and the window's average send wait (`governor.rs:1362`) | `None` = no split this window |
| `reported` | any report this window — an age, a link report, or the raw count (`governor.rs:1380`) | the gap rule's subject |
| `inflight_bytes`, `budget_bytes`, `gate_skips`, `send_wait_max_ms`, `frames_sent` | `WindowSenderStats` (`governor.rs:399`), handed over by the pump on every pass and **summed per window** since FR-119 P2b-8 (`absorb`, `:430`) | ⚠️ until P2b-8 (2026-10-23) the governor kept only the last pass, so `frames_sent` and `gate_skips` read 0–2 where a window held 30–120, and a gate skip outside the final pass never made the window `overproduced`; every test fed whole windows, so the tests describe the classifier the field runs from then on |
| `blocked_sends` | the goodput sink's samples, drained at the fold | the same samples the fold then accepts or drops |
| `struggling` | the viewer's decode-backlog bit | |

The floors are learned the way the FR-15 age loop learns its floor — the
smallest share seen (`pipe_state.rs:202`) — so a permanently slow path is not
a permanent stall, and a stall never lowers the floor it is measured against.

**The heartbeat.** `pipe_state` is the last window's verdict, `pipe_states` the
session's windows per verdict as `[unknown, clear, overproduced,
transit-stalled, viewer-late]`, and `pipe_gap_stalls` the subset of the
transit-stalled count that came from the gap rule (`peer.rs:10204`–`:10214` for
the FFmpeg pump; `:5233`–`:5244` for VP9's, whose transit share is an upper
bound because that pump keeps no send-wait figure). Three rules for reading them:

- a heartbeat spans about two windows, so `pipe_state` and the histogram are
  **offset by one window** when read side by side — count stalls from the
  histogram's deltas, not from the current verdict (measured 2026-09-07);
- the counters are per session: the governor is built with the session, so a
  new `session_id` restarts them at zero;
- on `0.4.67` every session showed one `transit-stalled` window at its start —
  the viewer's first report had not arrived yet. The "has reported once" clause
  (#1370) fixed it from `0.4.68`; nothing needs subtracting after that.

**What the field taught**, with the record in FR-71's spec:

- Finding 4 has **two shapes** on the corporate hosts, and the gate answers each
  with a different fact. The post-ack kind — an age in the hundreds of
  milliseconds to seconds with 0 B in flight and a 0.03 ms send wait (CORPLAP-2,
  2026-09-07 and 09-08 08:48) — is the split rule's `TransitStalled`. The
  blocked-send kind — a DC send blocked for 2.9 s and 7.4 s inside pump passes
  the runtime had stalled (CORPLAP-1 on the Check Point VPN, 2026-09-08 12:15)
  — is `AgentStalled`, the gate's first reason, whatever the classifier said of
  the window. The B0 freeze fixtures model a third: a frozen drain that fills
  the **sender's** queue, which the classifier rightly calls `Overproduced`
  (`t1a_freeze_stalls_are_sender_visible`, `sim.rs:2326`); the simulator's
  `transit_stalls` (acked on time, observed late) and the `finding4_transit_stall`
  cell (`sim.rs:1716`) model the first.
- **96 % of `transit-stalled` verdicts are report gaps**: 209 of 218 over four
  days of the operator's sessions (2026-09-05 → 09-08, about 15,000 constrained
  windows on `0.4.69` → `0.4.85`), one silent window every few minutes on a
  healthy relay, and nothing was ever classified stalled while its queue was
  over budget. A gap is still a rejected window; whether a hidden tab's silence
  should count as a stall is an open decision in FR-71.
- The cell that keeps the rate is `finding_4_keeps_the_rate_with_the_validity_gate`
  (`sim.rs:2452`): no window below the pre-stall target and the path reading
  clear within the stall's own length. Its FAIL arm,
  `finding_4_cuts_the_rate_without_the_validity_gate` (`:2284`), keeps the
  defect on record — the gate has no daemon switch, so the arm lives only in
  the simulator — and the thin pipe, the LAN burst and a genuinely slow relay
  trace **byte-identical** with the gate on and off
  (`t1b_hold_is_inert_where_nothing_stalls`, `:2355`).

⚠️ The classifier was a shadow for one release: `transit_classify` (T1a) and
the hold behind `transit_hold` (T1b, default off) shipped in `0.4.67`, and
FR-79 V1 **deleted** both the day the gate was built on the verdict. A
`transit_hold = true` left in a host's `config.toml` is an inert key.

### What each consumer does with the verdict

All of it is in one function, `RateGovernor`'s window fold (`governor.rs:1348`
onward). `valid` is the gate's `None`; `stall_is_ours` is V5's exception, below.

| consumer | on a valid window | on a rejected window | site |
|---|---|---|---|
| goodput fold (`GoodputEstimator::observe_window`) | folds the window's blocked sends | samples **dropped**, not quarantined; the next valid window is measured on its own merits | `governor.rs:1511` |
| FR-35 hard ×0.5 (a send blocked ≥ `HARD_STALL` = 1 s, `ceiling_learn.rs:71`) | `apply_hard_md` | nothing — only the window boundary knows whether the block was the pipe (12:15: 2.9 s and 7.4 s blocked sends inside stalled passes, on a path that carried 6.6 M twenty seconds later) | `governor.rs:1528` |
| FR-63 opener ramp | doubles on a clean window, ends on congestion | neither — unless `stall_is_ours`, which ENDS it | `governor.rs:1534` |
| FR-15 age loop | fires on a streak over the learned floor; always learns the floor | learns, does not fire, and the streak resets — unless `stall_is_ours`, which keeps both | `governor.rs:1574` |
| FR-59 P3 arrival-rate clamp | armed by a growing queue the sender offered (V5b rule (b)), released by the viewer's report | **neither armed nor released** — whatever it held, it still holds (14:52: the report *for* the stall set 834,800 on a path that had just carried 7.45 M) | `governor.rs:1617` |
| the rate-limited ×0.85 (`aimd.rs:180`, ≥ 500 ms apart, `aimd.rs:84`) | on `age_over` or `link_over` | on `stall_is_ours` | `governor.rs:1728` |
| FR-70 P1 prior | re-anchors on a live measurement, decays on a clean window | does not move. V5b: a stall carries no number, so it no longer walks it either | `governor.rs:1744` |
| the refutation (V5b rule q2) | a delivery that raised the floor takes out a reading it beats by 2× | nothing: a stall's shadow is its backlog draining | `governor.rs:1692` |
| FR-79 V3b belief (shadow) | a delivery every window; a capacity when the fold accepted, or the viewer's queue grew | the delivery is still recorded; no capacity | `governor.rs:1448`, `:1514`, `:1646` |
| pair-memory write-back (V2 + V4 + V5b) | a carried hold, else a reading, else the opener's measurement | nothing measured ⇒ nothing written | `media_pair_memory.rs:238` |

### V5 — a rejected window is not a measurement, but a stall is still push-back

V1 shipped the gate as an unconditional DROP, and on `agent-v0.4.95`
(2026-09-09 07:41, CORPLAP-2, AV1 over `relay:derp/tcp`) that was a live
regression: one accepted goodput sample of 1,058,145 all session, eleven
transit-stalled windows, `target_bps` 1.6–2.1× the measurement, paint age
240 → 4,022 → 7,784 ms, and **the rate ended higher than it started** —
because every loop that could lower it read `valid`, the local send queue
never filled (`bytes_inflight=0`), and the viewer's age report landed in
rejected windows.

🔑 The gate answers *is this window a measurement of the pipe?* V1 let every
consumer read it as *does this window say anything at all?* Those differ for
exactly one reason: a stalled transport is not a measurement, but it **is the
transport pushing back** — and on a relay-TCP session, where the queue sits
downstream of the agent, it is the only push-back there is.

| half | rule | anchor |
|---|---|---|
| which rejection also speaks about the rate | `Reason::is_congestion()` is true **only** for `TransitStalled`. `AgentStalled` would re-create the 12:15 defect, `StallShadow` double-counts one stall, `CarrierChanged` cuts for a path the session has left, `Opening` cuts for the opener draining into a transport still ramping | `evidence.rs:143` |
| whose fault the stall is | `stall_is_ours` = congestion **and** `desired > measured_bps × 1.2`. Above the measured pipe the stall is the rate's doing; at or below it, the transport's (FR-71's finding 4: an 8 Mbps leg head-of-line-blocked for 4.9 s with 1485 bytes queued, where a cut costs quality for an event the sender did not cause); **with nothing measured there is no claim to contradict, so no cut** | `governor.rs:1494` |
| what it reaches | only the rate-limited ×0.85, the ramp's end and the age streak. **Never `apply_hard_md`** — it has no spacing and would halve once per stalled window. Everything that sets a *number* still reads `valid` | `governor.rs:1534`, `:1574`, `:1728` |
| which estimate | since V5b, `measured_bps` (`governor.rs:765`: blocked-send goodput, the viewer's arrival rate inside its 15 s TTL, the lower of the two) and never the prior; before it, `pipe_bps` (`:716`), and **never** `blocked_send_bps` (`:682`). `0.4.96` shipped on the wrong accessor: the agent's sends block only when the queue is LOCAL, and CORPLAP-3 read `goodput_bps=None` with `goodput_samples=(0, 5)` for a whole relay session — V5 inert on that host. Fixed in **V5a, `agent-v0.4.97`** | `governor.rs:1472`–`1216` |

⚠️ The control that must stay green is
`finding_4_keeps_the_rate_with_the_validity_gate`. Its field twin is every
relay stall on a session that is *not* overdriving: on 2026-09-24 a
three-hour CORPLAP-2 session over `relay:derp/tcp` held its target through
**117 of 118** transit stalls (the exception, 10:33:30–34, is an 800 ms
locally blocked send inside an agent-stalled window — the AIMD's own occupancy
loop, not the gate) at a paint age of p50 76 ms / p95 99 ms / max 988 ms.

⚠️ **AC9, partly met 2026-10-31**: the positive half — the target *converging*
toward the measurement across repeated stalls — needs a stall inside a
**measured** minute. A sweep of the three relay laptops' daemon logs for
2026-09-10 → 09-25 (24,013 heartbeats, 7,442 constrained windows, 193 transit
stalls) found the pipe estimate absent at every one of the 193: the
blocked-send source was live in 2 % of the constrained windows and the prior in
none. That absence is the V3b finding, below. The first sustained run came on
2026-10-30 on CORPLAP-2, `0.4.187`: 27 stalls in 56 s at a live
`goodput_bps=180,484`, the target held at 1.11–1.40× it, bounded by the 200 kbps
floor. The weak point is the reading it converges to (the A/B below).

⚠️ **The 2026-10-29 fleet read found the inverse (FR-71).** Over 15 days and
40,654 constrained windows on the corporate laptops (`0.4.137` → `0.4.184`), a
transit stall on a session with no pipe estimate was held 433 times out of
482 — but 49 clean stall episodes cut, every one on a session whose prior stood
at 0.02–0.8 M: `stall_is_ours` compares against `pipe_bps`, whose last source
is the prior's stand-in (`governor.rs:750`), and pushes that prior back without
a measurement on every window it fires on (`:1655`), so each cut lowers the
number the next stall is judged against. One 25-minute HEVC session over DERP
went 3.63 → 0.94 M on 24 gap windows with 0 B in flight. A second edge: the
viewer's congested report in a stall's **onset** window (transit still under
the classifier's 200 ms slack, so the window is `clear` and valid) arms the P3
clamp at the collapsed arrival rate — 3.0 → 0.62 M at 09:17:07 on 10-19, 13
such drops in the fortnight. Both are recorded with their arcs in
[`fr/FR-71-transport-stall-classification.md`](fr/FR-71-transport-stall-classification.md)
(§Open decisions, field log); the law is not changed on this page.

⚠️ **The first is fixed by [V5b](#v5b--only-a-measurement-is-evidence-about-the-rate).**
A convergence read on a build before it may be toward the stand-in rather than a
measurement, so it cannot count as AC9. The rig AC8 and AC9 still need is in
[`fr/FR-79-one-validity-gate.md`](fr/FR-79-one-validity-gate.md) §Open decisions. The
second edge, the onset, stays FR-71's: there the sender really is offering.

⚠️ That prior's size did not come from the walk. The session's stand-in moved on
the measured ×11/10 ladder (`prior.rs:80`, `:167`) above a base of at most 17,799
bps, so a live reading anchored it, and the walk only moved it between rungs. A
congested viewer report stores the arrival rate with no floor (`governor.rs:1640`),
and the same window re-anchors the prior on it (`:1644`). V5b stops the cuts, but
the anchor stays in `pipe_bps` until it climbs back or a delivery takes it out.

⚠️ **The field A/B (2026-10-30 11:48–12:19, CORPLAP-2, `0.4.187`,
`relay:derp/tcp`).** Over one session, stalls cut the target 13 times of 16 while
a stand-in stood and 0 of 26 while nothing did. The same session shows that a live
reading can be wrong too. One 102 ms blocked send folded into a live
`goodput_bps=223,794`, because a window needs only 60 ms of blocked time
(`goodput.rs:82`), on a path that delivered 5.39 M. While it stood the target
fell 3.0 → 0.68 M in 22 s, most of it through the queue budget, which is
denominated in the same `pipe_bps`.

### V5b — only a measurement is evidence about the rate

Four field reads, one mistake: a number nobody measured was treated as one. A
stand-in judged stalls, a reading stood after the path disproved it, a hold nobody
carried was written as the pair's rate, and a trickle became the pipe.

`measured_evidence` (`ROOMLERD_MEASURED_EVIDENCE`, default on) is one switch for five
rules. All five are constrained-only, so a direct window moves nothing, and off is
V5a's rules byte for byte, apart from the heartbeat's new fields. The record, with
the chain they break to the unit, is in
[`fr/FR-79-one-validity-gate.md`](fr/FR-79-one-validity-gate.md).

| rule | what it says | anchor |
|---|---|---|
| **q1** a stall needs a reading | `stall_is_ours` reads `measured_bps`: the goodput, or the viewer's arrival rate inside its TTL, the lower of the two, and never the stand-in. With neither there is no claim to contradict, so nothing cuts, and a stall no longer walks the prior | `governor.rs:1489`, `:765` |
| **q2** a delivery can refute a reading | in a VALID constrained window whose delivery raised the demonstrated floor, every reading that delivery beats by `CONTRADICTION_FACTOR` (2×) stops counting: the goodput is forgotten, the arrival-rate clamp lifts, and with no reading left the prior re-anchors on the delivery | `governor.rs:838`, `:1692` |
| **q3** a hold must be carried | a hold reaches `stable_bps` only if a valid window sent at least 70 % of it | `ceiling_learn.rs:147`, `governor.rs:1786` |
| **(a)** the write-back records a reading or nothing | `session_evidence_bps`: a carried hold, else a live reading, else the last reading the session took. Never the stand-in. A session with none of them leaves the entry's value and age alone | `governor.rs:806` |
| **(b)** a growing queue must be offered | a congested verdict counts only once two windows running each offered more than arrived: `sent > 1.1 × rx`, or the classifier called the window `Overproduced` | `governor.rs:1605`, `viewer_rate.rs:676` |

```mermaid
flowchart TB
    W["a viewer report closes the window"] --> V{"valid and<br/>constrained?"}
    V -- no --> N["nothing here moves:<br/>a stall's shadow is its backlog draining"]
    V -- yes --> G{"queue grew two windows running?"}
    G -- yes --> O{"the sender offered, both windows?<br/>sent > 1.1 × rx, or Overproduced"}
    O -- yes --> ARM["the arrival rate is a reading:<br/>the clamp arms"]
    O -- no --> UN["link_unoffered += 1<br/>neither arms nor releases"]
    G -- no --> F{"did the delivery raise<br/>the demonstrated floor?"}
    ARM --> F
    UN --> F
    F -- no --> STAND["what stood stands"]
    F -- yes --> R{"a reading at or under<br/>delivery ÷ 2?"}
    R -- no --> STAND
    R -- yes --> OUT["it stops counting · readings_refuted += 1"]
    OUT --> L{"a reading left?"}
    L -- yes --> KEEP["the prior anchors on it"]
    L -- no --> ANC["the prior re-anchors on the delivery;<br/>past the band that ends it"]
```

⚠️ **q2 asks whether the floor rose, not whether a reading sits below it.** The true
reading of a thin week of field data (180,484, 0.23 of the floor) sat lower than the
false ones (0.39 and 1.00). What told them apart was the order: both false readings
were followed within 2 s by a delivery at least 2× above them, and the floor never
moved in the true one's 60 s.

⚠️ **(b) reads two sources because `bytes_written` is not the whole offer.** It counts
what left the pump's own channel, and both pumps stamp a frame before it (`peer.rs:5233`,
`:9643`), so a queue growing INSIDE that channel reads sent ≈ rx. `Overproduced` is
that case. A window with no link report keeps the streak, and an unoffered congested
window neither arms nor releases, because the queue did grow.

⚠️ **The two streaks can sit a queue apart.** The growth a report reads is about
frames made a queue's depth ago, so a burst shorter than the queue may never line up
with its report. By then the sender has stopped, and the age loop and the drain still
read the viewer directly. Neither reads `offered`.

⚠️ **(a) reverses FR-70 P1**, which wrote the decayed prior so a session measuring
nothing could free a pair misremembered slow. Such a session no longer raises a low
entry at all, so the entry stands until a session measures or carries a hold, or its
7-day TTL runs out. Reasons and numbers are in FR-79.

The heartbeat carries `pipe_measured` (what a stall is judged against),
`readings_refuted`, and `link_unoffered`, a subset of `link_stats`' congested count: a
link loop gone quiet with this climbing is rule (b) (`peer.rs:5370`, `:10240`).

### The ceiling learner (FR-35 P1) — growth above the nominal

On a constrained (relayed) session the AIMD runs under a ceiling, and that
ceiling used to be a fleet constant: `relay_max_bps` = 3 Mbps (`mod.rs:906`,
env `ROOMLERD_RELAY_MAX_KBPS`), the encoder opened at 85 % of it. One constant
cannot be right for the fleet. The real `neo16 → CORPLAP-2` DERP path, measured
2026-08-29, sustains ~6–9 Mbps and absorbs short bursts far above; other corp
relays measured ~2 Mbps. At 2.55 Mbps that first pair's opening keyframe is a
5.6 KB max-QP smear repaired by inter frames over ~1.2 s; at a 25.5 Mbps cap the
whole desktop arrived as one 680 KB frame at +0.3 s — yet 12.8 Mbps *sustained*
choked it with a 7.9 s stall. So the cap must be **learned per pair**, and —
because every NVENC rate change *down* forces a starved keyframe that replaces
the picture (FR-31) — within a session it may only ever **climb**.

`encode::ceiling_learn::CeilingLearner` (`ceiling_learn.rs:96`) sits between the
plan's nominal ceiling and the AIMD. `RateGovernor::effective_ceiling`
(`governor.rs:668`) returns `plan.max(learned.min(hi))` on a constrained
session, so the learned ceiling only ever RAISES the plan's, never past `hi`
(`relay_max_hi_bps` = 8 Mbps, `mod.rs:964`). Once per viewer window the governor
hands the learner that window's evidence (`governor.rs:1780` → `on_window`,
`ceiling_learn.rs:220`), and it steps the ceiling up only when **all five** gates
pass:

```mermaid
flowchart TB
    WIN["viewer window (1 s)<br/>governor.rs:1752 → on_window"] --> G{"all five gates?"}
    G --> P["PINNED — AIMD target ≥ 90 % of the ceiling<br/>(it wants more)"]
    G --> CAR["CARRIED — the window WROTE ≥ 70 % of the ceiling to the<br/>data channel (you cannot measure capacity you are not using)"]
    G --> CL["CLEAN — no decrease and no stall in the last 10 s"]
    G --> AGE["AGE — viewer paint age ≤ 2× its learned floor<br/>(when the viewer reports it)"]
    G --> SP["SPACED — ≥ 5 s since the last step"]
    P & CAR & CL & AGE & SP --> STEP["step up: ceiling += max(ceiling/16, 150 kbps),<br/>capped at hi — logs 'the pair carried the ceiling'"]
    G -- any fails --> HOLD["hold"]
    DEC["an AIMD DECREASE (full channel · overflow ·<br/>stall · age excess — AimdController::decreases)"] --> PULL["the learned ceiling follows it down to the<br/>post-decrease target, never below the nominal"]
    HS["a send blocked ≥ 1 s (HARD_STALL)"] --> MD["×0.5 at the window boundary, not the ×0.85<br/>ladder (the 7.9 s P0 stall took three ×0.85 over 4 s)"]
    MEAS["a held goodput measurement < ceiling / 2<br/>(CONTRADICTION_FACTOR)"] --> AB["ABANDON the learned ceiling back to the nominal<br/>(FR-59 P6 — a relay-keyed fast day carried onto a slow one)"]
```

| gate / rule | constant | anchor |
|---|---|---|
| pinned | `PINNED_PCT` 90 | `ceiling_learn.rs:62` |
| carried | `CARRIED_PCT` 70 | `ceiling_learn.rs:64` |
| clean | `GROW_CLEAN` 10 s (decrease **and** stall) | `ceiling_learn.rs:58` |
| age | `AGE_FLOOR_FACTOR_PCT` 200 — was 150, until the first field run's 66–80 ms over a 43 ms floor turned out to BE the evidence | `ceiling_learn.rs:69` |
| spaced | `GROW_INTERVAL` 5 s | `ceiling_learn.rs:60` |
| step | `max(ceiling / 16, 150 kbps)`, capped at `hi` | `ceiling_learn.rs:76`, `:267` |
| hard stall | `HARD_STALL` 1 s ⇒ ×0.5, applied at the window boundary (FR-79: only there is it known the block was the pipe, not a pass the runtime stalled under) | `ceiling_learn.rs:71`, `governor.rs:1528` |
| contradiction | learned > 2 × a held measurement ⇒ abandon to the nominal, exactly once | `ceiling_learn.rs:202`, `:86` |

Two things the gates measure that read differently from their names, both found
in the 2026-10-27/28 field reads below:

- **CARRIED counts bytes the send task WROTE to the data channel in the window**
  (`governor.rs:1304`, the send task's running byte total over the elapsed
  window), not bytes the pipe delivered, and `on_window` runs on **every**
  constrained window — it does not read the FR-79 verdict (`governor.rs:1780`).
  So an opening burst the socket absorbs is "carried": on 2026-10-28 07:57:43 the
  first step landed 0.36 s *before* the opener grace ended, inside a 697 KB
  opener that had blocked a send for 164 ms — the very `Opening` window V4
  refuses as write-back evidence. The exposure is one step (+1/16) per session,
  capped by `hi`; the later steps need windows that really carry.
- **DECREASE means an AIMD decrease** — `AimdController::decreases` ticking
  (`aimd.rs:245`, `:259`; read at `governor.rs:1176`). The FR-59 P3 viewer bound
  (`governor.rs:1031`: the ceiling capped at 90 % of the viewer's reported arrival
  rate while its queue grows) lowers the AIMD's ceiling through `set_ceiling`,
  which clamps `desired` **without** counting a decrease (`aimd.rs:188`). The
  learner neither follows that bound down nor restarts its 10 s clean window on
  it — by the P3 ordering's own argument ("a live report that the receiver is
  falling behind outranks" the learner), but a reader of the heartbeat sees
  `target_bps` fall ×0.9 while `learned_ceiling_bps` holds.

The session's **stable rate** — the highest target that held 10 s
(`STABLE_HOLD`) with no decrease — is what the pair memory persists
(`ceiling_learn.rs:73`; next section), so the next session opens at 85 % of what
the pair proved and the learner climbs from that floor instead of the constant.
⚠️ "Held" was a hold of the AIMD *target*, not a delivery: on an idle session the
target sits at the opener's seed for the whole session, so `stable_bps()` was
that seed — see the write-back rows below for what that did to the memory. Since
V5b's q3 a hold counts only if a valid window carried at least 70 % of it.
Kill switches: `relay_ceiling_learn` (tribool, default on; off ⇒ `hi` = 0 ⇒ the
plan ceiling verbatim and no memory) and `relay_max_hi_kbps`
(`crates/agent-core/src/config_surface.rs:1145`, `:1257`; `mod.rs:964`). Direct
sessions never touch the learner (`governor.rs:668` gates on `constrained`).

⚠️ **Only the FFmpeg DC pump runs the learner.** `relay_max_hi_bps()` has one
caller (`peer.rs:6621`, `media_pump_ffmpeg_dc`), the VP9-444 pump builds its
governor with `hi = 0` (`peer.rs:4276`), and the RTP pump has no governor. There
is no software FFmpeg encoder, so a host with no hardware encoder never reaches
the learner, and `relay_ceiling_learn` reads the same on and off there. A VM
cannot prove that switch (FR-35 AC6). ⚠️ **On `h264_vaapi` the ceiling does not show in the bytes:** on jupiter (0.4.197, Raphael iGPU) one scroll delivered 10.7 Mbps under `maxrate` 2.55, 3.0 and 5.84 M alike, so on that cell read the target, `learned_ceiling_bps` and the memory ([FR-35's kill-switch A/B](fr/FR-35-relay-ceiling-learns-the-pair.md#the-kill-switch-ab-2026-11-02)).

> **Field, `CORPLAP-2` over `relay:derp/tcp`, 2026-10-27/28 — agent 0.4.177,
> 0.4.178 and 0.4.181** (the FR-35 surface is byte-identical from
> `agent-v0.4.177` through `agent-v0.4.185` and master; the host took 0.4.178 at
> 13:41Z and 0.4.179 at 18:01Z on 10-27, so no read here ran 0.4.179). Seven
> sessions on the pair `100.65.4.2|relay:derp/tcp` (AV1 NVENC, 1920×1200, DERP
> rtt 106–258 ms), read from the host's own log and `rate_memory.json`:
>
> - **Seeding (P2):** every session opened at 0.85 × the remembered rate —
>   `seed_bps` 4,750,677 ⇒ `opener_maxrate_bps` 4,038,075; 4,759,961 ⇒
>   4,045,966; 4,402,963 ⇒ 3,742,518 (×5) — against the 2.55 M a cold pair gets.
> - **Growth (P1):** the 10-27 09:23 session (79 s) stepped 4.04 → 4.29 → 4.56 →
>   4.84 M in its first 15 s while the 2 s heartbeats carried 6.2 / 6.3 / 4.4 /
>   7.6 Mbps, sat at 4.84 M through 45 s of an idle desktop (0.28–0.35 Mbps at
>   18 fps — not carried, no step), and stepped to 5.15 M at 09:24:34.8 on a
>   6.4 Mbps window. The 10-28 16:46:31 session (90 s) stepped 3.74 → 3.98 → 4.22
>   → 4.49 M during 12 s of motion (3.1–7.4 Mbps windows), then nothing for 60 s
>   of idle. Of the four 11–19 s sessions, three stepped once and the 07:57 one
>   three times (to 4.49 M on 4.7–7.3 Mbps windows); the idle 15:27 session never
>   stepped.
> - **Health** (the four sessions read heartbeat by heartbeat: 09:23 and 15:27
>   on 10-27, 07:57 and 16:46:31 on 10-28): `send_stalls=0` in every one;
>   `send_wait_max_ms` 0.04–2.1 except the heartbeat holding an opener burst
>   (52.4 ms on 09:23:26, 163.9 ms on 10-28 07:57:43) — never near `HARD_STALL`.
>   Viewer paint age p50 66–68 / p95 103–117 ms over a 45–50 ms floor, the maxima
>   (438, 775 ms) in a session's opening ten seconds (4 s and 10 s in). The
>   ceiling never passed 5,146,238 (`hi` = 8 M) in any of the seven.
> - **What held the opener at ~1.5×, not 2.5×:** the memory sat at 4.40–4.76 M
>   while the pipe carried 6–7.6 Mbps windows with sub-ms send waits. The
>   write-back rows below show why: a session that *proved* 4.84 M stable moved
>   the memory +0.2 % (`ALPHA_UP` 0.10 of the gap), the next idle session moved
>   it −7.5 % (its own seed written back as evidence). Nothing in these reads
>   measured a sustainable limit; the memory's level is the write-back's
>   dynamics, not the pipe's. (FR-35 AC1's numbers passed on 2026-10-31 without
>   moving this, and a cold opener passes them too:
>   [the opener A/B](fr/FR-35-relay-ceiling-learns-the-pair.md#the-opener-ab-2026-10-31).) ⚠️ **Compare opening keyframes only at equal `dc_unopen_drops`.** Every frame the pump encodes before the video channel opens is dropped and forces another keyframe (`peer.rs:9860`, `:9866`), and the one delivered shrinks as the count rises: 63,284 B at 2 drops and 23,435 B at 4 on one `maxrate`. At equal drops it follows `maxrate` ([what moved it](fr/FR-35-relay-ceiling-learns-the-pair.md#what-moved-the-openers-keyframe-2026-11-02), [#3334](https://github.com/gjovanov/roomler-ai/issues/3334)).

### The write-back and the seed (V2 + V4) — the pair memory

FR-35 P2 remembers a pair's rate so the next session opens at 85 % of it
instead of at the fleet constant. FR-79 changed **what** is remembered and
**under which key**. FR-148 P1 changed **which entry is read**: only a measured
one (the provenance row below):

```mermaid
flowchart LR
    subgraph key["the key (V2)"]
        K["remote overlay address<br/>+ carrier tag<br/>e.g. 100.65.4.2 · relay:derp/tcp"]
    end
    subgraph evidence["the session's evidence (V4)"]
        B["a hold a valid window carried<br/>(the learner's stable rate)"]
        L["a reading: live, or the last<br/>the session took (V5b)"]
        O["opener measured by the<br/>goodput estimator<br/>(a burst that QUEUED)"]
        N["nothing measured:<br/>a stand-in does not count"]
    end
    B -- "first choice" --> D{"a measured entry,<br/>fresh (7 d)?"}
    L -- "second choice" --> D
    O -- "third choice" --> D
    N --> Z["write NOTHING —<br/>not the value, not the timestamp"]
    D -- "no: none, stale<br/>or assumed" --> AD["adopt the measurement outright"]
    D -- yes --> DM["damp(old, measured):<br/>half the gap down,<br/>a tenth of the gap up"]
    AD & DM --> F["rate_memory.json<br/>(temp + rename)<br/>provenance: measured;<br/>assumed entries dropped"]
    F --> S{"the next open:<br/>a measured entry,<br/>fresh (7 d)?"}
    S -- yes --> SEED["seeded from the entry;<br/>slow-link profile at or under 1 Mbit/s"]
    S -- "no: none, stale<br/>or assumed" --> NEVER["opens as a pair never seen"]
```

| piece | rule | anchor |
|---|---|---|
| the key | `remote_addr\|carrier` — `direct`, `tunnel`, `relay:<kind>/<transport>` from the LocalAPI peer record; a mid-churn carrier (`blocked` / `offline`) leaves the bare pre-FR-79 key. DERP-over-TLS and a UDP relay are different pipes on the same address, four times apart in capacity | `rate_memory.rs:42`, `:65`; `media_pair_memory.rs:163`, `:140` |
| the provenance | FR-148 P1. Every entry written carries `"provenance": "measured"`. An entry with no provenance, which is every entry written before this release, or with a value this agent does not know, is **assumed**. An assumed entry seeds nothing, so the pair opens as one never seen: no slow-link profile and no opening below the floor. It is no base for `damp`, so the first measurement replaces it outright. The next write for any pair drops it, because the file is written whole and an assumed value is never stored (FR-148 §4.1). A session that measured nothing leaves it as it is, unread, until it ages out. At the open, a fresh assumed entry logs `FR-148 P1 rate memory: … read as assumed …` with `assumed_bps` and `age_s`. **Rollback:** an older agent ignores the field and reads every entry again. Its whole-file write drops the field, so after a re-upgrade those entries are assumed: one session of learning per pair and carrier | `rate_memory.rs:107`, `:164`, `:173`, `:192`; `media_pair_memory.rs:571` |
| the opener | measured by the goodput estimator (bytes over blocked time) for a burst that queued; `None` for one the socket absorbed. The old `bytes × 8 / longest_single_wait × 75 %` recorded 6–8 M against a 1.8–3.4 M pipe | `peer.rs:9416` (`opener_measured_bps`) |
| the evidence | `RateGovernor::session_evidence_bps`: first `stable_bps()`, the highest AIMD target that held 10 s with no decrease, when above the nominal and, since V5b, carried by a valid window; then a live reading; then the last reading the session took; else 0 = nothing. ⚠️ Before V5b the first was a hold of the *target*, not a measurement: an idle session held the opener's seed for its whole length and wrote 0.85 × the memory back as evidence, and the second was `pipe_bps`, whose last source is the prior's stand-in, so a prior that stood low was written back too (field rows below) | `media_pair_memory.rs:708`; `governor.rs:806` |
| the write | `record_session(peer, evidence, now)`: `None` or 0 ⇒ **no write**; first evidence, or the first after an assumed entry ⇒ adopted outright; otherwise `damp` with the goodput estimator's own asymmetry (`ALPHA_DOWN` 0.50 / `ALPHA_UP` 0.10, `goodput.rs:94`, `:98`). The maximum rule, `had_decrease`, the unqueued ×1.5 step and its constants are gone | `rate_memory.rs:230`, `:79`; `media_pair_memory.rs:238` |
| the file | `rate_memory.json` in the daemon's data dir (`appdirs::project_dirs().data_dir()` — on a Windows SYSTEM service `…\config\systemprofile\AppData\Roaming\roomler\roomler\data\`); whole-file atomic write; a missing or corrupt file is an empty memory; an entry older than 7 days, or an assumed one, never seeds and is pruned on the next write | `rate_memory.rs:269`, `:22` |
| the log line | `FR-79 V4 rate memory: … peer=… evidence_bps=… kept_bps=…` at every session end that had a key. **`kept` lies between the old value and the evidence**, so `kept > evidence` on one line proves the entry moved DOWN — no old value needed | `media_pair_memory.rs:247` |

This memory moved out of the pump with FR-148 P1 (`RateMemoryGuard`,
`agents/roomlerd/src/media_pair_memory.rs:210`) and is otherwise unchanged: it still
seeds the opener, and its file is written byte for byte as before. Beside it P1 keeps
the path model's own memory, under a key that names the path
([§ The path memory](#the-path-memory-fr-148-p1--the-models-floor-under-a-key-that-names-the-path)).

Field reads of the write-back so far (key `100.65.4.2\|relay:derp/tcp` unless noted):

| when (UTC) | host · build | evidence → kept | what it shows |
|---|---|---|---|
| 2026-09-08 19:12–19:18 | CORPLAP-1 · 0.4.93 | 1.44 / 1.09 / — / 3.32 / 6.13 M, the max rule still in force | five openers 1.44–2.82 M inside a carrier measuring 1.09–6.13 M (AC5) |
| 2026-09-08 22:14–22:17 | CORPLAP-1 · 0.4.95 | `None → kept 2,264,993`, twice (`100.65.0.5`) | a session that measured nothing wrote nothing; the old rule would have written 3,825,000 from an absorbed burst (AC8, first half) |
| 2026-09-10 08:39:41 | CORPLAP-2 · 0.4.97 | `Some(3187500) → 3210512` from a seed of 3,233,525 | the entry moved **down** — `damp` exactly; the max rule would have kept 3,233,525. Marginal (0.7 %), but a direction the old rule could not take |
| 2026-09-10 14:03:44 | CORPLAP-1 · 0.4.97 | `Some(8000000) → 6318171` from 6,131,302 | the entry moved **up**, damped, after a session that measured the relay at 11.5–17.0 M — one ×0.10 step toward the learner's 8 M cap |
| 2026-10-27 09:24:41 | CORPLAP-2 · 0.4.177 | `Some(4843519) → 4759961` from 4,750,677 | the learner's stable rate (4.84 M held 57 s, windows carrying 6–7.6 Mbps) moved the entry **+0.2 %** — `ALPHA_UP` of a 93 kbps gap |
| 2026-10-27 15:29:46 | CORPLAP-2 · 0.4.178 | `Some(4045966) → 4402963` from 4,759,961 | **the P2b decay, in V4 form**: a 130 s session on an idle desktop (0.16–0.25 Mbps at 18 fps, a 405-byte opener) never stepped and never decreased, so `stable_bps()` was its own `opener_maxrate_bps` — 0.85 × the memory — and `damp` took half the gap: **−7.5 % for having measured nothing**. Ten such sessions halve the memory; one proving session restores a tenth of its gap |
| 2026-10-27 16:31 · 10-28 07:58, 12:38, 16:46 | CORPLAP-2 · 0.4.178 / 0.4.181 | `None → kept 4402963`, four times | sessions of 11–19 s: nothing held 10 s, nothing written — not the value, not the timestamp (as designed) |
| 2026-10-28 16:48:01 | CORPLAP-2 · 0.4.181 | `Some(4489010) → 4411567` from 4,402,963 | the 90 s session's stable rate (4.49 M after three steps) moved the entry **+0.2 %**; the file now holds `100.65.4.2\|relay:derp/tcp` = 4,411,567 at `at_unix` 1793206081 |
| 2026-10-30 12:22:06 | CORPLAP-2 · 0.4.187 | `Some(84195) → 84195`, a new entry | **the prior's stand-in written as evidence**: anchored at 180,484, it walked eight rungs in 28 s once the goodput expired, and `pipe_bps`'s last source wrote it. Nobody measured 84,195 |
| 2026-10-25 → 10-30 | CORPLAP-2 · up to 0.4.188 | 18 writes with evidence, `5955306 → 3379312` | **every one a rung of the session's own opener**, 0.85 × the memory × (17/16)^k, six at k = 0. 17 of the 18 sessions held no live measurement in any heartbeat, and the entry lost 43 % while they delivered up to 10.2 M |

⚠️ An entry written before V2 is keyed by the bare address, never matches
again and ages out. An entry that ages out is *replaced* by the next session's
first evidence, not damped toward it — the damping only ever acts inside the
7-day window.

⚠️ The 2026-10-27 15:29 row is the open question FR-79 q3 turns on. V4
dropped `had_decrease` because "the gate can tell a measurement from a
non-measurement" — but `stable_bps()` is not gated: a target that nothing pushed
is a hold, not a measurement, and on the connect-look-leave sessions this fleet
actually has it writes the opener's own seed back as evidence. The asymmetry
that protects against a flooded opener (believe a drop fast) then compounds it:
the pair that carried 6–7.6 Mbps windows is remembered at 4.4 M. A fix belongs
with the governor (`media_pair_memory.rs:708`: require the hold to have been *carried*, or
route the first-choice evidence through the gate); nothing here is changed yet.
The 2026-10-25 → 10-30 row is the same defect at scale: every write on that pair,
not only the idle sessions.
*2026-11-03: decided and built, V5b's q3 requires the hold to have been carried.*

**That week, replayed (2026-10-31).** `damp` reproduces all 24 sessions' logged
`kept` to within 2 bps, so the replay is the write-back itself. All 18 writes lie
within 0.2 % of a rung of the session's own opener, `0.85 × memory × (17/16)^k`,
which fixes what a write can do:

| rungs climbed `k` | evidence ÷ memory | the memory moves | writes that week |
|---|---|---|---|
| 0 | 0.850 | −7.50 % | 6 |
| 1 | 0.903 | −4.84 % | 2 |
| 2 | 0.960 | −2.02 % | 3 |
| 3 | 1.020 | +0.20 % | 4 |
| 4 | 1.083 | +0.83 % | 2 |
| 9 | 1.467 | +4.67 % | 1 |

A session raises the memory only by climbing at least three rungs and then
holding ten seconds; the week's mean was −2.98 % per write. Only 6 of the 18 held
a target that a 2 s window carried at ≥ 70 %. Counting only carried holds helps:

| rule | writes that count | end value, evidence as logged | end value, same rungs from the higher opener | opener, × the cold 2.55 M |
|---|---|---|---|---|
| today | 18 | 3.38 M | 3.38 M | 1.13× |
| the session's peak window reached 70 % of the evidence | 11 | 3.78 M | 5.51 M | 1.26–1.84× |
| a window at the held target reached 70 % of it | 6 | 4.13 M | 5.79 M | 1.38–1.93× |
| only rungs above the seed (`k ≥ 3`) | 7 | 4.23 M | 6.25 M | 1.41–2.08× |

The fourth column is an upper bound, because CARRIED is harder to pass under a
higher ceiling.

**Corrected 2026-10-31.** This section used to say a 2.5× opener needs a memory
of 7.5 M, on the assumption that the keyframe follows `maxrate`. It does not
follow it in a straight line ([the opener
A/B](fr/FR-35-relay-ceiling-learns-the-pair.md#the-opener-ab-2026-10-31)): on a
dense screen on AV1, 1.6× the maxrate gave 3.5–5.8× the keyframe, and on H.265
4:4:4, the same 2.55 M gave 23 to 63 KB. FR-35 AC1's numbers pass at the pair's
own 2.85–3.0 M, on a bar a cold opener clears too, so AC1 stays `[~]` until the
operator says which reading counts. The hold question is worth deciding for what
the memory buys on AV1, and AC1's numbers do not depend on it.

**Three writes in one night (2026-10-31, CORPLAP-2).** Evidence was
`stable_bps()`, else `pipe_bps()` (`peer.rs:9073`), and nothing clamps the
second to `hi`. ⚠️ V5b leaves this open: the 13.04 M was a live goodput, not a hold
and not a stand-in, so it is evidence by the new rules too:

| session | evidence | the memory |
|---|---|---|
| a 60 s drag, eight learner steps | 4,872,507 | 3,379,312 → 3,528,631, +4.4 % |
| opened and maximized an Explorer window | 3,187,500 | → 3,358,065, −4.8 % |
| 45 s, closed that window | 13,044,940 | → 4,326,752, +28.9 % |

One burst moved the memory six times further than a minute of real load. That
is the same question as the hold, from the other side: which sessions measured
the pair.

Below a memory of about 3.53 M the opener sits under the plan's 3 M, so an idle
session has no `stable_bps()` above the nominal (`ceiling_learn.rs:266`) and, with
nothing measured, writes nothing: an 8 min idle session read `evidence=None`. That
is where the week's decay stopped, at 3.38 M.

### V3b — ONE belief about the path (SHADOW since `agent-v0.4.98`, one consumer since V5b)

`encode::pipe::Pipe` (`pipe.rs:152`) answers a defect the shadow data made
undeniable: **there is no trustworthy, always-available estimate of what the
path carries.** The blocked-send source is silent whenever the queue is
downstream (every relay-TCP session), the viewer's arrival rate was being
thrown away unless the link loop called the window congested, and the pair
memory had almost nothing to remember. The type separates two kinds of
evidence the old `min(goodput, link_rx)` averaged together:

| evidence | meaning | source | accessor |
|---|---|---|---|
| a **delivery** | *"the path carried at least X"* — a lower bound, available every window on every carrier | the viewer's arrival rate (`governor.rs:1397`) | `floor_bps` — a MAX for the session, lowered only by contrary evidence, never by time |
| a **push-back** | *"the path refused above X"* — a limit | an accepted goodput fold (`governor.rs:1508`), or the arrival rate while the viewer's queue grows (`:1646`) | `capacity_bps` (`pipe.rs:369`), 60 s TTL |
| the anchor a ceiling may be built on | `max(capacity, floor)` — never `min` | — | `ceiling_anchor_bps` (`pipe.rs:395`) |

⚠️ **It is a shadow, with one exception.** The heartbeat prints `pipe_belief=(anchor,
floor, capacity)` and `pipe_belief_n=(deliveries, capacities)` (`peer.rs:8520`), and
none of the three values has a consumer: not V5, not the ceiling, not the memory.
V5b reads one fact and nothing else, whether a delivery RAISED the floor
(`Pipe::observe_delivered`, q2). The shadow corrected its own design three times in
five days, which is why this page stops here rather than describing a law:

| release | what the shadow found | correction |
|---|---|---|
| 0.4.99 (V3b-1) | a healthy idle relay session read `belief=1.8 M` from 35 deliveries and zero push-backs; a rule reading that as "the pipe" would have called a fine session overdriving | no single `believed_bps()`: `capacity_bps` answers *am I over?* (`None` = no claim to contradict), `ceiling_anchor_bps` answers *what may the ceiling be?* |
| 0.4.99 (V3b-2) | a host that had demonstrated 8.4 M read 12.8 kbps hours later on a static screen (`n=(10696, 0)`); a 30 s window cannot tell a degraded path from a quiet one | the floor is retired by **evidence**, not time: only a push-back lowers it, and damped (`rate_memory::damp`), never snapped |
| 0.4.100 (V3b-3) | a capacity of 13,163,844 expired after one TTL and the anchor fell back to a content floor of 4,050,945 while the target ran to the 34.56 M constant | a capacity sample **is also a delivery** — those bytes got through — so it raises the floor, and the demonstration survives the estimate expiring |

What two more weeks of shadow say (2026-09-24, CORPLAP-2, three hours over
`relay:derp/tcp`): `pipe_belief_n=(9912, 1)` — nine thousand deliveries, **one
push-back**, while the transport stalled 118 times with no number attached.
Whether V5 should read `capacity_bps`, and what a transit stall should feed
into the belief, is the open question the shadow exists to answer; do not
build on the anchor's semantics until it has. FR-74 P5's gate was read against
this shadow on 2026-09-25 — the result, and the floor-damping interaction it
found, are under [§ The direct path](#the-direct-path-fr-74--a-ceiling-that-is-a-bound-a-gate-that-measures-lag).

## The Priority dial

The viewer's Priority dial (`Sharper` / `Balanced` / `Smoother`) is the only
user-facing rate knob. Since rc.445 all three run at **native resolution all
the time**; they differ in the **bitrate ceiling** handed to the encoder:

| Dial | Ceiling factor | Feel |
|---|---|---|
| Sharper | 100 % | Maximum per-frame quality; fps dips first under load |
| Balanced | 85 % | Default |
| Smoother | 70 % | Smallest motion frames → steadiest fps on thin links |

Why this works: the encoders run **constant-quality VBR with a maxrate cap**
(`cq`/`global_quality` ≈ 22 + `maxrate` + a 2× HRD window). A settled desktop
costs almost nothing, so it *never touches the ceiling* — at rest every dial
delivers identical, full-quality text. During motion the HRD binds and the
encoder raises QP **continuously, frame by frame** — smaller frames, steadier
arrival, more fps through the same pipe. A lower ceiling simply moves that
trade further toward fluidity. No mode switch, no rebuild, no seam.

### Why resolution flips were removed (rc.445)

Until rc.443 Smoother/Balanced dropped to a 1024/1280 rung during motion and
refined back to native at rest. Field measurement (2026-08-21, three hosts)
killed the design: every flip is a **blocking encoder open on the pump
thread** — 865 ms down / 654 ms up on an Iris-Xe-class iGPU — plus a
new-resolution IDR queued behind stale frames. Users felt it as "drag takes
off ~1 s, freezes ~1 s, continues", and unanimously preferred Sharper (the
one dial that never flipped). The rungs remain available behind the
`priority_res_cap` config key for A/B, but the default is: **resolution is
not a rate-control lever**. (An explicit resolution pick by the viewer, and
the encode-bound auto-downscale tier, still apply — those change rarely and
deliberately, not per drag.)

## The per-session control loops

Each DC video pump runs these, owned by `encode::governor::RateGovernor`
(P8c) and executed by the pump:

| Loop | Signal | Actuator | Cadence |
|---|---|---|---|
| **CQ + HRD** | encoder-internal | per-frame QP | every frame, zero cost |
| **AIMD bitrate** (`encode::aimd`) | send-channel occupancy + byte budget | `set_bitrate` → ladder-coarsened maxrate | MD ≤1/500 ms, AI ≤1/5 s |
| **Byte-budget queue gate** (rc.442; FR-74 P1b on direct paths) | relay: bytes in flight vs `constrained_queue_ms` (450 ms) of the relay ceiling. Direct: the **measured send wait** vs `direct_queue_ms` (150 ms) once bytes exceed 150 ms of the ceiling; bytes alone gate only at the encoder's HRD reservoir ([§ The direct path](#the-direct-path-fr-74--a-ceiling-that-is-a-bound-a-gate-that-measures-lag)) | skip producing a frame | every loop iteration |
| **Viewer-rate divisor** (`encode::viewer_rate`) | browser's decoded-fps + struggling report. Since FR-119 P2b-7 the browser judges "struggling" on its decode backlog in TIME — a window whose mean backlog exceeds 33 ms, or a frame shed at 67 ms — not on a frame count, which halved its meaning at every doubling of the rate | send every Nth frame | 1 s windows |
| **Encode pressure + auto tier** (`encode::encode_pressure`) | avg encode ms | maxrate factor; long-edge cap when encode-bound | 2 s heartbeat |
| **Goodput estimate** (`encode::goodput`, rc.453) | blocked-send goodput from the send task, folded **only when the validity gate accepts the window** (§ above) | the measured-ceiling clamp (stage 1), FR-59 P1/P6 via `pipe_bps`, V5's `stall_is_ours` via `measured_bps`, the pair memory's write-back | folded on the viewer window |

Three rules keep them from stepping on each other:

- **Rebuild-bound bitrate applies are motion-deferred** (rc.445). NVENC
  reconfigures maxrate in place; QSV/AMF must rebuild the encoder — a
  blocking open. The pump holds AIMD applies while any real frame encoded
  ≥4 KB in the last 1.2 s (the motion clock; caret/keystroke deltas at
  0.5–3 KB never hold it) and flushes once quiet — the rebuild then stalls a
  static image nobody can see, and its first-frame IDR doubles as the
  post-motion refresh. ⚠️ At constant bitrate QSV pads every frame to `maxrate ÷ fps ÷ 8` bytes (10,638 at 2.55 Mbit/s), so every real frame, a caret blink's too, reads as motion, the clock never goes quiet while anything on the screen changes, and a deferred apply waits for good: [FR-148](fr/FR-148-path-measured-plan-follows.md) measured 0 rate moves in 16,719 s, and a still screen spending about 0.9 Mbit/s on keepalive filler. `qsv_rate_mode = qvbr` (FR-148 P2) opens QSV off constant bitrate, so a frame can be as small as its content ([encoders.md](encoders.md#qsv-off-constant-bitrate-fr-148-p2)); whether a still QSV frame then falls under 4 KB is FR-148 AC13's read, owed.
- **A rebuild bumps the send epoch** (rc.445): the send task discards
  queued frames from the previous encoder, so the fresh IDR ships
  immediately instead of behind up to 450 ms of obsolete motion.
- **A rate move is not a viewer limit** (FR-119 P2b-4). The viewer-rate cap
  starts at the capture rate, which means no cap, and it follows the capture
  rate when that moves: FR-119's 60 → 120 swap, or a relayed session going
  direct. Before, it stayed at the old rate and read as a clamp, so every
  other frame was dropped for the 3–5 s of the recovery climb. A cap below the
  capture rate is the viewer's own number and stays where it is.

Rebuilds also reuse the session's **proven encoder name** first instead of
re-walking the vendor cascade (a failed tiered open of an absent vendor's
encoder costs 100–300 ms), and open at `min(ceiling, AIMD target)` so the
governor's forced reapply cannot trigger an immediate second rebuild.

## The direct path (FR-74) — a ceiling that is a bound, a gate that measures lag

Everything above is about paths that push back. A direct path on a LAN or a
good WAN mostly does not, and until `agent-v0.4.77` the controller there was
fighting itself. The operator's own test that opened FR-74 (2026-09-06,
CORPLAP-3, `av1_qsv` 1920×1200 at 4 ms RTT, a Notepad++ scroll — the record is
[`fr/FR-74-text-clarity-on-direct-paths.md`](fr/FR-74-text-clarity-on-direct-paths.md)):
the direct ceiling was a constant, `0.07 bpp × 1920 × 1200 × 60 = 9.68 Mbps`,
and Sharper was 100 % of it; every scroll burst overran it; the send queue
crossed a byte budget denominated in the **applied** target; the gate skipped
frames; and the AIMD read its own cap as congestion — three ×0.85 cuts in six
seconds, ~40 s of additive climb back, and on QSV a rebuild plus an IDR at
every ladder crossing (37 swaps and 35 settle keyframes in 11 minutes). The
budget was self-reinforcing: at 2.5 Mbps it was ~47 KB, one text frame tripped
it, and a session sat at 2–3.7 Mbps for minutes.

Two changes fixed it, both on the direct branch of the FFmpeg pump, relay
paths untouched byte for byte, and **no new switch** — the operator's overrides
(`FFMPEG_MAXRATE_KBPS`, `direct_queue_ms`) are the way back in both directions.

### P1 (0.4.77) — the ceiling is a content-generous bound

`rate_profile::ffmpeg_maxrate_bps_scaled`
(`agents/roomlerd/src/encode/rate_profile.rs:219`) uses **0.25 bpp/s on a
direct path** (0.07 on a constrained one, min'd with the relay clamp as
before), clamped to **[3, 48] Mbps × the codec/chroma factor**
(`codec_rate_factor_pct`, `:129` — AV1 100, HEVC and VP9 125, H.264 150;
`chroma_rate_factor_pct`, `:155`, for 4:4:4 cells). The fleet's heartbeats
show the products: 34.56 M for AV1 at 1920×1200 @ 60, 43.2 M for HEVC and
`vp9_qsv` there, 38.88 M for HEVC at 1920×1080. It is a **ceiling, never a
target**: the encoders run constant quality under `maxrate`, so a settled
desktop spends almost nothing and only motion approaches the bound.

`policy::rate_plan` (`encode/policy.rs:241`) finishes the chain: × the dial
(`dial_rate_factor_pct` — 70 / 85 / 100, clamped to [30, 100]) × the
encode-pressure factor (`governor.encode_factor()`, `governor.rs:1967`),
floored at `MIN_BITRATE_BPS` (1.5 M, `encode/mod.rs:272`). On a direct
transport `effective_ceiling` is the plan's value unchanged (`governor.rs:664`
— the learner lifts it on constrained sessions only); the shared-pipeline split
divides it by the follower count; and the result is both the AIMD's ceiling
and the gate's reference rate (`last_ceiling_bps`, `peer.rs:7337`).

⚠️ The encode-pressure factor multiplies the bound, so a host whose encoder is
struggling lowers its own ceiling with no path evidence in it. On 2026-09-25
CORPLAP-3's ceiling fell to 13.82 M (0.4 ×) at 4 ms of viewer age and 0 bytes
in flight while its `av1_qsv` passes took 120–170 ms and the cadence was paced
60 → 20 fps, and it was back at 27.8 M eleven seconds later. Read
`ceiling_bps` on the `set_bitrate` line before calling a `target_bps` drop a
path event.

### P1b (0.4.79) — the gate is the measured wait's call

P1's budget (150 ms of the ceiling) still tripped once on 0.4.77: AV1's HRD
window is floored at 200 % of `maxrate` (Intel's VDENC hangs on a forced IDR
larger than its reservoir — the rc.443 incident), so the encoder was
*configured* to emit an 8.6 MB burst against a 648 KB budget, and the
controller cut on a burst it had itself legalised, at ≤ 20 ms of viewer age.
Bytes cannot tell a burst the wire is draining from a backlog the viewer
feels; the send wait can, and it is the quantity `direct_queue_ms` was always
meant to bound.

| piece | rule | anchor |
|---|---|---|
| soft budget | `direct_queue_ms` (150) × ceiling ÷ 8, floored at 48 KiB; `0` disables the gate | `rate_profile.rs:559` (`direct_queue_budget_bytes`) |
| hard budget | `max(soft, HRD reservoir)`, the reservoir being `maxrate × open_hrd_pct ÷ 8` — 100 % on direct (`direct_hrd_pct`, `:590`; config `direct_hrd_pct`, [25, 200]), 200 % for every `av1_*` encoder regardless | `rate_profile.rs:578` (`direct_queue_hard_budget_bytes`); `ffmpeg/encoder.rs` `open_hrd_pct` |
| measured wait | the larger of an EMA (α 0.3 per pass) of completed frames' enqueue→wire-complete waits and the **live age of the frame the send task is writing** (`send_head_enqueued_us`) — a stalled pipe completes nothing, so the EMA alone reads stale-low exactly when the queue grows | `peer.rs:6639`–`6261`; set and cleared by the send task at `:5903`, `:5971` |
| the verdict | bytes ≥ hard ⇒ trip; bytes ≥ soft **and** wait ≥ `direct_queue_ms` ⇒ trip; otherwise pass | `rate_profile.rs:597` (`direct_gate_trips`), called at `peer.rs:6683` |
| on a trip | the pass skips producing a frame (`frames_skipped_backpressure`) and drives the AIMD's multiplicative decrease, as the full-channel arm always did; the first trip logs `direct byte-budget gate engaged` with `inflight`, `budget`, `hard_budget` and `measured_wait_ms`, so a field read can tell which arm fired | `peer.rs:6762`, `:6637` |
| relay paths | unchanged: bytes vs `constrained_queue_ms` (450) of the relay ceiling, no wait term | `peer.rs:7997-8014` |

```mermaid
flowchart TB
    subgraph ceiling["the ceiling — a bound, never an operating point"]
        G["w × h × fps × 0.25 bpp/s<br/>(0.07 on a constrained path)"] --> C["clamp [3, 48] M × codec/chroma factor<br/>rate_profile.rs:219"]
        C --> D["× dial 70 / 85 / 100 %"]
        D --> E["× encode-pressure factor,<br/>floor 1.5 M — policy.rs:241"]
        E --> S["÷ followers (shared split)"]
        S --> CEIL["ceiling_bps<br/>= the AIMD's ceiling = the gate's reference"]
    end
    subgraph gate["the direct gate — one pass, before capture (peer.rs:6522–6670)"]
        CEIL --> SOFT["soft = 150 ms × ceiling"]
        CEIL --> HARD["hard = max(soft, HRD reservoir)"]
        W["wait = max(EMA of completed waits,<br/>age of the head-of-queue frame)"]
        Q["bytes in flight"] --> H{"≥ hard?"}
        HARD --> H
        H -- yes --> TRIP["skip the frame · AIMD ×0.85"]
        H -- no --> SQ{"≥ soft AND<br/>wait ≥ 150 ms?"}
        SOFT --> SQ
        W --> SQ
        SQ -- yes --> TRIP
        SQ -- no --> PASS["capture → encode → send"]
    end
    TRIP -.->|"target ≤ ceiling"| CEIL
```

**Field.** The P1b gate on the release's defaults (0.4.79, 2026-09-07,
CORPLAP-3, the operator judging): no blur on AV1, VP9 4:2:0 or H.264, and the
sessions' heartbeats read 0 cuts, 0 gate skips and 0 gate lines at 20–36 Mbps
and 30–47 fps; the rate ladder stopped firing on direct paths altogether (0
swaps in 17 sessions on three hosts, where the baseline had 37 in 11 minutes),
which retired the planned QSV hysteresis. What the gate deliberately does not
see: a pipe throttled **below the socket** (an OS QoS policy, a thin Wi-Fi
driver queue) queues where `bytes_inflight` cannot look — measured 2026-09-07
with `roomlerd.exe` capped at 15 Mbps, the gate shed nothing and the cost
surfaced as 160–380 ms of viewer age instead. That standing lag is the viewer
age's call (FR-15), not this gate's.

The libvpx VP9 4:4:4 pump is a different machine and got its own fix in the
same arc (P3, 0.4.80: `rc_max_quantizer` 16 on direct transports, because
libvpx resets q to the worst on every wheel notch — [encoders.md](encoders.md)),
and the viewer's pixel chain, which no encoder setting can undo, got the
display-scale pill (P4 — [remote-control.md](remote-control.md) §18.6.1).

### P5 — the ceiling follows the path: decided, refined, and NOT built

A higher constant is still a constant. On 2026-09-10 a direct HEVC session on
another host (the Regal cell, 1920×1080 at 6–11 ms of paint age) ran for hours
between a 38.88 M ceiling with no measurement in it and a path that pushed
back at ~6 M: 4.5 minutes to first reach the ceiling, then a collapse to the
measurement every two to three minutes, every collapse a visible softening.
The decision (FR-74 §P5, refined 2026-09-15): the bpp product becomes a
**cap**, and the ceiling follows the FR-79 V3b belief above — with the
asymmetry that **only a refusal (`capacity`) may lower the ceiling, a delivery
(`floor`) may only raise it**, and the capacity trace decays upward while
nothing pushes back, so the loop has no ratchet in either direction.

It is gated on the shadow reading sane, and the gate was read on 2026-09-25
from the hosts' own daemon log files over Fleet RPC (agents 0.4.99–0.4.102;
four hosts; 104,698 heartbeats in 37 sessions, 09-11 → 09-25; the exec-enabled
population only — three corp laptops and the Regal cell):

| the gate asked | what the shadow said |
|---|---|
| on relay hosts the anchor is not `None` | **holds** — the two relay sessions (5,240 constrained windows) carried a floor within their first five windows and an anchor of 3.5–5.2 M for the rest (`pipe_belief_n` 9,912 / 1 over three hours, 155 / 0 over three minutes) |
| a quiet screen must not read as a thin pipe | **holds since 0.4.99 (V3b-2)** — 19 of 19 push-back-free direct sessions ended at their maximum floor, and each of the 7 whose floor fell carried a push-back; CORPLAP-3 held 34.27 M through 3.5 min of idle after one scroll. The 0.4.98 control on the same host: 16.4 M → 13.5 kbps over 4.3 h with no push-back |
| on a direct session the anchor tracks the path, not the content | **not as written, and not meetable** — before the first burst the anchor *is* the content's weight (the Regal sessions open at 0.07–2.4 M and take 4 s to 2.2 h to reach half their eventual floor; a 4.6-min CORPLAP-3 session never rose above 3.5 M on a path that carries 34 M). Nothing measures capacity on an uncongested direct path. The refinement already makes this harmless: a delivery never lowers the ceiling |
| the push-backs are real refusals | on the LAN-class hosts the only capacities are the encoder's burst drain **at the cap** (34.3 M against a 34.56 M ceiling; 45.6–59.7 M against 43.2 M) — not refusals, and `limit ≥ bound` leaves the ceiling alone. On the Regal cell: **14 accepted push-backs in ~51 h of sessions, 0.85–19.9 M**, send waits up to 1.36 s around them (two ≥ 1 s hard stalls); at every one the target had already collapsed from 38.88 M to 1.5–17.0 M (median 3.9 M) — today's sawtooth, at one accepted fold per ~3.6 h rather than the 2–3 min period of 09-10 |
| the floor guard holds | **it erodes** — V3b-2 damps the floor by half the gap on *every* push-back below it, whatever the spacing: 28.84 → 18.09 → 11.82 → 7.76 M on three push-backs 5.5 h apart; 38.90 → 22.26 → 14.04 → 8.57 → 9.78 → 14.21 M on five in 1.6 h, the demonstration re-arming to 16–25 M between them. The refined law's "never below what the path has demonstrably carried" assumed an undamped demonstration; with the damped floor, P5's ceiling on that cell would have sat at 8–18 M for hours of sessions whose path also carried 24–39 M, and an encoder under an 8 M ceiling cannot demonstrate 25 M, so recovery would rest on the upward decay alone |

⚠️ So P5 is **not built**. Whether a ceiling at 8–18 M beats the constant with
its ~90 % collapses cannot be read from the belief alone — it needs the
viewer's outcome under both laws, and the 09-23 daytime viewer on that cell
reported no paint age at all. What unblocks it, in order: (1) a decision on the
guard — an undamped demonstrated maximum kept beside the damped floor, or a
time-aware damping — a `Pipe` change and FR-79's to make; (2) the two numbers
the design leaves open, `HEADROOM` and the upward-decay rate (FR-70 P1's law);
(3) a replay of the Regal 09-25 arc in `encode::sim` under both laws, before
any release; (4) the release, and an A/B on that cell with a viewer that
reports age — the same log sweep reads the after.

## Crisp at rest

Orthogonal to the dials (the P7→P8 "sharp all the time" arc):

- **Damage-gated capture** — static screens produce no frames; DXGI/WGC
  report real dirty rects, judged by area (rung-invariant).
- **Polish loop** — at rest the pump re-encodes the last frame on the
  keepalive cadence; CQ-driven VBR spends the idle budget sharpening, so
  text converges to full quality within ~1 s of motion ending.
- **Settle IDR** (`SettleKeyframeGate`) — one resync keyframe after a real
  motion burst (≥10 frames), burst-gated so caret blinks never metronome
  IDRs.

## Software encoders at 60 fps (FR-112)

The two software pumps, VP9-444 on the DataChannel and OpenH264 on the RTP track, target **60** on Linux.
A software pace brings them down to what their encoder sustains, never below the 30 they had before. Full
treatment, with the clock that made 60 reachable: [`encoders.md` § Software encoders](encoders.md#software-encoders-the-clock-and-60-fps-fr-112).
What it changes here:

| Lever | Before FR-112 | Now |
|---|---|---|
| frame rate | a constant 30 | 60, paced by `SoftwarePace` (`encode/encode_pressure.rs:242`) in 5-fps steps, never below 30: down at once, up only with 2.5 fps of headroom (P2d) |
| bitrate base | `w·h·fps·0.20` at the target | `bitrate_base_for_fps` (`encode/mod.rs:837`): the old figure at 30, ×√(fps/30) at the rate the pump runs at |
| OpenH264's per-frame budget | configured for 30 whatever the rate | `set_frame_rate`: the rate the pump runs at, the keyframe period kept at 2 s |
| the auto-fps-cap (a software encoder dropped to 30) | always | only where the tier is off: Windows and macOS by default |

⚠️ The pace steps per heartbeat window on the encode cost alone. A host whose capture is the costly stage is not
paced by it, and is limited by its own loop as before.

⚠️ **A step up waits for headroom** (P2d, `SW_PACE_UP_MARGIN_FPS`, `encode/encode_pressure.rs:255`). FR-1's
`FpsPace` was meant to hold a pace until the estimate moved a full step, but it compared two 5-fps steps, which
always differ by at least a whole step. So on 0.4.142 an estimate hovering on a step boundary moved the pace every window it
crossed (a field trace: 55 → 50 → 55 → 50 → 45 → 50, five moves in ten windows). The software pace now steps down at
once and up only when the encoder clears the higher step by 2.5 fps. The hardware pace keeps margin 0 until it
is measured on hardware ([#2432](https://github.com/gjovanov/roomler-ai/issues/2432)).

## Above 60 fps (FR-119) — the ceiling law and the pipe, per rate

A viewer may ask for 120 or 144 fps, and the agent grants a rate only where the host has shown it can sustain
it ([`remote-control.md` §5.4](remote-control.md#54-above-60-fps-the-viewer-asks-the-host-proves-it-fr-119)).
Here is what the rate controller does differently above 60. At and below 60, every ceiling is byte-identical
to before.

**The rate law.** The ceiling is computed at `rate_law_fps` (`encode/rate_profile.rs:184`): the rate itself up to
60, and above that the 60 fps rate scaled by `(fps / 60)^α`, with `HIGH_FPS_ALPHA = 0.8`
(`encode/rate_profile.rs:179`). The band's top grows the same way. α is the upper envelope of FR-119 P0(d)'s
offline measurement, where α ran from about 0 to 0.8 by codec and content. A linear law would double the bits
at 120; a fixed band would halve them per frame.

| Rate | `rate_law_fps` | Bits per frame, as a share of 60 fps |
|---|---|---|
| 60 | 60.0 | 1.00 |
| 120 | 104.5 | 0.87 |
| 144 | 120.9 | 0.84 |

So 1080p at 144 gets 62.7 Mbit/s where the linear law clamped it to 48, and 2560×1600 or 4K at 144 reach the
class top, 96.7 Mbit/s.

**The pipe, per rate.** Before granting a rate the agent checks that rate's own ceiling against the session's
pipe push-back (`pipe_covers`, `encode/frame_rate.rs:297`). Since P2b-18, a pipe that covers 120's ceiling but
not 144's runs the session at 120 instead of the base rate. Only a push-back refuses: the FR-79 belief's
`capacity_bps`. The belief's floor is never read as a limit. At 60 fps the floor is the content's weight at 60,
which is always under the ceiling at 120, so read as a limit it would refuse every healthy session.

⚠️ **A keyframe is not a capacity measurement** (P2b-13, P2b-14, `Reason::KeyframeBurst`,
`encode/evidence.rs:90`). On a direct path, a window holding a blocked keyframe send is rejected as evidence.
Before that, one ~625 KB keyframe whose send blocked for 72 ms read as 70 Mbit/s of capacity on a mesh that
carried 311, and took 120 back on `pipe`.

## The media plan, in shadow (FR-70 M3a)

FR-70's M3 moves every rate and geometry decision out of the media loop into a
**plan** the loop reads. M3a (`agent-v0.4.144`+) ships the plan's producer and
runs it in SHADOW: nothing reads the plan yet, and the pump's inline decisions
are unchanged, but once per pass the FFmpeg DC pump computes the plan from the
facts it has at that instant and compares it, field by field, against the
inline value it computes later in the same pass. The retirement gate for the
inline arithmetic is the operator's standing rule — the counter must read
**fleet-zero** before M3b flips the loop — so a divergence is a measured bug in
the plan, not a regression in the field. Design and anchors:
`docs/fr/FR-70-media-pipeline-leaves-the-runtime.md` §"M3a — as built".

```mermaid
flowchart TD
    TOP["loop top: swap adoption, transport recheck"] --> SNAP["snapshot the FACTS<br/>native dims · merged target · dial caps · refine · soft cap<br/>pipe belief · last ceiling · encoder HRD · paced fps · standing target"]
    SNAP --> PROD["produce(PlanInputs, PlanKnobs) → Plan{epoch}<br/>knobs resolved ONCE per pump"]
    PROD --> GATE["inline byte gate: budget · hard · measured wait · verdict"]
    GATE -. "check_budget · check_gate_verdict" .-> CNT["plan_divergences<br/>budget · hard_budget · lag_bound · gate · fps · dims · bitrate"]
    GATE --> PACE["inline pacer"]
    PACE -. "check_fps" .-> CNT
    PACE --> CAP["capture"]
    CAP --> DIMS["inline plan_dims → plan_target"]
    DIMS -. "check_dims(dims, inputs)" .-> CNT
    DIMS -. "inputs moved since the snapshot" .-> APART["counted apart<br/>plan_lags · plan_stale_swaps · plan_stale_ticks"]
    DIMS --> BUILD["inline open at the plan's target, the controller ticked first (M3a.4)<br/>or an adopted replacement · note_built(rate, dims)"]
    BUILD --> TICK["governor.pre_encode_tick"]
    TICK -. "after_tick(target, ceiling): republish bitrate, judge the build" .-> CNT
    TICK -. "a replacement re-rated within the ceiling on its first tick" .-> APART
    TICK --> OUT["encode → send → heartbeat<br/>plan_divergences · plan_epoch · plan_lags · plan_stale_*"]
```

| plan field | what it is | the inline value it is compared with |
|---|---|---|
| `queue_budget_bytes` | FR-59 P2 / FR-74 P1 — link time at the reference rate (the pipe belief on a relay, the LAST frame's ceiling on direct) | the pump's `queue_budget` at the gate |
| `queue_hard_budget_bytes` | FR-74 P1b — the larger of the soft budget and the encoder's HRD reservoir; equal to the soft budget on a relay | the pump's `hard_budget` |
| `queue_lag_bound_ms` | the measured-wait bound that qualifies the soft budget; **0 = bytes alone trip it** (the relay's rule), which is what lets `direct_gate_trips` serve both transports | the bound the gate used |
| `fps` | the pace in force, else the target — the loop paces itself only below the target (rc.93) | `paced_fps.unwrap_or(target_fps)` |
| `dims` | `plan_dims` reduced by the resampler's rule; `(0, 0)` before the first frame | `target_dims(native, plan_target)` each frame, and the dims of every encoder the loop builds |
| `bitrate_bps` | the governor's standing target on the ladder — the one input that is itself a decision (the controller is M4's) | the rate of every encoder the loop builds by itself (the inline open, an adopted replacement), judged after the controller's next tick |
| `epoch` | moves when any other field moves | — (`plan_epoch` on the heartbeat counts distinct plans) |

What stays with the loop as a **fact**, deliberately: the bytes in flight, the
send-wait EMA and its cursors (a measurement that only reads like a decision),
the live head-of-queue age, the captured frame's own dims and the M2 pin.

Under `ROOMLERD_INFLIGHT_BUDGET=on` (FR-148 D46 B3) the relay byte gate shuts no
relayed pass. The pump still computes it as before and hands it to
`check_gate_verdict` before the budget's gate, so `plan_divergences` reads as it
does with the budget off, in every mode
([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)).

**Reading it.** The heartbeat carries
`plan_divergences = PlanDivergences { budget: 0, hard_budget: 0, lag_bound: 0, gate: 0, fps: 0, dims: 0, bitrate: 0 }`
and `plan_epoch = N`. A field's FIRST divergence of the session also logs once,
with both values:

```
WARN FR-70 M3a: the shadow plan diverged from the pump's inline value (first time this session for this field)
     field="dims" plan="(1920, 1200)" inline="(944, 590)"
```

Read the line, not the count: it names the field and the two numbers.

Three things are counted **apart** from the divergences, because none is a bug
in the plan, and each logs its first occurrence once:

| heartbeat field | counts | since |
|---|---|---|
| `plan_lags` | a geometry difference whose INPUTS moved between the plan's snapshot and the inline read: a native-dims change under a live session (a display-mode switch), a resolution or Priority write landing in between, a viewer joining a shared pipeline, a refine flip or a soft-cap move. The log line names the input that moved (`moved="native"`, `"target"`, `"priority_cap"`, …) | FR-70 M3a.3 |
| `plan_stale_swaps` | a replacement adopted after the plan moved while it opened | FR-70 M3a.1 |
| `plan_stale_ticks` | a replacement adopted while the plan held, whose rate the controller moved on its very next tick within the frame's ceiling (a congestion decrease, often set off by the replacement's own IDR) | FR-70 M3a.3 |

The gate reads `plan_divergences` alone. A geometry divergence there had its
inputs hold, and a bitrate divergence is an encoder built at a rate the plan
did not stand behind: an inline open re-rated on its first tick, a replacement
built above its frame's ceiling, or one re-rated on a tick that did not move.
Each of those is a plan bug and blocks M3b. ⚠️ A `pipe_bps` TTL expiry between
the two reads would still read as a budget divergence: no counter classifies
the budget fields yet, and none has been seen.

**What a full rebuild is built at (FR-70 M3a.4).** The pump asks
`encode::plan::frame_step` (`agents/roomlerd/src/encode/plan.rs:974`) whether a
frame is served by the live encoder, dropped, or opened for. An open is for the
frame resampled to the plan's target, never to M2's pin, and
`encode::plan::open_rate` (`plan.rs:1024`) ticks the controller first, under the
ceiling of the dims it opens at, so the encoder opens at the rate the plan
holds when its first frame encodes; the pass's own tick is then skipped. A
keepalive of a frame the capture scaled under a cap since raised cannot reach
the plan's geometry: it waits for a fresh capture out of an allowance of its
own, 25 keepalives (about 1.5 s) refilled with the three-frame budget at each
cap change (`encode::plan::StaleBudget`, `plan.rs:915`), and only then spends
that budget and opens at its own size, as before. The AC10 week from
2026-11-11 found the two shapes this closes: a capture flapping between one
display and the virtual screen, whose rebuild opened at the virtual screen's
native size behind a `Native` pin, and an idle screen whose keepalives took the
rebuild at the old size just after a swap up was adopted. The cells are
`a_rebuild_racing_a_dims_swap_opens_at_the_plan_s_size_and_rate`,
`a_rebuild_through_a_native_geometry_flap_opens_at_the_plan_s_size_and_rate`,
`an_inline_open_is_built_at_the_rate_the_plan_holds_when_its_first_frame_encodes`
and, for the wait's bound,
`a_capture_that_never_reaches_the_plan_s_geometry_still_gets_an_encoder`, in
`encode::plan::tests`. ⚠️ What it leaves: after a swap to a larger geometry is
adopted on a still screen, a backend that does not capture again (DXGI-direct
captures again only on a cap lifted to none) encodes nothing for about 1.7 s
and then opens at the old size, as before, which the shadow counts as a `dims`
divergence. Design and anchors: FR-70 §"M3a.4".

⚠️ **No kill switch, on purpose.** The shadow changes no decision, takes no
await, adds no env read to the frame path (every knob is resolved once per pump
into `PlanKnobs`) and logs at most seven lines per session; a switch would be
the twelfth of the kill switches FR-70 exists to delete, guarding nothing.
⚠️ The VP9-444 pump and the RTP pump run no shadow (M3b decides whether the
software pump reads the same plan; the RTP pump has no governor).

## The shadow rate controller (FR-63 B1)

Everything above is eight estimators of one quantity — the pipe's rate —
composed with "the lower of" rules (the occupancy AIMD, the blocked-send
goodput, the FR-35 learner and pair memory, FR-59's floor relief / arrival clamp
/ drain / seed contradiction / remembered-rate opener, the FR-15 age loop). FR-63
([`fr/FR-63-one-delay-based-rate-controller.md`](fr/FR-63-one-delay-based-rate-controller.md))
replaces them with **one** delay-based controller, `encode::ratectl::Controller`
(`agents/roomlerd/src/encode/ratectl.rs`), and does it the way the rate-control
plan requires — **shadow-first**. B1 (`agent-v0.4.188`+) builds the controller
and runs it beside the live governor, emitting what it *would* decide into six
`shadow_*` heartbeat fields. It drives nothing; B2 flips it live on a fleet-log
flip criterion, B3 deletes the eight.

⚠️ **Containment is the whole safety property of this phase.** The governor owns
the controller as `shadow: Option<ratectl::Controller>` (`governor.rs`), fed only
through its methods at the five live positions and read only by the `shadow_*`
accessors. No live decision is a function of anything it computes, and
`governor::tests::the_shadow_never_moves_the_live_decision` proves it: two
governors identical but for `ratectl` (Off vs Shadow), driven through the same
frames and windows, must produce byte-identical `applied_bps`, relieved floor,
pipe-state tally, link stats, goodput samples and viewer age — and a red-run that
lets the shadow write a live field fails it.

```mermaid
flowchart TB
    OPEN["open: constrained floor 200 k<br/>(NOT the 1.5 M flat pin)<br/>or 0.85 × remembered, a 10 s soft cap"] --> SU["Startup<br/>×1.08/window &lt; 50% ceiling,<br/>additive above"]
    SU -->|"full channel · hard stall ·<br/>2 windows: age level ≥ 70 ms /<br/>loss ≥ 2% / goodput &lt; 0.9×target"| OU["OverUse<br/>target = 0.85 × measured<br/>(≥ 0.5× last, ≥ floor)"]
    OU --> HO["Hold<br/>holds while the age LEVEL is elevated<br/>(not the derivative)"]
    HO -->|"2 clean windows"| UU["UnderUse<br/>climb again"]
    UU -->|"settles / reaches ceiling"| NO["Normal"]
    NO -->|"congestion"| OU
    UU -->|"congestion"| OU
    HO -->|"deep level + constrained"| PA["+1 bounded pause<br/>(never on direct;<br/>counted only, in shadow)"]
    PA --> HO
    OUT["output → apply_deadband<br/>(decrease ≤ 1/500 ms,<br/>increase ≤ 1/2 s, |Δ| ≥ 5%)<br/>→ shadow_target_bps"]
    SU -.-> OUT
    OU -.-> OUT
    UU -.-> OUT
```

`measured` is the viewer's arrival rate on a constrained path, `min(goodput, rx)`
on a direct one; an `fps == 0` window (a DERP stall) is no evidence. The path
floor is **200 kbps constrained** — the controller's thesis is that the
governor's flat 1.5 M `area_min_bitrate_bps` is a pin, not evidence — and the
governor's area floor on a direct path.

The controller is fed at the five positions the governor already instruments,
from locals the live loops computed, with no hot-path signature change:

| position | call | signal |
|---|---|---|
| `pre_encode_tick` (per frame) | `set_domain` + `observe_frame` | the resolved floor/ceiling and the send-channel occupancy (the AIMD's own signal) |
| `on_backpressure_skip` | `on_budget_skip` | the byte-budget gate tripped — a congestion sample, NOT a blocked send |
| `on_send_overflow` | `on_overflow` | a send-channel overflow |
| `note_send_stall` | `on_stall` | a blocked send (hard ≥ 1 s halves; softer is rate-limited) |
| `tick_viewer_window` | `fold_window` | the window's age / arrival rate / queue growth / goodput / fps |

The heartbeat (both DC pumps) carries, in SHADOW:

| field | meaning |
|---|---|
| `shadow_target_bps` | what the controller would be targeting |
| `shadow_state` | `startup` / `normal` / `overuse` / `hold` / `underuse`, or `off` when `ROOMLERD_RATECTL=off` |
| `shadow_measured_bps` | the controller's own pipe estimate |
| `shadow_disagree_pct` | `(shadow − live) / live` — **negative = the shadow asked for less**, the safe direction, and the quantity the flip criterion counts |
| `shadow_pauses` | bounded pauses it would have ordered (constrained only; it cannot pause anything in B1) |
| `shadow_reason` | why its target last moved |

A `RateLaw` adapter (`sim::tests::RatectlLaw`) runs the controller on the same
B0 cells as the shipped laws, so its numbers are the baseline B2 must beat: on
the recorded 213 kbps cell its peak commitment is under half arm A's 2.55 M, it
never leaves `[floor, ceiling]`, and it never pauses on a direct cell.

⚠️ **B1 is the code, not the verdict.** Whether the shadow agrees with the live
governor closely enough to flip (AC2) is a release plus a week of `agent_logs`
read per path class — a field read, not a test, and the instrument for it is the
next section's. The config-surface tribool
`ratectl = shadow|live|off` and the `live` apply seam are **B2**; B1 is env-only
(`ROOMLERD_RATECTL`), so a B1 fleet cannot be made to let the shadow steer.

### Reading the shadow — AC2's instrument (FR-63 B1b)

`scripts/rc-shadow-report.cjs` measures the flip criterion. It does not decide it:
whether the bar is right, and whether to flip, stay the operator's calls.

> ≥ 20 constrained + ≥ 20 direct sessions, and shadow ≤ live in ≥ 90 % of the
> elevated-age windows (and all four B0 fixtures green).

Since FR-148 P0 the report's verdict has a second half, the **under-drive clause**,
because the agreement above cannot fail a shadow that asks for too little (§ "The
under-drive clause" below):

> and, per path class and never pooled, spent ≤ 1.25 × shadow in ≥ 90 % of the clean
> windows at or above the class's gate (2 Mbit/s direct, 0.5 Mbit/s constrained), over
> ≥ 20 sessions per class that reached the gate; fewer sessions is INCONCLUSIVE.

```mermaid
flowchart LR
    HB["DC pump heartbeat<br/>(FFmpeg ≈2 s · VP9-444 ≈1 s)<br/>target_bps + six shadow_*"] -->|"INFO line"| DISK["the host's rolling log<br/>roomlerd.log.* · 14 days"]
    HB -->|"uploader · 5 s batches"| AL[("agent_logs<br/>every daemon and RC worker,<br/>every org · 7-day TTL")]
    AL -->|"FLEET read: mongosh outside the Mongo pod, via a port-forward<br/>$match created_at (index) → $filter msg →<br/>$unwind → $project 14 fields"| CORE["ShadowReport<br/>counters + sparse histograms"]
    DISK -->|"HOST read: node over text<br/>(evidence about that host only)"| CORE
    CORE --> OUT["per-class table · the agreement verdict<br/>· the under-drive clause per class<br/>· the overall verdict: MET / NOT MET / INCONCLUSIVE<br/>· B1's invariants · what the encoder spent"]
```

**One window is one DC-pump heartbeat.** A window without `shadow_state` came from a
build before B1 and is counted, never compared; `shadow_state="off"` is the kill
switch; an unopened shadow (`shadow_target_bps=None`) or a zero live rate is not
compared. Every other window compares the shadow with `target_bps` (the governor's
`applied_bps`, the same rate `shadow_disagree_pct` divides by): **shadow ≤ live is
agreement**, the safe direction. A window is **elevated** when
`viewer_age_ms − viewer_age_floor_ms ≥ 70 ms` — the controller's own level threshold
(`ratectl::AGE_SLACK_MS`; the self-test fails if the two drift apart). The path class
is the heartbeat's `constrained` flag, which the FFmpeg pump logs every window; the
VP9-444 heartbeat has no such field, so a VP9 session takes the `relay=` of its
"per-session ICE path detected" line and is `unknown` without one, never `direct`.
Windows classed that way are counted apart, because `relay` misses one case the
pump's flag catches (an overlay remote on a relay tier).

The agreement's verdict, the report's `Verdict:` line (`criterion.verdict` in `--json`),
is unchanged since B1b. The overall verdict reads it with the under-drive clause (below):

| agreement verdict | when |
|---|---|
| **MET** | both session clauses met and the pooled agreement ≥ 90 % over at least one elevated window |
| **NOT MET** | any clause below its bar |
| **INCONCLUSIVE** | no clause fails, but there is no elevated window to judge — zero evidence is never a pass |

The agreement clause is pooled, as the criterion words it; each class's own agreement
is printed beside it, and a class below 90 % on its own is flagged, so a pool that
passes on the strength of one class cannot hide the other. The fixtures clause is
CI's (`cargo test -p roomlerd --lib`), so the report says "not checked here" rather
than claiming it. The same pass reads two of **B1's invariants off the field**: a
`shadow_pauses` rise inside a DIRECT window (the "never on direct" rule — a count
carried over from an earlier constrained stretch is not a rise), and a constrained
shadow below its 200 kbps floor. Both must read 0.

⚠️ **The criterion judges elevated windows only, and the first field read showed why
that is not enough on its own.** On ten direct sessions on `agent-v0.4.188`
(2026-10-30) every window carried the shadow, and in all 43 clean windows it asked
for **96 % less** than the governor: it opens at the direct path's area floor
(2.46 Mbps at 2560×1600) and climbs ×1.08 per window, while the governor opens at its
content-generous ceiling (~90 Mbps). Asking for less passes the criterion. So the
report prints the clean-window disagreement under its own heading, with no
threshold of its own; the detail and the B2 question it raises are in the FR-63 spec
(§ "Day 0").

#### What the encoder spent: the reading the criterion cannot make

⚠️ **The agreement criterion can fail a shadow that asks for too much, never one that
asks for too little.** Pinned at its floor, the shadow scores 100 %:

| clause | why it cannot catch a shadow asking for too little |
|---|---|
| sessions | counts sessions, whatever the shadow asked for |
| agreement | `shadow ≤ live`, and the floor is one the live AIMD holds too: both take the same `floor_bps` (`governor.rs:1150`, `:1167`), and on a constrained path the shadow's 200 k (`ratectl.rs:293-294`) is the live floor's default stop (`encode/mod.rs:401-406`). The shadow also shares the live ceiling (`ratectl.rs:298`, `:318`) |
| fixtures | the four fixtures run the shipped laws. `RatectlLaw`'s three tests (`sim.rs:1843-1906`) assert a domain, an upper bound and no pause on direct, and a shadow pinned at its floor passes all three |

The self-test pins the result: pinned at its floor, with the encoder spending 20 Mbps,
the agreement verdict is MET (the under-drive clause reads NOT MET in both classes, and so
does the overall verdict). The clean-window disagreement cannot show it either, because
`target_bps` is a cap, usually far above what the encoder spends: the live cap climbs
past 200 Mbps on a desktop sending 5 kbps. So −96 % reads the same on a resting desktop
that lost nothing and on a scroll spending 14 times the shadow's target.

So the report also compares the shadow with what was actually sent. `bytes_written` is
the send task's running total of whole frames the transport took (`peer.rs:7389`,
`:4341`), on both heartbeats (`peer.rs:10173`, `:5331`):

```mermaid
flowchart LR
    HB["heartbeat<br/>bytes_written + its timestamp"] --> SAME{"same pump,<br/>counter did not fall,<br/>0.5 s ≤ gap ≤ 10 s?"}
    SAME -->|no| NR["no rate: counted apart,<br/>never read as a low one"]
    SAME -->|yes| RATE["spent = Δbytes × 8 / gap"]
    RATE --> OVER{"spent > the higher of the<br/>shadow's targets at the two ends?"}
    OVER -->|yes| CAP["over: it would have<br/>capped real spending"]
    OVER -->|no| FIT["covered: the window's<br/>average fit under it"]
    CAP --> CUBE["one sparse cube per age class:<br/>first 120 s or later · spent · how far over ·<br/>encoder · shadow state"]
    FIT --> CUBE
```

| reading | what it proves | what it does not |
|---|---|---|
| `spent > shadow`, clean window | the shadow would have capped spending the path was carrying | that the picture needed those bits. Every FFmpeg backend but NVENC anchors `bit_rate = maxrate` (`ffmpeg/encoder.rs:1887-1896`), so a high target can also draw spend, which is why the report splits by encoder. A small excess can be overshoot, hence the "by > 25 %" column |
| `spent ≤ shadow` | the window's average fit | that no burst inside the window was capped |
| the encoder's demand | `spent` bounds it from below | nothing bounds it from above: the heartbeat has no maxrate field (`peer.rs:10059-10074`), and a new field cannot help a read of logs already written |

The live cap cannot hide a bite: while `shadow < live`, a window capped by live spends
about live, which is over the shadow. Elevated windows are kept out of the table, since
spending over the shadow there is the safe direction; a window with no viewer age is
counted beside it, not judged clean. The table applies no bar of its own; the under-drive
clause below reads it at one gate per class. Each row carries the sessions, the share over
and the share over by more than 25 % at its spend or more, so any gate can be read off one
report, and `--json` keeps the whole cube.

**First read, 2026-11-02.** Every shadow heartbeat in one dev box's service log since
`agent-v0.4.188`: 20 sessions, 235 windows, all direct, all NVENC. It is evidence about
that host only. The last column is a counterfactual over the same lines, a shadow that
never left its opening target:

| spent, Mbps | clean windows | spent > shadow | at this spend or more: sessions | over | over by > 25 % | pinned shadow: over |
|---|---|---|---|---|---|---|
| < 0.5 | 143 | 0 | 19 | 8/200 = 4.0 % | 6/200 = 3.0 % | 14/200 = 7.0 % |
| 0.5 – 1 | 35 | 0 | 9 | 8/57 = 14.0 % | 6/57 = 10.5 % | 14/57 = 24.6 % |
| 1 – 2 | 8 | 0 | 8 | 8/22 = 36.4 % | 6/22 = 27.3 % | 14/22 = 63.6 % |
| 2 – 5 | 11 | 5 | 3 | 8/14 = 57.1 % | 6/14 = 42.9 % | 14/14 = 100 % |
| 10 – 20 | 1 | 1 | 1 | 3/3 = 100 % | 3/3 = 100 % | 3/3 = 100 % |
| 20 – 50 | 2 | 2 | 1 | 2/2 = 100 % | 2/2 = 100 % | 2/2 = 100 % |

- **The −96 % was mostly a gap between two caps.** In 192 of the 200 clean windows the
  shadow's target covered the spend and the session would have lost nothing.
- **Where the encoder worked, the bite was hard.** One session's second heartbeat spent
  40.7 Mbps against a 2.87 M target, ×14, on a path that carried it with a clean age.
- **A bar over all clean windows has no power.** B1 scores 96 % and the pinned shadow
  93 %, because resting windows dominate. Only a gate on the spend separates a working
  controller from a pinned one, which is why the table is binned.
- **It says nothing yet about after the climb.** All 8 fell within 14 s of the pump
  opening, with the shadow still in `startup`, and no session on that box ran past 59 s.
  The fleet read's "after the first 120 s" column is what answers it.
- **The derived rate is real.** It sits within ×2 of `shadow_measured_bps`, the viewer's
  own arrival rate on most windows, in 197 of 215 windows. The report prints that control
  on every run.

#### The under-drive clause: the verdict's second half (FR-148 P0)

The wording recommended on #1243 on 2026-11-02 is now part of the report's verdict (the
FR-63 spec, § "Open decisions", holds the same text with the reasons). Its numbers were
published before the seven-day read and FR-148's owner fixed them, so they are named
constants, `UNDERDRIVE_*` (`scripts/rc-shadow-report.cjs:115-119`), never tuned to fit a read:

| number | value | why, as published |
|---|---|---|
| the gate, direct | spent ≥ 2 Mbit/s | the direct floor is 1.5 to 4 M, so most lower spend cannot be capped at all; 186 of the first read's 200 clean windows sat under it |
| the gate, constrained | spent ≥ 0.5 Mbit/s | the constrained floor is 200 k against a ~3 M nominal cap. Not measured when published: no real constrained line had been read |
| the ratio | spent ≤ 1.25 × shadow | a small excess can be overshoot |
| the bar | ≥ 90 % of the gated windows, ≥ 20 sessions per class | the agreement's own numbers, so there is no new one to defend |

How it is read:

- **The sample is the spend view's.** A clean window with a rate counts when it spent its
  class's gate or more. `spent`, `shadow` (the higher of its targets at the window's two
  ends) and the class are the table's. Each gate is a row's lower edge and ×1.25 is a
  column's, so the clause is read off the same cube.
- **A pump's first 120 s count, on purpose.** B1's opener is what the clause is expected to
  fail first. The report prints the opening and later split beside the clause, for
  information: it decides nothing.
- **A session counts as the agreement counts one** (`--min-windows` applies), when it gave
  the class at least one clean window at the gate. With fewer than 20 such sessions the
  class reads INCONCLUSIVE whatever its share: never a pass, and never a fail.
- **Per class, never pooled.** The `unknown` class (VP9-444 without its ICE line) is judged
  at the lower gate, 0.5 Mbit/s, and printed. It never decides.

| overall verdict | when |
|---|---|
| **MET** | the agreement verdict is MET and the clause is MET for both `constrained` and `direct` |
| **NOT MET** | the agreement verdict is NOT MET, or either class's clause is NOT MET |
| **INCONCLUSIVE** | otherwise: a class with fewer than 20 sessions at its gate, or an agreement with no elevated window |

The agreement and its `Verdict:` line read the same numbers on the same input as before.
After them the report prints the clause, one line per class (the clean windows at the gate
or more, how many spent ≤ ×1.25 the shadow, the sessions, the opening and later split, the
status), then `Overall verdict:`. `--json` adds `underDrive` (the bar, and each class's
counts) and `overall`; no existing key moved. On the published row 2 (20 + 20 synthetic
sessions pinned at 200 k and 2.46 M while the encoder spends 20 Mbit/s) master's file read
MET. It now reads agreement MET, the clause NOT MET in both classes (0 of 40 gated windows
within ×1.25, from 20 sessions each), and overall NOT MET.

**Why the fleet read is server-side.** The plan assumed the API could serve it
(`list_for_agent(?session_id=…)`). It cannot: `GET /api/tenant/{tid}/agent/{aid}/logs`
takes only `limit`, clamped to 500 recent batches per agent — about 40 minutes of a
session. An exec sweep of host logs is worse than incomplete: it reaches only
exec-enabled, online hosts of one org, which is a biased sample of exactly the
proportion AC2 asks for. `agent_logs` holds the whole population, because every
daemon and every RC worker runs the uploader (a Windows host's RC pump runs in its
console-session worker, `roomlerd run --supervisor scm`, which uploads like the service
does).

**The fleet read is bounded by construction**, because an unbounded read inside the
Mongo container OOM-killed production Mongo once (2026-10-26). Its client now runs outside
that container, through `scripts/prod-mongo-read.sh` ([production-mongo-reads.md](production-mongo-reads.md)),
and the server still runs the query inside it:

| bound | how |
|---|---|
| time | a window of at most 8 days (`agent_logs` keeps 7), read one day per query, each with `maxTimeMS` 240 s |
| plan | `$match` on `created_at` + `$sort` served by an index — `IXSCAN created_at_1` for the fleet, `tenant_id_1_source_1_created_at_-1` (backward) for one org; `explain` shows no blocking SORT |
| memory | only streaming stages after the index scan, `allowDiskUse: false`; mongosh holds one row at a time and folds it into counters and sparse histograms. 2,000,000 synthetic rows over 2,000 sessions peak at 14 MiB of heap |
| data | only 14 fields are projected — never an address, a name or another line |
| credentials | from the database's secret into mode-600 files that the client container reads into its own environment: never argv, never `docker inspect`, never the output |
| consent | `--kubectl` / `--mars` refuse without `--yes`; the mongodb pod's restart count and its container's memory are printed before and after, and a restart or an OOM kill fails the run (exit 3) |

**Rehearsed, not assumed.** `--to-ejson` turns real log text into `agent_logs` batches
exactly as the uploader stores them (an integer an Int64, a `?Debug` value its printed
string, a lone quoted value its text), and `--docker NAME --seed FILE` runs the fleet
read against a throwaway `mongo:7` — the image production runs. On 2,274 lines (a dev
box's real 2026-10-29 heartbeats, all pre-B1, plus synthetic B1 lines) the Node read
over the text, the mongosh read over the seeded batches, and the Node read over the
fleet read's own `--rows` output agreed exactly on every class, the pool, the
criterion and the invariants.

The spend reading was rehearsed the same way on 2026-11-02, on the 235 real lines of the
table above. Master's file printed no spend section and its rows held no `bytes_written`.
The file as pushed printed the table, and the same three reads agreed exactly, cube
included, there and again with 17 synthetic lines for what that log does not hold (a
constrained session, VP9-444 with and without its ICE line). `explain` still reads
`IXSCAN created_at_1` with no SORT stage, and `rc-shadow-report.sh` did not change.

The under-drive clause was rehearsed the same way on 2026-11-16, on 2,979 invented lines in
27 batches: 48 sessions, constrained ones under-driven in their opening, direct ones covered,
five direct sessions under the gate, and VP9-444 with and without its ICE line. The mongosh
read, the Node read of its own `--rows` output and the Node read of the text agreed exactly
on every class, the pool, the criterion, the invariants, the clause and the overall verdict:
agreement MET; constrained 180 of 1,360 gated windows within ×1.25 from 21 sessions, NOT MET;
direct 1,300 of 1,300 from 20, MET; overall NOT MET. That run found `--docker` without an
`RC_SHADOW_*` option handing `docker exec` an empty container name; the runner now passes no
`-e` argument at all in that case.

```bash
node scripts/rc-shadow-report.cjs --self-test                       # 102 cases, incl. the bar's edges, red runs, the spend's edges and the clause's
node scripts/rc-shadow-report.cjs roomlerd.log.2026-10-30 …          # a host's logs: evidence about that host only
bash scripts/rc-shadow-report.sh --mars --yes --since 2026-10-30T14:21:00Z   # the fleet read AC2 is judged by (--until: now)
bash scripts/rc-shadow-report.sh --docker NAME --seed seed.ejson     # the rehearsal
node scripts/rc-shadow-report.cjs rows.ndjson                        # a read's --rows output, offline: the same verdict
```

FR-148's path report reads the same `agent_logs` per session: what it opened at beside what its pair had carried, the plan beside the encoder, what the path carried while bytes waited, a change of path, and whether the picture left the slow-link profile (`bash scripts/rc-path-report.sh --mars --yes`, [production-mongo-reads.md](production-mongo-reads.md) §8). One device's window reads line by line with `--timeline` (§8a there).

⚠️ **The window starts at `agent-v0.4.188`** (published 2026-10-30 14:21Z), the first
release with B1. `agent_logs` deletes a batch 7 days after it arrives, so a window
anchored at the release loses its first hours the moment it turns 7 days old: read it
during its 7th day (`--until` = the time of the read), not after.

## Delivery samples (FR-148 P0) — what the path carried, and whether the sender was waiting

FR-148 P0, step 2 (#3953). **A shadow: nothing reads these numbers.** No rate, pacing or
queue budget changes, and neither the viewer nor the wire does. Each data-channel pump
cuts its session's SCTP association into **delivery samples** of about 2 s, marks each
one by whether bytes were waiting while it ran, and writes one line of its own every
10 s. The heartbeat lines stay as they were.

**Why the transport, not the write.** The goodput estimator (`encode::goodput`) times
the pump's `dc.send()`, which returns once SCTP has queued the bytes in its pending
queue of 128 KiB: it says how fast SCTP took them, and it is silent whenever the queue
is downstream ([§ V3b](#v3b--one-belief-about-the-path-shadow-since-agent-v0498-one-consumer-since-v5b)). A
sample reads the association's **acknowledgement clock** instead, two counters a patch
of the vendored webrtc-sctp keeps where the events happen:

| Counter | Counted in | Means |
|---|---|---|
| acknowledged bytes | the SACK handler (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:1518`) | user data the peer acknowledged it received, each chunk once, over every stream of the association: by a gap block, or by the cumulative point. A partially reliable chunk is left out when a SACK reported it missing, or when it was outstanding as the retransmission timer expired, and the cumulative point then passed it: a FORWARD TSN skipped it (`DeliveryCounters::loss_mark`, `crates/vendored/webrtc-sctp/src/association/delivery.rs:41`) |
| fills and empties | the pending queue (`crates/vendored/webrtc-sctp/src/queue/pending_queue.rs:135`, `:530`) | how many times the queue of written, not yet sent chunks went from empty to holding bytes, and back |

The agent reads both without the association's lock (`Association::delivery_clock`,
`crates/vendored/webrtc-sctp/src/association/mod.rs:638`), through the peer
connection's SCTP transport (`RTCSctpTransport::delivery_clock`,
`crates/vendored/webrtc/src/sctp_transport/mod.rs:474`, a patch of the vendored
webrtc). That accessor never waits: `stop()` holds the transport's slot across the
association's close, so a held slot answers `None` and the sampler keeps the clock it
already has.

```mermaid
flowchart LR
  P["the pump's write<br/>dc.send()"] --> Q["SCTP pending queue<br/>counts fills, empties"]
  Q --> F["in flight<br/>(cwnd, rwnd)"]
  F --> V["the viewer"]
  V -- "SACK" --> A["SACK handler<br/>counts acknowledged bytes"]
  Q -.-> C["DeliveryClock::read"]
  A -.-> C
  C --> S["DeliverySampler::heartbeat<br/>one call per pump heartbeat"]
  S --> L["the path delivery line<br/>every 10 s"]
```

**A sample** runs between two readings at least 1.5 s apart (`SAMPLE_MIN`): the first
heartbeat that far after the last reading closes it. That is about 2 s on both pumps:
the FFmpeg pump beats every 2 s, and the VP9-444 pump every 30 encoded frames (about
1 s at 30 fps), so every other one of its beats closes a sample. Samples tile the time
from the association's first reading on, with no gap and no overlap. A new association
is a new clock, and no sample spans the two. Because the queue counts its own fills and
empties, a sample is classed right whatever its length and however late a heartbeat
runs (`agents/roomlerd/src/media_delivery.rs:227`):

| Class | When | What it says about the path |
|---|---|---|
| **backlogged** | the queue held bytes at the sample's start and never ran empty in it, and some bytes were acknowledged | it carried this rate while bytes waited: **evidence about capacity** |
| **application-limited** | the queue was empty at the start, or ran empty at least once: the path took everything offered | a **lower bound**, which never lowers a capacity |
| **stalled** | the queue held bytes throughout and nothing was acknowledged | **evidence about stalls, never about capacity** |

Calling a sample application-limited when in doubt is the safe direction: such a
sample can only raise a floor.

**The line** is `path delivery (FR-148 P0, shadow, ≈10s)`, at INFO, written by
`media_delivery::emit` (`agents/roomlerd/src/media_delivery.rs:1117`) at the first
heartbeat 10 s after the last line (`LINE_EVERY`), and once more when the pump ends with
samples not yet written. A test pins its field names (`the_line_names_its_fields`):

| Field | Meaning |
|---|---|
| `session_id` | the session |
| `span_ms` | the milliseconds the line's samples cover |
| `delivery_samples` | `(backlogged, application-limited, stalled)`: samples per class |
| `backlogged_bps` | `Some((lowest, over all their bytes, highest))`, bit/s, over the backlogged samples; `None` when there was none |
| `app_limited_bps` | the highest application-limited sample's rate, bit/s; `None` when there was none |
| `stalled_ms` | the milliseconds of stalled samples |
| `acked_bytes` | user-data bytes acknowledged over the line's samples, every stream of the association |
| `ack_ms` | `Some((session, lowest, highest))`, ms: the session's least acknowledgement delay, and the lowest and highest least of the line's samples that count; `None` when none counts ([§ The standing queue](#the-standing-queue-what-a-tcp-carrier-holds)) |
| `standing_ms` | `Some((lowest, highest))`, ms: how far those samples' least stood over the session's least |
| `queued` | how many of the line's samples were queued: they count, and stood 250 ms or more over the session's least |
| `inflight` | `(lowest, highest)`: the bytes in flight at a sample's close |
| `cwnd` | `(lowest, highest)`: the congestion window at a sample's close, bytes |
| `ssthresh` | the slow start threshold at the last sample's close, bytes |
| `rwnd_low` | the lowest peer receive window at a sample's close, bytes |
| `pending_high` | the most bytes waiting in SCTP's pending queue at a sample's close |
| `loss` | `(fast retransmissions, T3-rtx expiries)` over the line's samples |

The fields from `ack_ms` on are the standing queue's, from SCTP's gauges. They follow
when a sample of the line was read with the gauges, and are left out when none was:
the sampler fetches the gauges beside the clock, and a sample is read with them only
when both its start and its close took them. None of them carries an address. With
`ROOMLERD_INFLIGHT_BUDGET` not `off`, FR-148 D46's fields close the line after them
([§ The in-flight budget](#the-budgets-fields)).

**Its cost** is at most 360 lines a session hour per pump. In the daemon's log-file
format a busy line is 435 bytes and an idle one 368: 156,600 and 132,480 bytes a session
hour, measured by the same test. A busy line without the gauges is 264 bytes, as before
them. The VP9-444 pump's heartbeat counts frames, so a pump that encodes nothing writes
nothing until frames flow again; its next sample spans the quiet time and reads
application-limited.

**What a healthy line looks like.**

| Screen and path | Reads |
|---|---|
| an idle screen | every sample application-limited: `delivery_samples=(0, 5, 0)` at the FFmpeg pump's 2 s beat (the VP9-444 pump beats by frames, so it can write fewer, longer samples), `backlogged_bps=None`, `app_limited_bps` about what the still screen sends (its frames and the other channels), `stalled_ms=0` |
| a busy screen on a path with room (a LAN, a fast direct pair) | mostly application-limited: the encoder's rate is under the path's, so `app_limited_bps` is about the encoder's rate. A sample is backlogged only if the queue held bytes for its whole 2 s, which a keyframe's burst alone does not do, so backlogged samples are rare |
| a busy screen on a path at its limit (a relay, a slow link) | backlogged samples whose lowest, overall and highest rates sit close together near the path's rate, `stalled_ms=0` |
| a stall | `stalled_ms` above 0, and the third count of `delivery_samples` with it: bytes waited and the peer acknowledged none |
| a busy screen over a carrier that holds a queue (TURN over TCP) | `queued` above 0 and `standing_ms` in the hundreds or thousands, while the samples may read application-limited ([§ The standing queue](#the-standing-queue-what-a-tcp-carrier-holds)) |

**What a sample cannot tell.**

- A sample partly stalled reads as a low backlogged rate: the class covers the whole
  sample (the shaped link's first runs showed it, below). FR-148 §4.1 keeps FR-71's
  classifier and FR-79's gate for stalled windows.
- A viewer that stops reading closes SCTP's receive window. The queue stays full, and
  the sample reads backlogged or stalled though the path is fine.
- A pump that stops at its own mark (`dc_buffered_high`: 256 KiB on a constrained relay,
  1 MiB otherwise) under a wide congestion window leaves the queue empty while the path
  is full. Such a sample reads application-limited; its rate is still a valid floor.
  Under `ROOMLERD_INFLIGHT_BUDGET=on` a relayed session's VP9-444 pump sheds nothing at
  its mark: the budget's hold stops it before capture instead
  ([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)).
- A path that dies while nothing is offered reads as idle (application-limited) until
  the queue fills.
- The acknowledged bytes are the association's: video, and the control, input and file
  channels beside it.
- On a partially reliable channel the count is close, not exact. The viewer's unordered
  opt-in opens video with `maxRetransmits: 0`; the vendored SCTP marks each such chunk
  abandoned at its first send and sends a FORWARD TSN after nearly every SACK, over
  everything then in flight. A lost chunk that a later one follows is reported missing
  and left out. A lost chunk that nothing follows before such a FORWARD TSN reaches the
  viewer is never reported missing, and it is counted: SCTP does not tell a sender which
  skipped chunks arrived. The lossy shaped-link cell measures that share (below): from a
  twentieth to over a third of the loss, depending on the timing. A chunk
  reported missing that then arrives late is left out, which reads low. On a reliable
  channel the count is exact: a FORWARD TSN never skips its chunks.
- A starved runtime runs its heartbeats late. The counts stay exact, but a sample
  stretches.
- Not sampled: the RTP pump (its media rides SRTP, not SCTP), and a shared pipeline's
  followers, whose packets the owner's pump fans out over their own associations.

**The switch** is `ROOMLERD_PATH_MODEL` (since P1 also the config key `path_model`), read
once per pump at its start through `tunnel_core::env::node_env`
(`agents/roomlerd/src/media_delivery.rs:157`). `off`
reads no clock, takes no sample and writes no line, which a test shows
(`off_takes_no_sample_and_writes_no_line`). Any other value, or none, samples. Since
FR-148 P1 it reads three values, `off`, `shadow` (the default) and `on`
([§ The switch](#the-switch-d28)), and it is deleted in P5. Its ledger row is `p0:PATH-1`
(`docs/plans/rc-media-debt.tsv`); the two pumps' calls are `p0:PATH-2` and `p0:PATH-3`.

| Where | Anchor |
|---|---|
| the sampler: readings, samples, the line | `agents/roomlerd/src/media_delivery.rs:516` (`DeliverySampler`), `:618` (`heartbeat`), `:719` (`on_reading_with`), `:789` (`take_line`) |
| the standing queue: the rule, the one take, the reading, the fold | `agents/roomlerd/src/media_delivery.rs:95` (`QUEUED_OVER_MS`), `:117` (`counts`), `:692-701` (`DeliverySampler::observe`, the one take), `:760` (`judge`), `:441` (`Fold::add_queue`) |
| the gauges | `crates/vendored/webrtc-sctp/src/association/gauges.rs:134` (`Gauges`), `:179` (`take`), `:187` (`peek`); through the transport, `crates/vendored/webrtc/src/sctp_transport/mod.rs:482` (`RTCSctpTransport::gauges`) |
| the VP9-444 pump | `agents/roomlerd/src/peer.rs:3978` (made), `:5396` (drained, after the heartbeat line) |
| the FFmpeg pump | `agents/roomlerd/src/peer.rs:6546` (made), `:10361` (drained, after the heartbeat line) |
| the clock | `crates/vendored/webrtc-sctp/src/association/delivery.rs:93` (`DeliveryClock`), `:101` (`read`) |

**The tests** (`cargo test -p roomlerd --lib -- media_delivery`):

| Test | Shows |
|---|---|
| `a_queue_that_held_bytes_throughout_and_delivered_is_backlogged`, `a_queue_that_ran_empty_once_is_application_limited`, `bytes_that_waited_with_nothing_acknowledged_are_a_stall`, `an_idle_association_is_application_limited_not_stalled` | the three classes, and an idle association |
| `samples_tile_the_readings_from_the_first_on`, `the_line_comes_every_ten_seconds_and_folds_its_samples`, `a_dropped_sampler_writes_its_last_samples` | the boundaries: the first reading, `SAMPLE_MIN`, the line's fold and its 10 s |
| `an_idle_trickle_yields_only_application_limited_samples` | the jupiter chain of 2026-11-02 as a cell: a still screen's keepalives give only application-limited samples, so none of them can stand for the path |
| `shaped_link::backlogged_at_{0_4,2,8}_mbps_reads_the_link`, `shaped_link::a_trickle_at_{0_4,2,8}_mbps_is_application_limited` | two in-process SCTP associations over a link the test shapes (serialised at its rate, 10 ms each way, a drop-tail queue of 200 ms, a viewer window of 7/8 of that queue, and messages that fit it): a writer that always has more reads backlogged at the link's rate within 20 %; a 164 kbit/s trickle reads application-limited at every rate |
| `shaped_link::a_new_association_starts_a_new_sample` | a new clock starts over, and a held slot keeps the clock the sampler has; the new association's gauges come with it, and the session's least starts over |
| `shaped_link::a_lossy_unordered_channel_counts_what_the_viewer_took` | an unordered `maxRetransmits: 0` channel, as the viewer's opt-in opens it, writing 1,000-byte messages over the 2 Mbit/s link while one full DATA packet in ten is lost: at each sample's end the clock has counted at least nine tenths of what the viewer took and less than that plus two thirds of what was lost. Of about 215,000 bytes lost by the third sample's end, four runs on a Windows box counted 11,000 to 21,000 more than the viewer took, and two on CI's Linux runners, under the whole suite, 75,000 and 70,000. Before the clock left lost chunks out it counted 2,128,000 against 1,920,000 taken, with 216,000 lost |

The fork's own tests (`src/association/association_internal/association_internal_test.rs`,
`test_delivery_clock_*`) pin the rule chunk by chunk: a chunk reported missing that a
FORWARD TSN then passes adds nothing, a chunk acknowledged by a gap block counts once,
a chunk out at a retransmission timeout adds nothing, and an abandoned chunk nothing
reported missing counts. The standing queue's cells are in
[§ The standing queue](#the-standing-queue-what-a-tcp-carrier-holds).

The shaped link proves the classes and the rate on a known pipe. It does not prove a real
carrier's loss, jitter, reordering or stalls, a browser's SCTP at the far end, or DTLS
and ICE beneath SCTP: those are FR-148 P0's standing relay cell (its step 5). Its first
runs showed the first limitation above, twice. With SCTP's default window of 1 MB,
three backlogged samples at 8 Mbit/s read 3.6, 0.9 and 6.9 Mbit/s. The likely cause,
inferred and not traced, is slow start overrunning the 200 ms queue and the
retransmission timeouts after it (1 s at least) idling the link inside the samples.
With a 48 KiB window over the 10,000 bytes the 0.4 Mbit/s queue holds, one sample read
68 % of the link during a run of the whole suite. The window is now 7/8 of the queue
at every rate (8,750, 43,750 and 175,000 bytes), and the writer's messages are a
quarter of it, at most 16 KiB: the receiver counts a message's chunks against its
window before the message is whole, and drops new data once the window is full. So
what is in flight stays under the queue, and the cells measure the sampler, not SCTP's
loss recovery.

### The standing queue: what a TCP carrier holds

FR-148 P0 (#3953), in shadow like the samples: nothing reads these fields. The classes
above cannot see a queue that a TCP carrier holds. TURN over TCP or TLS carries the
association to the TURN server over a TCP connection, which buffers what the link cannot
carry yet and loses nothing. So SCTP's congestion window grows past what the link
carries, the bytes wait in the carrier, and the queue of unsent chunks keeps running
empty: every sample reads application-limited while seconds of bytes stand on the link.
The shaped link's TCP cells showed it (`shaped_link::tcp_shape`,
`agents/roomlerd/src/media_delivery.rs:4537`), and so did the lab: on TURN over TCP 443
at 2 Mbit/s the viewer's round trip ended at 1,262 ms, against 154 ms on TURN over UDP at
the same rate.

SCTP's acknowledgement delay holds that wait. The vendored webrtc-sctp stamps a chunk
when it leaves SCTP's queue for the wire
(`crates/vendored/webrtc-sctp/src/association/association_internal.rs:1975`), and the
SACK handler stores the delay of the newest chunk sent once that a SACK acknowledges
(`:1316-1318`). The carrier's queue is in that delay; SCTP's own queue is not. The
association's gauges keep the least of those delays since the last take, beside the
congestion window, the slow start threshold, the peer's window, the bytes in flight and
pending, and the counts of fast retransmissions and T3-rtx expiries. The agent reads
them without the association's lock, through the transport, as it reads the clock.

```mermaid
flowchart LR
  Q["SCTP pending queue"] -- "stamped as it leaves" --> W["the wire"]
  W --> T["the carrier's queue<br/>(TURN over TCP: no drop)"]
  T --> V["the viewer"]
  V -- "SACK" --> D["the SACK handler<br/>the delay since the stamp"]
  D --> G["the gauges<br/>the least since the last take"]
  G -- "taken where a sample<br/>starts and closes" --> S["DeliverySampler"]
  S --> L["the path delivery line<br/>ack_ms · standing_ms · queued"]
```

Over the shaped link's TCP cells, the 69 loaded seconds of the four deep cells that had
any and the 80 seconds of the four controls, the least and the most of four runs
(`shaped_link::tcp_shape`'s own table):

| Signal | Deep queue, no loss (TURN over TCP's shape) | Controls, a queue that drops (TURN over UDP's) |
|---|---|---|
| SCTP's acknowledgement delay 250 ms or more over its least | 69 | 0 (at most 65 ms) |
| in flight at least 3 times what the second delivered in the least delay | 69 | 7 to 11 |
| the viewer's transit 200 ms or more over its least (FR-71's) | 69 | 35 to 37 |
| backlogged | 24 | 76 |

Only the acknowledgement delay read every loaded second and no control second. The
viewer's transit also holds the wait in SCTP's own queue, so it read the controls'
backlog too.

**One reader takes.** A take of the gauges resets the least delay, so the least a take
reads covers the SACKs since the take before it. The delivery sampler is the one reader
that takes. It fetches the gauges with the clock, from the same slot, and takes them
only where a sample starts and where it closes, beside the clock's reading
(`DeliverySampler::observe`, `agents/roomlerd/src/media_delivery.rs:692-701`). So a
sample's least is the least of its own SACKs. A second reader that took would split a
sample's SACKs with the sampler, and each would read a least too high. A reader that
only looks peeks (`GaugeReader::peek`), which resets nothing, as FR-148 O10's open line
is built to do. The open-time train takes only in its own tests, over associations of
their own. A new association brings new gauges, and the session's least starts over with
it.

**When a sample is queued.** A sample is queued when it **counts** and its least stands
250 ms or more over the session's least (`QUEUED_OVER_MS`, `:82`). A sample counts
(`counts`, `:104`) when it acknowledged more than `max_payload × (ms / 200 + 2)` bytes.
The session's least is the lowest least of the samples that counted since the
association started, and `standing_ms` is how far a sample that counts stood over it.

The byte rule keeps a receiver's delayed SACK out of the reading. A receiver acknowledges
the second of two packets at once, and holds the SACK of a lone packet for up to 200 ms
(`DELAYED_ACK_MS`, `:87`: the vendored SCTP's `ACK_INTERVAL`, and the hold of Chrome's
dcSCTP and of usrsctp), a wait its delay then carries. A still screen sends lone packets,
so its least can stand 200 ms or more over a busy screen's with no queue anywhere. For
every SACK of a sample to have been held, each packet came after the hold of the one
before it ran out, so the SACKs left the receiver at least 200 ms apart. While the way
back varies by less than that, at most `ms / 200 + 2` of them reach the agent in a sample
of `ms`, each for one packet of at most `max_payload` bytes of user data: 1,200 on the
agent's association, whose packets are 1,228 bytes. A sample that acknowledged more had
a SACK sent at once, whose delay has no hold in it, and the sample's least is no higher
than that delay. A receiver that holds longer leaves fewer lone packets in a sample than
the rule allows.

| A sample of | Counts above | As a rate |
|---|---|---|
| 1.5 s (`SAMPLE_MIN`) | 10,800 bytes | 57.6 kbit/s |
| 2 s (the FFmpeg pump's heartbeat) | 14,400 bytes | 57.6 kbit/s |
| 6 s (a late heartbeat) | 38,400 bytes | 51.2 kbit/s |

Until #4033 the vendored SCTP's own timer made a lone packet's delay worse still: once
its delayed SACK had fired, the timer was not started again until a SACK went out at
once, so a lone packet after it waited for the sender's retransmission timer instead.
The still-screen cells' lines counted those expiries, `loss=(0, 4)` in each of four runs,
and as a chunk sent more than once gives no delay, only one to three of a line's five
samples carried one. With the timer armed again, every line reads `loss=(0, 0)` and
every sample carries a least, 263 to 266 ms: the 200 ms hold over the cells' 60 ms round
trip.

**What the cells read** (`cargo test -p roomlerd --lib -- media_delivery::shaped_link::tcp_shape
--skip clipboard:: --nocapture --test-threads=1`, on a Windows box):

| Cell | The sampler's samples | A line |
|---|---|---|
| deep, 0.4 Mbit/s, path I | 9 of 9 loaded samples queued, standing 744 to 6,438 ms | `ack_ms=Some((107, 3652, 6545)) standing_ms=Some((3545, 6438)) queued=5` |
| deep, 2 Mbit/s, path I, the pump's skip over 256 KiB | 9 of 9 loaded samples queued, standing 792 to 953 ms | `ack_ms=Some((75, 1002, 1028)) standing_ms=Some((927, 953)) queued=5` |
| the four controls (0.4 and 2 Mbit/s, paths I and C) | none queued, standing at most 25 ms | `ack_ms=Some((63, 63, 82)) standing_ms=Some((0, 19)) queued=0` |
| a still screen, one 200-byte message a second, deep and shallow | none counts: 400 bytes a sample (200 to 600 before #4033, as late SACKs moved bytes between samples) | `ack_ms=None standing_ms=None queued=0` |

The first sample of a deep cell stands at 0: it is where the session's least comes from.
The two deep cells that run in CI assert that at least half of their loaded samples are
queued, the controls that no sample is, and the still-screen cells that no sample counts.
The still screen's lines read no queue even without the byte rule, because the session's
least then comes from the same lone packets: the cells fail on the counted sample, not on
`queued`. The rule is for a session that was busy and then went still, and the unit cells
hold that case.

**What it cannot tell.**

- The session's least is learned from the session. A session that opens onto a standing
  queue has the queue in its least, and reads less standing than it holds. A path whose
  delay rises for good in a session (a renomination to a farther relay, a route change)
  reads the rise as standing.
- A sample's least is its best moment: a queue that drains for a moment inside a sample
  reads as that moment.
- A carrier whose way back varies by 200 ms or more could let a held SACK into a sample
  that counts.
- The delay is the association's: the control, input and file channels count too.
- The same pumps as the samples: not the RTP pump, nor a shared pipeline's followers.

| Test | Shows |
|---|---|
| `a_sample_counts_past_what_lone_packets_could_carry` | the byte rule's edges: 14,400 bytes in 2 s does not count and 14,401 does; 10,800 and 10,801 in 1.5 s; 38,400 and 38,401 in 6 s; no gauges stored counts nothing |
| `a_sample_is_queued_250_ms_over_the_sessions_least` | 249 ms over the session's least is not queued, and 250 ms is |
| `the_session_least_comes_from_samples_that_count` | a session that opens on a still screen (leasts of 40 ms, its SACKs sent at once) and then loads keeps the least of the first loaded sample, 60 ms: from the still screen's, one more sample would read queued |
| `a_still_screens_held_sacks_never_read_queued` | a session busy at a least of 61 ms whose screen goes still, its SACKs held to 321 ms: the still samples read no queue, and the same least with a loaded sample's bytes is queued |
| `the_gauges_are_taken_where_a_sample_starts_and_closes_only`, `a_sample_without_a_take_at_its_start_reads_no_gauges` | one take at a sample's start and one at its close, none at a reading sooner than `SAMPLE_MIN`; a sample whose start took none reads none |
| `the_line_folds_its_samples_gauges`, `the_line_names_its_fields` | the fold over a line's samples, and the line's field names and their order, with and without the gauges |
| `shaped_link::a_peek_leaves_the_least_to_the_samplers_take` | over a real association: a peek leaves the least to the sampler's take, and a second reader that took leaves the next sample none |
| `shaped_link::tcp_shape::deep_0_4_mbps_path_i_reads_under_half_backlogged`, `deep_2_mbps_path_i_with_the_pump_skip_reads_under_half_backlogged` | the two deep cells above: the queue drops nothing, fewer than half the loaded seconds read backlogged (the TCP cells' own hypothesis), at least half the loaded samples read queued, and SCTP's acknowledgement delay stands 250 ms or more over its least on at least half the loaded seconds. That pin was on every loaded second, where the lowest reading stood 1.36 to 1.40 times the 250 ms. Under FR-148 D44 it moved to half, so that a busy runner cannot flip it. Three runs of each, alone on a Windows box, after the change: 19 of 19 loaded seconds, the lowest 344, 350 and 346 ms over the least at 0.4 Mbit/s and 343, 339 and 343 ms at 2 Mbit/s under the skip; 9 of 9 loaded samples queued; 0 of 19 seconds backlogged |
| the other four `shaped_link::tcp_shape::deep_*` | ignored: red on the TCP cells' own hypothesis, as before |
| `shaped_link::tcp_shape::shallow_*_control` | the four controls above |
| `shaped_link::tcp_shape::a_still_screen_on_the_{deep,shallow}_queue_reads_no_queue` | the still screens above: at least two samples in their 10 s, none that counts, a delay from some SACK, and no sample queued. The precondition was four samples, and the first runs took five. Under D44 it moved to two: a still screen's 200 bytes a second never pass `counts`' byte bound, so two samples show the guard holds. Three runs of each, alone on a Windows box, after the change: five samples in every run, 200 to 600 bytes each |
| `shaped_link::tcp_shape::stalls_2_mbps_path_i_lift_the_floor_over_the_link`, `stalls_under_the_pump_skip_2_mbps_path_i_lift_the_floor_over_the_link` | ignored, a measurement (D44): head-of-line stalls of 0.8 s every 3.3 s on the deep lossless link at 2 Mbit/s, path I. The viewer window after a stall carries the stall's bytes too, and `Pipe`'s floor takes every window, so the floor ends over 1.3 times the link's 1,954 kbit/s of payload. The cells pin that defect, so as gates they would protect nothing in the product. Three runs of each with `--ignored`, alone on a Windows box: 1.50 to 1.55 times the link under a writer that offers 80 % of it, and 1.77 to 1.78 times under the pump's skip at 150 %, which is 1.15 to 1.20 and 1.36 to 1.37 times the 1.3 they assert |
| `shaped_link::tcp_shape::stalls_2_mbps_path_c_lift_the_floor_over_the_link` | ignored, red: path C's window caps what the link carries in a stall, at 1.27 to 1.28 times the link in 20 s |

They add about 23 s of test time, each cell timed alone: 10.1 s for each still-screen
cell, 2.2 s for the relay pair below, 0.3 s for the peek, and the unit cells nothing that
shows. The two still-screen cells wait their turn with the TCP cells, which run one at a
time, so that line of cells grows by about 20 s. Since D44 the line runs eight cells, in
141 s on a Windows box: the two stall cells and their 20 s each left it.

**The carrier's relay transport.** The carrier line (`session carrier recorded (FR-102
P9c)`, [remote-control.md §11.9](remote-control.md#119-the-carrier-a-session-rides-fr-102-p9c))
names, right after `carrier`, what carries the agent's own TURN allocation for the
nominated pair: `relay_transport` (`relay_transport_of`,
`agents/roomlerd/src/session_carrier.rs:256`; the line, `:349`). A relay candidate names
its relayed address, which is UDP whatever carries the allocation to the TURN server, so
the pair alone cannot tell TURN over UDP from TURN over TCP or TLS. The vendored
webrtc-ice stamps each relay candidate it gathers with its TURN URL's transport and
scheme (`Candidate::relay_transport`, `crates/vendored/webrtc-ice/src/candidate/mod.rs:95`,
stamped at `crates/vendored/webrtc-ice/src/agent/agent_gather.rs:1094`), and the vendored
webrtc hands the stamp over with the selected pair
(`RTCIceCandidatePair::local_relay_transport`,
`crates/vendored/webrtc/src/ice_transport/ice_candidate_pair.rs:58`), both on the read
the carrier line makes and on the pair the selection's change handler is given
(`crates/vendored/webrtc/src/ice_transport/mod.rs:93`, `:148`).

| The nominated pair's local candidate | `relay_transport` |
|---|---|
| a relay allocated over `turn:…?transport=udp` | `udp` |
| a relay allocated over `turn:…?transport=tcp` | `tcp`. The fork gathers none today: it refuses the URL (`agent_gather.rs:981`) |
| a relay allocated over `turns:…?transport=tcp`, TLS over TCP (`tcp_turn_conn`) | `tls` |
| host, srflx or prflx | `none`: no allocation of the agent's carries the pair, though the browser's relay may |
| a relay without a stamp, a relay over a URL pair the fork does not gather (TURN over DTLS), or a type webrtc-rs could not name | `unknown`, never a guess |

Every relay the fork gathers is stamped where it is made, so a relay should not read
`unknown` today. The browser's own transport to its TURN server never reaches the agent:
a session on the browser's relay reads `none`, and its leg may be TURN over UDP, TCP or
TLS. The field is in the log only: no wire carries it and the server stores nothing of
it. A session with no nominated pair within the wait writes no carrier line, as before.

| Test | Shows |
|---|---|
| `session_carrier::tests::the_relay_transport_reads_the_local_relays_stamp_and_never_guesses` | every answer: `udp`, `tcp` and `tls` from a relay's stamp; `none` on host, srflx and prflx whatever a stamp says; `unknown` on a relay without a stamp, on TURN over DTLS, a STUN URL or a part the URL parser could not name, and on an unnamed type |
| `session_carrier::tests::the_relay_transport_spellings_are_locked_and_distinct`, `the_carrier_line_names_its_fields` | the five spellings, and the carrier line's fields in order |
| `session_carrier::tests::a_pair_on_the_agents_udp_relay_names_its_transport` | two peer connections that gather relay candidates only, from a TURN server on loopback over UDP: the agent's nominated pair reads `Relay` and `udp`, through both vendored crates |
| the fork's `candidate_relay_test.rs`, `a_relay_candidate_names_what_carries_its_allocation` | a relay gathered over a UDP TURN URL carries `(Udp, Turn)`, and a host candidate carries none (CI's vendored-ice lane) |

FR-148's path report reads both from the fleet's logs: each session's queued samples and
its relay transport, and per device its sessions by relay transport and, by carrier
class, those with a standing queue ([production-mongo-reads.md](production-mongo-reads.md)
§8).

## The open-time train (FR-148 P0): reading the path before the first frame

FR-148 P0, step 4a (#3953). **Built and tested, and run by no session yet**: step 4b
wires it into a session's open, behind a switch. Nothing a session sends, reads or logs
changes.

At a session's open the encoder takes 0.65 to 2.8 s to start, and the path carries none
of the session's media in that time. The train spends it reading the path: it writes
bytes on a stream of its own, lets SCTP's slow start step them up, stops at the first
push-back, and reads the rate from SCTP's acknowledgements. Its rules are a pure state
machine, `media_train::Train` (`agents/roomlerd/src/media_train.rs:258`). It does no
I/O, starts no task, reads no clock or environment variable, and logs nothing. Each
call of `Train::step` (`:355`) takes the time, a reading of the association's
acknowledgement clock (the delivery samples' clock, above) and of its gauges, and
whether the first frame is ready, and answers `Write(n)` or `Stopped(reading)` (`:202`).
Every byte it asks for is a byte SCTP sends, after the stop too, so what the train costs
is what it wrote, and the reading says how much.

**The gauges.** The acknowledgement clock says what the peer received. The train also
needs to know why the sender sent what it did, so the vendored webrtc-sctp keeps the
association's congestion state as gauges: one relaxed store, add or min each, where the
association already holds its lock or where the pending queue counts its own bytes. A
reader takes them without the lock (`Association::gauges`,
`crates/vendored/webrtc-sctp/src/association/mod.rs:647`; the reading is `Gauges`,
`crates/vendored/webrtc-sctp/src/association/gauges.rs:134`), and nothing in the
association reads them back.

| Gauge | Stored | Means |
|---|---|---|
| `max_payload` | once, when the association is made (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:199`) | the user data one DATA chunk carries, 1,200 bytes at SCTP's MTU of 1,228: the train's message length |
| `cwnd`, `ssthresh`, `rwnd`, `inflight_bytes` | by `store_gauges` (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:353`) when the association is made (`:200`), at the INIT ACK (`:815`), after every SACK (`:1596`) and every send (`:2073`), and at a T3-rtx expiry (`:2469`) | the windows and the bytes in flight |
| `pending_bytes` | where the pending queue adds and pops bytes (`crates/vendored/webrtc-sctp/src/queue/pending_queue.rs:139`, `:532`) | bytes written and not yet sent, over every stream |
| `fast_retransmits`, `t3_expiries` | where the association's stats count them (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:512`, `:2468`) | loss events since the association began; the train counts only those after its first step |
| `ack_delay`, `ack_delay_min` | in the SACK handler (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:1318`) | how long the newest chunk a SACK newly acknowledged took from its send to that SACK, among the chunks sent once (Karn), and the least of those since the reader's last take |

**The twin test** shows that a reader changes nothing the association sends.
`test_gauges_a_reader_changes_nothing_the_association_sends`
(`crates/vendored/webrtc-sctp/src/association/association_internal/association_internal_test.rs:1072`)
runs one scripted exchange twice, with a loss, a fast retransmission and a T3-rtx expiry
in it, the second time with a thread reading both sides' gauges as fast as it can. It
then compares every byte both sides sent, packet by packet. Before the comparison it
rewrites only what each association draws at random (`normalised`, `:729`): a
verification tag becomes the side it belongs to, a TSN its offset from its sender's
initial TSN, and the state cookie and the checksum become zeros. Each run sent 101
packets and 76,168 bytes, and the two runs were the same. The same script losing the
fourth packet instead of the third sends differently, so the comparison does see a
change of one packet. Two more fork tests pin the acknowledgement delay: the newest
chunk sent once is the one that counts (`:1131`), and a clock that went backwards
stores nothing (`:1203`).

**A step**, in the order the machine takes it:

```mermaid
flowchart TD
  A["Train::step"] --> B{"the first step?"}
  B -- yes --> W1["write the congestion window<br/>and one message more"]
  B -- no --> C["take the least acknowledgement delay<br/>of a SACK of two messages or more"]
  C --> D{"a fast retransmission or<br/>T3-rtx expiry since the start?"}
  D -- yes --> X1["stop: loss"]
  D -- no --> E{"the round ended?"}
  E -- yes --> F{"four samples or more,<br/>and its least delay stood<br/>above the least seen?"}
  F -- yes --> X2["stop: delay"]
  F -- no --> G{"held at the cap, by no window<br/>and no write budget,<br/>flat for a second round?"}
  G -- yes --> X3["stop: plateau"]
  G -- no --> E2
  E -- no --> E2{"a run of four samples in a row,<br/>every one standing, over half<br/>a round trip and 50 ms?"}
  E2 -- yes --> X2
  E2 -- no --> H{"ready, deadline,<br/>budget or stall?"}
  H -- yes --> X4["stop: a floor"]
  H -- no --> I{"a slow link's write budget spent,<br/>and all but a message back?"}
  I -- yes --> X4
  I -- no --> W2["write what keeps slow start growing,<br/>within round 1's window, the cap<br/>and the write budget"]
```

| Stop | When | The low edge is |
|---|---|---|
| `delay` | a queue stood: a delay 10 ms and one message's serialisation at the estimated rate above the least seen (`STANDING`, `agents/roomlerd/src/media_train.rs:34`; `standing`, `:630`). Either a round of four samples or more ended with its least sample standing (`ROUND_SAMPLES`, `:70`; `close_round`, `:672`), or four samples or more in a row stood, over half the least round trip and 50 ms (`EARLY_SPAN`, `:71`; `run_stands`, `:568`) | the link's rate: push-back |
| `plateau` | two rounds held at the in-flight cap, and by neither of SCTP's windows nor the write budget, each delivered less than 10 % more than the round before (`PLATEAU_ROUNDS`, `:100`; `close_round`, `:672`) | the link's rate: push-back |
| `loss` | a fast retransmission or a T3-rtx expiry after the first step (`:409`) | the link's rate: push-back |
| `budget` | the next message would pass 512 KiB (`BUDGET`, `:24`); or on a slow link the write budget is spent, and all but a message of what was written came back without a judgment (`:472`) | a floor |
| `deadline` | 2.5 s less 25 ms, or less two step intervals where steps come further apart (`DEADLINE`, `:27`; `DEADLINE_LEAD`, `:84`) | a floor |
| `ready` | the first frame is ready, and the session needs the path | a floor |
| `stall` | bytes in flight and nothing acknowledged for twice a delayed acknowledgement (200 ms) and the least round trip (`:441`) | a floor |

**What it writes.** The first step writes the congestion window and one message more,
within the budget (`:368`). SCTP sends the window at once, and the extra message is
waiting when the first SACK comes, because the vendored SCTP grows its window only
while bytes wait
(`crates/vendored/webrtc-sctp/src/association/association_internal.rs:1358`). Every
later step writes enough that bytes keep waiting until the next step: twice the most one
step saw acknowledged, and a message (`agents/roomlerd/src/media_train.rs:459`). Three
limits hold it back. Until the first round ends, what is in flight and waiting stays
within the first window (`:449`), so what that window acknowledges releases as much
again and not twice as much. Once a sample has sat 10 ms above the least (`QUEUED`,
`:40`; `sample`, `:491`), the bottleneck has queued, and an in-flight cap applies (`cap`,
`:658`): the estimated rate's pipe and a queue of a third of the least round trip (of
50 ms at least), never less than the best round's pipe and a message, nor three
messages, and rounded to the nearest whole message. The estimate (`:605`) is the
highest of three: the best whole round's delivery, the rate over the newest span of at
least 10 ms and four SACKs (`disperse`, `:507`), and the first sample's lead over the
least, read as one packet's serialisation (`pair`, `:611`).

On a slow link, one where that lead is 10 ms or more (`PAIR_LEAD`, `:50`), about
1 Mbit/s and slower, a write budget applies from the same moment (`write_budget`,
`:598`): the train writes no more than what had been acknowledged when the bottleneck
first queued and what the estimate carries in 700 ms (`WRITE_SPAN`, `:80`). A round in
which the budget held the writer back is spent, and a spent round is never read as a
plateau. Once all but a message of what was written has come back without a judgment,
the train stops on `budget` (`:472`). At 0.4 Mbit/s and 250 ms that is about 8 KB and
34 KB, so the train writes inside P-f's 64 KB for an estimate up to 1.6 times the
link's rate, however late its judgment comes, and the packets it leaves in flight carry
the run of samples that judges the queue.

**The rules, and the cell that forced each.** The rulings set the train's shape; these
are the numbers in it. The first four were found in a packet model of the vendored SCTP
before the cells ran, the rest by a cell below that failed without them:

| Rule | Anchor | Forced by |
|---|---|---|
| In flight and waiting stay within the first window until round 1 ends | `agents/roomlerd/src/media_train.rs:449` | 0.4 Mbit/s at 20 ms: SCTP's release of four packets for each SACK of two queued 77.7 ms, over P-e's 50 ms |
| A round ends when the first byte sent after it began is acknowledged | `Round::mark`, `:231` | a round counted from bytes already in flight closed in 2 ms and read 1.23 times the link |
| Twice the most one step acknowledged, and a message, wait | `:459` | 50 Mbit/s: with less waiting, slow start stopped growing between steps |
| No sample from a SACK of one message | `:393` | a lone packet's SACK may have waited for the delayed acknowledgement |
| The cap starts at a sample 10 ms above the least | `QUEUED`, `:40` | 2 Mbit/s at 80 ms, path I: two packets sent together put 4.9 ms between samples, and a cap from 5 ms held the climb at round 1's rate, a plateau at 0.11 of the link |
| The cap's queue is a third of the least round trip, of 50 ms at least | `QUEUE_FLOOR`, `:45` | 2 Mbit/s at 250 ms, path I: with half, the peak queue reached 253 ms, over P-e's 250 |
| The cap is never below three messages | `CAP_FLOOR_MESSAGES`, `:91` | 0.4 Mbit/s at 20 ms and 2 Mbit/s at 80 ms, path C: with one message in flight the train stalled, every SACK a lone packet's, on the vendored acknowledgement timer that #4033 fixed (below). Any receiver still holds a lone packet's SACK up to 200 ms, so the floor stays |
| The cap is the nearest whole number of messages | `cap`, `:658` | 0.4 Mbit/s at 80 ms, path C: a pair's round trip is the least and one serialisation, so a train that writes in pairs fills a pipe of 4.4 messages only with about 5.4 in flight; cut down to whole messages, the cap kept 4, and the rounds read a plateau at 0.74 |
| A standing queue is 10 ms and one message's serialisation at the estimate | `STANDING`, `:34` | 0.4 Mbit/s at 250 ms, path I: a pair's serialisation read 22.7 ms in round 4 with no queue at all |
| A plateau needs rounds held by the cap and by no window, and no queue | `PLATEAU_ROUNDS`, `:100` | 0.4 Mbit/s at 20 ms: three messages fill the path and no sample shows a queue, so the train ran to its deadline and wrote 118 KB, over P-f's 64; 2 Mbit/s at 80 ms, path C: rounds held by the congestion window grow one packet a round, and read as a plateau at 0.52 |
| Four samples over half the least round trip and 50 ms, every one standing, judge the queue before the round ends, and their delivery counts toward the low edge | `ROUND_SAMPLES`, `EARLY_SPAN`, `:70`, `:71`; `run_stands`, `:568` | 0.4 Mbit/s at 250 ms, path I: waiting for the round's end wrote 74.4 KB, over P-f; judged early in any round, 8 Mbit/s at 250 ms stopped on delay at 0.16; over a shorter span and with the round's own delivery left out, 2 Mbit/s at 250 ms stopped at 0.52 and 0.69 |
| The four are a run of samples in a row: one that does not stand starts it again, and so does one that comes later after the run's newest than its own queue and a message's serialisation, since the link may have idled between them | `extend_run`, `:538` | CI run 44383834538 (below): a dip sank the whole round it fell in, rounds 3 and 4 each took one, and the judgment came in round 5. Under noise, a run that passed over the samples of packets sent below the cap spanned an idle link at 2 Mbit/s and 250 ms and read 0.69, and a run across 148 ms without a SACK read 0.29 at 8 Mbit/s and 250 ms |
| A round's end judges delay only with four samples | `close_round`, `:672` | 50 Mbit/s at 20 ms, path C, under noise: one late sample, alone in its round, read as a queue and stopped the train on delay at 0.04 of the link |
| Once a slow link queued, the train writes no more than what was acknowledged then and its estimate's 700 ms, and a round that budget held is no plateau | `WRITE_SPAN`, `:80`; `write_budget`, `:598` | CI run 44383834538 (below): at 0.4 Mbit/s and 250 ms the train wrote 85,200 and 64,800 bytes while its judgment waited, over P-f |
| The write budget is a slow link's only | `write_budget`, `:598` | 2 Mbit/s at 250 ms, path I: with a budget on every link the estimate was 38 kB/s when the queue first showed, and the train stopped on its deadline at 0.45 |
| A rate from the spacing of SACKs spans 10 ms and four SACKs at least | `DISPERSION_SPAN`, `DISPERSION_ACKS`, `:56`, `:57` | 2 Mbit/s at 250 ms, path I: over one SACK it read 2.55 Mbit/s on a link that carries 1.95 |
| The deadline comes 25 ms early, or two step intervals | `DEADLINE_LEAD`, `:84` | 50 Mbit/s at 250 ms, path C: the train ran past 2,500 ms |
| The first write keeps within the budget | `:368` | a congestion window of 4 MiB (`the_first_write_keeps_within_the_budget`) |

**The reading** (`Reading`, `agents/roomlerd/src/media_train.rs:164`):

| Field | Means |
|---|---|
| `low_bps` | the best whole round's delivery rate; when a run of samples judges the queue, its delivery since the run's first sample counts too |
| `high_bps` | the rate offered in the round that pushed back, when that round's queue stood and it delivered less than it was offered; `None` otherwise, on a floor, and where it would sit below the low edge |
| `stop`, `floor_only` | why it stopped; `floor_only` is true for every stop but `delay`, `plateau` and `loss`, and then the low edge is only a floor |
| `unit` | `SctpPayloadBps`: bits a second of SCTP payload, without SCTP's headers or anything beneath them |
| `rounds` | the rounds closed |
| `written_bytes`, `wire_bytes` | the payload written, and the SCTP packets that carry it once each: 28 bytes of common and DATA chunk header a message (`PACKET_OVERHEAD`, `:108`). What DTLS, UDP, IP and a relay add beneath is the carrier's |
| `min_rtt` | the least acknowledgement delay seen |
| `ssthresh_at_start` | the slow start threshold at the first write, which says the open path (below) |
| `fast_retransmits`, `t3_expiries` | the loss events during the train |
| `duration` | from the first step to the stop |

**The two open paths.** The agent's association and the browser's both start as clients
and send INIT, and which answer the agent's end takes first is a race. The two answers
leave SCTP in different states:

| Path | The agent's end is established by | Its slow start threshold at the first write | So the train climbs by |
|---|---|---|---|
| **I** | the peer's INIT ACK, which sets the threshold to the peer's window (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:814`) | the peer's window: 1 MiB by default, 64 KiB in the window cell | slow start: each SACK adds what it acknowledged to the window while bytes wait (`:1359`), so a SACK of two packets releases four |
| **C** | the peer's COOKIE ECHO (`handle_cookie_echo`, `:896`), which sets neither | 0, the constructor's (`:169`); the peer's window is 0 until the first SACK | congestion avoidance from the first byte: one MTU more a round trip (`:1393`) |

On path C the train comes within 20 % of the link only where congestion avoidance
reaches it inside the train's time and budget: at 0.4 and 2 Mbit/s with 20 and 80 ms,
and at 8 Mbit/s with 20 ms. At 2 Mbit/s and 80 ms and at 8 Mbit/s and 20 ms that is a
floor at the link's rate, ended by the deadline and the budget. Everywhere else it reads
a floor below the link, and the floor is the association's own ramp, not the path's: a
session that opens on path C cannot send faster at first either. The ignored
measurement `path_c_ramp_to_the_link_rate` (`agents/roomlerd/src/media_train.rs:1464`)
keeps 16 messages waiting and records when the last round trip, of 200 ms at least,
first carried 80 % of the link's payload rate:

| Link | Path I | Path C | Path C's congestion window then |
|---|---|---|---|
| 2 Mbit/s, 80 ms | 529 ms | 1,866 ms | 20,344 bytes |
| 8 Mbit/s, 80 ms | 633 ms | 5,120 ms | 67,008 bytes |
| 2 Mbit/s, 250 ms | 1,562 ms | 12,062 ms | 52,272 bytes |
| 8 Mbit/s, 250 ms | 2,190 ms | not within 15 s: 2.04 Mbit/s, 26 % of the link, at 15 s | 68,236 bytes at 15 s |

At one MTU a round trip, 80 % of 8 Mbit/s at 250 ms is about 155 round trips away,
about 40 s, and 80 % of 50 Mbit/s at 80 ms about 25 s. At 50 Mbit/s and 250 ms a window
of 1 MiB holds either path under 34 Mbit/s.

**Which path a session took (O10).** How often each path wins on the fleet is not known,
so each data-channel pump writes one line at its first media write:
`path open (FR-148 P0, O10)` (`OPEN_LINE`, `agents/roomlerd/src/media_delivery.rs:1268`).
The call sits where the pump hands each media message to its send task
(`agents/roomlerd/src/peer.rs:5243` in the VP9 4:4:4 pump, `:9915` in the FFmpeg one),
and only its first call reads anything (`DeliverySampler::on_first_write`,
`agents/roomlerd/src/media_delivery.rs:849`). It reads the gauges through
`RTCSctpTransport::gauges` (`crates/vendored/webrtc/src/sctp_transport/mod.rs:482`),
which never waits for the transport's slot, with `GaugeReader::peek`
(`crates/vendored/webrtc-sctp/src/association/gauges.rs:187`), which resets nothing, so
the least acknowledgement delay stays for the reader that takes it (the fork's test,
`crates/vendored/webrtc-sctp/src/association/association_internal/association_internal_test.rs:1167`).
The answer is a pure function of the gauges (`OpenedBy::of`,
`agents/roomlerd/src/media_delivery.rs:1310`):

| `open_path` | When |
|---|---|
| `init_ack` | the slow start threshold is above 0, and no loss came first |
| `cookie_echo` | the threshold is 0, and no loss came first. An INIT ACK that arrives after the COOKIE ECHO is discarded (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:782`), so it cannot set the threshold later |
| `unknown` | a fast retransmission or a T3-rtx expiry came first: either sets the threshold, on both paths (`crates/vendored/webrtc-sctp/src/association/association_internal.rs:1442`, `:2465`) |
| `unread` | no gauges: no association, or the transport's slot was held at that moment |

The threshold tells the paths apart and the peer's window does not, since any SACK sets
the window on both. The line also carries `ssthresh`, `rwnd`, `cwnd`, `inflight_bytes`,
`pending_bytes`, `max_payload`, `fast_retransmits` and `t3_expiries`; an `unread` line
carries only the session, the pump and `open_path`. No field can carry an address. It is
written at INFO, like the other P0 lines, so the default filter ships it
(`crates/agent-core/src/logging.rs:435`), and `ROOMLERD_PATH_MODEL=off` writes none. A
follower of a shared pipeline runs no pump and writes no line, and a pump that starts
again for the same session writes its own. The cells open a pair by each path over the
shaped link and read `init_ack` and `cookie_echo` at the first write
(`agents/roomlerd/src/media_delivery.rs:3553`, `:3572`). The path report counts one
device's sessions by answer (`docs/production-mongo-reads.md` §8a).

**The properties and their cells.** Each cell opens two in-process associations of the
vendored SCTP over `media_delivery`'s shaped link (`shaped`,
`agents/roomlerd/src/media_delivery.rs:3927`; `pair_opened`, `:4054`, by either path,
`OpenPath`, `:4039`), steps the machine every millisecond, writes what it asks, and
after the stop waits for every byte written to be acknowledged. A grid cell's link
serialises each direction at its rate, adds half the round trip to every packet, and
queues up to a second, deeper than anything the train can fill (`Cell::grid`,
`agents/roomlerd/src/media_train.rs:800`). The shaper records the longest any packet of
the agent's waited for the wire (`peak_wait`, `agents/roomlerd/src/media_delivery.rs:3743`)
and its tail drops (`:3748`). `broken` (`agents/roomlerd/src/media_train.rs:1084`) checks
every cell; it also checks that `floor_only` matches the stop, and that the slow start
threshold at the first write is 0 on path C and not 0 on path I.

| Property | Says | Checked | Read over five runs of each cell |
|---|---|---|---|
| **P-a** | a push-back stop's low edge is within 20 % of the link's payload rate | every cell | 0.89 to 1.07 of the link at every push-back stop of the grid, and 0.95 to 1.03 in the noise cells. RED in the shallow buffer cell on path I (below) |
| **P-b** | a budget, deadline or ready stop is a floor, at most 20 % above that rate | every cell | floors from 0.00 to 1.06 of the link |
| **P-c** | no high edge below the low edge | every cell, and `a_high_edge_below_the_low_edge_or_on_a_floor_is_never_reported` | 14 high edges in the grid, all on path I (2 Mbit/s at 80 and 250 ms, 8 Mbit/s at 20 and 80 ms), at 1.05 to 2.00 times their low edges, and 5 in the noise cell at 2 Mbit/s on path I, at 1.25 to 1.88 |
| **P-d** | no delay stop while every round's least queueing delay is under 10 ms; a link with room ends on budget or deadline | every cell (a delay stop where no packet of the agent's waited 10 ms), the room cells at 50 Mbit/s and 250 ms (`room_is_not_push_back`, `:1390`), and `a_round_under_ten_ms_above_the_least_is_no_delay_stop` | a packet waited 21.5 ms or more in every cell that stopped on delay; the room cells ended on `budget` (path I) and `deadline` (path C) |
| **P-e** | a peak queue of at most the round trip or 50 ms, whichever is longer, at most 512 KiB written, at most 2.5 s, and everything written acknowledged | every cell | peak queues up to 190.6 ms at 250 ms (202.1 ms in a noise cell), 74.2 ms at 80 ms and 49.1 ms at 20 ms; 523,200 bytes; 2,478 ms (2,485 ms in a noise cell) |
| **P-f** | at 0.4 Mbit/s, at most 64 KB written | the six 0.4 Mbit/s cells and the two 0.4 Mbit/s noise cells | 44,400 bytes at most (250 ms, both paths, and under noise), where it was 61,200 before the write budget |
| **P-g** | a first frame ready at 0.3 s ends the train with a floor | four cells, 8 Mbit/s at 80 and 250 ms on both paths (`ready_is_a_floor`, `:1424`) | `ready` at 300 to 302 ms, a floor every time |

At 0.4 Mbit/s and 20 ms the 49.1 ms is SCTP's own initial window: three packets go at
once, and the third waits two serialisations of 24.6 ms.

**The grid.** The low edge as a share of the link's payload rate (the link's rate times
1,200 / 1,228): the stop, the mean and the range over five runs, and the highest peak
queue.

| Link | Path I | Path C |
|---|---|---|
| 0.4 Mbit/s, 20 ms | plateau, 1.00 (0.98 to 1.01), 49.1 ms | plateau, 1.01 (1.00 to 1.01), 49.1 ms |
| 0.4 Mbit/s, 80 ms | delay and once plateau, 0.98 (0.89 to 1.01), 73.6 ms | delay, 1.01 (0.99 to 1.02), 49.1 ms |
| 0.4 Mbit/s, 250 ms | delay, 0.99 (0.98 to 1.01), 190.6 ms | deadline, 0.42 (0.42 every run), 50.7 ms |
| 2 Mbit/s, 20 ms | plateau and once delay, 1.04 (1.01 to 1.07), 44.9 ms | plateau and once delay, 1.02 (1.01 to 1.05), 21.5 ms |
| 2 Mbit/s, 80 ms | delay, 1.00 (0.97 to 1.02), 74.2 ms | deadline, 0.99 (0.96 to 1.03), 22.5 ms |
| 2 Mbit/s, 250 ms | delay, 1.00 (0.97 to 1.02), 182.2 ms | deadline, 0.12 (0.11 to 0.13), 11.1 ms |
| 8 Mbit/s, 20 ms | delay, 1.01 (0.99 to 1.05), 29.3 ms | budget, 1.05 (1.01 to 1.06), 12.9 ms |
| 8 Mbit/s, 80 ms | delay, 1.00 (0.96 to 1.05), 73.2 ms | deadline, 0.34 (0.33 to 0.35), 6.5 ms |
| 8 Mbit/s, 250 ms | budget, 0.32 (0.28 to 0.36), 90.5 ms | deadline, 0.03 (0.03 to 0.04), 5.2 ms |
| 50 Mbit/s, 20 ms | budget, 0.62 (0.45 to 0.80), 20.3 ms | budget, 0.24 (0.22 to 0.26), 3.2 ms |
| 50 Mbit/s, 80 ms | budget, 0.17 (0.15 to 0.19), 20.4 ms | deadline, 0.07 (0.06 to 0.08), 2.8 ms |
| 50 Mbit/s, 250 ms | budget, 0.05 (0.04 to 0.07), 18.2 ms | deadline, 0.01 (0.01 every run), 1.3 ms |

The noise cells, over the same five runs (one seed each, so what varies is the box's own
scheduling): at 0.4 Mbit/s path I stopped on delay at 1.01 (0.99 to 1.03) with 37,200 to
43,200 bytes and a peak queue up to 188.4 ms, and path C on its deadline at 0.42 with
44,400; at 2 Mbit/s path I stopped on delay at 0.98 (0.95 to 1.03) with a peak queue up
to 202.1 ms, and path C on its deadline at 0.13 (0.11 to 0.14).

Path I reads the link wherever slow start fills it within the budget, up to 8 Mbit/s at
80 ms. Beyond that the budget runs out before the window reaches the pipe, and the
reading is a floor.

| Named cell | Path I | Path C |
|---|---|---|
| A shallow buffer: 8 Mbit/s, 80 ms, a queue of 20 ms (`shallow`, `agents/roomlerd/src/media_train.rs:1304`) | RED: `loss` in every run, the low edge 0.26 to 0.46 | `deadline`, 0.35 (0.33 to 0.36): congestion avoidance never fills the queue, so nothing is lost |
| The viewer's window of 64 KiB, at 50 Mbit/s and 80 ms (`peer_window`, `:1358`) | `budget`, 0.15 (0.14 to 0.16), a floor; the gauges showed the peer's window holding the sender at 149 to 220 steps | `deadline`, 0.07 (0.07 to 0.08), a floor; held by the window at 118 to 120 steps |
| A link with room: 50 Mbit/s at 250 ms (P-d) | `budget` | `deadline` |
| The first frame ready at 0.3 s: 8 Mbit/s at 80 and 250 ms (P-g) | `ready`, a floor | `ready`, a floor |

**The shallow buffer is RED, and kept as written.** At 8 Mbit/s and 80 ms with a queue
of 20 ms, P-a fails on path I in every run: the train stops on `loss`, and its low edge
read 0.14 to 0.52 of the link over twelve runs. Each SACK of two packets releases
four, so inside a round the bottleneck's queue grows by about what the round
acknowledges, which is the window of the round before. The 20 ms queue holds a quarter
of the 82 ms pipe, so it overflows in the round after the window first passes a quarter
of the pipe, and each round that closes before that loss is seen acknowledges at most
about half the pipe: no whole round before the first loss carries much more than half
the link's rate. The in-flight cap does not bind that early, and the queue comes from
the burst, not from what is in flight. Reading the link there would take a train that
grows more gently than slow start's doubling, writing less than each SACK would
release, and the rulings have the train let slow start set the pace. The cell keeps its
assertions and is ignored with that reason (`agents/roomlerd/src/media_train.rs:1332`).

**Under a busy runner's noise.** CI run 44383834538, on PR #4034, broke P-f in the
quiet cell `path_i_0_4_mbps_250_ms` on both its runners, where every quiet local run had
held it:

| Run | Stop | Rounds | Written | Time | Peak queue | Low edge |
|---|---|---|---|---|---|---|
| CI, Linux | `delay` | 6 | 85,200 bytes | 2,397 ms | 121.6 ms | 1.01 |
| CI, Windows | `delay` | 4 | 64,800 bytes | 1,952 ms | 190.2 ms | 1.00 |
| local, quiet, five runs | `delay` | 4 | 61,200 bytes | 1,905 to 1,911 ms | 141 to 166 ms | 0.99 to 1.01 |
| local, in the whole lib suite, one run of two | `delay` | 4 | 64,800 bytes | 1,955 ms | 188.6 ms | 0.99 |

The shaped link now adds a busy runner's scheduling noise on request, seeded so that a
cell meets the same draws on every run (`Noise`,
`agents/roomlerd/src/media_delivery.rs:3761`; `Draws`, `:3781`; `shaped_noisy`,
`:3932`). Every wake-up of a task comes up to the jitter late, and about once a period
the scheduler takes the task's core away for up to the stall, so that a wake-up due then
comes when it returns. On the link a wake-up is a packet's delivery, and the packets
behind a late one wait for it, in order. Under the rule before this one, 2 ms of jitter
alone wrote 64,800 bytes in 1,963 ms with a peak queue of 190.5 ms, the Windows line,
and 5 ms with a stall of up to 30 ms on one packet in 20 wrote 66,000 to 88,800 bytes,
the last in six rounds, as on Linux.

The trace of those runs shows the rule that let the extra rounds through. A round was
judged on delay by its least sample, and at the cap a delayed acknowledgement of a lone
packet held the sender for 130 to 228 ms. The link idled, the next pair went into an
empty queue, and its sample dipped to one serialisation above the least, so the round
that took it could not stand. Rounds 3 and 4 each took such a dip, so even a quiet run
was judged only in round 5, and nothing bounded what the train wrote meanwhile: quiet,
it wrote 38,400 bytes after the cap began and left P-f 2,800 bytes, 57 ms of the link.
A few milliseconds of noise moved the judgment one SACK later and spent them. The write
budget (above) is the rule that holds however late the judgment comes, and the run of
samples in a row lets a dip start the count again instead of sinking a round.

The cells' noise (`runner_noise`, `agents/roomlerd/src/media_train.rs:1242`) is 2 ms of
jitter on every packet each way and on every step of the writer, and a stall of up to
20 ms about every half second on each of the three. Four standing cells run under it
(`noisy!`, `:1251`, seeds 1 to 4): 0.4 and 2 Mbit/s at 250 ms on both paths,
where a judgment that comes late writes the most. The ignored measurement
`grid_under_noise` (`:1279`) runs the 24 grid cells under five seeds each. In its two
runs on these rules, P-f held in all 240 runs, at 44,400 bytes at most, and the four
standing cells' links broke nothing under any seed. Three runs broke another property,
each in a cell the write budget does not touch:

| Cell | Broke | How |
|---|---|---|
| 2 Mbit/s, 80 ms, path C, twice | P-a: a plateau at 0.21 and 0.26 | a SACK a stall delayed sat 10 ms above the least while the estimate was a tenth to a sixth of the link, the cap began there, and the rounds it held grew less than 10 % |
| 8 Mbit/s, 80 ms, path I, once, and once quiet in six runs of the whole lib suite on the dev box | P-e: a peak queue of 80.6 and 96.9 ms, over 80 | SACKs a stall bunched read 1.5 and 1.6 times the link over the newest 10 ms, and the cap followed the estimate. In the lib run the stall was the box's own: 67 ms without a SACK |

A harsher draw of the noise found the pair estimate's limit. With each stall falling on
the first wake-up after its draw, so that a link idle across it delayed the next packet,
the sample that sets the least came late at 0.4 Mbit/s and 250 ms on path I in two seeds
of five: the least read 292.0 and 294.6 ms where quiet runs read 277.9 to 279.5, the
first sample led it by 13.4 and 15.2 ms instead of about 25, and the pair estimate read
89 and 78 kB/s, 1.8 and 1.6 times the link. The cap and the write budget follow the
estimate, so the peak queue reached 385 and 353 ms, over P-e's 250, and the first run
wrote 66,000 bytes, over P-f. The write budget holds P-f for an estimate up to 1.6 times
the link and the cap holds P-e up to about 1.4 times, so at 0.4 Mbit/s a stall that
delays the least by about 7 ms, under a third of a serialisation, breaks P-e, and one of
about 9 ms breaks P-f. All three are the estimate's limits and not the write budget's,
and they stand as found.

**Seen in the vendored SCTP, and fixed (#4033).** The delayed acknowledgement timer can
be armed only while its `close_tx` is empty (`AckTimer::start`,
`crates/vendored/webrtc-sctp/src/timer/ack_timer.rs:36`), and only `stop` empties it
(`:65`). A timer that fires calls `on_ack_timeout`
(`crates/vendored/webrtc-sctp/src/association/association_internal.rs:2380`), which
sends the SACK. Upstream it left `close_tx` set, so after one delayed acknowledgement
timed out, the next lone packet was acknowledged only when another packet arrived and
the immediate acknowledgement called `stop` (`:2247`), or when the sender's T3-rtx sent
it again. With one message in flight the train stalled. `on_ack_timeout` now calls
`stop` itself (`:2393`), so every lone packet arms a timer of its own. A receiver still
holds a lone packet's SACK up to 200 ms, so the cap keeps its floor of three messages,
with which the train never waits for the timer. The fix reaches the agent's own
receiver, for what a browser sends it, and both ends of a tunnel on `webrtc-dc-v1`. In a
session the train's receiver is the browser's SCTP, which shares none of this code.

**What a reading cannot tell.**

- On path C a floor is the association's congestion avoidance, not the path.
- A floor never lowers anything: a viewer's small window, a link with room, a ready
  frame and the deadline all read as floors, and only `delay`, `plateau` and `loss` say
  where the path is.
- At 0.4 Mbit/s SCTP's initial window alone queues 49.1 ms, so a slower link at a short
  round trip would pass P-e's 50 ms whatever the train does.
- The shaped link proves the rules on a known pipe. It does not prove a real carrier's
  jitter, reordering and loss, a browser's SCTP at the far end, or DTLS and ICE beneath
  SCTP.

**The tests** (`cargo test -p roomlerd --lib -- media_train`). The cells run in real
time and one at a time (`TURN`, `agents/roomlerd/src/media_train.rs:779`), and ask
Windows for 1 ms timers. Windows 11 stops honouring that request from a background
process after a few seconds: on the dev box a probe's 1 ms sleeps took 15.4 ms from its
sixth second on, and two module runs broke seven and nine cells, every 20 ms cell
reading a least round trip of 30 to 60 ms with the steps 15.6 ms apart. So each cell
first takes the timer resolution out of the power throttling's hands
(`keep_timer_resolution`, `:959`), as Microsoft documents it for a process that needs
its timers. In five runs on a Windows box the cells took 75.7 to 76.4 s, 12.5 s of it
the four noise cells. On the rule before this one, the noise cell at 0.4 Mbit/s on path I
broke P-f in one run of two, with 73,200 bytes written, and wrote 62,400 in the other.

**What CI runs, and what is a measurement (FR-148 D44).** A cell runs the train in real
time over the vendored SCTP, so a busy runner's scheduling moves what it reads. After the
write budget, one of six runs of the whole lib suite on the dev box still broke P-e in
the quiet grid cell at 8 Mbit/s and 80 ms, and three of 240 noisy runs broke P-a or P-e.
Each break went through the rate estimate, whose limits stand as found (above). So
`cargo test` runs only what timing cannot flip:
- the seventeen machine tests, which drive the rules on readings made up by hand;
- the nine named cells, which ended on a floor (`budget`, `deadline` or `ready`) and broke
  nothing in eleven runs, six of them inside the loaded lib suite.

The 24 grid cells and the four noise cells are ignored. They are run alone before a
change to the train merges, and their numbers are recorded here:

```
cargo test -p roomlerd --lib media_train::cells -- --ignored --skip shallow_buffer_stops_on_loss --skip grid_under_noise --skip ramp
```

| Test | Shows |
|---|---|
| `cells::path_{i,c}_{0_4,2,8,50}_mbps_{20,80,250}_ms` | ignored, a measurement (D44): the 24 grid cells |
| `cells::path_{i,c}_{0_4,2}_mbps_250_ms_under_noise` | ignored, a measurement (D44): the four noise cells, a busy runner's seeded noise on the slow links at 250 ms |
| `cells::path_c_a_shallow_buffer_reads_a_floor_or_stops_on_loss`, `cells::path_{i,c}_a_peer_window_limit_is_a_floor`, `cells::path_{i,c}_a_link_with_room_ends_on_budget_or_deadline`, `cells::path_{i,c}_a_window_of_0_3_s_at_{80,250}_ms_reads_a_floor` | the named cells, run by `cargo test` |
| `cells::path_i_a_shallow_buffer_stops_on_loss` | ignored: P-a's RED cell, run with `--ignored` |
| `cells::grid_under_noise` | ignored: the 24 grid cells under five seeds of the noise, about four minutes, run with `--ignored --nocapture`; it fails when any run breaks a property, as three of 240 did (above) |
| `cells::path_c_ramp_to_the_link_rate` | ignored: the ramp, about 40 s, run with `--ignored --nocapture` |
| `machine::*` | seventeen tests of the machine alone, on readings made up by hand: the first write and its budget, round 1's window, a stopped train's answer, the floors, the train's own losses, the deadline, the budget, a delay stop at a round's end and none in a round of fewer than four samples, P-d's edge, the high edge, a run of samples that stands, one that a sample or a silence starts again, a spent write budget, and a fast link without one |

## Path numbers and their provenance (FR-148 P0) — how each was obtained, and how old it is

FR-148 P0, step 3 (#3953). **A shadow: nothing reads a tag or the model.** Every path
number a data-channel pump reports now says how it was obtained and how old it is, on
one line of its own every 10 s. No number changes: `Pipe`'s floor and capacity, the
governor's readings and every heartbeat field are what they were, and a test pins it
bit for bit. The heartbeat lines and the `path delivery` line are unchanged.

**The three provenances** are FR-148 §4.1's (`encode::pipe::Basis`):

| Provenance | Written | Means | Lowers a floor? |
|---|---|---|---|
| measured while bytes waited | `backlogged` | the sender offered more than arrived: the path refused above this rate | yes: evidence about capacity |
| measured, the sender offering no more | `app-limited` | the path carried everything offered: a lower bound | no: it only raises one |
| assumed | `assumed` | computed, not measured: the prior's stand-in, a ramp, a cut | never: it is never stored, and never stands below a measured floor |

A fourth word, `remembered`, is the pair memory's seed: measured, but in an earlier
session. The entry does not keep its class, so the model reads it as the weaker one,
application-limited, aged from the entry's `at_unix`. FR-148 P1 stores the class beside
the value in the model's own file
([§ The path memory](#the-path-memory-fr-148-p1--the-models-floor-under-a-key-that-names-the-path));
today's entry, which still seeds the opener and this model, does not keep it. A stalled
window is none of the three: FR-79's gate keeps it from setting a
reading, a capacity or a step. The one number it still gives, its arrival rate as a
delivery, can only raise a floor, and it carries its window's class like any other.

```mermaid
flowchart LR
  R["a window's readings<br/>arrival rate · goodput fold"] --> T["classed: backlogged<br/>or application-limited"]
  T --> P["Pipe<br/>floor · capacity, unchanged"]
  T --> M["PathModel (shadow)<br/>reads the class"]
  S["the pair memory's seed"] --> M
  P --> V["RateGovernor::path_view"]
  M --> V
  G["prior · learner · slow-link floor<br/>tagged where they move"] --> V
  V --> L["path model line<br/>every 10 s"]
```

**How each number is tagged.** Each field is the heartbeat's field of the same name, read
by the same accessor at the same heartbeat; a test checks that at every window of the
replays (`same_numbers`).

| Field | The number | Its tag | Its age runs from |
|---|---|---|---|
| `goodput_bps` | the blocked-send estimate (`blocked_send_bps`) | `backlogged`: the estimator folds only sends that blocked | the last window it folded |
| `viewer_rx_bps` | the last per-window arrival rate the governor took (not in the heartbeat) | `backlogged` when the window offered more than arrived (`sent > 1.1 × rx`, or the classifier said `Overproduced`), else `app-limited` | that window |
| `pipe_belief` | `Pipe`'s (anchor, floor, capacity) | the reading that set each: a delivery its window's class; a goodput push-back `backlogged`; an arrival-rate push-back the offer streak's verdict, so `app-limited` when the switch-off law (V5a) takes it unoffered. The anchor takes the capacity's tag on a tie | when it was set |
| `pipe_measured` | the reading a stall is judged against | the goodput's tag, or the held arrival rate's (the goodput's on a tie) | that reading |
| `prior_bps` | the stand-in | always `assumed`, at any rung | the reading its base was anchored on, or the seed's entry until a reading anchors it |
| `learned_ceiling_bps` | the learner's ceiling | `remembered` when the seed set it at the open; `app-limited` at a growth step (a rate carried with the target pinned under the ceiling); `assumed` after a cut or the seed's abandonment; `0`, the nominal start, `assumed` | when it last moved |
| `slow_link_floor_bps` | the relieved floor | the tag of the reading or stand-in it was derived from; the opener's ramp is `assumed` | that reading, or the open |
| `seed_bps` | the seed (not in the heartbeat) | `remembered` | the entry's age at the open, plus the session's |

**The shadow model** (`encode::pipe::PathModel`) takes the same inputs as `Pipe` and
reads their class:

| Input | `Pipe` | The model |
|---|---|---|
| a delivery, either measured class | raises the floor | raises the floor |
| a push-back, `backlogged` | sets the capacity; lowers the floor by half the gap, or raises it | the same |
| a push-back, `app-limited` | the same as a backlogged one | raises the floor, as a delivery does; the capacity does not move |
| the seed, at the open | never an input | an `app-limited` reading, marked `remembered` |
| a reading in a window FR-79's gate rejected | a delivery raises the floor | never enters (FR-148 P1, D24) |
| anything assumed | never an input | never enters |

So the model's floor rises on any measured reading and falls only on a backlogged one,
and its capacity moves only on a backlogged one (60 s, `CAPACITY_TTL`, as `Pipe`'s).
`model_stand_in_bps` is the stand-in as the model lets it stand: where the floor is at
or above it, the floor, with its own tag. What it would offer the pair memory at a
session's end (`PathModel::offer`) is its floor, with the tag of the reading that set
it, or nothing: never an assumed value, and since FR-148 P1 never the remembered floor
either (D23). A floor no reading of this session set is not this session's to write
back (`a_remembered_seed_is_an_application_limited_floor`). Nothing reaches a rate or a
ceiling. What P1 stores is in
[§ The path memory](#the-path-memory-fr-148-p1--the-models-floor-under-a-key-that-names-the-path).
When every input is backlogged the model is `Pipe`'s law exactly
(`the_model_is_pipes_law_when_everything_is_backlogged`).

**The line** is `path model (FR-148 P0, shadow, ≈10s)`, at INFO, written by
`media_delivery::emit_model` (`agents/roomlerd/src/media_delivery.rs:1228`). The delivery
sampler takes the numbers at every pump heartbeat (`on_view`, `:817`) and writes them
at the first heartbeat 10 s after the last line; the first heartbeat starts the count,
and a pump that ends between two lines writes its last numbers as it drops. Each value
is `<bps>/<provenance>/<age>ms`, or `None`. A test pins the names and their order
(`the_model_line_names_its_fields`):

| Field | Meaning |
|---|---|
| `session_id` | the session |
| `goodput_bps`, `pipe_belief`, `pipe_measured`, `prior_bps`, `learned_ceiling_bps`, `slow_link_floor_bps` | the heartbeat's numbers of those names, tagged (above) |
| `viewer_rx_bps`, `seed_bps` | the arrival rate and the seed, tagged |
| `model_floor_bps`, `model_capacity_bps`, `model_stand_in_bps` | the shadow model's floor, capacity and stand-in |
| `model_floor_push_back`, `model_capacity_push_back` | which delivery sample's push-back last set the model's floor and its capacity: `p1` or `p3` (D46 B2, [§ The delivery samples' push-backs](#the-delivery-samples-push-backs-d46-b2)), or `-`, a window's or none |

**Its cost**: a line with every number set is 662 bytes and an idle one 522, at most
238,320 and 187,920 bytes a session hour, measured by the same test (610 and 471 before
D46 B2's two fields). **The switch** is
the delivery samples' (`ROOMLERD_PATH_MODEL`): `off` takes no path number and writes no
`path model` line (`off_takes_no_path_numbers_and_writes_no_model_line`).

**What feeds the shadow model** (D24): since #4014 the model behind `model_floor_bps`,
`model_capacity_bps` and `model_stand_in_bps` takes a delivery only from a window the
validity gate accepted; before it every window's arrival rate reached the model. AC3 is
read on this line, so a line from an older release is not comparable with one from that
release on. The path memory's counted evidence (D29) does not touch this line: a sample
set aside for the memory still feeds the model. Since D46 B2 the delivery samples'
push-backs feed it as well, P1 and P3, and the window after a stall the in-flight
budget saw does not ([§ The delivery samples' push-backs](#the-delivery-samples-push-backs-d46-b2)),
so a line from a release before B2 is not comparable either.

**What a healthy line looks like.** The jupiter replay on today's laws, at the end of its
busy minute and on the still screen 30 s later
(`a_healthy_session_reads_application_limited` prints both with `--nocapture`):

```text
path model (FR-148 P0, shadow, ≈10s) session_id=… goodput_bps=None pipe_belief=(15915240/app-limited/0ms, 15915240/app-limited/0ms, None) pipe_measured=None prior_bps=None learned_ceiling_bps=6209663/app-limited/0ms slow_link_floor_bps=None viewer_rx_bps=15915240/app-limited/0ms seed_bps=None model_floor_bps=15915240/app-limited/0ms model_capacity_bps=None model_stand_in_bps=None model_floor_push_back=- model_capacity_push_back=-
path model (FR-148 P0, shadow, ≈10s) session_id=… goodput_bps=None pipe_belief=(15915240/app-limited/30000ms, 15915240/app-limited/30000ms, None) pipe_measured=None prior_bps=None learned_ceiling_bps=6209663/app-limited/30000ms slow_link_floor_bps=None viewer_rx_bps=9120/app-limited/0ms seed_bps=None model_floor_bps=15915240/app-limited/30000ms model_capacity_bps=None model_stand_in_bps=None model_floor_push_back=- model_capacity_push_back=-
```

| Screen and path | Reads |
|---|---|
| a busy screen on a path with room | every measured number `app-limited`; no capacity (`pipe_belief`'s third and `model_capacity_bps` are `None`); the model's floor is the busiest window |
| the still screen after it | the same numbers, older; `viewer_rx_bps` is the trickle's, `app-limited`, and the floor does not fall |
| a path at its limit | `backlogged` readings: the goodput, the arrival rate of windows that offered more than arrived, and a `model_capacity_bps`. Not yet read in the field: FR-148 P0's standing relay cell is step 5 |
| a stand-in in use | `prior_bps` `assumed`, as old as the reading it walked from; `model_stand_in_bps` is the floor wherever the floor is at or above it |

**The two field chains, as cells.** Both lose the floor their path had demonstrated on the
laws of 2026-11-16 (`measured_evidence` off), and keep it when the tags are read. With
the model reading every push-back as backlogged, both fail.

| Cell | The chain | On the laws of 2026-11-16 | Read with provenance |
|---|---|---|---|
| `the_jupiter_chain_read_with_provenance_keeps_its_floor` | jupiter, 2026-11-02: a busy minute delivered 15,915,240 bit/s, then a still screen trickled | the trickle's 9,489 was a push-back, so the floor fell to 7,962,364, and the stand-in's last rung, 3,322, went to the pair memory | the push-back is `app-limited` and the 3,322 `assumed`; the model's floor stays 15,915,240 `app-limited`, with no capacity, and is what it would offer |
| `the_corplap2_chain_read_with_provenance_keeps_its_floor` | CORPLAP-2, 2026-11-11: 45 minutes of relayed AV1, replayed from its heartbeats (`agents/roomlerd/tests/fixtures/fr148-corplap2-20261111.txt`); 1,399,272 delivered at 394.9 s | at 909.1 s the viewer's queue grew while the pump wrote 498,304, and the arrival rate, 564,865, became the reading and a push-back: the floor fell to 982,068. The stand-in walked 178 steps to 179,980, and the session wrote that | the reading is `564865/app-limited`, the stand-in `assumed` and as old as that reading; the model's floor stays 1,399,272 and is what it would offer. Nothing later in the session delivered more |

**What a tag cannot tell.**

- An application-limited number is a lower bound and nothing more: it cannot say how
  much more the path would carry. Only a backlogged reading bounds it from above.
- Backlogged is the sender's verdict from the viewer's report: the window offered more
  than arrived. A viewer that stops reading, or decodes slowly, makes a good path read
  backlogged.
- A window with no link report has no arrival rate, so there is nothing to tag.
- An age is the evidence's, not the number's: the stand-in, a step a window, is as old
  as the reading its base was anchored on, however many steps it has taken since.
- Today's seed carries no class (P1's own file does, but nothing opens from it yet), so
  it is read as the weaker class whatever it was.
- The `path delivery` line's classes are not wired in: `viewer_rx_bps`'s class is the
  governor's per-window verdict, not the SCTP sample's. P0 keeps the two apart.

**The RTP pump writes no `path model` line.** It has no governor, no `Pipe` and no
delivery sampler. Its heartbeat (`media pump heartbeat (≈1s window)`,
`agents/roomlerd/src/peer.rs:3572`) carries no path number: `bytes_written` and
`write_errors` are its only transport fields. Its one path number is REMB, the browser's
estimate (`remb_bps` on the `applying adaptive bitrate` line, `peer.rs:3436`, written
when the target moves past its 15 % hysteresis): the target is the quality target,
capped at REMB × 85 % but never below the larger of `MIN_BITRATE_BPS` and a quarter of
the base (`peer.rs:3405`).

| Where | Anchor |
|---|---|
| the tags and the model | `agents/roomlerd/src/encode/pipe.rs:484` (`Basis`), `:519` (`PathNumber`), `:590` (`PathModel`), `:776` (`PathView`); `Pipe` takes the class at `:209` (`observe_delivered`) and `:270` (`observe_capacity`) |
| the governor's tags and the view | `agents/roomlerd/src/encode/governor.rs:1448` (a delivery, classed), `:1646` (an arrival-rate push-back, classed), `:2076` (`path_view`), `:2230` (`PathTags`) |
| the line and the seed's age | `agents/roomlerd/src/media_delivery.rs:817` (`on_view`), `:1228` (`emit_model`), `:1255` (`seed_at_open`) |
| the pumps | `agents/roomlerd/src/peer.rs:5396` (VP9-444), `:10361` (FFmpeg), `:6632` (the FFmpeg pump's seed and its age) |

**The tests** (`cargo test -p roomlerd --lib -- pipe:: governor:: media_delivery`):

| Test | Shows |
|---|---|
| `encode::pipe::tests::tags_change_no_number`, and every existing `Pipe` cell | each runs through a twin that holds a verbatim copy of the law before the tags: every number equal, on 100,000 random operations |
| `the_model_is_pipes_law_when_everything_is_backlogged`, `an_application_limited_push_back_lowers_nothing_in_the_model`, `an_assumed_value_never_stands_below_the_floor_and_is_never_offered`, `a_remembered_seed_is_an_application_limited_floor`, `a_number_is_written_as_value_word_and_age` | the model's laws, and the written form |
| `encode::governor::tests::the_jupiter_chain_read_with_provenance_keeps_its_floor`, `the_corplap2_chain_read_with_provenance_keeps_its_floor` | the two field chains, above |
| `encode::governor::tests::a_healthy_session_reads_application_limited` | the healthy lines above |
| `same_numbers`, at every window of every `V5bSession` replay | the line's numbers are the heartbeat's |
| `media_delivery::tests::the_model_line_names_its_fields`, `the_model_line_comes_every_ten_seconds_and_a_drop_writes_the_last`, `off_takes_no_path_numbers_and_writes_no_model_line`, `the_seed_at_open_comes_with_its_entrys_age` | the line: its names and order, its size, its cadence, the switch, the seed's age |

## The path memory (FR-148 P1) — the model's floor, under a key that names the path

FR-148 P1, steps 2 to 4 (#3953). A constrained session of the FFmpeg data-channel pump
keeps a second memory for the path model, `path_memory.json`, under a key that names the
path, beside today's `rate_memory.json`, which is written byte for byte as before
(`todays_file_is_written_byte_for_byte_as_master`). **Under the switch's default,
`shadow`, nothing reads the new file to drive a rate**: the opener is today's, bit for
bit (`shadow_is_todays_opener_exactly`). **Under `on`**, a session whose pair has a key
and a fresh entry opens from the model ([below](#which-rule-opens-the-session-step-3)).
Two lines a session say what the open read, which rule opened the session and why, and
what the close wrote.

| | Today's memory (FR-35, FR-79 V2, V4) | The path model's (FR-148 P1) |
|---|---|---|
| File | `rate_memory.json` | `path_memory.json`, in the same directory |
| Key | `remote_addr\|carrier` | `v2\|<carrier>\|u:<user>\|<viewer network>` |
| An entry | one rate, its time, its provenance | the model's floor, with the class and time of the reading that set it; a push-back record |
| Written from | `session_evidence_bps`: a carried hold, else a reading, else the opener's measurement | the model's floor and the counted push-backs, from valid windows only (D24, D25, D29, D30), and the delivery samples' push-backs under the last valid window (D46 B2) |
| Read by | the opener: 85 % of it seeds the opening ceiling, unless the model decides | the open line; under `on`, the opener (D25, D26) |
| Code | `RateMemoryGuard` (`agents/roomlerd/src/media_pair_memory.rs:210`) | `ModelGuard` (`:262`) over `encode::pair_memory` |

```mermaid
flowchart TD
  O["the pump opens<br/>constrained, switch not off"] --> K["the key<br/>carrier · user · viewer network"]
  K --> E["path_memory.json<br/>the pair's entry, checked alone"]
  E --> D{"on, a key,<br/>a fresh entry?"}
  D -- yes --> GM["the governor opens from the model<br/>D25 · D26"]
  D -- no --> GT["the governor opens from today's seed"]
  E --> OL["open line<br/>the entry · both openings · taken · why"]
  E --> S["SessionMemory<br/>the floor law, from the entry's floor"]
  W["a viewer window<br/>that FR-79's gate accepted,<br/>its samples counted (D29)"] --> S
  S --> T["the would-be write, taken each window<br/>until the path changes"]
  T --> C["the close<br/>case by case"]
  C --> F["path_memory.json<br/>written whole, under one lock"]
  C --> CL["close line<br/>wrote · why"]
```

### The key (D22)

| Part | From | Rule |
|---|---|---|
| `v2` | | the key's version |
| carrier | FR-102's word for the selected pair (`session_carrier::carrier_of`) | `direct`, `turn`, `turn_local`, `overlay_direct`, `overlay_turn`, `derp`, `org_relay`. An overlay relay adds its transport when the overlay names it (`derp/tcp`), as FR-79 V2's key did. An overlay carrier never shares a word with any other. `unknown` gives no key |
| `u:<user>` | the controlling user's id, from the session's `rc:request` | 24 hex, lower case. No user gives no key: an anonymous key would merge every viewer |
| viewer network | the viewer's candidates, as it trickled them, and the selected remote | the first that exists: (1) its server-reflexive addresses that are not overlay addresses, a public one before a private one, then the lowest IPv4, else the lowest IPv6, masked: `n:203.0.113.0/24`, `n:2001:db8:1::/48`; (2) the selected remote when it is the viewer's own address (not a relay candidate, not an overlay address), masked; (3) the selected remote when it is an overlay address, whole, `o:<address>`: it names a device, not a network; (4) `n:?` |

The relay worker's address is never in the key, so two TURN workers serving one viewer
are one entry (AC8). The key stays in the local file. A line carries `key_id` (the first
12 hex of the key's SHA-256), `key_carrier`, and `key_net`, the word for what the network
part was made from: `srflx`, `remote`, `overlay` or `unknown`. A session with no key
reads and writes nothing, and `no_key` says why: `no_pair` (no selected pair within the
pump's 4 s wait), `carrier_unknown`, `no_user`, or `learner_off` (`relay_max_hi_bps` is 0).

| One entry | Two entries |
|---|---|
| two TURN workers for one viewer (`two_relay_workers_are_one_entry`) | two viewer networks (`two_viewer_networks_are_two_entries`) |
| two overlay addresses of one viewer on one network (`two_overlay_addresses_of_one_viewer_on_one_network_are_one_entry`) | an overlay carrier and any other (`overlay_and_other_carriers_never_share_a_word`) |
| a viewer that gave no srflx candidate, on one overlay device (`no_srflx_keys_the_overlay_device`) | two controlling users |

**Where the viewer's facts come from.** `PairFacts` (`media_pair_memory.rs:61`) is
registered by session id in `AgentPeer::new`, before the pump starts. The user comes from
the `rc:request`'s record (`agents/roomlerd/src/signaling.rs:4760`). The srflx addresses
are taken from the candidates the viewer trickles (`AgentPeer::add_remote_candidate`), at
most 16. They are not read through `get_stats`: the vendored ICE agent's accessor is
crate-private, and a stats read takes the ICE and SCTP locks. **Limit**: a candidate
inside the offer's SDP is not read. The controller trickles its candidates, so a viewer's
srflx address arrives the trickled way.

### The file (D23)

`path_memory.json` sits in the directory of `rate_memory.json`. It is written whole at a
close that changed an entry (a temporary sibling, then a rename), as pretty JSON, under
one lock in the process around the read, the change and the save (`PathMemory::update`).
Without it two pumps that end at once share the temporary sibling's name, and the later
save drops the key the earlier one wrote: 53 of 160 concurrent saves failed in the red
run (`pumps_that_end_at_once_each_keep_their_key`). Two sessions of one key still end as
the later close says, as today's memory does.

```json
{
  "v": 1,
  "pairs": {
    "v2|turn|u:<24 hex>|n:203.0.113.0/24": {
      "floor": { "bps": 722500, "basis": "backlogged", "at_unix": 1763420000 },
      "push_back": { "bps": 640000, "at_unix": 1763420000, "first_at_unix": 1763419987, "windows": 3 }
    }
  }
}
```

Each entry is checked on its own when the file is read, so one bad entry never costs the
others:

| On disk | Read as | The open line's `entry` |
|---|---|---|
| no file, an unreadable one, or `v` other than 1 | an empty memory; the next write replaces the file | `none` |
| a floor at most 7 days old | the entry | `fresh` |
| a floor older than that | not opened from; dropped at the next write | `expired` |
| a rate outside 50 kbit/s to 100 Mbit/s, a class other than `backlogged` or `app_limited`, a missing or mistyped field, or a record with no window or a first sample after its newest | dropped alone, and never written back | `invalid` |
| a time more than 300 s ahead (a clock that jumped) | dropped alone | `future` |
| a record whose newest sample is more than a day old | the record dropped, the floor kept | `fresh` |

**Why a file of its own.** A release that does not know the model writes
`rate_memory.json` whole, with only the fields it knows. A shared file would lose every
model entry at the first close of such a daemon, after a rollback or on a host whose
update is frozen (`a_frozen_master_loses_the_whole_file_if_the_model_shares_it`). An
older release never opens `path_memory.json`, so a rollback leaves it whole. A host with
no such file opens every pair at the nominal
(`with_no_new_file_every_pair_opens_at_the_nominal`).

### What a session measures (D24, D25)

The governor folds a window's delivery after FR-79's gate has ruled
(`agents/roomlerd/src/encode/governor.rs:1448`). `Pipe` takes it whatever the verdict,
as before. P0's shadow model and the session's memory take it only from a valid window.
So the opener's burst, the drain after a stall, and a rejected window set no stored
floor (`the_opener_burst_and_a_stall_drain_set_no_stored_floor`,
`a_session_of_rejected_windows_writes_nothing`).

`SessionMemory` (`agents/roomlerd/src/encode/pair_memory.rs:828`) opens from the pair's
entry, with its floor as remembered and its record while that is fresh:

| A valid window's reading | The floor | The push-back record |
|---|---|---|
| a delivery, either measured class | raises it (`PathModel`'s law) | ends it only when the delivery carries more than twice the record's rate (`ceiling_learn::CONTRADICTION_FACTOR`) |
| a push-back, `backlogged`, counted | lowers it as `Pipe` does (D30) | a sample |
| a push-back, `backlogged`, not counted | stays | stays; the close line counts it: `samples_unread`, `samples_viewer_late`, `samples_under_plan` or `samples_not_counted` |
| a push-back, `app-limited` | a delivery | a delivery |
| a delivery sample's push-back, P1 or P3 (D46 B2), counted | lowers it, damped, when under it; never raises it ([below](#the-delivery-samples-push-backs-d46-b2)) | a sample |
| anything assumed | never enters | never enters |

**One definition of counted evidence (D29).** A backlogged push-back counts for the
memory only when all four hold, in this order (`pair_memory::Take::of`):

| Test | Set aside as | Why |
|---|---|---|
| the classifier read the window: not `Unknown`, and for `Clear`, `TransitStalled` or `ViewerLate` a plausible viewer age, at least half the round trip (`viewer_rate::age_is_plausible`); `Overproduced` is always read | `samples_unread` | a still screen on a relay that stalls, with no viewer age, left a trusted record of tens of kbit/s on 24 of 336 swept sessions (`a_still_screen_on_a_stalling_relay_leaves_no_trusted_record`) |
| the classifier did not blame the window on the viewer: not `ViewerLate` (the browser's share of the age 100 ms or more over its floor, or the viewer reported `struggling`) | `samples_viewer_late` | the key names the path (the carrier, the user, the viewer's network), not the viewer's device: a record from a slow browser would open the same user's other devices on that network low (`a_window_the_classifier_blames_on_the_viewer_is_not_a_reading_of_the_path`) |
| the sender pushed against the plan: it wrote at least half the target the window ran with (`applied_bps`), or the window is `Overproduced` (`pushing_against_plan`) | `samples_under_plan` | a 40 kbit/s screen whose viewer age stays flat while its link report says the queue grows (`the_screens_families_on_r1_r2_and_the_plan_limited_test`) |
| D25's: at least half the session's best valid delivery | `samples_not_counted` | a dip beside a real delivery |

Inside the session nothing changes: a sample set aside still feeds `Pipe`, P0's model and
the governor as before. Setting samples aside never creates a trusted record where there
was none. It can lower a record's rate, though: with a higher sample set aside, the record's
`P` is the highest of the samples left, and `P ≤ T` is one of D26's two conditions, so the
slow-link profile can engage where the release before left it off (for example counted
samples of 0.8 Mbit/s beside an unread one of 1.2 Mbit/s).

**What a counted sample's window was.** After the gate (which rejects a `TransitStalled`
window and the one after it) and the tests above, a counted sample comes from one of two
states, and the close line counts each (`Read`, `counted_samples_split_by_their_windows_state_and_a_late_viewer_reads_nothing`):
`samples_counted_overproduced`, the sender's own counters (its queue at the budget, a
frame the budget gate skipped, a blocked send, a send that waited 100 ms or more), and
`samples_counted_clear`, a window whose transit share sat under 200 ms over its floor and
the browser's under 100 ms over its own, where only the viewer's link report said the
queue grew. The second is the open class (FR-148 §9, O7): a measurement, not a rule. On
the slow-link replay all five counted refusals are `clear`.

The record is `{bps, at_unix, first_at_unix, windows}`: the highest sample it counted, the
time of its newest and of its first, and how many windows it counted. A session's
samples add to a fresh record. **A record is trusted** when its newest sample is at most
a day old, it counted at least 3 windows, and those span at least 10 s: one sample is
never enough (`trust_needs_three_windows_over_ten_seconds_within_a_day`). A delivery
just above the floor does not end it: on the slow-link replay the record ends trusted
whichever side of the floor the link last refused
(`a_slow_link_ends_trusted_whichever_side_of_the_floor_it_last_refused`).

### The delivery samples' push-backs (D46 B2)

FR-148 D46, step B2 (#3953), in shadow like the rest of this section. A [delivery
sample](#delivery-samples-fr-148-p0--what-the-path-carried-and-whether-the-sender-was-waiting)
closes at a heartbeat about every 2 s, and two kinds of sample are evidence of what the
path carries (D46 item 10). The pump hands each to the governor at a heartbeat
(`DeliverySampler::heartbeat`, then `RateGovernor::on_sample_push_back`): the one that
closes it with the budget off, and with the budget computing the first one after its
settle time (`media_delivery::settle`, below), so one still waiting when the session
closes is never handed. The governor gives it to P0's shadow model
and the session's memory only (`Pipe::observe_sample_capacity`). `Pipe`'s own law, every
rate and gate, and the opener under `shadow` never see it.

| The sample (`media_delivery::push_back_of`) | The budget `off` | The budget in `shadow` or `on` |
|---|---|---|
| **P1**: `queued`, its least acknowledgement delay 250 ms or more over the session's least (O11's standing queue) | a capacity at its own acknowledged rate | the same |
| **P3**: counted (`standing_ms` known), backlogged (SCTP's own class, or the budget held a frame in each of its seconds), the clock never stood still for `least + S` inside it, and it starts at or after the budget's first measured rate on its association | nothing: SCTP's `Backlogged` class alone feeds nothing | a capacity at its own rate |
| a still span of `least + S` met it, from the span's first still reading to the step that read its release | (nothing computes a still span) | nothing, P1 or P3: [the burst](#the-push-backs-in-the-shadow-model-d46-b2) |
| it closed in a stall: the budget marks one within its settle time, `least + S` and one held SACK (200 ms) after the close | (nothing marks a stall) | nothing, P1 or P3: [the stall's start](#the-push-backs-in-the-shadow-model-d46-b2) |
| it starts before that first measured rate, in the budget's opening: the opening rate stood in for the rate, and SCTP's window was still growing | (nothing computes a rate) | no P3; a `queued` one is still P1: [the opening](#the-push-backs-in-the-shadow-model-d46-b2) |
| anything else, an application-limited sample among them | nothing | nothing |

**The law.** A sample's push-back is a capacity at its rate (`PathModel::sample_pushed_back`).
It lowers the floor, damped as a counted push-back lowers it (`rate_memory::damp`), only
when its rate is under the floor, and it never raises a floor or makes one: a sample
over-reads the path only when it holds bytes a stall held back, and a raise is the one
place an over-read would reach. A raise still comes from a valid window's delivery. It
under-reads the path when it closes in a stall, which the settle time keeps out. The
`path model` line says which push-back last set each number (`model_floor_push_back`,
`model_capacity_push_back`): `p1`, `p3`, or `-` for a window's.

**In the memory** a sample is filed as a window's push-back is (D25, D29), under the take
of the last viewer window the gate accepted. After a window the gate rejected it reaches
nothing: it ran beside a stall the viewer saw. A counted one adds to the record and lowers
the floor as D30 says; the close line counts it in `samples_counted` and its split, and
again in `samples_counted_p1` or `samples_counted_p3`.

**The windows after a stall the budget saw.** With the budget computing, each step that
finds a still span standing for `least + S`, and the step that reads its release, mark the
governor (`Shadow::take_mark`, then `RateGovernor::note_path_stall`). The next viewer
window that delivers anything (`pipe::STALL_WINDOWS`, one) is then no reading for the
model or the memory (`Pipe::note_stall`); `Pipe`'s own law takes it as before. The steps
inside the stall mark the window that ends in it, and the step that reads the release
marks the window its burst comes back in, so it is D24's "that window and the one after
it", read from the sender's side: under the budget the transit stays short through a
stall, so FR-79's gate passes the windows that carry its release. Keeping a second
window out kept 15 of a 20 s run's windows out of the model on a stall shape (a stall
every 3.3 s), and the floor did not come back from a sample that read under the path.
The rules measured, and the numbers, are in
[§ The push-backs in the shadow model](#the-push-backs-in-the-shadow-model-d46-b2).

### What a close writes

`pair_memory::close` (`agents/roomlerd/src/encode/pair_memory.rs:1251`) decides, case by
case, from the entry the session opened from and its last would-be write:

| The session | `wrote` | `why` |
|---|---|---|
| measured nothing in a valid window | `nothing` | `nothing_measured` |
| had no key | `nothing` | `no_key` |
| changed path before its first viewer window was taken | `nothing` | `carrier_flipped` |
| measured only at or below the remembered floor | `push_back` when the record moved, else `nothing` | `remembered_only` |
| measured, on a pair with no fresh entry | `floor`, or `floor_and_push_back` | `first_measurement` |
| measured, on an entry whose floor passed its TTL during the session | `floor`, or `floor_and_push_back` | `floor_expired` |
| delivered above the floor | `floor`, or `floor_and_push_back` | `raised` |
| measured a floor equal to the remembered one | `floor` when its time moved to the later reading, else `nothing` | `refreshed` |
| was pushed back below it, with at least one counted sample | `floor`, or `floor_and_push_back` | `pushed_back` |
| measured a lower floor with no counted sample | `push_back` when the record moved, else `nothing`: the remembered floor stays, with its own age | `floor_kept` |

**The floor is a maximum inside its TTL (D30).** A floor is a lower bound on what the
path carries, and lower bounds combine by maximum: only a counted push-back lowers it,
in the session and at the close (`a_quiet_session_keeps_the_remembered_floor`,
`a_counted_push_back_lowers_the_floor_and_a_dip_beside_it_does_not`,
`the_close_keeps_the_higher_floor_inside_its_ttl`). Since the in-session floor moves
down only on a counted sample, `floor_kept` is a guard the close keeps rather than a case
a session reaches.

**A sample at or after the close** reaches no record, by construction, so no test pins it
(a cell that tried could not fail). The guard writes the closing of the last viewer window
the pump handed it (`PumpMemory::window`, right after each window), a copy taken then, so
nothing that happens after that window reaches the write. The delivery sampler holds no
reference to the governor, the session's memory or the guard. Since D46 B2 the pump lends
it the governor for two calls: at the heartbeat, a sample's push-back
(`RateGovernor::on_sample_push_back`), and at a frame, a stall's mark
(`RateGovernor::note_path_stall`). Both change the governor's shadow model and its memory
only, and a push-back reaches the write through the next viewer window's copy, so one
taken after the last window reaches no record either. The pump drops the guard before the sampler
(`peer.rs` declares `pair_memory` after `delivery`), so the sampler's last line comes after
the close line.

A floor is written with the class and time of the reading that set it, never the close's
time. The remembered floor is never written back as if this session had measured it:
`PathModel::offer` offers nothing while the floor is the remembered one (D23,
`a_remembered_floor_is_not_written_back`). An entry that would be written unchanged is
not written.

**The freeze.** A model guard exists only on a session that opened constrained. It
takes each viewer window's would-be write. The first window whose transport is not
constrained freezes it for good, and the close writes what the last window before it
would have (`a_change_of_path_freezes_the_offer`, design §2.7).

**A move between two relay tiers is kept, not frozen** (TURN to the mesh's relay, or
back; `a_move_between_relay_tiers_keeps_the_write`). The session stays constrained, and
the close writes its last window under the opening carrier's key. Why kept: the pump's
recheck reads relay or not (`current_pair_is_relay`, which the VP9 4:4:4 pump shares and
this phase does not change), not the carrier, and a carrier read at each recheck is a
LocalAPI call and new pump lines. What a write under the wrong carrier costs is bounded
as for a key that merges two paths: the opening is at most `hi`, a floor moves down only
on a counted sample, a record needs three counted windows over 10 s, and under `shadow`
nothing reads it. A freeze on a carrier change belongs with the carrier rank (FR-148 P4),
which makes such a move deliberate.

### The model's opening (D25, D26)

```text
O       = trusted ? clamp(⌊0.85·D⌋, 200 kbit/s, hi) : max(N, min(⌊0.85·D⌋, hi))
profile = trusted && P ≤ T && O ≤ T
```

| Symbol | Is |
|---|---|
| `N` | `nominal_bps`: the relay cap at the session's Priority dial, at least `MIN_BITRATE_BPS` (`media_pair_memory.rs:487`) |
| `hi` | the constrained learner's ceiling, `relay_max_hi_bps` |
| `D` | the entry's floor |
| `P` | the record's rate |
| `T` | the slow-link threshold, `slow_link_profile_bps`, while `slow_link_profile` is on; else 0, never |

With no fresh entry, `O = N` and no profile (`the_models_opening_follows_the_rule`). The
open line writes `O` and the profile beside today's rule's opening, in every mode.

### Which rule opens the session (step 3)

`Taken::decide` (`agents/roomlerd/src/encode/pair_memory.rs:685`), once, at the pump's
open (`PumpMemory::decide`, `agents/roomlerd/src/media_pair_memory.rs:616`):

| Switch | Key | The pair's entry | Opens by | `taken` | `taken_why` |
|---|---|---|---|---|---|
| `off` | | | today's rule | no line | |
| `shadow` (the default, D28) | any | any | today's rule | `today` | `shadow` |
| `on` | none | | today's rule | `today` | `no_key` |
| `on` | yes | none | today's rule | `today` | `no_entry` |
| `on` | yes | dropped at the read: a bad value, or a time ahead | today's rule | `today` | `entry_invalid` |
| `on` | yes | a floor older than 7 days | today's rule | `today` | `floor_expired` |
| `on` | yes | fresh | the model | `model` | `-` |

Today's rule is FR-35's and FR-59 P8's: the measured seed of `rate_memory.json`, so an
entry written before #3374 is read as assumed and seeds nothing (AC7). How each consumer
of the opening takes it (`OpenPlan`, `pair_memory.rs:600`; `RateGovernor::new_with`,
`agents/roomlerd/src/encode/governor.rs:482`):

| What the session acts on | Today's rule, seed `s` | The model, floor `D` |
|---|---|---|
| the ceiling learner's seed: 85 % of it is the learned ceiling, so the first tick's ceiling is `max(N, min(0.85·seed, hi))` | `s` | `D` |
| where the AIMD's first target, the first encoder, a replacement before the first tick and slow start open, when under the ceiling (`open_at_bps`) | `s`, when under 1.5 Mbit/s with the floor relief on (P8) | `O` after a trusted push-back (with the relief off, at least 1.5 Mbit/s, where the AIMD's floor would lift it a tick later); else the ceiling, which is `O` |
| the floor relief and the prior (P8) | `s` under 1.5 Mbit/s, the relief on | `O` under 1.5 Mbit/s, the relief on |
| the shadow controller, and the rate P0's model remembers | `s` | `D` |
| the slow-link profile, resolved once at the open | `s ≤ T` | D26: a trusted push-back at or under `T`, and `O ≤ T` |
| the viewer's cap detail, "remembered N kbps" | `s` | `O` when D26 engages the profile; else none (`PumpMemory::seed`) |
| the heartbeat's seed age | the entry's | the floor's |

Two lines say "remembered" when the model opened the session (ledger `p1:OPENER-3`): the
cap detail above, and FR-59 P8's open line, "the pair is remembered slower than the
nominal floor: opening at the remembered rate" (`agents/roomlerd/src/peer.rs:7210`), which
under the model fires when its trusted opening is under 1.5 Mbit/s. Its `seed_bps` is then
`O`, not a remembered rate; the open line's `taken=model` tells them apart. Its words stay
until P5, since `scripts/rc-path-report.cjs` matches them.

N does not enter what drives the session: after a trusted push-back the opening is
`clamp(⌊0.85·D⌋, 200 kbit/s, hi)`, and without one the learner seeded with `D` gives the
ceiling at the plan's own N. **The opener's grace stays** (D27): 2 s on a constrained
session, 3 s at most, ended at 1.2 s when nothing drains. **Nothing changes for a direct
path or the VP9 4:4:4 pump**: neither reads either memory. In the pump the change is in
place: `new_with` with `pair_memory.plan()`, and the two encoder opens read
`open_at_bps` (`agents/roomlerd/src/peer.rs:7201`, `:8760`, `:8962`).

| Test | Shows |
|---|---|
| `media_pair_memory::tests::shadow_is_todays_opener_exactly` | D28's twin: under `shadow` and `off`, for 8 of today's seeds × 3 model entries × 3 governor set-ups, the plan, the profile seed and its age are today's, and the governor acts on master's values, recorded on the base before step 3 (`MASTER`); `open_at_bps`, which the pump's two encoder opens read, is master's `open_seed_bps`, which they read on master. Moving P8's 1.5 Mbit/s bound by 0.5 Mbit/s turns it red |
| `on_opens_where_the_model_says` | the design's §3.3 rows through the governor under `on`, with master's opening beside the two AC3 chains and §2.1's 281,732 bit/s: master opens each lower, on the profile |
| `the_open_line_says_which_rule_opened_and_why` | the table above, line by line |
| `twenty_still_screens_keep_the_floor_and_its_age` | AC6 under `on`, through the file: after a session of 6.5 Mbit/s, twenty still screens each open at 5.525 Mbit/s or more, and the entry does not move |
| `roomler_node_core::config_surface::tests::path_model_set_echo_clear_and_validate` | the config key |

### The two lines

Both are at INFO, and only on a constrained session of the FFmpeg data-channel pump with
the switch not `off`.
`FR-148 P1 path memory: the pair's entry at the open` is written once the governor is
built (`ModelGuard::open_line`, `media_pair_memory.rs:326`).
`FR-148 P1 path memory: what the close wrote` is written as the pump drops, after
today's file (`media_pair_memory.rs:381`). A test pins the names and their order
(`the_two_lines_name_their_fields`).

| Open line field | Meaning |
|---|---|
| `session_id`, `codec_label` | the session and its codec |
| `mode` | the switch: `shadow` or `on` |
| `key_id`, `key_carrier`, `key_net` | the key's hash, its carrier word, and what its network part was made from; `-` with no key |
| `no_key` | why there is no key; `-` with one |
| `entry` | `none`, `fresh`, `expired`, `invalid` or `future` (the file's table) |
| `floor_bps`, `floor_basis`, `floor_age_s` | the fresh entry's floor, its class, and the age of the reading that set it |
| `push_back_bps`, `push_back_age_s`, `push_back_windows`, `push_back_span_s` | its record |
| `model_trusted` | the record is trusted |
| `nominal_bps` | `N` |
| `model_open_bps`, `model_profile` | what the model would open at, and whether at the slow-link profile |
| `today_seed_bps`, `today_open_bps`, `today_profile` | today's seed, the opening today's rule takes, and whether its slow-link profile engages |
| `taken` | which rule opened the session: `today` or `model` |
| `taken_why` | why today's rule opened it: `shadow`, `no_key`, `no_entry`, `entry_invalid` or `floor_expired`; `-` when the model did ([the table](#which-rule-opens-the-session-step-3)) |

| Close line field | Meaning |
|---|---|
| `session_id`, `key_id`, `key_carrier`, `key_net` | as at the open |
| `wrote` | `nothing`, `floor`, `push_back` or `floor_and_push_back` |
| `why` | the close's case (above) |
| `floor_bps`, `floor_basis`, `floor_age_s` | the floor as it stands after the close: the one written, else the one the session opened from |
| `best_bps`, `best_basis` | the session's highest valid delivery |
| `push_back_bps`, `push_back_windows`, `push_back_span_s` | the record as it stands |
| `samples_counted` | backlogged push-backs that counted (D29) |
| `samples_counted_overproduced`, `samples_counted_clear` | the counted ones by the state of their window: the sender's own counters, or `Clear`, where only the link report said the queue grew (the open class, FR-148 O7); they sum to `samples_counted` |
| `samples_not_counted` | backlogged push-backs under half the best delivery |
| `samples_unread` | backlogged push-backs from a window the classifier could not read (D29) |
| `samples_viewer_late` | backlogged push-backs from a window the classifier blamed on the viewer, `ViewerLate` (D29) |
| `samples_under_plan` | backlogged push-backs from a window whose sender wrote under half its target (D29) |
| `record_ended` | a delivery over twice the record's rate ended the record |
| `frozen` | the path changed during the session |
| `samples_counted_p1`, `samples_counted_p3` | of `samples_counted`, the delivery samples' push-backs: a queued sample's (P1), and a budget-backlogged one's with no still span (P3) (D46 B2, [§ The delivery samples' push-backs](#the-delivery-samples-push-backs-d46-b2)); a field read tells them from a window's push-back by these |

A session pushed back three times, once in an `Overproduced` window and twice in `Clear`
ones, with a late viewer's push-back beside them, and the session after it (the test
prints both with `--nocapture`; each line is 494 to 600 bytes without its timestamp, two a
session):

```text
FR-148 P1 path memory: what the close wrote session_id=… key_id=b2eb8bbf6a9f key_carrier=turn key_net=srflx wrote=floor_and_push_back why=pushed_back floor_bps=Some(722500) floor_basis=backlogged floor_age_s=Some(7) best_bps=Some(1200000) best_basis=app_limited push_back_bps=Some(640000) push_back_windows=Some(3) push_back_span_s=Some(13) samples_counted=3 samples_counted_overproduced=1 samples_counted_clear=2 samples_not_counted=0 samples_unread=0 samples_viewer_late=1 samples_under_plan=0 record_ended=false frozen=false samples_counted_p1=0 samples_counted_p3=0
FR-148 P1 path memory: the pair's entry at the open session_id=… codec_label="AV1" mode=shadow key_id=b2eb8bbf6a9f key_carrier=turn key_net=srflx no_key=- entry=fresh floor_bps=Some(722500) floor_basis=backlogged floor_age_s=Some(7) push_back_bps=Some(640000) push_back_age_s=Some(7) push_back_windows=Some(3) push_back_span_s=Some(13) model_trusted=true nominal_bps=2550000 model_open_bps=614125 model_profile=true today_seed_bps=None today_open_bps=2550000 today_profile=false taken=today taken_why=shadow
```

The model opens that pair at 614,125 bit/s (0.85 × 722,500) on the slow-link profile;
under `shadow` today's rule opened it at the nominal, 2,550,000. Under `on` the same
line reads `mode=on`, `taken=model taken_why=-`, and the session opens at 614,125.

### The switch (D28)

`ROOMLERD_PATH_MODEL`, or the config key `path_model`, read once per pump
(`media_delivery::mode`, `agents/roomlerd/src/media_delivery.rs:157`):

| Value | Delivery samples, path numbers, their lines | The path memory, its file and its lines | The opener |
|---|---|---|---|
| `off` | none: the release before P0 | none | today's memory |
| `shadow`, unset, or any value but `off` and `on` | taken and written | kept and written | today's memory |
| `on` | as `shadow` | as `shadow` | the path memory, when the pair has a key and a fresh entry; else today's memory ([the table](#which-rule-opens-the-session-step-3)) |

A test reads the three (`the_switch_has_three_modes_and_defaults_to_shadow`). Today's
memory is not under the switch: it seeds and writes as before in every mode.

**The config key** is built as `rc_carrier_rank`'s was (#4013): the field, its env
fallback, the key's metadata, its echo and its check
(`crates/agent-core/src/config.rs`, `crates/agent-core/src/config_surface.rs`).
⚠️ **`on` is set on no device until FR-148's open decision O7 is ruled**; until then it is
for tests. One false record under `on` lowers the pair's floor with it, and every session
on that key for up to 24 h (renewed by each push-back that counts) then opens at about
0.85 of it, on the slow-link profile for the whole session when the record is under the
profile's threshold, as a false slow record is (D26). A device found at `on` is
taken back with `roomler config set path_model shadow`, then `roomler restart`; a device
with `ROOMLERD_PATH_MODEL` set loses the variable as well. A set
`ROOMLERD_PATH_MODEL` wins over the key, and the value is checked: anything but `off`,
`shadow` or `on` is refused at `config set`. `shadow` is stored, not cleared, so a device
taken back stays at `shadow` when a release changes the default. Remote config cannot
set it ([remote-config.md](remote-config.md)). The pump's one read of the switch stays
in `media_delivery`, so the env-read count of the FR-135 ratchet does not move.

| Where | Anchor |
|---|---|
| the key, the file, the session, the close, the opening | `agents/roomlerd/src/encode/pair_memory.rs:248` (`pair_key`), `:226` (`viewer_net`), `:416` (`PathMemory`), `:515` (`PathMemory::update`, the lock), `:388` (the entry's check), `:828` (`SessionMemory`), `:1251` (`close`), `:563` (`model_opening`), `:600` (`OpenPlan`), `:685` (`Taken::decide`) |
| counted evidence (D29) | `agents/roomlerd/src/encode/pair_memory.rs:729` (`window_read`), `:750` (`pushing_against_plan`), `:796` (`Take::of`); `viewer_rate::age_is_plausible` at `agents/roomlerd/src/encode/viewer_rate.rs:709` |
| the glue: facts, guards, the lines | `agents/roomlerd/src/media_pair_memory.rs:61` (`PairFacts`), `:210` (`RateMemoryGuard`), `:262` (`ModelGuard`), `:326` (the open line), `:381` (the write and the close line), `:518` (`open`), `:616` (`decide`), `:660` (`plan`), `:672` (`at_open`), `:705` (`window`) |
| the governor's opening | `agents/roomlerd/src/encode/governor.rs:482` (`new_with`), `:591` (`open_at_bps`), `:600` (`opening_of`) |
| the gate's verdict before the fold | `agents/roomlerd/src/encode/governor.rs:1448`; `Pipe` at `agents/roomlerd/src/encode/pipe.rs:221` (`observe_delivered_in`), `:767` (`PathModel::offer`) |
| the samples' push-backs (D46 B2) | `agents/roomlerd/src/encode/governor.rs:1444` (the take they are filed under), `:2118` (`on_sample_push_back`), `:2129` (`note_path_stall`); `agents/roomlerd/src/encode/pipe.rs:332` (`observe_sample_capacity`), `:350` (`note_stall`), `:647` (`PathModel::sample_pushed_back`); `agents/roomlerd/src/encode/pair_memory.rs:1071` (`sample_pushed_back_in`) |
| the FFmpeg pump | `agents/roomlerd/src/peer.rs:6622` (open), `:7201` (`new_with`), `:7221` (`at_open`), `:9341` (`window`), `:9432` (`opener_measured`) |

**The tests** (`cargo test -p roomlerd --lib -- pair_memory governor:: media_delivery`):

| Test | Shows |
|---|---|
| `encode::pair_memory::tests::two_relay_workers_are_one_entry` and the seven key cells beside it | the key's merges and splits, the srflx rule, no key without a user or a carrier, the line's hash |
| `a_bad_entry_drops_alone`, `a_future_entry_is_dropped`, `a_version_other_than_one_reads_as_empty`, `the_file_round_trips_and_drops_what_expired` | the file's checks; a dropped entry is never written back |
| `todays_file_is_written_byte_for_byte_as_master`, `a_frozen_master_loses_the_whole_file_if_the_model_shares_it`, `with_no_new_file_every_pair_opens_at_the_nominal` | today's file untouched, and why the model's is its own |
| `trust_needs_three_windows_over_ten_seconds_within_a_day`, `windows_add_up_across_two_sessions_while_the_record_is_fresh`, `a_sample_under_half_the_sessions_best_valid_delivery_does_not_count`, `a_delivery_ends_the_record_only_above_twice_its_rate`, `the_corrected_rule_keeps_the_record_the_first_wording_ended` | the push-back record (D25) |
| `a_remembered_floor_is_not_written_back`, `a_session_that_measured_nothing_writes_nothing`, `the_write_takes_the_readings_class_and_time`, `the_models_opening_follows_the_rule` | the close and the opening |
| `encode::governor::tests::the_opener_burst_and_a_stall_drain_set_no_stored_floor`, `a_session_of_rejected_windows_writes_nothing`, `a_slow_link_ends_trusted_whichever_side_of_the_floor_it_last_refused` | D24 and D25 on the governor's replays |
| `encode::governor::tests::a_still_screen_on_a_stalling_relay_leaves_no_trusted_record` | D29's read test: 336 still screens on a 6 Mbit/s relay that stalls, swept over the viewer's age, the stall's depth and length; under the release before's law (`set_read_any`) 24 close with a trusted record, under D29 none |
| `the_screens_families_on_r1_r2_and_the_plan_limited_test` | D29's plan test, on the ruling's three families: shallower slowdowns, the viewer's age flat while its queue grows, a steady offer under the target; each family's count of sessions that leave a trusted record or a lowered floor, with and without the plan test (`set_plan_any`) |
| `encode::pair_memory::tests::a_quiet_session_keeps_the_remembered_floor`, `a_counted_push_back_lowers_the_floor_and_a_dip_beside_it_does_not`, `the_close_keeps_the_higher_floor_inside_its_ttl` | D30 |
| `pumps_that_end_at_once_each_keep_their_key` | the lock: eight pumps end at once, twenty times; every key is kept |
| `a_file_held_without_read_sharing_keeps_every_other_key` | a close whose read fails other than on a missing file saves nothing, and every other pair's entry stays (Windows: a handle with no read sharing, OS error 32) |
| `counted_samples_split_by_their_windows_state_and_a_late_viewer_reads_nothing`, `encode::governor::tests::a_window_the_classifier_blames_on_the_viewer_is_not_a_reading_of_the_path` | D29's viewer test and the close line's split: a late viewer's push-backs reach the session's belief and not the memory, and the classifier's tie stays `TransitStalled`, rejected by the gate |
| `media_pair_memory::tests::a_srflx_candidate_line_gives_its_address`, `a_sessions_facts_live_as_long_as_its_peer`, `todays_tag_is_formed_as_before`, `a_change_of_path_freezes_the_offer`, `the_two_lines_name_their_fields` | the glue: the facts, today's key unchanged, the freeze, the lines |
| `a_move_between_relay_tiers_keeps_the_write` | a relay-tier move is kept, not frozen |
| `media_delivery::tests::the_switch_has_three_modes_and_defaults_to_shadow` | the switch |
| `encode::pipe::tests::a_sample_push_back_moves_only_the_model_and_never_raises_its_floor`, `a_stall_the_budget_saw_keeps_the_next_window_out_of_the_model`, `encode::governor::tests::a_sample_push_back_moves_only_the_shadow_model` | D46 B2: a delivery sample's push-back in the model and the memory, counted and split as a window's and again as `samples_counted_p1` and `samples_counted_p3`, nothing after a rejected window, and the window after a stall ([above](#the-delivery-samples-push-backs-d46-b2)) |

The cells of step 3 are in [its section](#which-rule-opens-the-session-step-3).

**What is left.** The default stays `shadow` until the fleet's shadow lines and the
lab's shaped cell are read (AC6 to AC9, AC14); D28 sets the order. P5 deletes today's
memory. Until then two memories sit side by side under two keys, recorded in the debt
ledger as `p1:MEMORY-1`, the pump's four calls into them as `p1:MEMORY-2`, and the
opener's two rules as `p1:OPENER-1` (`docs/plans/rc-media-debt.tsv`). Under `on`, a
screen of about 300 kbit/s whose viewer age stays flat while its link report says the
queue grows can still leave a false record (D29); no test against the target a window
ran with separates it from a real slow link. That is the open class, FR-148 O7: the close
line's `samples_counted_clear` measures it on the fleet's shadow lines, and `on` is set on
no device until it is ruled ([§ The switch](#the-switch-d28)).

**Limits.**
- The record's and the floor's times are the session's monotonic clock, turned into Unix
  time at each viewer window. On Linux and macOS that clock stops during a host suspend,
  so a window taken after the agent's host wakes, with the session still open, writes them
  younger by the time it slept, and the record stays trusted that much longer (ledger
  `p1:MEMORY-3`).
- The lock serializes the close's file I/O: when several pumps end at once, each close
  blocks its runtime worker while the others read, write and rename the file. It is
  bounded, since nothing awaits under the lock and nothing nests, and today's memory does
  the same.
- The twin test (`shadow_is_todays_opener_exactly`) holds at one `N` (2.55 Mbit/s) and
  one `hi` (8 Mbit/s), with the flags the cell sets rather than `from_env`'s: master's
  values for another `N` or `hi` were not recorded. It compares `open_at_bps`, the value
  the pump's two encoder opens read, but not the opens themselves (ledger `p1:OPENER-4`).

## The in-flight budget (FR-148 D46) — what may be out, in time

FR-148 D46, step B1 (#3953), in shadow: both data-channel pumps compute the budget once a
frame and count what it would have done, and nothing reads the verdict. Since step B3 the
budget acts under `on` on a relay session
([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)). Every bound on
what a pump has out is in bytes or frames today: the send channel's depth, the VP9-444
pump's watermark on `dc.buffered_amount()` (256 KiB on a constrained relay), and the
FFmpeg pump's byte ledger and its gate. A byte bound is a different wait on every link
(256 KiB is 1 s at 2 Mbit/s and 5 s at 0.4), and none bounds how long the bytes wait on a
carrier that queues, as TURN over TCP does
([§ The standing queue](#the-standing-queue-what-a-tcp-carrier-holds)). The budget
bounds what is out in time:

```text
out    = the pump's unsent bytes + SCTP's pending bytes + SCTP's bytes in flight
window = min(cwnd, rwnd + inflight)
budget = max( min(rate, window / least) × (least + S),  3 × max_payload )

a frame is admitted while out < budget, and held at out = budget or over, except:
  out = 0                                      nothing is out
  cwnd > 0, pending = 0 and inflight ≥ cwnd    SCTP's own window binds
  a still span after a head-of-line release,   written through the stall
    pending = 0 and out < budget + rate × still_age     (D46's classifier, below)
```

| Term | What it is |
|---|---|
| `out` | over the whole association: the pump's own count of the bytes it handed its send task and SCTP has not taken yet (below), and SCTP's pending and in-flight bytes from its gauges |
| `rate` | the acknowledgement clock's bytes over the last second, the most of those figures over the last two; a second that starts inside a released still span, or while its release comes back, counts from the span's first still reading (below). Before the first full second, the rate it last stood at, else the pump's opening rate (the governor's applied rate, or the rate it opened at); with neither, the window alone bounds it |
| `least` | the least acknowledgement delay a peek of the gauges showed on this association and pair; S before the first SACK |
| `S` | 150 ms, a constant (`SLACK`) |
| `window` | SCTP's congestion window, or less where the peer's receive window binds |
| the floor | three full packets |

**The two exceptions.** A frame goes whatever the budget when nothing is out. And the
vendored SCTP grows its congestion window only while a chunk waits in its queue (slow
start and congestion avoidance both ask for one), so where its own window binds and
nothing waits, a frame goes too: without that clause the prototype left a window that a
T3 had cut to one packet there for 3 s of a control's run, waiting on it.

**The rate falls only on evidence.** A second in which the budget did not hold was
application-limited: the clock saw what the pump had, not what the path carries. Such a
figure never lowers the rate and never lets it lapse, so a still screen that wakes gets
the last busy rate back. A figure lowers the rate only when the budget held in each
quarter of its second; a second held only at one edge is the edge of a still stretch.
The holds are kept as one bit per 50 ms, and the clock's readings and the figures in
fixed rings, so a step allocates nothing.

**The least starts over** on a new association, which a new clock handle shows, and on a
new selected candidate pair, which the pumps read at their 5 s re-check of the
transport, outside its env pin. It is a read, not the ICE transport's pair-change
handler: that handler is one slot per transport, which a second registration silently
replaces, and the pumps already look at the transport every 5 s, so a new pair's least
starts over within 5 s. The least takes a gauge's delay only when it changed since the
last peek, so the delivery sampler's takes, which reset `ack_delay_min`, cannot carry an
old least across a reset.

### Through a stall: the stall classifier (D46)

FR-148 D46's next step after B1, in shadow like it. B1 holds through every still span
alike: once the clock has stood still for `least + S` with bytes out, `out` stays at the
budget until the clock moves. Where the carrier is a TCP leg (TURN over TCP) and one
segment is lost, the leg keeps serializing behind the hole, and the receiver hands over
the whole stall at once when it fills. The frames written in the stall's last 300 ms then
still arrive fresh, and B1 held them: its fresh delivery on the 80 % path I stall shape
was 0.876, 0.880 and 0.952 of today's writer's. The classifier tells that stall from one
through which nothing moved, from the acknowledgement clock alone:

```text
a still span:  bytes out, the clock unmoved for least + S             (B1's, unchanged)
its release:   read at the first frame 50 ms or more after the span's last still reading
  head-of-line   600 ‰ or more of what SCTP had in flight at that reading is acknowledged:
                 a burst, so the carrier serialized through the stall
  drain          less, or a first frame at 600 ‰ of least + S or later: the release
                 acknowledges at the rate the link drains (an outage, or a TCP RTO)
the next still span, once its age is least + S, after one head-of-line release:
  a frame goes while  out < budget + rate × still_age  and SCTP's pending = 0
  otherwise it holds, as in B1; the first still span on an association holds too
a new association or a new selected pair forgets the class, with the least
the rate: a 1 s figure whose second starts inside a released still span, or while its
          release comes back, counts from the span's first still reading
```

```mermaid
flowchart TD
  S["a still span:<br/>bytes out, the clock unmoved<br/>for least + S"] --> C{"the last release<br/>read head-of-line?"}
  C -->|"no, or none yet"| H["hold at the budget, as B1"]
  C -->|yes| P{"SCTP pending = 0 and<br/>out < budget + rate × still_age"}
  P -->|yes| W["write through"]
  P -->|no| H
  S -. "the clock moves" .-> R{"in-flight bytes back<br/>at the first frame 50 ms on"}
  R -->|"600 ‰ or more"| HL["head-of-line:<br/>the next span writes through"]
  R -->|"less, or too late to tell"| D["drain:<br/>the next span holds"]
```

| Term | What it is |
|---|---|
| `still_age` | how long the span has stood, from its first still reading |
| `rate` | the budget's rate at that frame: the write-through goes no faster than the clock measured the carrier |
| what SCTP had in flight | its in-flight bytes at the span's last still reading. Pending bytes are not counted: SCTP had not sent them, and no release brings them back |

**Why in flight, why 50 ms, why 600 ‰.** A head-of-line release brings back at once
what the carrier serialized through the stall; a drain at the link's rate brings back in
`t` what the link carries in `t`, and with a budget's worth in flight (the rate over
`least + S`) that is `t / (least + S)` of it. Read at `least + S`, a drain has brought
back most of it too, so the read comes early: at the first frame 50 ms or more after the
last still reading, at most one frame interval later. A first frame at 600 ‰ of
`least + S` or later is too late to tell, and reads as a drain. The values are the
`tcp_shape` stall and outage cells' (below), every release of every cell under one set,
never per cell. On the first exploratory pass (live: 100 ms, 750 ‰, and SCTP's out with
its pending bytes as the share's base) its 40 releases, 20 head-of-line stalls and 20
outages, read:

| Read at, ms after the last still reading | Share of | Least head-of-line, ‰ | Most drain, ‰ | Misread at 600 ‰ | Misread at 750 ‰ |
|---|---|---|---|---|---|
| the first frame | SCTP's out | 15 | 178 | 8 + 0 | 11 + 0 |
| 50 | SCTP's out | 247 | 282 | 4 + 0 | 9 + 0 |
| 100 | SCTP's out | 247 | 567 | 3 + 0 | 8 + 0 |
| 33 | in flight | 99 | 439 | 1 + 0 | 3 + 0 |
| **50** | **in flight** | **694** | **439** | **0 + 0** | 3 + 0 |
| 80 | in flight | 707 | 576 | 0 + 0 | 2 + 0 |
| 100 | in flight | 707 | 879 | 0 + 2 | 2 + 1 |
| 133 | in flight | 707 | 923 | 0 + 8 | 2 + 3 |

A misread is a head-of-line release read as a drain, plus a drain read as head-of-line.
SCTP's out never separates them: on path C SCTP's window binds, and a stall's backlog
waits in its queue, where no release can bring it back. 500, 700, 800 and 900 ‰ were
swept too. 500 and 600 ‰ both separate the 40 at 50 ms; 600 also keeps a drain under it
at any least when the read comes one 33 ms frame late (83 ms over a `least + S` of
150 ms is 553 ‰), and the least head-of-line release, 694 ‰, still clears it. Read the
same way, the three passes below misread 6 + 0 of the budget's 171 releases (111 on the
stall shapes, 60 in the outages) at 50 ms and 500 to 700 ‰: the six are stalls a lone
packet's acknowledgement split (the open edges, below). At 67 ms they misread 6 + 4, at
100 ms 6 + 9.

**Only while SCTP queues nothing.** On path C, SCTP's window binds through a stall, and
what is written through waits in its queue for the release, then goes ahead of the
frames captured after it. On the exploratory pass a 778 ms stall on the 80 % path C shape
ended with 99,990 bytes pending against 59,994 in flight, and the shape's transit reached
449 ms at most against B1's 106 to 144 ms, for the same fresh delivery (58 % against
57–59 %).

**The rate counts a stall from its start.** A 1 s figure whose second starts inside a
still span sees the span's whole burst come back in less time than the carrier took to
serialize it. With the write-through that burst is the carrier's rate over the stall,
and on the exploratory pass the rate stood at 2,651 to 2,838 kbit/s on the 80 % path I
shape and at 3,361 kbit/s on the 150 % path I shape, on a link of 1,954. Such a second
now counts from the span's first still reading and the count the clock stood at, and
the budget it sizes stays at the link's. A release can come back over two frames: on
the first full pass one on the 80 % path I shape brought back 30,264 bytes of the
150,918 in flight by its first frame, 46 ms after the span's last still reading, and
146,652 by the next, 76 ms after it, and the rate stood at 2,421 kbit/s at the next
release. A second that starts while the release comes back, up to the frame that reads
it, now counts from the span's start too.

**One head-of-line release is enough.** The cells' last writer asks two in a row
(`HOL_RELEASES` = 2). It delivered less fresh than one release in each of the nine runs
with a ratio, and on the 80 % path I shape fell under the bar in the third: 1.055, 1.039
and 0.944 of today's writer's, against 1.072, 1.059 and 0.957. A second release guards
against a drain read as head-of-line, and no cell read one: every release in the outage
cells read as a drain, 120 of 120 over the three runs.

**The AIMD is unchanged:** a still span calls no decrease (D46).

### Where it runs

```mermaid
flowchart LR
  G["today's gates:<br/>the send channel's capacity,<br/>the FFmpeg ledger"] -->|let through| B["the budget's step:<br/>a peek of the gauges,<br/>a read of the clock"]
  G -->|held| N["bound_full,<br/>bound_ledger"]
  B -->|"the verdict, counted only (B1)"| W["the line's window"]
  B --> P["pacing, capture,<br/>encode"]
  P --> T["try_send Ok:<br/>the frame's bytes taken"]
  T --> S["the send task:<br/>each 16 KiB chunk's dc.send<br/>releases it"]
```

Both data-channel pumps step it, the VP9-444 pump and the FFmpeg pump, once per pass
that today's gates let through, before pacing and capture. The step reads SCTP's gauges
by `peek`, never `take`, so the delivery sampler's takes stay its own (FR-148 O10), and
the clock by `read`. The pump's unsent bytes are a count of the budget's own, apart from
today's ledger: taken where `try_send` returns Ok, released after each 16 KiB chunk's
`dc.send`, and the rest released when a send fails or the send task drops the frame (a
stale epoch, no channel). In shadow the FFmpeg ledger and every gate are untouched.
Under `on`, on a relay session, the step runs before today's gates instead, and its
verdict acts there ([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)).
The RTP pump has no SCTP association and no budget.

**Shadow acts on nothing.** Every call into the budget returns nothing to the pump; the
verdict reaches the line's counters and nothing else.

| Proof | What it shows |
|---|---|
| `media_delivery::tests::the_inflight_budget_in_shadow_answers_every_pump_call_as_off_does` | over 900 frames of a deep queue, a sampler with the budget in shadow and one without it get the same calls and answer each alike: the same samples, and the same lines but for the budget's field, while the shadow's verdicts held |
| `media_delivery::shaped_link::tcp_shape::the_inflight_budget_in_shadow_holds_nothing_on_a_shaped_link` | today's writer on the deep 2 Mbit/s path I shape, off and then in shadow through the pumps' own `Shadow`: in each of three runs it writes at every tick and holds and skips nothing in both (off 480, 480 and 481 ticks, shadow 480, 479 and 481), while the shadow's verdicts would have held 479, 478 and 479 frames. Today's skip writer, whose skips follow the link's timing, skipped 193 to 195 frames a run off and 193 to 195 in shadow, and held none. With the classifier, in the three runs below: off 480, 478 and 478 frames written of 481, 479 and 479 ticks, shadow 480, 479 and 478 of 481, 480 and 479, nothing held or skipped in either, while the shadow's verdicts would have held 480, 479 and 478; the skip writer skipped 193 to 194 a run off and in shadow, and held none |
| the diff | every line B1 added to the pumps is a count or a read; no path leads from the verdict to a gate, an AIMD call or a keyframe request. B3's one path, the budget's pass, acts only under `on` on a relay session: in shadow the pass is today's (`off_and_shadow_gate_as_today`, below) |

### The budget's switch

`ROOMLERD_INFLIGHT_BUDGET`, or the config key `inflight_budget`, read once per pump
beside `ROOMLERD_PATH_MODEL` (`media_inflight::mode`,
`agents/roomlerd/src/media_inflight.rs:150`, from `DeliverySampler::from_env`,
`agents/roomlerd/src/media_delivery.rs:558`):

| Value | The pumps |
|---|---|
| `off`, unset, or any other value | read nothing and count nothing; the `path delivery` line is as before, to the byte |
| `shadow` | compute the budget, count its verdicts and today's bounds beside them, and write them at the end of the `path delivery` line; hold nothing. Since B2, hand the shadow model what the budget saw: P3, the burst rule and the stall's windows ([below](#the-push-backs-in-the-shadow-model-d46-b2)) |
| `on` | on a relay session, act on the budget (D46 B3, [below](#the-budget-acts-under-on-d46-b3)): its hold skips a pass before capture, a hold in a still span of `least + S` calls no AIMD decrease, and the FFmpeg relay byte gate and the VP9-444 watermark arm hold nothing. On a direct session, as `shadow`. The pump says once which mode acted on its session |

With `ROOMLERD_PATH_MODEL=off` the budget stays off whatever its switch says, with one
INFO line: its fields ride on the `path delivery` line, which `off` does not write. The
config key is built as `path_model`'s: the field, its env fallback, the key's metadata,
its echo and its check (`crates/agent-core/src/config.rs:1228`,
`crates/agent-core/src/config_surface.rs:1451`). A set variable wins over the key, any
value but `off`, `shadow` or `on` is refused at `config set`, and remote config cannot
set it ([remote-config.md](remote-config.md)). The read is in `media_inflight`, inside
the FR-135 ratchet's media path, so the ratchet counts it (ledger `p2:BUDGET-1`).

### The budget's fields

The budget's window is the line's: its fields close `path delivery (FR-148 P0, shadow,
≈10s)`, after `loss`, or after `acked_bytes` on a line without the gauges, and only with
the switch not `off`. No name holds `standing_ms=` or `queued=`, which field reads grep
for.

| Field | Meaning |
|---|---|
| `budget_held` | `(held, offered)`: the frames the budget would have held, of those it was asked about |
| `budget_max` | the most the budget stood at, bytes |
| `budget_out_max` | the most `out` stood at, bytes |
| `budget_rate_bps` | the rate at the window's last frame, bit/s; 0 when none was known |
| `budget_least_ms` | the least acknowledgement delay then, ms; `None` before the first SACK, when S stands in for it |
| `budget_slack_ms` | S, ms |
| `budget_ack_over_ms` | `Some((least, median, most))`, ms: the acknowledgement delays over the least, at each frame the clock had moved since the frame before; the median to 5 ms under 1 s and to 100 ms up to 10 s. `None` when there was none |
| `budget_still` | `(count, longest ms)`: spans in which the clock stood still with bytes out for `least + S` or longer |
| `bound_watermark` | the VP9-444 pump's sheds over its watermark; under `on`, on a relay session, the frames over it, which it no longer sheds |
| `bound_ledger` | the FFmpeg pump's gate passes its ledger held; under `on`, on a relay session, the passes its byte gate was shut at, which it no longer holds |
| `bound_full` | gate passes a full send channel held, and `try_send`s that found it full |
| `budget_released` | `(head-of-line, drain)`: the still spans whose release was read in the window, by class (D46's classifier) |
| `budget_still_admitted` | the frames admitted while a still span stood, whatever let them go: under the budget, on SCTP's window, or written through |
| `budget_acted` | `(held, asked)`, D46 B3: under `on`, on a relay session, the passes the budget's hold shut by itself, of the passes it acted on; `(0, 0)` in shadow and on a direct session |
| `budget_acted_stalled` | of those holds, the ones in a still span of `least + S`, which called no AIMD decrease (B3's no-decrease rule); 0 where the budget does not act |

A line with made-up values:

```text
path delivery (FR-148 P0, shadow, ≈10s) session_id=… span_ms=10004 delivery_samples=(3, 2, 0) backlogged_bps=Some((1712000, 1840000, 1951000)) app_limited_bps=Some(1206000) stalled_ms=0 acked_bytes=2301441 ack_ms=Some((62, 74, 391)) standing_ms=Some((12, 329)) queued=1 inflight=(18240, 58650) cwnd=(61440, 98304) ssthresh=65536 rwnd_low=1015808 pending_high=24576 loss=(0, 0) budget_held=(212, 300) budget_max=61440 budget_out_max=79680 budget_rate_bps=1987000 budget_least_ms=Some(62) budget_slack_ms=150 budget_ack_over_ms=Some((3, 45, 1257)) budget_still=(1, 812) bound_watermark=0 bound_ledger=37 bound_full=4 budget_released=(1, 0) budget_still_admitted=9 budget_acted=(0, 0) budget_acted_stalled=0
```

With them a busy line is 771 bytes in the daemon's log-file format, against 435 without:
at most 277,560 bytes a session hour per pump (723 and 260,280 before B3's two fields), measured by
`the_inflight_budget_s_fields_close_the_line`, which also pins their order and the line
with the switch `off`.

### Fresh delivery on the shaped link

The `tcp_shape` cells (`media_delivery::shaped_link::tcp_shape`) run the D46 prototype's
cells on the product core: today's writer, then the budget's, on the same shape. On the
stall and outage shapes B1's budget runs between them, its still spans never written
through, and on the stall shapes the classifier asking two head-of-line releases in a
row runs last; since B2 B1's budget also runs after the budget on the deep shapes and the
controls. A writer under the budget holds a frame at the core's verdict
(`media_inflight::Budget::step`) and writes one through where the verdict says so; the
pumps do so only under `on`, on a relay session
([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)). Each run prints a `D46 |` line and a `D46 B2 |` line (the shadow model's
floor, [below](#the-push-backs-in-the-shadow-model-d46-b2)), a `D46 B2 sample |` line for
each sample that pushes back under no rule (its span, rate and what the budget saw over
it, and what the rule makes of it: the rate taken, or `stood`, `marked` or `ended`), and
a holding budget's run a
`D46 release |` line for each still span it saw released: what SCTP had in flight and
pending at its last still reading, the rate and the budget, the bytes back at each step
of the next 400 ms, and the class it read. **Fresh delivery** is what the viewer read
within 300 ms of the fastest a frame can go (the one-way delay and the frame's packets
on the link, `fastest_ms`), as a share of the link's payload rate; read is all it read.
The link queue is the least each second's queue stood at, and the transit each second's
least, write to read, over the fastest, both as median / most over the 20 seconds.
Three runs, Windows, debug build; each cell is the least and the most of the three. The
budget runs at S = 150 ms unless a row says otherwise, with the classifier. B1's rows
measured beside it run B1's verdicts with the classifier's rate (a second that starts
inside a released still span counts from the span's start); the rows marked "B1's own
runs" are B1's three runs before the classifier.

**The stall shapes, D46 item 11's bar.** 800 ms stalls every 3.3 s on the 2 Mbit/s
deep-queue link. At 1.5 times the link today's writer is the VP9-444 pump's skip over
256 KiB: a writer without one queues without end there. The 1.5 times path C shape is
new with the classifier; today's skip reads nothing fresh on it, so it has no ratio.
"Written through" counts the frames the write-through let go, and the releases are the
still spans' by class, per run.

| Stall shape | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held or skipped | Written through | Released head-of-line / drain | Fresh over today's, each run |
|---|---|---|---|---|---|---|---|---|---|
| offer 80 %, path I | today's | 62–69 | 80 | 0 / 0–176 | 1–3 / 49–454 | 0 | | | |
| | B1's own runs | 61 | 66–67 | 0 / 0 | 1–3 / 1–5 | 100–103 | – | – | 0.876, 0.880, 0.952 |
| | B1's budget | 61 | 66 | 0 / 0 | 0–3 / 1–4 | 104–105 | 0 | 5 / 0–1 | 0.987, 0.987, 0.873 |
| | **the budget** | 66–67 | 72–74 | 0 / 0 | 0–3 / 1–3 | 49–59 | 45–59 | 5 / 0–1 | **1.072, 1.059, 0.957** |
| | the budget, two releases | 65–66 | 70–73 | 0 / 0 | 0–3 / 1–4 | 50–73 | 31–57 | 5 / 0 | 1.055, 1.039, 0.944 |
| offer 80 %, path C | today's | 19–23 | 77–79 | 0 / 228–254 | 267–374 / 608–637 | 0 | | | |
| | B1's own runs | 57–59 | 63–65 | 0 / 0 | 0–3 / 106–144 | 113–129 | – | – | 3.32, 3.54, 2.58 |
| | B1's budget | 58–59 | 63–64 | 0 / 0 | 0–1 / 105–157 | 122–125 | 0 | 5 / 0–1 | 2.557, 2.515, 2.920 |
| | **the budget** | 59–60 | 65–66 | 0 / 0 | 0–1 / 106–108 | 109–111 | 12–13 | 5 / 0 | **2.594, 2.537, 3.020** |
| | the budget, two releases | 59 | 64–65 | 0 / 0 | 0–3 / 108–126 | 110–119 | 3–15 | 5 / 0 | 2.559, 2.531, 2.961 |
| offer 150 %, path I | today's skip | 8–10 | 96–97 | 470–560 / 948–976 | 448–467 / 971–986 | 199–201 | | | |
| | B1's own runs | 73–74 | 80–81 | 0 / 132–147 | 117–120 / 149–175 | 277–279 | – | – | 9.04, 7.79, 7.25 |
| | B1's budget | 73 | 80–81 | 0–6 / 112–127 | 103–118 / 140–146 | 277–282 | 0 | 5 / 0 | 7.603, 7.021, 8.227 |
| | **the budget** | 79–80 | 88–92 | 50–96 / 114–130 | 114–116 / 134–147 | 232–250 | 37–60 | 5 / 0–1 | **8.209, 7.559, 9.009** |
| | the budget, two releases | 78–79 | 87–88 | 55–87 / 113–123 | 115–117 / 124–165 | 246–256 | 25–45 | 5 / 0 | 8.182, 7.556, 8.796 |
| offer 150 %, path C | today's skip | 0 | 77–79 | 0–16 / 274–305 | 617–623 / 1259–1282 | 0 | | | |
| | B1's budget | 68–71 | 75–78 | 0 / 107–125 | 103–118 / 155–213 | 291–300 | 0 | 5 / 0 | – |
| | **the budget** | 68–70 | 76–78 | 0 / 104–124 | 102–118 / 154–165 | 285–295 | 5–7 | 5 / 0 | – |
| | the budget, two releases | 68–70 | 76–78 | 0 / 105–125 | 115–118 / 157–222 | 285–293 | 5–9 | 5 / 0 | – |

**The bar is met in each run.** On the 80 % path I shape the budget's fresh delivery is
1.072, 1.059 and 0.957 of today's writer's, where B1's own runs read 0.876, 0.880 and
0.952; on path C and at 1.5 times the link it is 2.5 to 9 times today's. That shape is
the hard one: today's writer has no standing queue there, so only what it writes in a
stall arrives late, and what it writes in a stall's last 300 ms still counts as fresh.
The ratio moves with today's writer as much as with the budget: today's delivered 62 %
fresh in the first two runs and 69 % in the third (64–69 % in B1's own runs), and B1's
budget, run beside it, read 0.987, 0.987 and 0.873. Against B1's budget in the same run
the classifier adds 5 to 6 points of fresh delivery (66–67 % against 61 %) and 6 to 8 of
read on the 80 % path I shape, and 6 to 7 points of fresh at 1.5 times the link on path
I. On the path C shapes it moves fresh delivery by −1 to +2 points: SCTP's window binds
through a stall, and the pending gate lets 5 to 13 frames a run through. Its link queue
and transit stay at B1's on every shape but the 1.5 times path I one, where the queue's
median rises to 50–96 ms (B1's budget: 0–6) and the transit stays under 150 ms at most.
What still separates it from today's writer on the 80 % path I shape is what it holds,
49 to 59 frames a run: the first stall on the association holds by design (17 to 20
frames), and each later stall holds until its span is `least + S` old and while SCTP
queues anything (2 to 14 frames a stall).

**The outage shapes.** The stall shape's link, but in each of its 800 ms stalls the
link neither serializes nor delivers, in both directions, and after it drains what
queued at its rate (`media_delivery::shaped_link::Outages`): a still span through which
nothing moved, where writing through would only queue. A packet whose turn on the link
falls in an outage starts at its end, and one due to arrive in an outage arrives that
much later. Offer 80 % and 150 %, paths I and C; at 1.5 times the link today's writer is
the skip writer. After each outage's end with a second of the run after it, the cell
reads when the link's queue and each arrival's transit over the fastest last stood at
300 ms or more before the next outage began (the queue until 300 ms before it, since a
packet sent in that stretch waits for that outage too), and prints a `D46 outage |` line
for it; the cell's bar is under 1 s for both, and fresh delivery at least today's
writer's.

| Outage shape | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held or skipped | Released head-of-line / drain | Back under 300 ms after an outage, the most, each run: queue / transit, ms | Fresh over today's, each run |
|---|---|---|---|---|---|---|---|---|---|
| offer 80 %, path I | today's | 13 | 74–75 | 751–759 / 1756–1761 | 775–789 / 1629–1637 | 0 | | 2198, 2199, 2198 / 2499, 2495, 2485 | |
| | B1's budget | 61–62 | 66–67 | 0 / 51–57 | 0–3 / 85–794 | 102–105 | 0 / 5 | 0, 0, 0 / 185, 190, 224 | 4.569, 4.537, 4.635 |
| | **the budget** | 61–62 | 66–67 | 0 / 52–58 | 0–3 / 80–89 | 102–107 | 0 / 5 | 0, 0, 0 / 185, 190, 226 | 4.571, 4.536, 4.645 |
| offer 80 %, path C | today's | 2–4 | 72–74 | 171–199 / 277–305 | 463–488 / 1011–1068 | 0 | | 2188, 2195, 2195 / 2499, 2499, 2490 | |
| | B1's budget | 58–60 | 63–64 | 0 / 58–59 | 1–3 / 107–128 | 119–128 | 0 / 5 | 0, 0, 0 / 191, 191, 226 | 13.751, 14.195, 25.494 |
| | **the budget** | 58–60 | 63–64 | 0 / 12–53 | 1–3 / 105–787 | 118–128 | 0 / 5 | 0, 0, 0 / 190, 185, 227 | 13.783, 14.228, 25.544 |
| offer 150 %, path I | today's skip | 2 | 77 | 976–979 / 1595–1598 | 972–978 / 1775–1776 | 277 | | 2197, 2199, 2199 / 2497, 2497, 2485 | |
| | B1's budget | 70–71 | 76–77 | 107–123 / 133–135 | 117–120 / 130–146 | 294–296 | 0 / 5 | 0, 0, 0 / 235, 235, 254 | 27.967, 27.965, 27.527 |
| | **the budget** | 71 | 77 | 104–122 / 126–135 | 118–120 / 131–136 | 294–295 | 0 / 5 | 0, 0, 0 / 230, 236, 241 | 27.974, 27.965, 27.843 |
| offer 150 %, path C | today's skip | 0 | 72–74 | 163–198 / 274–305 | 731–748 / 1589–1608 | 0 | | 2190, 2199, 0 / 2492, 2497, 2497 | |
| | B1's budget | 66–68 | 72–74 | 116–131 / 131–140 | 120–130 / 155–181 | 306–312 | 0 / 5 | 0, 0, 0 / 230, 236, 243 | – |
| | **the budget** | 68 | 74 | 117–122 / 132–139 | 119–122 / 156–165 | 306 | 0 / 5 | 0, 0, 0 / 235, 232, 231 | – |

Every release in the outage cells read as a drain, 120 of 120 over the three runs, B1's
budget's included, so the classifier held through each outage as B1's budget does: the
two read alike to within two points. After every outage the budget's link queue was
under 300 ms at once and its transit within 241 ms (B1's budget: 254). Today's writers'
transit stood at 300 ms or more for 2,161 to 2,499 ms after each outage, nearly to the
next; their link queue did too on path I (2,061 to 2,199 ms), and on path C, where
SCTP's window bounds it, it was under 300 ms at once after some. The budget delivered
4.5 to 25.5 times today's writer's fresh at 80 %, and 28 times today's skip at 1.5 times
the link on path I; on path C there today's skip reads nothing fresh. A run's most
transit, up to 787 ms for the budget and 794 ms for B1's, falls in a second an outage
cut through; the recoveries are the cell's measure.

**The four deep shapes.** Offer 150 %, a deep queue.

| Shape | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held or skipped |
|---|---|---|---|---|---|---|
| 2 Mbit/s, path I | today's | 1–2 | 98–99 | 4141–4172 / 4175–4177 | 3552–3575 / 4742–4744 | 0 |
| | today's skip | 2 | 99 | 974–975 / 981–983 | 972–973 / 977 | 193 |
| | **the budget** | 99 | 99 | 119–121 / 125–128 | 118–119 / 123–124 | 210 |
| | B1's own runs | 98–99 | 98–99 | 101–124 / 112–131 | 105–132 / 119–136 | 210–211 |
| 2 Mbit/s, path C | today's | 0 | 95 | 205–229 / 325–355 | 745–759 / 870–882 | 0 |
| | today's skip | 0 | 94–95 | 202–232 / 328–353 | 738–759 / 870–883 | 0 |
| | **the budget** | 94–95 | 94–95 | 109–121 / 114–127 | 117–119 / 157–209 | 221–227 |
| | B1's own runs | 94–95 | 94–95 | 97–120 / 111–127 | 115–118 / 158–197 | 221–227 |
| 0.4 Mbit/s, path I | today's | 3 | 98 | 5539–5578 / 10492–10534 | 3560–3579 / 6751–6778 | 0 |
| | today's skip | 3 | 98 | 5280–5281 / 5290–5294 | 3560–3576 / 5286–5288 | 111–112 |
| | **the budget** | 98 | 98 | 116–117 / 121–125 | 115 / 118–141 | 211 |
| | B1's own runs | 97–98 | 98 | 85–134 / 97–142 | 88–136 / 102–148 | 211–212 |
| 0.4 Mbit/s, path C | today's | 0 | 96–97 | 556–585 / 821–842 | 3167–3194 / 3474–3479 | 0 |
| | today's skip | 0 | 96–97 | 557–584 / 814–843 | 3178–3193 / 3474–3479 | 0 |
| | **the budget** | 96–97 | 96–97 | 116–129 / 123–142 | 114–133 / 117–148 | 216–218 |
| | B1's own runs | 96–97 | 96–97 | 78–116 / 99–124 | 87–115 / 103–118 | 216–219 |

No deep run saw a still span, so the classifier read no release there and the verdicts
are B1's. The budget reads what today's writers read, its fresh and read delivery within
B1's own runs, and its standing queue and transit stay under 300 ms in every run (the
cells' bar), where today's link queues stand at 0.2 to 5.6 s. Its link queue on the
0.4 Mbit/s path C shape (116–129 / 123–142 ms) and its transit there and on the 2 Mbit/s
path C shape sit a little over B1's own runs, on B1's verdicts.

**The four controls.** Offer 150 %, a shallow queue: the link drops instead of queueing,
so the queue today's writer builds is SCTP's own.

| Shape | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held or skipped |
|---|---|---|---|---|---|---|
| 2 Mbit/s, path I | today's | 1 | 85–90 | 0 / 42–55 | 554–559 / 606–1724 | 0 |
| | **the budget** | 76–81 | 83–87 | 0 / 40–61 | 104–116 / 1613–2973 | 256–271 |
| | B1's own runs | 75–85 | 85–90 | 0 / 46–66 | 109–119 / 134–1032 | 245–261 |
| 2 Mbit/s, path C | today's | 0 | 92 | 0–13 / 39–67 | 557–577 / 595–634 | 0 |
| | **the budget** | 90–92 | 92–93 | 0–14 / 39–63 | 119–120 / 152–164 | 232–236 |
| | B1's own runs | 86–91 | 89–93 | 0–10 / 42–62 | 118–119 / 160–213 | 233–247 |
| 0.4 Mbit/s, path I | today's | 1 | 79–84 | 0 / 0 | 2988–3078 / 3304–3463 | 0 |
| | **the budget** | 33–47 | 75–83 | 0 / 0 | 84–116 / 184–700 | 258–299 |
| | B1's own runs | 45–54 | 78–85 | 0 / 0 | 102–114 / 304–746 | 259–288 |
| 0.4 Mbit/s, path C | today's | 0 | 78–82 | 0 / 0 | 3015–3168 / 3700–4009 | 0 |
| | **the budget** | 42–44 | 81–84 | 0 / 0 | 114–122 / 268–1124 | 268–272 |
| | B1's own runs | 47–59 | 81–85 | 0 / 0 | 102–115 / 212–358 | 258–276 |

⚠️ **Outside B1's own runs, on B1's verdicts.** The controls' cell asks the budget to
read at least 0.95 of what today's writer reads. B1's own runs missed it once (0.945 on
the 0.4 Mbit/s path I control); the third run here missed it twice: 0.892 on the
0.4 Mbit/s path I control (296 against 332 kbit/s, 299 frames held against B1's 259–288)
and 0.927 on the 2 Mbit/s path I control (1,631 against 1,761 kbit/s). The classifier
changes nothing until a still span is released, and neither run released one: both are
B1's verdicts. Over the twelve control runs it read one release, on the 0.4 Mbit/s path
I control in the second run, and wrote nothing through. The budget's steps ran slower in
the third run than in the two before (34 µs on average, against 31 and 29). The fresh
delivery on the 0.4 Mbit/s controls (33–47 % on path I and 42–44 % on path C, against
B1's 45–54 and 47–59) and the most transit on the 2 Mbit/s path I control (to 2,973 ms,
against B1's 1,032 and today's writer's 1,724) fall outside B1's own runs as well, with
nothing written through.

**The slack.** 2 Mbit/s, path I, offer 150 %; the stall shape is the one above at 1.5
times the link.

| Shape | S, ms | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held | Written through |
|---|---|---|---|---|---|---|---|---|
| deep | 75 | **the budget** | 99 | 99 | 22–46 / 46–54 | 25–45 / 52–56 | 211–212 | 0 |
| | | B1's own runs | 99 | 99 | 21–47 / 34–56 | 26–45 / 52–57 | 211 | |
| deep | 150 | **the budget** | 98–99 | 98–99 | 107–120 / 116–128 | 117–119 / 119–125 | 210–211 | 0 |
| | | B1's own runs | 98–99 | 98–99 | 107–123 / 120–129 | 116–119 / 121–124 | 210–211 | |
| deep | 300 | **the budget** | 54–78 | 98–99 | 262–276 / 276–291 | 270–272 / 348–356 | 207–208 | 0 |
| | | B1's own runs | 48–74 | 98 | 253–264 / 259–284 | 257–271 / 316–352 | 207 | |
| stalls | 75 | **the budget** | 75–77 | 84–87 | 0–19 / 35–50 | 28–45 / 57–74 | 249–265 | 24–48 |
| | | B1's own runs | 73–74 | 78–79 | 0 / 34–54 | 26–47 / 56–75 | 289–290 | |
| stalls | 150 | **the budget** | 78–80 | 88–90 | 45–89 / 108–121 | 105–118 / 134–149 | 240–248 | 34–52 |
| | | B1's own runs | 73–75 | 80–81 | 0 / 140–155 | 118–120 / 149–174 | 277–279 | |
| stalls | 300 | **the budget** | 45–71 | 89–93 | 94–172 / 278–285 | 258–272 / 350–366 | 228–242 | 26–46 |
| | | B1's own runs | 28–30 | 86–87 | 138–159 / 407–434 | 269–271 / 425–429 | 247–251 | |
| shallow | 75 | **the budget** | 74–90 | 88–94 | 0–16 / 34–48 | 40–42 / 1118–1502 | 227–245 | 0 |
| | | B1's own runs | 85–90 | 90–94 | 0–19 / 28–44 | 39–42 / 851–1510 | 227–243 | |
| shallow | 150 | **the budget** | 75–84 | 84–90 | 0–2 / 47–65 | 118 / 133–2359 | 244–266 | 0 |
| | | B1's own runs | 72–80 | 83–89 | 0 / 45–54 | 103–118 / 181–1625 | 247–268 | |
| shallow | 300 | **the budget** | 37–61 | 86–88 | 0–4 / 44–47 | 265–271 / 299–1727 | 248–254 | 0 |
| | | B1's own runs | 33–65 | 86–89 | 0 / 46–65 | 267–272 / 624–2434 | 245–256 | |

The standing queue follows S in every run. On the stall shape the classifier writes
through at every S: fresh 75–80 % against B1's 73–75 % at S = 75 and 150 ms. At S =
300 ms the transit's median sits near the 300 ms that counts as fresh (258–272 ms), and
fresh delivery swings with it: 45–71 % against B1's 28–30 %, with the link queue at
278–285 ms at most against B1's 407–434. The deep shapes saw no still span, and the
shallow ones one, in one run at S = 150 ms, released as a drain: neither wrote anything
through. A step of the core took 26 to 53 µs on average per run, in the unoptimised test
build (B1's runs: 24 to 39).

### The push-backs in the shadow model (D46 B2)

FR-148 D46's step B2, in shadow. What the budget sees now reaches P0's shadow model, and
nothing else: at each delivery sample's close the sampler asks the budget what it saw
over the sample (`Budget::over`), and after each frame's step the pump hands the governor
the budget's mark (`Shadow::take_mark`), which also drops each push-back still inside its
settle time (`media_delivery::settle`). The push-backs' law and their place in the
memory are in [§ The delivery samples' push-backs](#the-delivery-samples-push-backs-d46-b2);
this section has the burst, the opening and the numbers.

```text
over a sample's span, from its start to its close (Budget::over):
  stood          a still span of least + S met it, from the span's first still reading
                 to the step that read its release
  still_inside   that span stood still for least + S inside the sample itself
  held           the verdicts held a frame in each whole second of it
  opening        it starts before the budget's first measured rate on its association
P1   queued                                                  a capacity at its own rate
P3   standing_ms known, SCTP's Backlogged or held, not still_inside, not opening
                                                             a capacity at its own rate
the burst rule: stood -> no capacity, P1 or P3
                a push-back waits settle = least + S + 200 ms (one held SACK) past
                its close, and a mark inside the wait drops it: it closed in a stall;
                one still waiting at the session's close is never handed
the opening:    a step's rate is measured when the 1 s figures gave it, or when the
                application-limited rule kept a rate they gave (Budget::rate_at);
                before, the opening rate stands in. The association's first measured
                rate ends its opening, and a new association opens again
the mark: each step at which a still span of least + S stands, and the step that reads
          its release; the next viewer window that delivers anything is no
          reading for the model or the memory
```

**Where the burst comes in.** A stall on the `tcp_shape` link holds what it serialized
for 800 ms and then delivers all of it at once: the association's clock stands still
through it, and then acknowledges everything within 50 ms. Three readings meet it:

- **a viewer window.** Today's writers queue through a stall, the transit grows, and
  FR-79's gate rejects the stall's window and the one after it (D24). Under the budget
  the transit stays short through the same stall, so the gate passes the window that
  carries the release, and its arrival rate raises the model's floor: with no window
  kept out the budget's floor ended at 1.04 to 1.67 times the link on the stall
  cells, from those windows alone.
- **a sample that holds the release** reads the burst at its acknowledged rate, and is
  backlogged when the budget held through the stall: the release rule's case. The
  41 samples the rule dropped so over the three runs read 0.87 to 1.08 times the link.
- **a sample that closes in a stall** reads less than the path carries. The SACK of a
  lone packet delivered as the stall begins comes back one held SACK later (200 ms),
  inside the stall, and ends its first still span short of `least + S`; the span after
  it stands later. A sample that closed before either stood was held (holds start once
  `out` reaches the budget, about 155 ms into the stall at 80 % of a 2 Mbit/s link), had
  no still span of `least + S` in it, and was P3: the 45 the settle wait dropped
  over the three runs read 0.73 to 0.90 times the link. Under the release rule
  alone (`s1`) B1's budget and the budget asking two releases ended at 0.76 to 0.77
  times the link on the 80 % stall cells, three runs each; the budget stayed at 0.82
  to 0.84 there.

**The rules tried.** Every run folds its samples, marks and windows through the shadow
model under each rule (`model_fold`, in the governor's order, with the opening rule
built: below), so they differ only in what they let through. For samples: `o`, each at
its own rate (P3's own `still_inside` only); `c`, the classifier's rate correction, a
sample that starts inside a released span counting from the span's first still reading
(D46's precedent); `s`, the release rule alone, read at the close; `e`, the rule built:
`s`, and the push-back waits its
settle time, a mark inside the wait dropping it. A run's last sample closes as its
writer stops, so under `e` its push-back is still waiting then and is never handed, as a
session's close drops what its sampler holds; each `D46 B2 sample |` line says which
(`stood`, `marked`, `ended`, or the rate taken). For windows: 0 to 3 windows that
deliver anything after each mark are no reading. On the twelve loaded cells, three runs
each, the model's floor at the run's end as a share of the link's payload rate, and how
many of the cell runs ended outside 0.80 to 1.05:

| Rule | the budget | B1's budget | the budget asking two releases |
|---|---|---|---|
| `e0` | 0.95–1.67, 11 of 36 out | 0.91–1.16, 6 of 36 out | 0.96–1.67, 10 of 12 out |
| `e1` | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 12 out |
| `e2` | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 12 out |
| `e3` | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 36 out | 0.82–1.00, 0 of 12 out |
| `s0` | 0.95–1.47, 10 of 36 out | 0.81–1.16, 4 of 36 out | 0.87–1.66, 7 of 12 out |
| `s1` | 0.82–1.00, 0 of 36 out | 0.76–1.00, 3 of 36 out | 0.76–1.00, 3 of 12 out |
| `s2` | 0.78–1.00, 8 of 36 out | 0.74–1.00, 6 of 36 out | 0.76–0.95, 6 of 12 out |
| `s3` | 0.78–1.00, 8 of 36 out | 0.74–1.00, 6 of 36 out | 0.76–0.95, 6 of 12 out |
| `c0` | 0.95–1.47, 10 of 36 out | 0.81–1.16, 4 of 36 out | 0.87–1.66, 7 of 12 out |
| `c1` | 0.82–1.00, 0 of 36 out | 0.76–1.00, 3 of 36 out | 0.76–1.00, 3 of 12 out |
| `c2` | 0.72–1.00, 8 of 36 out | 0.71–1.00, 6 of 36 out | 0.71–0.90, 6 of 12 out |
| `c3` | 0.72–1.00, 8 of 36 out | 0.71–1.00, 6 of 36 out | 0.71–0.90, 6 of 12 out |
| `o0` | 0.95–1.47, 10 of 36 out | 0.81–1.16, 4 of 36 out | 0.87–1.66, 7 of 12 out |
| `o1` | 0.82–1.00, 0 of 36 out | 0.76–1.00, 3 of 36 out | 0.76–1.00, 3 of 12 out |
| `o2` | 0.78–1.00, 8 of 36 out | 0.74–1.00, 6 of 36 out | 0.76–0.95, 6 of 12 out |
| `o3` | 0.78–1.00, 8 of 36 out | 0.74–1.00, 6 of 36 out | 0.76–0.95, 6 of 12 out |

The rule built is `e1`. The first build kept two windows out (`e2`), and its first
measured run put the budget's floor at 0.69 times the link on the 80 % stall cell on
path C: on a stall every 3.3 s, two windows after each mark kept 15 windows of the
20-second run out of the model, and the floor never came back from the session's first
sample (0.558 times the link, P3). One window was the only rule inside the bar on every
loaded cell of that run, for all three budget writers, so the build moved to it before
#4115's first three runs, in which `e2` missed three times, each at 0.68 on path C at
80 %: the budget on the same stall cell in the second and on the outage cell in the
third, and the budget asking two releases on the stall cell in the third. With the
opening rule (below) folded in, `e2` and `e3` stayed inside the bar in the three runs
here too: the session's first sample, which held the first build's floor down, no
longer reaches the model. One window stays the rule: the least that keeps the release
out.

**The opening.** #4115's first runs took each session's first sample (about 130 ms
to 2.13 s in) as P3 under every budget writer: the budget held in each of its first two
seconds while the association opened, and the 77 first samples read 0.52 to 0.92 times
the link. On the 80 % cells no later P3 replaced the capacity it set, and the model's
capacity ended at 0.52 to 0.78 times the link, under its own floor. AC2's bracket has
two edges (D45): the floor at 0.80 to 1.05 times the link, and the capacity absent or
at least 0.95. Those holds were the budget's own. Until its first 1 s figure the opening
rate stands in for its rate (D46 item 12: 80 % or 150 % of the link on these cells),
and SCTP's window is still opening: in the three runs here the 76 first samples that
were P3 without a rule held at 1 to 33 steps each, 1 to 22 of them at the stand-in rate
and up to 22 sized by SCTP's window rather than by the rate. P3 reads a held sample's
rate as what the path carried; in the opening it is what the stand-in and a young window
let out.

So a sample that starts before the budget's first measured rate on its association is
no P3 capacity (`Over::opening`, read by `push_back_of`). A step's rate is measured when
the 1 s figures gave it, or when the application-limited rule kept a rate they gave
(`Budget::rate_at`); a new association opens again (`Budget::on_new_association`). The
rule reads the budget and changes none of its verdicts. Nor does P1 change: a `queued`
sample in the opening is still P1, and the budget-off model still moves by P1 only.

The rules tried, each folded through the shadow model over the same runs (`Open` in the
harness, which reads the holding core's trace). What makes a sample the opening's, so no
P3: `n`, nothing (#4115's first commit); `a`, a hold in it at the opening's stand-in
rate; `b`, `a`, or a hold that SCTP's window sized rather than the rate (the
`min(rate, window / least)` of `Budget::size`); `c`, the rule built: it starts before
the first measured rate; `cw`, `c`, or `b`'s window; `f`, it is the session's first
sample. Both edges at the run's end on the twelve loaded cells, three runs each, times
the link's payload rate, under the burst rule built: the floor; the capacity, and the
runs that ended with none; and the cell runs off the bar.

| Rule | the budget | B1's budget | the budget asking two releases |
|---|---|---|---|
| `n` | 0.82–1.00; 0.54–1.00, none in 4; 8 of 36 off | 0.82–1.00; 0.54–1.00, none in 3; 9 of 36 off | 0.82–1.00; 0.54–1.00, none in 1; 5 of 12 off |
| `a` | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 1.00–1.00, none in 6; 0 of 12 off |
| `b` | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 1.00–1.00, none in 6; 0 of 12 off |
| `c` | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 1.00–1.00, none in 6; 0 of 12 off |
| `cw` | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 1.00–1.00, none in 6; 0 of 12 off |
| `f` | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 0.98–1.00, none in 12; 0 of 36 off | 0.82–1.00; 1.00–1.00, none in 6; 0 of 12 off |

Under `n` every run off the bar is a capacity under 0.95 on an 80 % cell (0.54 to 0.78),
set by the session's first sample. The first samples, under `n`: the budget's was P3 in
32 of its 36 loaded cell runs, B1's in 33 of 36 and the budget asking two releases' in
11 of 12, at 0.54 to 0.92 times the link; each of those 76 is the opening's under `a`,
`b`, `c`, `cw` and `f` alike, and the other 8 were no push-back under any rule. No later
sample on a loaded cell differs from rule to rule, so there the rules differ only in
the first sample. The capacity a rule leaves absent (12, 12 and 6 runs) is the 80 %
outage and stall cells': no sample after the opening was P3 there.

**After an outage** SCTP's window does not collapse on these cells: no run had a fast
retransmit or a T3 timeout, since the deep queue drops nothing, an outage holds what is
due in it (lossless), and 800 ms is under the 1 s floor of SCTP's retransmission timer
(`RTO_MIN`, `crates/vendored/webrtc-sctp/src/timer/rtx_timer.rs:10`); nor had any
sample after a session's first a hold that SCTP's window sized. The budget's rate stays
measured through an outage, so every rule but `n` leaves what follows one to the burst
rule, and they differ only on the first sample. SCTP's window does collapse and regrow
on the shallow queues, which drop (fast retransmits in each of the 45 shallow runs, T3
timeouts in 37, the window down to 1,228 bytes, one packet): on the shallow controls
`a`, `c` and `f` take the 187 P3 samples after the opening at their own rates, 0.70 to
1.00 times the link, and `b` and `cw` keep out 152 of them, held while the window
regrew (and 42 of the slack cell's 153). On a path that drops they would keep four of
five of its samples from the model. `c` is built: on every run it keeps out what `a` and
`f` keep out, it is one time in the budget (`first_measured`), and, unlike `f`, a new
association opens again.

**The bracket, every loaded cell**, under the rules built (`e1` and `c`), against the
classifier's runs (#4083), whose floor was `Pipe`'s own law (the model's floor did not
exist on these cells then): the floor and the model's capacity at the run's end, times
the link's payload rate (`-`: no capacity), today's writers beside the budget's.
At an offer of 80 % the floor ends at the offer, about 0.82 times the link: the
windows that read more carried a release, and are kept out. Every run of the three
budget writers is on the bar: the floor at 0.82 to 1.00, the capacity at 0.985 to 1.001,
or none on the 80 % outage and stall cells.

| Cell | Writer | Floor, runs 1 / 2 / 3 | Capacity, runs 1 / 2 / 3 | #4083's floor |
|---|---|---|---|---|
| 0.4 Mbit/s, deep queue, path C, offer 150 % | today's | 0.946 / 0.946 / 0.945 | - / - / - | 0.99–1.00 |
| 0.4 Mbit/s, deep queue, path C, offer 150 % | today's skip | 0.947 / 0.946 / 0.947 | - / - / - | 0.99–1.00 |
| 0.4 Mbit/s, deep queue, path C, offer 150 % | the budget | 0.992 / 0.992 / 0.991 | 0.997 / 0.998 / 0.997 | 0.99–1.00 |
| 0.4 Mbit/s, deep queue, path C, offer 150 % | B1's budget | 0.990 / 0.991 / 0.991 | 0.997 / 0.999 / 0.999 | (not run) |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | today's | 0.954 / 0.954 / 0.957 | - / - / - | 0.99–1.00 |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | today's skip | 0.954 / 0.955 / 0.954 | - / - / - | 0.99–1.00 |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | the budget | 0.990 / 0.991 / 0.990 | 0.997 / 0.985 / 0.986 | 0.99 |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | B1's budget | 0.991 / 0.992 / 0.990 | 0.986 / 0.985 / 0.986 | (not run) |
| 2 Mbit/s, deep queue, path C, offer 150 % | today's | 0.897 / 0.898 / 0.899 | 0.797 / 0.797 / 0.799 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 150 % | today's skip | 0.898 / 0.898 / 0.898 | 0.797 / 0.798 / 0.798 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 150 % | the budget | 0.999 / 0.999 / 1.000 | 1.001 / 1.001 / 1.001 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 150 % | B1's budget | 1.000 / 1.000 / 0.999 | 1.001 / 1.000 / 1.001 | (not run) |
| 2 Mbit/s, deep queue, path I, offer 150 % | today's | 0.975 / 0.974 / 0.972 | - / - / - | 1.00–1.01 |
| 2 Mbit/s, deep queue, path I, offer 150 % | today's skip | 0.972 / 0.972 / 0.975 | - / - / - | 1.00 |
| 2 Mbit/s, deep queue, path I, offer 150 % | the budget | 0.999 / 0.999 / 1.000 | 0.995 / 1.001 / 0.995 | 1.00 |
| 2 Mbit/s, deep queue, path I, offer 150 % | B1's budget | 1.000 / 0.999 / 1.000 | 1.001 / 1.001 / 1.000 | (not run) |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms outage every 3.3s | today's | 0.998 / 0.998 / 0.997 | - / - / - | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms outage every 3.3s | B1's budget | 0.820 / 0.819 / 0.820 | - / - / - | 0.95–0.96 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms outage every 3.3s | the budget | 0.821 / 0.820 / 0.820 | - / - / - | 0.95 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms outage every 3.3s | today's | 0.867 / 0.864 / 0.866 | - / - / - | 1.00–1.01 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms outage every 3.3s | B1's budget | 0.840 / 0.839 / 0.838 | - / - / - | 0.95–0.96 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms outage every 3.3s | the budget | 0.840 / 0.841 / 0.839 | - / - / - | 0.95–0.96 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms outage every 3.3s | today's skip | 0.671 / 0.556 / 0.579 | 0.671 / 0.556 / 0.579 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms outage every 3.3s | B1's budget | 0.999 / 1.000 / 1.000 | 1.000 / 1.001 / 1.000 | 1.00 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms outage every 3.3s | the budget | 1.001 / 0.999 / 1.000 | 1.001 / 0.996 / 1.001 | 1.00 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms outage every 3.3s | today's skip | 0.970 / 0.971 / 0.973 | - / - / - | 1.00–1.01 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms outage every 3.3s | B1's budget | 0.999 / 1.000 / 1.000 | 1.000 / 0.999 / 0.999 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms outage every 3.3s | the budget | 0.999 / 1.000 / 1.000 | 0.999 / 0.999 / 0.999 | 1.00–1.01 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | today's | 0.997 / 0.998 / 0.998 | - / - / - | 1.24–1.26 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | B1's budget | 0.822 / 0.819 / 0.820 | - / - / - | 0.91–0.95 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | the budget | 0.820 / 0.819 / 0.820 | - / - / - | 1.02–1.07 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | two releases | 0.820 / 0.821 / 0.819 | - / - / - | 0.96–1.07 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | today's | 1.447 / 1.446 / 1.450 | - / - / - | 1.46–1.55 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | B1's budget | 0.841 / 0.840 / 0.838 | - / - / - | 0.91–0.94 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | the budget | 0.839 / 0.839 / 0.840 | - / - / - | 1.37 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | two releases | 0.839 / 0.839 / 0.840 | - / - / - | 1.34–1.40 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | today's skip | 0.679 / 0.669 / 0.669 | 0.679 / 0.669 / 0.669 | 1.25–1.26 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | B1's budget | 1.001 / 1.000 / 0.999 | 1.001 / 1.001 / 0.997 | 1.13–1.16 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | the budget | 1.000 / 1.001 / 1.000 | 0.999 / 0.998 / 1.001 | 1.21–1.26 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | two releases | 0.999 / 1.000 / 0.999 | 0.999 / 1.000 / 1.001 | 1.22–1.26 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | today's skip | 0.973 / 0.970 / 0.971 | - / - / - | 1.78 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | B1's budget | 1.000 / 1.000 / 0.999 | 1.001 / 1.001 / 0.997 | 1.16–1.17 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | the budget | 0.999 / 1.000 / 1.000 | 0.996 / 1.000 / 1.000 | 1.65–1.72 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | two releases | 0.998 / 1.000 / 1.000 | 0.996 / 1.001 / 1.001 | 1.66–1.69 |

On the slack cell (the budget at 75, 150 and 300 ms of slack, on the 2 Mbit/s deep queue
with and without stalls and on the shallow one, path I, offer 150 %) the rules built
ended at 0.970 to 1.073 times the link in its 27 runs, over 1.05 once: on the shallow
queue at 75 ms in the first run (1.073), where no still span stood and a window FR-79's
gate passed read 1.82 times the link (`maxrx`), as on the shallow controls below. `n`
ends there alike, floor and capacity, in every run; `Pipe`'s own law read up to 2.08.

**Today's writers** (the budget off, P1 only): no push-back raised a floor on any cell,
asserted on every push-back of every run (`model_fold`). Today's writer's floor ended at
0.86 to 1.45 times the link, and today's skip's at 0.56 to 0.98. It ends over 1.05 only
on the 80 % stall cell on path I (1.446 to 1.450 in each run): a window FR-79's gate
passed carried part of a stall's release (the most a window the model took read,
`maxrx`, is the floor itself), and with the budget off nothing computes a still span, so
nothing marks the stall and no window after it is kept out. Today's skip's ends under
0.80 only at 1.5 times the link on path C, through the stalls and the outages (0.56 to
0.68): there windows' push-backs (`PathModel::pushed_back`, P0's law) lowered the floor,
and no sample reached the model.

Their capacity, the bracket's other edge, ended absent on every cell but path C at 1.5
times the link, and under 0.95 on each of those, in each run: 0.797 to 0.799 on the
deep queue (both writers), 0.56 to 0.68 through the outages and the stalls (today's
skip). P0's window push-backs set it: each valid viewer window hands the model the
goodput estimate as a capacity (`GoodputEstimator`,
`agents/roomlerd/src/encode/goodput.rs:211`). On path C SCTP's window is about 13 KB a
second in, growing in congestion avoidance (`ssthresh` 0), so the writer blocks from the
first second, and the estimator adopts that second's reading outright, 0.45 times the
link (0.53 once). It rises by `ALPHA_UP` of the gap a window (0.10, "down fast, up
slow", `goodput.rs:98`), and through the outages and the stalls each window an outage or
a stall held the writer in pulls it down by `ALPHA_DOWN` (0.50). The deep queue's
transit then takes the windows past FR-79's gate, no valid window takes a later
estimate, and the model keeps the last valid window's: 0.80 at 10.9 s on the deep queue,
0.56 to 0.68 at 7.4 s through the outages and the stalls. On path I the window is at
186 KB a second in, in slow start (`ssthresh` the peer's window): on the deep queue
today's writer first blocks at 9 s, when every window is past the gate, and today's skip
never blocks there, so no capacity is set. Both edges are the budget-off model as D24
left it, and B2 does not change it.

**The controls, same run.** Fresh and read delivery, kbit/s, today's writer, B1's
budget and the budget, in each run (`≫`: today's writer delivers almost nothing fresh):

| Control | Run | today's writer, fresh / read | B1's budget, fresh / read (× today's) | the budget, fresh / read (× today's) |
|---|---|---|---|---|
| 0.4 Mbit/s, path C | 1 | 1 / 316 | 139 / 307 (≫, 0.972) | 174 / 319 (≫, 1.010) |
| 0.4 Mbit/s, path I | 1 | 3 / 323 | 175 / 334 (≫, 1.034) | 180 / 324 (≫, 1.003) |
| 2 Mbit/s, path C | 1 | 0 / 1796 | 1800 / 1820 (≫, 1.014) | 1770 / 1800 (≫, 1.003) |
| 2 Mbit/s, path I | 1 | 29 / 1756 | 1565 / 1740 (≫, 0.991) | 1680 / 1765 (≫, 1.006) |
| 0.4 Mbit/s, path C | 2 | 1 / 316 | 156 / 310 (≫, 0.981) | 165 / 314 (≫, 0.994) |
| 0.4 Mbit/s, path I | 2 | 4 / 336 | 175 / 338 (≫, 1.006) | 169 / 325 (≫, 0.967) |
| 2 Mbit/s, path C | 2 | 0 / 1805 | 1766 / 1811 (≫, 1.003) | 1790 / 1815 (≫, 1.006) |
| 2 Mbit/s, path I | 2 | 29 / 1705 | 1465 / 1680 (≫, 0.985) | 1575 / 1745 (≫, 1.024) |
| 0.4 Mbit/s, path C | 3 | 1 / 317 | 169 / 316 (≫, 0.997) | 170 / 329 (≫, 1.038) |
| 0.4 Mbit/s, path I | 3 | 4 / 313 | 176 / 327 (≫, 1.045) | 160 / 329 (≫, 1.051) |
| 2 Mbit/s, path C | 3 | 0 / 1801 | 1675 / 1796 (≫, 0.997) | 1805 / 1825 (≫, 1.014) |
| 2 Mbit/s, path I | 3 | 29 / 1750 | 1660 / 1765 (≫, 1.008) | 1690 / 1771 (≫, 1.011) |

The cell asks the budget to read at least 0.95 of what today's writer reads. In the
three runs here every read met it: the budget's at 0.967 to 1.051 times today's, B1's
at 0.972 to 1.045. In #4115's first runs the budget missed it once (0.937, on the
2 Mbit/s path I control) and B1's budget once (0.921, on the 0.4 Mbit/s path I control),
and in the classifier's third run (#4083) B1's verdicts missed it twice (0.892 and
0.927). B2 changes nothing a writer sends, so these are the budget's verdicts and B1's,
from run to run.

**O7's split.** Per loaded cell and budget writer, the memory's counted push-backs
(`samples_counted`) and how many came from `Clear` windows, and of them the samples'
P3 (`samples_counted_p3`) and their `Clear` ones. `samples_counted_p1` stayed 0 on
every loaded cell:

| Cell | Writer | counted, runs 1 / 2 / 3 | of them `Clear` | the samples' P3 | of them `Clear` |
|---|---|---|---|---|---|
| 0.4 Mbit/s, deep queue, path C, offer 150 % | the budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 0.4 Mbit/s, deep queue, path C, offer 150 % | B1's budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | the budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 0.4 Mbit/s, deep queue, path I, offer 150 % | B1's budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 % | the budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 % | B1's budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 % | the budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 % | B1's budget | 8 / 8 / 8 | 0 / 0 / 0 | 8 / 8 / 8 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms outage every 3.3s | B1's budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms outage every 3.3s | the budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms outage every 3.3s | B1's budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms outage every 3.3s | the budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms outage every 3.3s | B1's budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms outage every 3.3s | the budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms outage every 3.3s | B1's budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms outage every 3.3s | the budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | B1's budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | the budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 80 %, a 800ms stall every 3.3s | two releases | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | B1's budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | the budget | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 80 %, a 800ms stall every 3.3s | two releases | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | B1's budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | the budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path C, offer 150 %, a 800ms stall every 3.3s | two releases | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | B1's budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | the budget | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |
| 2 Mbit/s, deep queue, path I, offer 150 %, a 800ms stall every 3.3s | two releases | 1 / 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | 0 / 0 / 0 |

Over the loaded cells the budget writers' memory counted 222 push-backs in the three
runs (108 the budget's, 108 B1's, 6 two releases'), every one a sample's P3 and none
from a `Clear` window; no P1 reached the memory on a loaded cell, and today's writers
counted only windows' push-backs (84, none from a `Clear` window). #4115's first runs,
before the opening rule, counted 297, 8 of them from `Clear` windows on the 80 % stall
and outage cells on path I. The rule keeps each session's first sample out (76 here,
one a run), so a run now counts 8 instead of 9 on the 150 % deep cells, 1 through the
150 % outages and stalls, and none on the 80 % cells, where those 8 `Clear` ones fell.
P1 did reach the memory on the slack cell, which is not a loaded cell: at 300 ms of
slack the queue stood, the budget's samples read `queued`, and 8 P1 a run were counted,
none from a `Clear` window (every one from an `Overproduced` window in #4115's first
runs); at 75 and 150 ms the samples were P3, 8 a run.

**What it leaves.** What the opening rule leaves, and three readings that stay as they
were, each measured here:

- The opening's samples reach nothing. On the loaded cells the session's first sample
  was held at the opening in 76 of the budget writers' 84 runs and read 0.54 to 0.92
  times the link; under the rule none of them reaches the model, and the capacity ended
  at 0.985 to 1.001 times the link, or absent on the 80 % outage and stall cells (12
  runs of the budget's, 12 of B1's, 6 of two releases'). Without it (`n`) the capacity
  ended under 0.95 in 22 of those runs, at 0.54 to 0.78, each time that first sample's.
  The floor ended at 0.82 to 1.00 times the link under every rule; the rule raised it in
  8 runs, by 0.001 to 0.022, 7 of them on path I's 80 % outage and stall cells, where
  the first sample had read 0.78. On these cells no later sample changes its fate under
  any candidate: they differ from `n` only on the first samples.
- With the budget off, P1 reaches the model only under a window the gate accepted. On
  the deep shapes, where the queue stands, the gate accepts none once the queue has
  built: today's writers made 4 to 9 P1 push-backs a run there, and none reached the
  model.
- On the shallow 0.4 Mbit/s controls the budget writers' floor ended at up to 1.78
  times the link. Six of the seven runs that ended over 1.05 had no mark (no still span
  stood), so no rule of B2's reaches them: a viewer window FR-79's gate passed read up
  to 1.785 times the link (`maxrx`), what `Pipe`'s own law reads there (#4083's 1.78),
  and the model's floor took it, lowered only partly by the samples after it. The
  seventh, B1's at 1.052, had three marks and kept one window out. B1's budget ended at
  0.94 to 1.78 there and the budget at 0.84 to 1.77, from run to run; neither is a
  loaded cell.
- On a path that drops, neither edge holds. On the shallow queues SCTP made fast
  retransmits in each of the 45 runs and T3 timeouts in 37, its window down to one
  packet. On the four shallow controls the budget writers' bracket was on the bar in 4
  of their 24 runs.
  The capacity ended under 0.95 in 17: in 16 a P3 sample set it, at 0.79 to 0.95 times
  the link, and in one a window's push-back (P0's law, 0.58). The floor ended over 1.05
  in 9. Keeping out as well a sample with a hold that SCTP's window sized rather than
  the rate (`cw`) leaves 5 capacities under 0.95 and puts 8 runs on the bar, but 15
  floors end over 1.05, at up to 1.78: the samples `cw` keeps out are what lowered the
  floor after windows FR-79's gate passed that read up to 1.79 times the link (`maxrx`).
  Both edges there belong to the step for a path that drops: P3 when SCTP's window and
  not the rate held the budget, and the windows that read over the link.

### The budget acts under `on` (D46 B3)

FR-148 D46's step B3. Under `ROOMLERD_INFLIGHT_BUDGET=on`, on a constrained (relay)
session, both data-channel pumps act on the budget. One call before the pump's capacity
gate decides (`DeliverySampler::gate`) takes B1's step there: it reads the clock, peeks
the gauges, steps the core and keeps the `Pass` its verdict answers. The capacity gate
asks that pass last, after the send channel and the followers; the gate's skip calls the
AIMD's decrease through it; and in the VP9-444 pump the watermark arm goes through it
too. On a direct session under `on`, and under `shadow` and `off`, the call reads
nothing, the pass is today's, and the step stays after the gate
(`DeliverySampler::on_frame`), as in shadow.

```text
under on, on a relay session, at each pass of a pump's capacity gate:
  the budget's gate   before the gate decides: a read of the clock, a peek of the gauges
                      and the core's step (B1's, moved), whose verdict the pass keeps
  the gate            the send channel is full, or a follower is congested, or the budget
                      holds: its term last
  a hold              skips the pass before capture, as the gate's arm does today: no
                      keyframe asked, fps_state.pause() in the FFmpeg pump, the 2 ms sleep
  its decrease        on_backpressure_skip once a skip, outside FR-35's opener grace
  the no-decrease     a hold that shut the pass by itself while the clock has stood still
    rule              for least + S with bytes out, a stall of the carrier, calls none
  out                 the FFmpeg relay byte gate and the VP9-444 watermark arm act on
                      nothing; both are still computed, and counted where they fire
```

```mermaid
flowchart TD
  L["a pass of the pump's loop"] --> G["DeliverySampler::gate:<br/>under on, on a relay,<br/>the budget's step"]
  G --> C{"the send channel full,<br/>a follower congested,<br/>or the budget holds?"}
  C -->|yes| K["skip before capture:<br/>no keyframe, the fps pause,<br/>2 ms, continue"]
  K --> D{"FR-35's grace open, or the hold<br/>alone in a still span<br/>of least + S?"}
  D -->|no| A["on_backpressure_skip"]
  D -->|yes| N["no decrease"]
  C -->|no| P["pacing, capture, encode"]
  P --> W["VP9-444: a frame over<br/>the watermark is counted,<br/>and not shed"]
  W --> T["try_send"]
```

**The no-decrease rule.** A still span of `least + S` with bytes out is a stall of the
carrier, which B1's verdict holds through: TURN over TCP serializing behind a lost
segment, or an outage. A decrease there would answer the carrier's stall as congestion.
So a hold in such a span skips its pass and calls no decrease, and a hold before the span
is `least + S` old decreases once, as a skip outside the grace does today. The rule asks
the core whether its step stood in such a span (`Budget::stalled`), and spares only a pass
the hold shut by itself: a pass the send channel or a follower shut decreases as today,
whether the budget held or not. Inside FR-35's opener grace nothing decreases, as today.

**What stays.** The send-channel gates and `try_send`'s Full arms, the followers' gate,
the AIMD and its drain, the fps cap, the relay cap and FR-35's opener grace. The grace's
stuck timer runs on the budget's holds as it ran on the byte gate's skips, and its reset
still reads the ledger. The FFmpeg pump still computes its relay byte gate as before and
hands it to FR-70's plan check first, so `plan_divergences` reads as it does with the
budget off, in every mode. The byte gate's firings still count as `bound_ledger` and the
watermark's as `bound_watermark`; under `on` on a relay neither shuts or sheds anything.

**A direct session** under `on` computes the budget as shadow does, beside its own gates:
the byte gate shuts what it shuts, the watermark sheds what it sheds, each skip outside
the grace decreases, and the step stays after the gate.

**The mode, said once.** `on` no longer runs as `shadow`, and `for_pump`'s warning is
gone. At its first pass the pump says which mode acted on its session, at INFO:
`budget_acts="on"` on a relay ("the in-flight budget acts on this relay session …") or
`budget_acts="shadow"` on a direct one ("… runs as shadow on this direct session …"),
and again when a change of transport changes it. `shadow` says nothing.

**The fields.** `budget_acted` and `budget_acted_stalled` close the `path delivery` line
([§ The budget's fields](#the-budgets-fields)): the passes the hold shut by itself, of
those the budget acted on, and the stalled holds among them. They read `(0, 0)` and `0`
in shadow and on a direct session. `budget_held` still counts the core's verdicts at
every step: under `on`, `held − acted` is the holds a full channel or a follower had
already shut.

**Off and shadow act as before.** The six protected proofs keep their bodies, and pass:

| Proof | What it shows |
|---|---|
| `media_delivery::tests::the_inflight_budget_in_shadow_answers_every_pump_call_as_off_does` | shadow acts on nothing ([above](#where-it-runs)) |
| `media_delivery::tests::off_takes_no_sample_and_writes_no_line`, `off_takes_no_path_numbers_and_writes_no_model_line` | `off` reads nothing and writes no line |
| `media_pair_memory::tests::shadow_is_todays_opener_exactly`, `encode::pair_memory::tests::todays_file_is_written_byte_for_byte_as_master` | the path memory's shadow opener and today's file, unchanged |
| `media_delivery::shaped_link::tcp_shape::the_inflight_budget_in_shadow_holds_nothing_on_a_shaped_link` | today's writer off and in shadow holds and skips nothing on the deep 2 Mbit/s path I shape (in each of B3's five passes) |
| `media_delivery::tests::off_and_shadow_gate_as_today` | `off` and `shadow` driven with B3's calls do what samplers driven as the pumps were at B2 do |
| `media_delivery::tests::plan_divergences_read_today_s_byte_gate_under_on` | `plan_divergences` under `on` read as with the budget off |

Each of B3's tests ([below](#the-tests)) was run red first, with its arm taken out: the
hold that shuts nothing, the decrease in a still span, each admitting verdict held in
turn (nothing out, under the budget, SCTP's window, written through), the budget acting
on a direct session or in shadow, the byte gate and the watermark kept in under `on` or
taken out everywhere, the grace ignored, the mode said at every pass, the fields left
off the line, the acted holds left uncounted, the gate's stall mark dropped, the step
run after the gate as well, and in `peer.rs` the budget's term out of the VP9-444 gate
and FR-70's check fed the budget's answer.

**On the shaped link.** `the_inflight_budget_on_holds_frames_on_a_shaped_relay` runs
today's plain writer off and then under `on` through the pumps' own `Shadow` and its gate
(`Shadow::gate_with`, `Shadow::shut`: the `tcp_shape` writer `Core::On`), on the deep
2 Mbit/s path I shape and on the 80 % path I stall shape. Off holds nothing; under `on`
each hold is one the line counts as acted, and nothing is shed. B3's five passes:

| Shape | Writer | Fresh % | Read % | Link queue, ms | Transit, ms | Held, of frames | Acted: held alone, in a still span | Stall marks | Fresh over off's, each pass |
|---|---|---|---|---|---|---|---|---|---|
| deep queue, offer 150 % | off | 1–2 | 98–99 | 4138–4173 / 4156–4178 | 3551–3602 / 4723–4748 | 0 of 479–481 | | | |
| | **`on`** | 98–99 | 98–99 | 112–121 / 126–136 | 118 / 123–138 | 209–210 of 600–601 | 209–210, 0 | 0 | ≫: 55.6, 38.7, 38.8, 38.7, 38.8 |
| a 800 ms stall every 3.3 s, offer 80 % | off | 62–69 | 80 | 0 / 0–193 | 0–2 / 19–471 | 0 of 601 | | | |
| | **`on`** | 65–67 | 70–74 | 0 / 0 | 0–3 / 1–4 | 46–75 of 601 | 46–75, 18–32 | 62–89 | **0.963, 1.061, 1.072, 1.046, 1.046** |

Every hold under `on` is one the line counts as acted (`budget_acted`'s first number is
the core's holds: the harness writer has no send channel to fill and no follower), and
nothing was shed. On the deep shape `on` holds about a third of the frames, as the
budget writer of `the_budget_on_the_deep_2_mbps_path_i` does (210 a run), and keeps the
transit at 118 ms median and 138 ms at most, where off's stands at 3.6 s and grows to
4.7 s: off reads as much, almost none of it fresh. No still span stood there, so no hold
was a stalled one and the core marked nothing. On the stall shape 18 to 32 of a run's
holds came in a still span: in the pumps each is a skip that calls no decrease. The core
marked 62 to 89 steps a run (each step at which a still span of `least + S` stood, held
or written through, and each that read a release), and under `on` the pumps' gate hands
each mark to the governor. Off reads more there (80 % against 70–74 %): it holds
nothing, and what it writes in a stall arrives late. `on` delivered 0.963 to 1.072 times
off's fresh, over D46 item 11's 0.95 in each pass, which the cell asserts. The ratio
moves with off's run as much as with `on`'s: off read 69 % fresh in the first pass, at
19 ms of transit at most, and 62 % in the other four, at 451 to 471 ms. On both shapes
`on` reads what the budget writer of the same cells reads in the same passes (on the
stall shape 66–67 % fresh, 72–75 % read and 34 to 61 holds a run), through the pumps'
gate instead of the core's step.

**B3's five passes against B2's runs.** Windows, debug build: the 19 `tcp_shape` cells in
a row (`--test-threads=1`), five times, with no sleep or wake event in the system log
over any pass, and all 19 passed in each, the shadow cell and B3's among them. B3 changes
no verdict of the core's and no writer of the other cells, so their budget writers read
here what they read in B2's three runs, within the runs' spread:

| Reading | B3's passes 1 / 2 / 3 / 4 / 5 | B2's runs 1 / 2 / 3 |
|---|---|---|
| the budget's read over today's writer's: the 0.4 Mbit/s path C control | 1.000, 1.101, 0.997, 1.032, 1.010 | 1.010, 0.994, 1.038 |
| the 0.4 Mbit/s path I control | 0.969, 1.067, 1.010, 0.975, 1.012 | 1.003, 0.967, 1.051 |
| the 2 Mbit/s path C control | 1.008, 1.014, 1.000, 0.997, 1.014 | 1.003, 1.006, 1.014 |
| the 2 Mbit/s path I control | 0.986, 0.983, 0.994, 1.000, 0.994 | 1.006, 1.024, 1.011 |
| the budget's fresh over today's writer's: the 80 % stall shape, path I (D46 item 11's bar, 0.95) | 1.078, 1.050, 1.059, 1.063, 1.065 | 1.072, 1.066, 1.028 |
| the 80 % stall shape, path C | 2.514, 3.475, 2.505, 2.449, 2.453 | 2.431, 2.472, 2.319 |
| the 150 % stall shape, path I, over today's skip | 7.681, 8.131, 8.005, 7.732, 8.104 | 8.372, 8.321, 8.156 |

| Deep shape, offer 150 % | The budget: transit, ms, median / most | Fresh / read % | B2's runs: transit, ms | Fresh / read % | Today's writer: transit, ms |
|---|---|---|---|---|---|
| 2 Mbit/s, path I | 118–119 / 123–124 | 99 / 99 | 118–119 / 124–125 | 99 / 99 | 3551–3555 / 4743–4745 |
| 2 Mbit/s, path C | 118–119 / 152–166 | 95 / 95 | 118–119 / 155–159 | 95 / 95 | 753–758 / 871–883 |
| 0.4 Mbit/s, path I | 114–116 / 117–139 | 98 / 98 | 115 / 118–119 | 98 / 98 | 3559–3561 / 6752–6753 |
| 0.4 Mbit/s, path C | 115–116 / 116–119 | 97 / 97 | 114–116 / 118–123 | 97 / 97 | 3192–3194 / 3477–3479 |

Every control read met the cell's 0.95, and the stall bar held on both 80 % shapes in
every pass. The second pass reads differently from B2's runs, by today's writer: it read
298 and 299 kbit/s on the 0.4 Mbit/s controls (300 to 336 in every other pass and in
B2's runs), so the budget's ratios rose to 1.101 and 1.067, over B2's 0.967 to 1.051,
while the budget itself read 328 and 319 kbit/s, inside B2's runs' 314 to 329. On the
80 % path C stall shape today's writer delivered 336 kbit/s fresh in that pass (461 to
501 in the other passes and in B2's runs), and the ratio rose to 3.475. B1's budget
read 0.931 of today's writer on the 2 Mbit/s path I control in the second pass and 0.963
on the 2 Mbit/s path C control in the fifth (B2's runs: 0.985 to 1.014), the first under
the 0.95 the cell asks of the budget only. On the path I controls the budget's own reads
sit a little under B2's runs in most passes (303 to 328 kbit/s at 0.4 Mbit/s against 324
to 329; 1,711 to 1,760 at 2 Mbit/s against 1,745 to 1,771), and the ratios hold: on the
shallow queues, which drop, SCTP's window collapses and regrows from run to run
([above](#the-push-backs-in-the-shadow-model-d46-b2)). On the 150 % path I stall shape
the ratio sits under B2's in each pass (7.68 to 8.13 against 8.16 to 8.37): the budget's
fresh delivery spans B2's (1,520 to 1,585 kbit/s against 1,550 to 1,590), and today's
skip's reached 205 kbit/s in two passes, against B2's 189 to 190. The deep shapes' most
transit rose to 139 ms on the 0.4 Mbit/s path I shape and 166 ms on the 2 Mbit/s path C
shape, both in the fourth pass (B2's runs: 119 and 159), under the cells' 300 ms; their
medians and their delivery are B2's.

**What it leaves.**

- No session runs `on` in B3's runs: the cell drives the pumps' `Shadow` and gate under a
  harness writer, with no encoder, no capture and no keyframe. AC32's lab runs and O11's
  field read come before `on` is anyone's default (the kill-switch registry, below).
- FR-35's stuck timer reads the budget's holds as congestion inside the grace, as it read
  the byte gate's skips, and its reset reads the ledger, which the budget does not bound.
  No cell runs the grace.
- The ledger row `p2:BUDGET-1` still says `on` runs as `shadow`. Rows are never edited:
  `p2:BUDGET-4` says so.

### Where

| Where | Anchor |
|---|---|
| the core | `agents/roomlerd/src/media_inflight.rs:521` (`Budget`), `:653` (`step`), `:1141` (`size`), `:979` (`rate_at`), `:1062` (`held_throughout`), `:956` (`fold_least`), `:1081` (`watch_still`), `:934` (`take_window`), `:830` (`reset_least`), `:838` (`on_new_association`) |
| the stall classifier | `agents/roomlerd/src/media_inflight.rs:105` (`BURST_PERMILLE`), `:111` (`BURST_WITHIN`), `:115` (`HOL_RELEASES`), `:225` (`Classifier`), `:889` (`read_release`), `:876` (`through`), `:866` (`still_age`), `:857` (`forget_releases`), `:683` (the write-through's verdict in `step`), `:1008` (the rate's start in `rate_at`); its fields: `agents/roomlerd/src/media_delivery.rs:1176` (`budget_released`) |
| the pump's side | `agents/roomlerd/src/media_inflight.rs:1334` (`Shadow`), `:1366` (`for_pump`), `:1403` (`on_frame`), `:1416` (`on_frame_with`), `:1580` (`on_pair_check`); the count: `:1156` (`PumpBytes`), `:1175` (`PumpCount`), `:1195` (`Unsent`) |
| the sampler and the line | `agents/roomlerd/src/media_delivery.rs:541` (the field), `:875` (`on_frame`), `:892` (`on_pair_check`), `:902` (`note_gate`), `:1018` (`on_handed`), `:1026` (`pump_count`), `:805` (`report`), `:1117` (`emit`) |
| the VP9-444 pump | `agents/roomlerd/src/peer.rs:4293` (the send task's count), `:4307` and `:4336` (each frame taken, each chunk released), `:4390` (the pair), `:4507` (the gate's count), `:4525` (the step), `:5212` (the watermark), `:5247` (`on_handed`), `:5264` (a full channel) |
| the FFmpeg pump | `agents/roomlerd/src/peer.rs:7306` (the send task's count), `:7328` and `:7384` (each frame taken, each chunk released), `:7722` (the pair), `:8125` (the gate's count, the ledger beside the channel), `:8200` (the step), `:9926` (`on_handed`), `:9952` (a full channel) |
| the config key | `crates/agent-core/src/config.rs:1228` (the field), `:3501` (the env fallback); `crates/agent-core/src/config_surface.rs:1451` (the key), `:2157` (its echo), `:2758` (its check) |
| the harness's outages and reads | `agents/roomlerd/src/media_delivery.rs:3684` (`Outages`), `:3882` (`out_direction`), `:5710` (`outaged`), `:6794` (`print_releases`), `:6854` (`recoveries`), `:7010` (`stalls_against_today`), `:7130` (`outages_against_today`) |
| B2's view of the budget | `agents/roomlerd/src/media_inflight.rs:580` (`Over`), `:769` (`over`), `:812` (`held_each`), `:724` (`still_after`), `:737` (`take_mark`), `:673` (the mark in `step`), `:1609` (`Shadow::take_mark`); the opening: `:597` (`Over::opening`), `:798` (its reading in `over`), `:669` (the first measured rate, in `step`), `:1030` (the rate's provenance, in `rate_at`), `:850` (a new association's opening, in `on_new_association`) |
| B2's push-backs | `agents/roomlerd/src/media_delivery.rs:254` (`SamplePushBack`), `:264` (`Burst`), `:308` (`BURST`), `:318` (`settle`), `:343` (`push_back_of`), `:355` (P3, with the opening's), `:744` (`Budget::over` at a sample's close), `:618` (`heartbeat`, the hand-off), `:642` (`due_push_back`), `:652` (`on_mark`), `:875` (`on_frame`, the mark) |
| the shadow model and the memory | `agents/roomlerd/src/encode/pipe.rs:190` (`STALL_WINDOWS`), `:332` (`observe_sample_capacity`), `:350` (`note_stall`), `:602` (`PushedBy`), `:647` (`PathModel::sample_pushed_back`); `agents/roomlerd/src/encode/governor.rs:1444` (the window's take), `:2118` (`on_sample_push_back`), `:2129` (`note_path_stall`); `agents/roomlerd/src/encode/pair_memory.rs:1071` (`sample_pushed_back_in`) |
| the harness's B2 | `agents/roomlerd/src/media_delivery.rs:5199` (`handed`), `:5233` (`model_fold`), `:5366` (`print_b2`); the opening rules: `:5112` (`Open`), `:5130` (`OPENS`), `:5152` (`holds_over`), `:5176` (`opening_of`) |
| B3's gate | `agents/roomlerd/src/media_inflight.rs:1253` (`Pass`), `:1281` (`Pass::shuts`), `:1303` (`decreases`), `:1309` (`sheds`), `:1474` (`Shadow::gate`), `:1497` (`gate_with`), `:1533` (`acts_on`), `:1540` (`acting`), `:1560` (`shut`), `:1571` (`shed`), `:1366` (`for_pump`, with no warning), `:743` (`Budget::stalled`), `:755` (`note_acted_hold`), `:289` (`WindowReport::acted`) |
| B3's sampler | `agents/roomlerd/src/media_delivery.rs:921` (`DeliverySampler::gate`), `:948` (`gated`: the stall mark and the mode line), `:968` (`shuts`), `:980` (`on_skip`), `:995` (`sheds`), `:875` (`on_frame`, which reads nothing while the budget acts), `:902` (`note_gate`), `:1093` (`say_budget_mode`), `:1178` (the fields) |
| B3 in the pumps | the VP9-444 pump: `agents/roomlerd/src/peer.rs:4504` (the budget's gate), `:4505` (the capacity gate), `:4517` (the decrease), `:4522` (the 2 ms sleep), `:5199` (the watermark arm); the FFmpeg pump: `:8069` (today's byte gate), `:8096` (FR-70's check of it), `:8110` (the budget's gate), `:8111` (the capacity gate), `:8129` (the fps pause), `:8151` (FR-35's stuck timer), `:8169` (the grace), `:8171` (the decrease) |
| the harness's B3 | `agents/roomlerd/src/media_delivery.rs:4611` (`Budgeted::On`), `:4726` (`Core::On`), `:5866` (its writer), `:5620` (`TcpRun::acted`), `:6955` (`turned_on`), `:7275` (`print_b3`), `:7316` (the cell) |

### The tests

`cargo test -p roomlerd --lib inflight -- --skip clipboard::` and
`cargo test -p roomlerd --lib media_delivery::tests:: -- --skip clipboard::` for the
sampler's, and `cargo test -p roomler-node-core --lib inflight_budget` for the key:

| Test | Shows |
|---|---|
| `media_inflight::tests::the_inflight_budget_admits_while_out_is_under_it`, `the_inflight_budget_holds_at_out_equal_to_it`, `the_inflight_budget_admits_with_nothing_out`, `the_inflight_budget_admits_when_sctp_s_window_binds_with_nothing_pending` | the verdict, its equality, and its two exceptions |
| `the_inflight_budget_is_never_under_three_packets`, `the_inflight_budget_never_lets_out_more_than_sctp_s_window_in_a_least` | the floor, and the window's bound on the rate |
| `the_inflight_budget_s_rate_is_the_opening_rate_before_the_first_full_second`, `the_inflight_budget_s_rate_is_the_most_of_its_1_s_figures_over_2_s`, `the_inflight_budget_s_rate_falls_after_a_held_window`, `the_inflight_budget_s_rate_neither_falls_nor_expires_while_nothing_holds`, `the_inflight_budget_s_rate_stands_across_a_still_screen_that_wakes` | the rate |
| `the_inflight_budget_s_least_is_s_before_the_first_sack`, `the_inflight_budget_s_least_falls_with_a_lower_reading`, `the_inflight_budget_s_least_starts_over_on_a_new_pair`, `the_inflight_budget_s_least_starts_over_on_a_new_association`; `media_delivery::shaped_link::the_inflight_budget_s_least_starts_over_on_a_new_clock` | the least and its two resets, the second also on two real associations |
| `a_still_clock_counts_only_with_bytes_out`, `a_still_clock_counts_only_past_the_least_and_s`, `the_inflight_budget_s_window_counts_and_starts_over`, `the_spread_s_median_is_to_its_slot`, `the_inflight_budget_s_rings_are_fixed` | the window's counters, and the fixed rings |
| `the_inflight_budget_switch_is_off_unless_shadow_or_on`, `media_delivery::tests::the_inflight_budget_on_says_once_which_mode_acted`, `roomler_node_core::config_surface::tests::inflight_budget_set_echo_clear_and_validate` | the switch, the line that says which mode acted under `on` (once, and again on a change of transport; none in shadow, and no warning where a pump reads the switch), and the key |
| `the_inflight_budget_s_pump_count_takes_and_releases_per_chunk` | the pump's count |
| `a_burst_release_is_head_of_line_and_one_at_the_rate_a_drain`, `a_release_whose_first_step_comes_too_late_to_tell_is_a_drain` | D46's classifier: the release's two classes, the share's edge (of what was in flight, not pending), and when it is read |
| `the_first_still_span_holds`, `after_a_head_of_line_release_a_still_span_writes_through_at_the_clock_s_rate`, `after_a_drain_release_a_still_span_holds`, `a_still_span_writes_through_only_while_sctp_queues_nothing`, `two_head_of_line_releases_in_a_row_when_two_are_asked` | the write-through: when it goes, up to `rate × still_age` over the budget and no more, and when it holds |
| `a_new_association_and_a_new_pair_each_forget_the_class` | the class's two resets, the second through the pumps' side |
| `a_second_that_starts_inside_a_released_still_span_counts_from_its_first_still_reading` | the rate across a release that comes back over two frames |
| `media_delivery::tests::the_inflight_budget_s_fields_close_the_line` | the fields, their order, and the line with the switch off |
| `the_inflight_budget_in_shadow_answers_every_pump_call_as_off_does` | shadow acts on nothing (above) |
| `a_step_marks_a_still_span_that_stands_and_the_release_it_reads`, `the_budget_says_what_it_saw_over_a_samples_span`, `the_budget_says_whether_it_held_in_each_second_of_a_span` | B2's view of the budget: the mark from a span's `least + S` to the step that reads its release, `over`'s facts at each edge of a span, and a hold in each second |
| `a_span_that_starts_before_the_first_measured_rate_is_the_opening` | the opening (`Over::opening`): a span that starts before the association's first measured rate is the opening's and one at or after it is not, a new association opens again, and while the application-limited rule keeps the opening rate nothing is measured until a second held throughout |
| `media_delivery::tests::a_queued_sample_pushes_back_at_its_own_rate_whatever_the_budget`, `a_backlogged_sample_with_no_still_span_pushes_back_with_the_budget_and_not_without`, `a_sample_with_a_still_span_of_least_and_s_in_it_is_no_p3_capacity`, `a_sample_that_holds_a_releases_burst_is_no_capacity`, `an_application_limited_sample_never_lowers_the_floor` | B2's push-backs: P1 whatever the budget, P3 with it and never without, a still span inside a sample is no P3, the burst rule against the correction and no rule, and a sample that is neither never lowers the floor |
| `media_delivery::tests::a_sample_the_budget_held_at_its_opening_is_no_p3_capacity`, `a_held_sample_at_a_measured_rate_with_no_still_span_is_p3`, `a_session_whose_only_held_sample_is_the_openers_leaves_the_capacity_absent` | the opening rule: a held sample that starts in the opening is no P3 and a `queued` one there is still P1, a held sample at a measured rate with no still span is P3, and a session whose budget held only while it opened leaves the model's capacity absent |
| `the_sampler_pushes_back_by_what_its_budget_saw` | the sampler's own push-back over 7 s of readings, with the budget in shadow and off: due at its close with the budget off, after its settle time in shadow, where the first sample starts in the opening and is none |
| `a_sample_that_closes_in_a_stall_is_no_capacity` | the rule's other half: a sample that closes while the clock stands still reaches nothing when the still span stands `least + S` inside its settle time, and is P3 when the clock moves again sooner |
| `encode::pipe::tests::a_sample_push_back_moves_only_the_model_and_never_raises_its_floor`, `a_stall_the_budget_saw_keeps_the_next_window_out_of_the_model`, `encode::governor::tests::a_sample_push_back_moves_only_the_shadow_model` | the model's law for a push-back, the window after a stall, and that `Pipe`'s own law and the applied rate do not move |
| `media_inflight::tests::under_on_every_verdict_but_hold_admits_a_relay_pass`, `the_budget_acts_only_under_on_on_a_constrained_session` | B3's pass: under `on` on a relay only a hold shuts it (nothing out, under the budget, SCTP's window binding and a written-through still span admit it, whatever the byte gate says), and the watermark sheds nothing; under `shadow`, and on a direct session under `on`, the pass is today's and the gate steps nothing |
| `under_on_a_hold_in_a_still_span_calls_no_decrease`, `media_delivery::tests::under_on_a_stall_hold_calls_no_decrease_and_marks_the_stall` | the no-decrease rule: a hold once the clock has stood still for `least + S` calls no decrease, one before that does, a pass the send channel shut first does, and inside the opener's grace none does; through the sampler the gate also marks the stall for the governor at those steps |
| `under_on_the_byte_gate_and_the_watermark_fire_and_act_on_nothing`, `media_delivery::tests::the_byte_gate_and_the_watermark_are_out_under_on_and_in_under_off_and_shadow` | today's two bounds under `on` on a relay: counted where they fire, acting on nothing; in under `off`, `shadow` and a direct session under `on` |
| `media_delivery::tests::under_on_the_budget_s_hold_skips_a_relay_pass_and_decreases_once_per_hold` | the hold through the sampler as both pumps call it: 200 of 300 passes skipped with one decrease each and no frame shed, and the line's `budget_acted` |
| `media_delivery::tests::fr35_s_opener_grace_keeps_the_aimd_through_the_budget_s_holds_under_on` | FR-35's opener grace under `on`: the holds inside it call no decrease, and after it one each |
| `media_delivery::tests::under_on_a_direct_session_runs_as_shadow`, `off_and_shadow_gate_as_today` | a direct session under `on` passes, skips, decreases and writes its lines as a shadow sampler does; `off` and `shadow` driven with B3's calls do what samplers driven as the pumps were at B2 do, on relay and direct passes |
| `media_delivery::tests::plan_divergences_read_today_s_byte_gate_under_on` | FR-70's `plan_divergences` under `on` read as with the budget off, where the budget's answer differs at every pass; a plan fed the budget's answer would diverge at each |
| `media_delivery::tests::both_pumps_gate_on_the_budget_s_pass` | both pumps' text in `peer.rs` as compiled: the budget's gate before the capacity gate, its term last, the arm's decrease through it with no keyframe asked, the step after the gate, the watermark arm through it, and FR-70's check of today's byte gate before the budget's gate |

The `tcp_shape` cells are real-time measurements, ignored in CI (FR-148 D44):
`cargo test -p roomlerd --lib media_delivery::shaped_link::tcp_shape::the_ --
--include-ignored --nocapture --test-threads=1 --skip clipboard::`.

**What is left.** B2 took P1 and P3 into the shadow model
([above](#the-push-backs-in-the-shadow-model-d46-b2)), and B3 turned `on` for relay
sessions ([above](#the-budget-acts-under-on-d46-b3)): the hold and its write-through, the
no-decrease rule, and the watermark arm and the relay byte gate out (D46 item 13). Direct
sessions go `on` only after their own shadow read. Until then the switch and its calls
are FR-135 debt: `p2:BUDGET-1` (the switch), `p2:BUDGET-2` and `p2:BUDGET-3` (9 lines in
the VP9-444 pump, 8 in the FFmpeg pump), and `p2:BUDGET-4` (B3's gate: 2 more lines in
the VP9-444 pump, 1 fewer in the FFmpeg pump), all retiring with FR-148 P5
(`docs/plans/rc-media-debt.tsv`). The classifier adds no switch, no read of the
environment and no line to a pump: the ratchet's counts are unchanged. Nor does B2: the
pumps' four calls that hand the sampler the governor change in place, and its counts stay
181, 33, 1312, 1482 and 3896. B3's are 181, 33, 1312, 1484 and 3895.

The classifier's open edges, none of them measured on a real carrier yet:

- It reads a release at a frame. At under about ten frames a second the first frame
  50 ms after a span's last still reading comes too late to tell, and every release
  reads as a drain: the budget holds through every still span, as B1 does.
- A drain read as head-of-line lets the next still span write through, up to
  `rate × still_age` over the budget. Where in flight was far under a budget's worth (an
  application-limited path whose link is much faster than the offer) a drain at the
  link's rate can bring back 600 ‰ of it within 50 ms. If the next span is an outage,
  what was written through drains at the link's rate after it, the release reads as a
  drain, and the span after holds.
- A lone packet delivered as a stall begins is acknowledged one `ACK_INTERVAL` later
  (200 ms, `crates/vendored/webrtc-sctp/src/timer/ack_timer.rs:7`), inside the stall. Its
  1,200 bytes end a still span of about 200 ms, whose release reads as a drain, and the
  rest of the stall holds as a new span: 6 of the 111 releases the budget read on the
  stall shapes over the three runs read so, four at S = 75 ms, where a span of 165 to
  200 ms is already `least + S` old, one on the first stall of an association, which
  holds anyway, and one at 1.5 times the link. The error is B1's hold. Not
  reading a release whose clock stands still again by the read would keep the class
  through such a stall, and would also write through an outage whose start lets one
  acknowledgement through: no cell has that, and the rule is not built.
- The rate remembers the last released still span only. When a second stall is released
  within a second of the first, the seconds that start inside the first and end after
  that count from where they start, and the first stall's burst can lift the rate for up
  to two seconds. No cell has two stalls that close.
- A stall that writes through releases more than the budget, and the run's fold reads
  the release as delivery. On the stall shapes the budget's floor and P1's ended at 1.02
  to 1.72 times the link, against 0.91 to 1.17 under B1's budget beside it, and its P3
  at 1.02 to 1.37 against B1's 0.91 to 1.16. Today's writers' floor ended at 1.24 to
  1.78 and their P1 at 1.04 to 1.55; at 1.5 times the link the budget's P1 ended over
  today's skip's on path I (1.65 to 1.72 against 1.30 to 1.31), and its floor over
  today's skip's on path C in one run (2,462 against 2,456 kbit/s). B1's cell asserted
  the lower floor on its budget, and now asserts it on B1's budget only. Through the
  outages every writer's floor stays at 0.95 to 1.01 times the link. **B2 closes this
  edge in the shadow model.** The release's burst no longer reaches the model's floor:
  over three runs it ended at 0.82 to 1.00 times the link on the stall shapes under the
  budget, and at 0.82 to 1.00 under B1's budget
  ([§ The push-backs in the shadow model](#the-push-backs-in-the-shadow-model-d46-b2)).
  `Pipe`'s own law still takes every window, the release's among them: its floor ended
  at 1.02 to 1.73 under the budget on the same cells. With the budget off nothing marks
  a stall, and today's writers' model floor ended at 0.68 to 1.45 there.

## Config / env reference

All keys live in the agent config (`roomler config set …`) with
`ROOMLERD_*` env twins; restart required.

| Key | Default | Meaning |
|---|---|---|
| `priority_res_cap` | off | Restore the pre-rc.445 dial resolution rungs (A/B only) |
| `smoother_rate_pct` / `balanced_rate_pct` | 70 / 85 | Dial ceiling factors (30–100) |
| `constrained_queue_ms` | 450 | Send-queue byte budget, ms of the relay ceiling; 0 = unbounded |
| `constrained_hrd_pct` | 200 | HRD window for relay sessions, % of maxrate. ⚠ sub-100 is per-host experiment only — a window smaller than a forced IDR makes Intel AV1 **error and hang** (rc.442 incident) |
| `constrained_cq_relief` | 4 | CQ softening at a sub-native rung on relay — only reachable via explicit picks / restored rungs |
| `idle_refine_settle_constrained_ms` | 1200 | Up-flip settle on relay when a rung exists |
| `gpu_scale` / `scale_threads` | on / 1 | HW-downscale Phase A/B levers (only active when something scales) |
| `ROOMLERD_RELAY_MAX_KBPS` | 3000 | The constrained-transport ceiling clamp (the nominal the learner grows above) |
| `relay_ceiling_learn` / `ROOMLERD_RELAY_CEILING_LEARN` | on | FR-35 — the constrained ceiling learns the pair; off ⇒ the fixed clamp and no rate memory ([§ The ceiling learner](#the-ceiling-learner-fr-35-p1--growth-above-the-nominal)) |
| `relay_max_hi_kbps` / `ROOMLERD_RELAY_MAX_HI_KBPS` | 8000 | FR-35 — the ceiling the learned rate may reach; `0` = nominal only (= `relay_ceiling_learn` off) |
| `measured_evidence` / `ROOMLERD_MEASURED_EVIDENCE` | on | FR-79 V5b — only a measurement is evidence about the rate; off ⇒ V5a's rules ([§ V5b](#v5b--only-a-measurement-is-evidence-about-the-rate)) |
| `ROOMLERD_SMOOTH_MAX_EDGE` / `RELAY_MAX_EDGE` | 1024 / 1280 | Rung sizes when `priority_res_cap` is on |
| `direct_queue_ms` | 150 | FR-74 P1/P1b — the direct gate's **lag bound**: bytes over 150 ms of the ceiling gate only once the measured send wait has also crossed 150 ms; 0 disables the gate ([§ The direct path](#the-direct-path-fr-74--a-ceiling-that-is-a-bound-a-gate-that-measures-lag)) |
| `direct_hrd_pct` | 100 | HRD window for direct sessions, % of maxrate ([25, 200]); also the gate's hard budget. ⚠ `av1_*` encoders stay at 200 regardless (the rc.443 hang) |
| `ROOMLERD_FFMPEG_MAXRATE_KBPS` | unset | Env only — replaces the computed ceiling in both directions (the FR-74 P0 A/B knob and the way back from P1) |
| `ROOMLERD_VP9_DIRECT_MAX_Q` | 16 | FR-74 P3 — the libvpx VP9 4:4:4 pump's worst quality on direct transports (q-index 64); 63 = the pre-P3 behaviour |
| `ROOMLERD_SW_FPS_60` | on (Linux), off (Windows, macOS) | FR-112 P2 — software encoders target 60, paced down to what they sustain, never below 30; `0` = 30 and the old base everywhere, `1` opts Windows or macOS in ([§ Software encoders](#software-encoders-at-60-fps-fr-112)) |
| `ROOMLERD_ENCODER_CLOCK` | on (Linux), on for software encoders (Windows), off (macOS) | FR-112 P1, P3 — the encoding and capture threads ask for their clock (Linux: a `uclamp_min`; macOS: a QoS class; Windows: out of power throttling); `0` = the scheduler's default, `1` asks on every thread that can, hardware encoders' included; each OS's default follows its own numbers ([`encoders.md`](encoders.md#the-clock-on-macos-and-windows-fr-112-p3)) |
| `ROOMLERD_SW_PACE_UP_MARGIN` | 2.5 | Env only. FR-112 P2d: how far (fps) a software encoder's estimate must clear a higher step before its pace moves up to it (0–10). `0` = 0.4.142's pace, which steps up as soon as the estimate quantises higher ([§ Software encoders](#software-encoders-at-60-fps-fr-112)) |
| `ROOMLERD_RATECTL` | shadow | Env only (B1). FR-63 B1 — whether the shadow rate controller runs beside the governor; `off` stops it, any other value is `shadow`. It drives NOTHING — its only output is the `shadow_*` heartbeat fields ([§ The shadow rate controller](#the-shadow-rate-controller-fr-63-b1)). The config-surface tribool and the `live` value are B2 |
| `ROOMLERD_PATH_MODEL` (config key `path_model`) | shadow | FR-148 P0, P1 (D28) — `off` is the release before P0: no delivery sample, no path number, no path memory, none of their lines. `shadow` (unset, or any value but `off` and `on`) takes the samples and the tagged path numbers ([§ Delivery samples](#delivery-samples-fr-148-p0--what-the-path-carried-and-whether-the-sender-was-waiting)) and keeps the path model's memory ([§ The path memory](#the-path-memory-fr-148-p1--the-models-floor-under-a-key-that-names-the-path)); the opener reads today's rate memory. `on` is `shadow`, and a constrained session of the FFmpeg data-channel pump whose pair has a key and a fresh entry opens from the path memory ([§ Which rule opens the session](#which-rule-opens-the-session-step-3)); ⚠️ it is set on no device until FR-148's O7 is ruled ([§ The switch](#the-switch-d28)). A set variable wins over the key; remote config cannot set it |
| `ROOMLERD_INFLIGHT_BUDGET` (config key `inflight_budget`) | off | FR-148 D46 B1 — `shadow` computes the in-flight budget once a frame on both data-channel pumps, counts what it would have held, and writes its fields at the end of the `path delivery` line; it holds nothing. Since B2 what it sees also reaches the shadow model, and only it: a backlogged delivery sample with no still span is a push-back (P3), one a still span met is none, and the windows after a stall it saw are no reading ([§ The delivery samples' push-backs](#the-delivery-samples-push-backs-d46-b2)). Since D46's step B3 `on` acts on relay sessions: the budget's hold skips a pass before capture, a hold in a still span of `least + S` calls no AIMD decrease, and the FFmpeg relay byte gate and the VP9-444 watermark arm hold nothing; a direct session computes it as `shadow` does ([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)). Any other value is `off`. With `ROOMLERD_PATH_MODEL=off` it stays off ([§ The in-flight budget](#the-in-flight-budget-fr-148-d46--what-may-be-out-in-time)). A set variable wins over the key; remote config cannot set it |
| `qsv_rate_mode` / `ROOMLERD_QSV_RATE_MODE` | `cbr` | FR-148 P2 — `qvbr` opens an HEVC or H.264 QSV encoder off constant bitrate: its target at 0.9 × the cap, under `maxrate` at the cap, at the session's quality (QVBR), so a frame is no longer padded to the cap. A driver that refuses it opens at `cbr`. Every rebuild and reopen follows. This device only: not pushable ([`encoders.md`](encoders.md#qsv-off-constant-bitrate-fr-148-p2)) |

⚠️ Three keys an older `config.toml` may still carry are **deleted, not off**
— the daemon ignores them: `transit_classify` and `transit_hold` (FR-71
T1a/T1b; removed by FR-79 V1 in `agent-v0.4.93` — the classifier always runs
and the validity gate replaced the hold) and `rate_prior_decay` (FR-70 P1;
removed by FR-79 V3a in `agent-v0.4.95` — the prior always decays, which
FR-70 P1 field-verified on 0.4.64 with a same-build control). Their
behaviour is described under the validity gate at the top of this page.

## Kill switches and when each retires — the registry (FR-62 AC4)

This is every rate-control kill switch in `agents/roomlerd/src/encode/mod.rs`, with
the evidence that retires it and the phase that deletes it. It implements rule 4 of
the [rate-control plan](plans/rate-control-architecture.md): a switch gets its
retirement condition the day it ships, and it leaves the code only through its row
here. The plan put this table in `docs/fr/README.md`. It lives on this page instead,
beside the keys it retires, because every FR session edits that registry and these
rows change with every deletion PR.

```mermaid
flowchart LR
  B["a switch ships<br/>with its row: default +<br/>the evidence that flips it"] --> E["evidence on the surface:<br/>a field read shown RED first,<br/>or a release of heartbeats"]
  E --> F["the default flips,<br/>or the condition is met"]
  F --> S["one release soak"]
  S --> D["the deletion PR removes the old arm<br/>AND the switch, and fills<br/>its row's Deletion PR"]
  D --> I["an older config.toml that still<br/>carries the key is ignored,<br/>not refused"]
```

⚠️ **Nothing below has been deleted, and nothing is due before FR-63's B2 flip plus one
release soak.** The plan sequences the deletions after that soak (its Sequencing row 5),
and `encode/` is shadow-first (its Process rule 1). FR-62's A0 also showed that a
heuristic which rations rate moves is dead weight on NVENC but load-bearing on QSV,
so every retirement is gated on the backend it is retired for.

**The switches.** Each one is `ROOMLERD_<KEY>` in the environment and the lowercase
key in the config (`roomler config set`), except the three marked *env only*. A
default of *on* guards shipped behaviour. A default of *off* means the behaviour is
still waiting for its evidence.

| Switch | `encode/mod.rs` | From | Default | What it guards | Retires when | Exit | Deletion PR |
|---|---|---|---|---|---|---|---|
| `ENCODER_INPLACE_RATE` | `:356` | FR-62 A1 | off | QSV applies a rate move in place; NVENC sizes its VBV to the open's window (`inplace_rate_bufsize`, `:369`) | QSV gets an in-place path (patch 0004), or the arm is dropped. On Iris Xe the Reset fails `-14` in both `low_power` modes | FR-62 | — |
| `ENCODER_NVENC_ASSUME_IDR` (env only) | `:385` | FR-62 A2 | off | sends NVENC back onto the IDR-rationing pump path | a release of constrained NVENC heartbeats with `idr_count` flat across `rate_moves` (FR-62 AC3's read) | FR-62 | — |
| `BG_REBUILD` | `:523` | FR-1 P3 | on | a rebuild-bound rate apply opens the new encoder off-thread and swaps between frames | ⚠️ not while QSV rebuilds: inline, a QSV rebuild is the 0.34–2.0 s pump freeze (#1254) | FR-62 A4 row 2, FR-63 B3 | — |
| `BG_REBUILD_CONSTRAINED` | `:544` | FR-62 (#1254) | on | the same on constrained sessions | never, while any rebuild backend exists | — | — |
| `RELAY_IDR_THRIFT` | `:688` | FR-10 | on | a constrained session suppresses the idle-settle keyframe and spaces its deferred rate applies | narrows to settle-keyframe suppression on a backend whose rate move costs no IDR. The plan's precondition, "A0 pass on QSV", failed | FR-62 A4 row 1, FR-63 B3 | — |
| `MEASURED_CEILING` | `:333` | measured-rate stage 1 | on | the ceiling is clamped to 85 % of the measured drain rate | one release after B2's `live` | FR-63 B3a | — |
| `SLOW_LINK_FLOOR` | `:347` | FR-59 P1 | on | the floor descends toward a measured pipe | one release after `live` | FR-63 B3a | — |
| `SEED_CONTRADICTION` | `:441` | FR-59 P6 | on | a held goodput far below the learned ceiling abandons it | one release after `live` | FR-63 B3a | — |
| `VIEWER_RATE_CLAMP` | `:462` | FR-59 P3 | on | the viewer's arrival report drives the constrained loop | one release after `live` | FR-63 B3a | — |
| `QUEUE_DRAIN` | `:480` | FR-59 P4 | on | a transit queue too deep to cut through pauses the pump | one release after `live` | FR-63 B3a | — |
| `MEASURED_EVIDENCE` | `:495` | FR-79 V5b | on | only a measurement is evidence about the rate: five rules on the constrained path | one release after `live`, or sooner if FR-79's AC8 and AC9 read clean on it, since the off arm is the defect | FR-63 B3a | — |
| `RELAY_AGE_FEEDBACK` | `:696` | FR-15 | on | the viewer's paint age caps the frame rate and cuts the rate | one release after `live` | FR-63 B3a | — |
| `CONSTRAINED_QUEUE_MEASURED` | `:425` | FR-59 P2 | on | the constrained queue budget is counted in the measured drain rate | the budget becomes `reference_bps` | FR-63 B3b | — |
| `SLOW_LINK_PROFILE` | `:510` | FR-59 P5 | on | a pair remembered as slow opens with fewer pixels and frames | it becomes a rung | FR-63 B3b | — |
| `RELAY_CEILING_LEARN`, with `RELAY_MAX_HI_KBPS` | `:964` | FR-35 | on | the constrained ceiling learns the pair | it becomes the prior's bound | FR-63 B3b | — |
| `AREA_MIN_BITRATE` | `:318` | area floor (2026-08-26) | on | the AIMD floor scales with the encoded area | it becomes a `RateDomain` rule | FR-63 B3b | — |
| `RATE_SLOW_START` | `:573` | FR-63 | off | a session opens with slow start instead of a constant | B2's evidence flips it, or it is deleted | FR-63 | — |
| `RATECTL` (env only) | `:613` | FR-63 B1 | shadow | whether the shadow controller runs beside the governor | B2 turns it into the `ratectl` tribool (`shadow`, `live`, `off`) | FR-63 B2 | — |
| `SHARED_RATE_SPLIT` (env only) | `:926` | shared pipeline | on | a constrained shared encoder divides its ceiling by the live viewer count | ⚠️ an open row: the plan's B3 list does not name it | — | — |
| `PATH_MODEL` | `agents/roomlerd/src/media_delivery.rs:157`, not `encode/mod.rs` | FR-148 P0 | shadow | the delivery samples and their line ([§ Delivery samples](#delivery-samples-fr-148-p0--what-the-path-carried-and-whether-the-sender-was-waiting)), the path numbers' tags and their line ([§ Path numbers](#path-numbers-and-their-provenance-fr-148-p0--how-each-was-obtained-and-how-old-it-is)), and since P1 the path memory and its two lines ([§ The path memory](#the-path-memory-fr-148-p1--the-models-floor-under-a-key-that-names-the-path)). Three values since P1 (D28): `off`, `shadow` (the default) and `on`, under which the path memory opens a constrained session of the FFmpeg data-channel pump ([§ Which rule opens the session](#which-rule-opens-the-session-step-3)); ⚠️ `on` is set on no device until FR-148's O7 is ruled | P5 deletes it with the composition it measures | FR-148 P5 | — |
| `QSV_RATE_MODE` | `agents/roomlerd/src/encode/ffmpeg/encoder.rs:3471`, not `encode/mod.rs` | FR-148 P2 (FR-62 A5) | `cbr` | constant bitrate on QSV: `qvbr` opens HEVC and H.264 QSV at QVBR, the target at 0.9 × the cap, and falls back to `cbr` on a refusal (`QsvRateOpen`, [`encoders.md`](encoders.md#qsv-off-constant-bitrate-fr-148-p2)) | FR-148 AC13 read with it on: a QSV keepalive no longer `maxrate ÷ fps ÷ 8` bytes, a still screen under 0.3 Mbit/s. FR-148 P5 then deletes the constant-bitrate path, and the switch with it | FR-148 P5 | — |
| `INFLIGHT_BUDGET` | `agents/roomlerd/src/media_inflight.rs:150`, not `encode/mod.rs` | FR-148 D46 B1 | off | the in-flight budget, computed in shadow on both data-channel pumps, and its fields at the end of the `path delivery` line ([§ The in-flight budget](#the-in-flight-budget-fr-148-d46--what-may-be-out-in-time)). Since D46's step B3 `on` acts on relay sessions, in place of the relay byte gate and the VP9-444 watermark arm ([§ The budget acts under `on`](#the-budget-acts-under-on-d46-b3)) | B3's gates: D46's fresh-delivery bar on the stall shapes, O11's field read of the sessions that ride a TCP leg, and AC32 in the lab; B3 then holds on it on relay sessions. FR-148 P5 deletes the switch, kept or not | FR-148 P5 | — |

**Heuristics with no switch of their own.** FR-62's A4 deletions, FR-63's B4 since 2026-11-05.
Each one retires through this table too, and its deletion PR cites the precondition below.

| Heuristic | Anchor | Plan row | Precondition | State |
|---|---|---|---|---|
| Motion-defer: `deferred_bps`, `DEFER_QUIET`, `relay_deferred_apply_allowed`, `rebuild_apply_allowed` | `peer.rs:7251`, `:7258`; `encode/mod.rs:706`, `:748` | A4 row 1 | A0 passes on QSV, and the switch is ON for one release | ❌ QSV's A0 failed (2026-09-02); both recorded causes are refuted (2026-09-04). NVENC already skips the `held_increase` arm (`peer.rs:9505`), because `NVENC_RECONFIG_FORCES_IDR` is `false` (`encode/ffmpeg/encoder.rs:893`, #1252) |
| The coarsen ladder: `BITRATE_LADDER_BPS`, `coarsen_bitrate` | `encode/aimd.rs:297`, `:317` | A4 row 2 | VideoToolbox is measured, or kept on the settle rule | VideoToolbox is measured (2026-11-05): an 8–12 ms rebuild with an IDR on every move, so the ladder stays for it too. It is load-bearing for QSV, which has no cheap apply. It keeps AMF, D3D12 and Vulkan's rate IDRs rare. It is dead weight on NVENC, and it caps NVENC at the 25 Mbps rung |
| The background swap: `rebuild_spec`, `open_rebuilt`, `adopt_rebuilt` | `encode/ffmpeg/encoder.rs:2560`, `:2608`, `:2730` | A4 row 2 | as above, plus a cheap QSV apply | load-bearing while QSV rebuilds |
| VP9's rebuild divergence: `on_encoder_rebuilt_mirror_only` | gone; the one hook left is `on_encoder_rebuilt` (`encode/governor.rs:1259`), called at `peer.rs:4826` | A4 row 3 | none in the plan, but the code called unifying it "a behavioral decision for the post-soak pass" | ✅ Retired by #4061 (2026-11-19), ahead of B2's soak, because #3472 measured it as a defect: a fresh libvpx encoder starts at its 8 Mbps boot target and nothing re-applied the session's, so a relay session that display match rebuilt 0.1 s in ran 6 min 18 s at 8.8 Mbps against its 3 Mbps ceiling, `target_bps=0` in every heartbeat (2026-11-04). The VP9-444 pump now calls `on_encoder_rebuilt`, as the FFmpeg pump does: the next tick hands the fresh encoder the session's target, and the heartbeat keeps the last target applied. Every rebuild reaches the call: new dims, a capture reopen (#3376) and the keyframe fallback. Pinned by `a_vp9_rebuild_hands_the_fresh_encoder_its_target` |

⚠️ **B3b deletes `aimd.rs` whole, and the ladder goes with it.** The plan was written
before A0-QSV, so whatever replaces the AIMD must carry QSV's rationing forward, or
every QSV session pays a 0.34–2.0 s rebuild per move.

Out of scope here are the feature switches, each owned by its FR: `media_thread`
(FR-70 M1, `:628`), `par_convert` (FR-1 P5, `:660`), `color_description` (FR-130,
`:672`), `pump_stall_watch` (FR-65 P0, `:559`), `sw_fps_60` (FR-112, `:860`) and
`fps_high` (FR-119, `:869`). `ICE_RELAY_TCP` (`:890`) is a test pin, not a kill
switch.

## Field history (why it is shaped this way)

| Release | Change | Field driver |
|---|---|---|
| rc.436–441 | HW downscale (CPU resampler rework, GPU scale-before-readback), deliverable refine-Up | Smoother's 1024 rung cost 26–45 ms CPU Lanczos on Iris Xe |
| rc.442 | Signed CQ bias (relief), byte-budget queue gate, settle 2000→1200 ms | 9 fps motion equilibrium; drag-start freeze = 0.5–1 MB queue; 4–5 s crystallize |
| rc.443 | HRD trim reverted; stale-pipeline eviction; encode-error ladder | Intel AV1 rejects + hangs on an over-budget forced IDR; a hung pump zombied the shared pipeline ("no video after 4 attempts") |
| rc.445 | **No-flip motion**: dial rungs off, dial ceiling factors, motion-deferred QSV bitrate, send-epoch flush, proven-encoder fast path | The remaining ~1 s mid-drag freeze measured as the flip's blocking encoder open (865/654 ms) + mid-motion ladder rebuilds |
| rc.446 | Deferral motion clock on any ≥4 KB frame | Light motion (GDI + AV1 window moves at 5–30 KB) slipped under the significance floor and let ladder rebuilds through mid-burst |
| 0.4.50 | FR-59 P8 — remembered-slow-pair opener; the coarsen ladder's bottom rung lowered | The ladder bottomed at 1.5 M, so no relieved target ever reached the encoder: bytes/frame 4.88 → 0.90, SACK drops 24 → 0 |
| 0.4.51 | FR-62 A1/A2 — in-place rate applies behind `encoder_inplace_rate`; the NVENC no-IDR patch | An NVENC rate move cost a forced keyframe on 20/20 rungs while the apply itself was 0.004 ms |
| 0.4.55–0.4.56 | FR-63 B-opener — slow-start on the session opener (`rate_slow_start`, default off) | The opener over-drove from BOTH directions on one host in one day: a remembered 6.13 M (6287 ms paint) and a nominal 2.55 M into a 213 kbps path (1550 ms) |
| 0.4.59–0.4.60 | FR-65 P0 — `open_ms`/`other_ms` on the stall watch; the encoder open moved off the shared runtime worker | A 0.5–1 s hole on every session's first pass sat in no measured phase; it was the encoder open, blocking a tokio worker at the moment the control plane is busiest |
| 0.4.64 | FR-70 P1 — the remembered rate is a decaying prior (`rate_prior_decay`); the FR-59 P5 cap attributed as `slow-link-cap`; `rc:video-info` carries `cap_reason`/`cap_detail` | A 200 kbps memory held a session at the floor for four minutes with nothing measuring the pipe: the P2 budget denominated in the memory tripped on every drag frame, so no queue ever formed and no measurement could contradict it; the write-back then recorded the pinned rate. Field A/B on one pair, one build, one flag: 200 k → 3.9 Mbps in 3 min with the decay, 200–285 k for 3 min without |
| 0.4.67 | FR-71 T1a + T1b — the pipe-state classifier in shadow (`transit_classify`, on) and the hold behind `transit_hold` (off); the B0 simulator gained post-ack transit stalls, the age loop's cut and the finding-4 cell | A 4.9 s DERP head-of-line block read as over-production and the rate was cut into an 8 Mbps link; the sender's queue held 1485 bytes throughout — the stall sat beyond the ack point, where no sender-side counter can see it. First live read (CORPLAP-1 over a pinned relay, 90 windows): 89 clear, one correct `overproduced` on a lock-screen burst, one `transit-stalled` on the opening window before the viewer's first report — a start-gap artefact this build still carries (fixed in #1370 for the next release) |
| 0.4.77 | FR-74 P1 — the DIRECT ceiling is a content-generous bound (0.25 bpp/s, [3, 48] Mbps per codec factor; relay keeps 0.07 / [3, 12] and its clamp) and the direct send-queue budget (`direct_queue_ms`) is denominated in the path's ceiling, not the AIMD's applied target — the applied-target budget shrank with every cut and tripped on a single text frame (six cuts to 2.24 Mbps, then 2–3.7 Mbps for minutes on CORPLAP-3). P0 on the operator's own scroll: at a 40 Mbps cap the blur could not be reproduced on AV1, VP9 4:2:0 or H.264. No new switch; `FFMPEG_MAXRATE_KBPS` and `direct_queue_ms` remain the way back |
| 0.4.79 | FR-74 P1b — the DIRECT byte-budget gate is the MEASURED send wait's call: bytes over `direct_queue_ms` × ceiling gate only when the wait (EMA of completed frames' enqueue→wire waits, or the live age of the frame on the wire, whichever is larger) has also crossed `direct_queue_ms`; bytes alone gate at `max(budget, HRD reservoir)` (`direct_queue_hard_budget_bytes`, the codec's effective `open_hrd_pct`). The 0.4.77 read tripped P1's budget on an AV1 burst the encoder was configured to emit (200 % HRD = 8.6 MB vs 648 KB) at ≤ 20 ms of viewer age. Relay paths untouched. |
| 0.4.80 | FR-74 P3 — the libvpx VP9 4:4:4 pump caps the WORST quality on DIRECT transports (`rc_max_quantizer` 16 = q-index 64; relay keeps 63; env `ROOMLERD_VP9_DIRECT_MAX_Q`). Measured offline against the real encoder: libvpx one-pass CBR + screen tune treats every mouse-wheel notch as a scene change, resets q to 255 and walks it down ~7/frame, so a choppy text scroll was rendered at the worst quality while spending a fifth of its target; the cap holds the notch frames at q 64, the steady scroll still refines to q 0 inside the budget, idle stays lossless. Constant-quality mode was measured too (sharp, ~9 Mbps) and rejected for losing the idle refine and the rate bound. |
| 0.4.90 | FR-71 T2 — a hard stall (a send blocked ≥ 1 s) is a DEFERRED verdict: its blocked-send samples are quarantined and the FR-35 ×0.5 held pending until the next REPORTED window — sends still blocking there (≥ `MIN_WINDOW_BLOCKED`) confirm it (fold + halve, one window late), nothing blocking is a pause (discard, no move); a gap window decides nothing. Field 2026-09-08 12:15 (CORPLAP-1 on the corp VPN): an overlay rekey storm blocked two sends for 2.9 s and 7.4 s on a pipe that carried 6.6 M twenty seconds later, and the hard halving, the goodput fold, FR-59 P6 and P1's floor relief read those blocked sends as a 1.6 M pipe — 6.60 → 0.68 M in 36 s. No new controller, no switch; heartbeat `hard_stalls_paused` / `hard_stalls_confirmed`. |
| 0.4.93 | FR-79 — ONE validity gate every estimator consumes (`encode::evidence`): a window is evidence about the pipe only if the agent's own loop was free, the transport did not stall, the window before it did not either, and the carrier did not move. V1 deleted `transit_hold`, `transit_classify`, T2's quarantine and T2b's shadow with their four counters (one `evidence_rejected` replaces them); FR-35's hard ×0.5 moved to the window boundary, where the gate can say whether the block was the pipe. V2 took the same rule to the write-back: the opener's growth target is the goodput estimator's measurement instead of the whole burst divided by one frame's wait (which recorded 6–8 M against a 1.8–3.4 M pipe), and the pair memory is keyed by CARRIER (`100.65.4.2|relay:derp/tcp`), because one overlay address is carried by direct, a UDP relay or DERP on different days. Field: five consecutive CORPLAP-1 sessions opened 1.44–2.82 M against a carrier measuring 1.09–6.13 M, where the same host had swung 1.26 ↔ 6.8 M. |
| 0.4.95 | FR-79 V3a + V4 — ONE belief (`pipe_bps`) composed in one place with one named source (`blocked_send_bps`); `remembered_candidate_bps`, a third inlined copy of the same composition and the `rate_prior_decay` switch deleted. And the pair memory keeps what a session MEASURED about the carrier instead of the MAXIMUM it ever saw: damped with goodput's own asymmetry (fast down, slow up), and a session that measured nothing writes nothing — not the value, not the timestamp. The `max` rule, `had_decrease`, the opener's arithmetic and its three constants are gone (−159 lines). Field: two idle CORPLAP-1 sessions wrote nothing where the old rule would have recorded 3.83 M from a socket-absorbed burst, twice in three minutes. |
| 0.4.96 | FR-79 V5 — a rejected window is not a measurement, but a transit stall while sending ABOVE the measured pipe is still the rate's doing: `Reason::is_congestion()` (true only for `TransitStalled`) + `stall_is_ours` (desired > 1.2 × the pipe) reach the rate-limited ×0.85, the ramp's end, the age streak and the prior's push-back; everything that sets a number still reads `valid`, and `apply_hard_md` is never on this path. | CORPLAP-2, 2026-09-09 07:41: one goodput sample (1,058,145) all session, eleven transit stalls, the target 1.6–2.1× over, paint age to 7,784 ms, and the rate ended HIGHER than it started — every loop that could lower it read `valid`. |
| 0.4.97 | FR-79 V5a — `stall_is_ours` compares against `pipe_bps` (goodput → the viewer's arrival rate → the prior), not `blocked_send_bps`. | CORPLAP-3 over the relay read `goodput_bps=None`, `goodput_samples=(0, 5)` for a whole session: the agent's sends block only when the queue is local, so 0.4.96's rule was inert exactly where it was needed. |
| 0.4.98 | FR-79 V3b — `encode::pipe::Pipe`, ONE belief split into a demonstrated FLOOR and a pushed-back CAPACITY, in SHADOW (`pipe_belief`, `pipe_belief_n` in the heartbeat; no consumer). | Three symptoms on three hosts and two carriers, one defect: no trustworthy, always-available estimate of what the path carries. |
| 0.4.99 | FR-79 V3b-1 + V3b-2 — no single `believed_bps()` (`capacity_bps` for "am I over?", `ceiling_anchor_bps` for "what may the ceiling be?"); the floor is retired by evidence, not by time. | An idle relay session read 1.8 M from deliveries alone with nothing wrong; a host that had demonstrated 8.4 M read 12.8 kbps on a still screen hours later. |
| 0.4.100 | FR-79 V3b-3 — a capacity sample is also a delivery, so it raises the floor and survives its own expiry. | A 13.16 M capacity expired after one TTL and the anchor fell to a 4.05 M content floor while the target ran to the 34.56 M constant. |
| 0.4.188 | FR-63 B1 — the ONE delay-based controller (`encode::ratectl`) runs in SHADOW beside the governor, emitting six `shadow_*` heartbeat fields ([§ The shadow rate controller](#the-shadow-rate-controller-fr-63-b1)). It drives nothing; a containment test holds the line. (Merged after 0.4.187 was cut: 0.4.188, published 2026-10-30, is the first release that carries it.) | Eight estimators of one quantity glued with "the lower of" rules. Shadow-first per the rate-control plan: a release of `shadow_*` logs against the flip criterion (AC2) decides B2's flip, so the controller is never the fleet's own first trial. |
| *next* | FR-79 V5b — only a measurement is evidence about the rate, five rules behind `measured_evidence`: a stall needs a live reading, a delivery takes out a reading it beats by 2×, a hold must be carried, the write-back records a reading or nothing, and a growing queue counts only while the sender offered more than arrived. New heartbeat fields `pipe_measured`, `readings_refuted`, `link_unoffered`. | 2026-11-02, jupiter: an idle session's 9,489 bps became the pipe, 6.2 M → 200 kbps, was walked to 3,322 and written, and each later session wrote its own stand-in back with a fresh TTL: 3,322 → 2,989 → 2,690 with no rise. 2026-10-30, CORPLAP-2: stalls cut 13 of 16 times with a stand-in and 0 of 26 with nothing. |
| *next* | FR-148 P1, step 2 — an entry written before this release is read as assumed. `Entry.provenance` is `measured` on every write. An entry without it seeds nothing, so there is no slow-link profile and no opening below the floor. The first measurement replaces it outright, and the next write drops it. No switch: an older agent reads the file, and its writes read back as assumed ([§ the pair memory](#the-write-back-and-the-seed-v2--v4--the-pair-memory)). | 2026-11-16, CORPLAP-2: the operator's pair was remembered at 281,732 bit/s, grown from a placeholder written before V5b, and opened at 1280×800 and 15 fps. Its sessions could only creep the entry, and none could refute it. V5b stops new placeholders, and this lifts the ones already on disk (FR-148 §2.1). |

## The measured-rate closed loop

The remaining constants (dial percentages, 450 ms budget, relay clamp) are
open-loop: they key off a NOMINAL relay clamp while the variable that matters
is what the session actually delivers. The AIMD only watches send-channel
occupancy and SCTP absorbs the mismatch, so it parks at the ceiling and never
learns the pipe — a field capture shows `target_bps=3000000` constant across a
session delivering 1.75 Mbps.

### How the measurement is taken (v2)

`encode::goodput::GoodputEstimator`, owned by the governor, folded on the
existing 1 s viewer-window tick, reported in the heartbeat as `goodput_bps`
and `goodput_samples=(accepted, rejected)`. **Read it against `target_bps` on
the same line — the gap between them is the open-loop error.**

The hard part is that *a fast sample is not evidence*. Handing a frame to SCTP
is not delivering it: with buffer headroom a frame serialises in microseconds,
which computes to an absurd rate and means only "at least this fast".

⚠️ **The first answer to that did not work, and the way it failed is worth
keeping.** Stage 0 (rc.453) bracketed a **busy period** — an unbroken ≥ 300 ms
stretch where the send task always had another frame waiting — on the reasoning
that a period that long can only end when the pipe drains. It turned out to be
structurally unsatisfiable at the frame rates we run: at 40 fps a ~30 KB frame
drains in ~24 ms, just under the ~25 ms inter-arrival, so the queue dried
*between frames* even while the cumulative deficit grew. No period ever formed,
and field heartbeats read `goodput_samples: (0, N)` for whole sessions — an
estimator that was wired, reported, and structurally incapable of producing a
number.

v2 keeps the philosophy and fixes the granularity. The send task times each
frame's chunked `dc.send()` serialisation; a frame that took at least
`MIN_BLOCKED_SEND` (10 ms) was flow-controlled by SCTP for its whole transit,
so its bytes-over-time **is** the drain rate. Sub-threshold sends are discarded
at the source, so no amount of idle traffic can bias the estimate upward. The
window's accepted samples are aggregated byte-weighted (Σbytes / Σelapsed) and
the window must carry at least `MIN_WINDOW_BLOCKED` (60 ms) of genuinely
blocked time before it counts.

The EWMA is asymmetric — down fast (α 0.5), up slow (α 0.1): a VPN throttling
mid-session is worth believing at once, one lucky burst is not proof the pipe
grew. Confidence decays to `None` after `CONFIDENCE_TTL` (60 s) without a
qualifying sample, so a stale number can never outlive the conditions that
produced it.

### What consumes it today

The measurement is **no longer observe-only**. Both consumers take the same
`MEASURED_CEILING_PCT` (85 %) margin, deliberately: the estimate is "what the
pipe carried", and a bound set exactly AT it leaves the controller nothing to
converge under.

| Consumer | Rule | Guard |
|---|---|---|
| FR-59 P1 — floor relief | the legibility floor descends toward `0.85 × measured`, floored at `slow_link_min_bitrate` | evidence-gated for an UNREMEMBERED pair: with no measurement it is byte-for-byte unchanged. A remembered pair's seed stands in for the measurement (FR-59 P8) — and since FR-70 P1 it **decays** (`prior_bps`) instead of holding the floor AND the P2 queue budget at the memory for the session, which on 2026-09-04 kept a session at 200 kbps for four minutes while the budget it set prevented the very measurement that would have freed it |
| FR-59 P3 — arrival clamp | the ceiling is bounded by what the VIEWER reports arriving while its transit queue grows | constrained paths only; applied AFTER the learner, because a live report outranks past evidence |

⚠️ The viewer reads that growth (`queue_ms`) when a frame is painted, so a decoder
falling behind reports as a growing queue. Since FR-135 R3 the viewer also sends
`queue_arr_ms`, the same sum read when each frame arrives, before the decoder. No
agent reads it yet; FR-63's B2 chooses between the two
([remote-control.md §11.14.2](remote-control.md#11142-the-arrival-reading-queue_arr_ms)).

⚠️ The two are **coupled**, and the coupling is not obvious: `set_ceiling`
raises any ceiling back up to the floor, so the P3 clamp is silently undone
unless the P1 relief lowers the floor with it. That coupling bit FR-63's opener
phase in exactly the same place — 0.4.55 shipped a ceiling cap with no floor
descent and the ramp was inert while looking wired.

⚠️ Measurement may only ever LOWER a clamp: the relay clamp also protects the
TURN path, so a measurement that reads high is not permission to exceed it.

### Where the program went next

The remaining constants are the subject of an approved plan
(`docs/plans/rate-control-architecture.md`) split into three FRs. Read those
for current state rather than this section:

- **FR-62** (#1242) — make an encoder rate change cost neither an IDR nor a
  rebuild, so the nine heuristics that exist only to ration that cost can go.
- **FR-63** (#1243) — replace eight estimators of one quantity with one
  delay-based controller, shadow-first, verified against a deterministic
  simulator (`encode::sim`) rather than against the fleet.
- **FR-64** (#1244) — remote control never rides the overlay.
