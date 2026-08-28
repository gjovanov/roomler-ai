# FR-22: Time-to-first-frame — connecting sometimes takes 10–15 s

Status: **parts 1 + 3 shipped, root cause still open** (2026-08-28). Tracking issue:
`FR-22` (#819).
UX rather than picture quality — but it is the first thing every session is judged on,
and the quality work is invisible to someone still looking at a blank stage.

## Report

Operator, 2026-08-28: *"In some occasions it can take up to over 10 or even 15 secs to
see the remote screen."* Usually much faster.

## Measured — the agent is not the cause

Ten consecutive CORPLAP-3 sessions, from the agent's own log timestamps:

| stage | span |
|---|---|
| session request → first ICE candidate | **127–253 ms** |
| session request → `video-bytes` DC open | **0.95–2.25 s** |
| session request → first pump heartbeat | **2.5–4.7 s** |

Consistent across all ten, no outliers. Capture bind (`DXGI-direct: bound Desktop
Duplication`) and encoder open together account for ~500 ms of that.

**ICE is already trickled correctly**, so the usual suspect is ruled out before we
start: `useRemoteControl.ts` sends the offer immediately after `setLocalDescription`
and streams candidates from `onicecandidate` as they arrive. It never waits for
`iceGatheringState === 'complete'`.

## The cost mechanism

`RC_SIGNALING_TIMEOUT_MS = 15000`.

An attempt that stalls in `requesting` or `negotiating` is not noticed for **15
seconds**. The ladder then retries after 250 ms (`RC_RECONNECT_LADDER_MS[0]`) and the
retry completes on the normal ~3 s path. **≈18 s total**, which matches the report.

So the 10–15 s case is not a slow connection — it is *one silently lost attempt plus a
timer that is three times longer than the path it is guarding*.

## ⚠️ What we have NOT proven

**Why the first attempt stalls.** The 15 s is the cost, not the trigger.

Circumstantial evidence in the same sample: session `…ff8c23` (07:53:00.923Z) reached
the agent, gathered candidates at +160 ms, and then produced no `video-bytes` DC and no
heartbeat; a fresh request arrived 2.4 s later and succeeded. So an attempt CAN die
after the agent has already accepted it.

Candidate causes worth testing, none confirmed:
- a **half-open agent control WS** — the server pushes into a socket that still ACKs
  but whose upstream leg is dead (the documented class that made agents look GREEN
  while `agent_offline`);
- a **pod split** — the RC hub is pod-local, so a browser and an agent hashed to
  different pods during a roll cannot meet;
- a lost `rc:sdp.offer`/`answer` on a WS that reconnected mid-negotiation.

We cannot currently distinguish these, because **there is no browser-side timing at
all**: "sometimes 10–15 s" cannot be turned into a distribution, and a fix cannot be
shown to work.

## Proposed direction — three parts, cheapest first

**1. Phase-aware signalling timeout.** ✅ **SHIPPED** (PR #821). 15 s is far beyond the
measured healthy path (worst observed end-to-end: 4.7 s agent-side). A short bound on
`requesting` — *has the server answered at all?* — with the existing longer bound kept
for `negotiating`, where ICE legitimately varies by network. Cuts the bad case from
~18 s to ~7 s and does not touch the good case. `awaiting_consent` stays exempt: the
SERVER owns that timeout, and a human approving a prompt may legitimately take longer
than any client-side number.

`signalingTimeoutFor(phase)`: `requesting` **4 s**, `negotiating` 15 s (unchanged),
everything else `null` = never arm. Exported and total over `RcPhase`, so a new phase
must declare its own bound instead of silently inheriting the ICE-sized one.

⚠️ **Mitigation, not diagnosis.** This shortens the cost of the stall; it does not
explain it. Criterion 5 stays open on purpose.

**2. Make the server answer instead of the client guessing.** ✅ **ALREADY IMPLEMENTED**
— checked against the tree rather than assumed, and deliberately NOT rebuilt (PR #821).
`Hub::create_session` returns `AgentOffline` when the request cannot reach a live agent;
`ws/remote_control.rs` runs a 250 ms cross-pod rehome probe and then sends
`ServerMsg::Error` with an attributable code (`agent_offline` / `agent_on_other_pod`);
the client surfaces it via `rcErrorMessage` and advances the ladder via
`isRetryableRcErrorCode`. So the undeliverable case already fails in well under a second.

⚠️ **This is the wrong-turn worth recording**: the proposal was written from the
assumption that the fast-fail was missing, and it is not. What the server genuinely
CANNOT see is the case actually observed — session `…ff8c23` **reached** the agent,
gathered candidates at +160 ms, and then went silent. A live agent that stops answering
is not detectable by another server-side check; it needs part 3.

**3. Instrument time-to-first-frame.** ✅ **SHIPPED** (PR #821). Request → first painted
frame, reported the way FR-1 P7 instrumented paint age. Without it every future claim
here is an anecdote. Feeds FR-16 (#798) L3.

`ui/src/composables/rcConnectTiming.ts` records eight marks — `request_sent`,
`session_created`, `ready`, `offer_sent`, `answer`, `pc_connected`, `dc_open`,
`first_frame` — and logs per-STEP deltas.

⚠️ **Marks are per ATTEMPT, not per connect.** A recorder shared across the ladder would
let a fast retry overwrite the lost attempt's marks and report an 18 s connect as a 3 s
one — hiding the exact defect being hunted.
⚠️ **An unreached step prints as `<name>:—`, not omitted**, and the abandoned/cancelled
paths log too. A MISSING mark is the finding: it names the step that never completed,
which is what separates a half-open agent WS from a cross-pod split from a lost SDP
frame. The three fail in different phases; a single total cannot tell them apart.
⚠️ Deltas rather than absolute offsets — the actionable quantity is *which wait was
long*, and with offsets every step after a 9 s stall looks equally late.

**3b. Say it to the OPERATOR, not the console** (PR #822). The console line is invisible
during exactly the sessions this exists to explain — nobody has devtools open when a
connect takes 15 s, so the report that comes back is still *"it was slow again"*.
`describeConnectTiming()` turns the marks into one sentence in the existing app
snackbar, naming which wait dominated in plain words, with the short mark name in the
tail so a reported snackbar is traceable without devtools.

⚠️ **Consent is never named as "what was slow".** `ready` is human-paced BY DESIGN — the
server owns its timeout for that exact reason — so it is excluded from the verdict while
still advancing the clock for the steps after it. Reporting "most of the wait was
someone approving the prompt" is true, useless, and points the operator at themselves
instead of at the slow step. Locked by a test, because getting this wrong yields a
CONFIDENTLY WRONG message rather than a missing one.
⚠️ **A normal connect says nothing** (`CONNECT_SLOW_MS` 7 s — above the measured healthy
band, below the reported 10–15 s). A message on every success is noise, and a threshold
inside the healthy band would train people to dismiss it.
⚠️ **A retry is always notable even when the total looks fine** — that is the FR-22
signature, and the operator waited through it either way.
⚠️ **Stall warnings throttle (20 s), resolutions never.** A flapping path would otherwise
bury its own message; but showing "it is failing" and suppressing "it finally connected"
leaves a warning with no ending.

## Acceptance criteria

- [x] Instrumented TTFF exists and a normal connect reports it. **Built** (#821);
      the p50 number itself is a FIELD reading and lands in the log below once a build
      carrying this has run — the criterion is not "we assumed 3–5 s".
- [x] An attempt that cannot reach the agent fails in **< 2 s** with an attributable
      reason, rather than after the signalling timeout. **Already true** — see part 2:
      `AgentOffline` → 250 ms cross-pod probe → `rc:error` with a code the UI shows.
      Verified by reading the path, not by rebuilding it.
- [x] A deliberately stalled attempt recovers in **< 8 s** end-to-end, down from ~18 s.
      Arithmetic locked by a unit test: 4 s bound + 250 ms ladder + ~3 s normal connect.
      ⚠️ Holds for a stall in `requesting`; a stall in `negotiating` still costs 15 s by
      design, because that bound is guarding ICE.
- [ ] The healthy path is not slowed: p50 TTFF unchanged within noise. Needs the field
      reading above; nothing healthy was near either bound, so the expectation is no
      change — but that is a prediction, not a result.
- [ ] The stall's ROOT CAUSE is identified from the new instrumentation and recorded
      here — shortening the timeout is mitigation, not a diagnosis. **Still open**, and
      the reason this FR does not close on the two shipped parts.

## Out of scope

- Picture quality after the first frame — that is FR-1 and its children.
- The base connect latency imposed by a relayed carrier (FR-9 / FR-18 territory).

## Field log

| date | build | result |
|---|---|---|
| 2026-08-28 | 0.4.12 | Investigated. Agent-side spans measured across 10 sessions (above); ICE trickle ruled out; 15 s signalling timeout identified as the cost mechanism. Trigger not yet proven. |
| 2026-08-28 | — | Parts 1 + 3 merged (#821): phase-aware bound (`requesting` 4 s) and eight-mark connect timing. Part 2 measured against the tree and found ALREADY PRESENT — recorded rather than rebuilt. **No field reading yet**; the p50 and the root cause both need a deployed build, so nothing here is a result. |
| 2026-08-28 | `v20260828-afeb977584f0` | Parts 1 + 3 DEPLOYED. Verified in the SERVED bundle, not just the rollout: `/assets/RemoteControl-*.js` carries the markers. Awaiting a field connect. |
| 2026-08-28 | — | 3b merged (#822): the verdict reaches the operator through the snackbar. Console-only reporting could not produce a root cause, because the console is closed during the sessions that stall. |
