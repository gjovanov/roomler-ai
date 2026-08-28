# FR-22: Time-to-first-frame — connecting sometimes takes 10–15 s

Status: **investigated, fix proposed** (2026-08-28). Tracking issue: `FR-22` (#819).
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

**1. Phase-aware signalling timeout.** 15 s is far beyond the measured healthy path
(worst observed end-to-end: 4.7 s agent-side). A short bound on `requesting` — *has the
agent answered at all?* — with the existing longer bound kept for `negotiating`, where
ICE legitimately varies by network. Cuts the bad case from ~18 s to ~7 s and does not
touch the good case. `awaiting_consent` stays exempt: the SERVER owns that timeout, and
a human approving a prompt may legitimately take longer than any client-side number.

**2. Make the server answer instead of the client guessing.** The hub already knows
whether a session request reached a live agent — that is exactly how the `agent_offline`
412 works on the exec and SSH paths. An undeliverable request should fail in under a
second with a reason the UI can show, rather than being discovered by a client-side
timer 15 s later. This is the part that removes the class rather than shortening it.

**3. Instrument time-to-first-frame.** Request → first painted frame, reported the way
FR-1 P7 instrumented paint age. Without it every future claim here is an anecdote, and
part 2's fast-fail cannot be told apart from a fast success. Feeds FR-16 (#798) L3.

## Acceptance criteria

- [ ] Instrumented TTFF exists and a normal connect reports it (p50 ≈ the measured
      3–5 s, not a number we assumed).
- [ ] An attempt that cannot reach the agent fails in **< 2 s** with an attributable
      reason, rather than after the signalling timeout.
- [ ] A deliberately stalled attempt (agent WS killed mid-request) recovers in
      **< 8 s** end-to-end, down from ~18 s.
- [ ] The healthy path is not slowed: p50 TTFF unchanged within noise.
- [ ] The stall's ROOT CAUSE is identified from the new instrumentation and recorded
      here — shortening the timeout is mitigation, not a diagnosis.

## Out of scope

- Picture quality after the first frame — that is FR-1 and its children.
- The base connect latency imposed by a relayed carrier (FR-9 / FR-18 territory).

## Field log

| date | build | result |
|---|---|---|
| 2026-08-28 | 0.4.12 | Investigated. Agent-side spans measured across 10 sessions (above); ICE trickle ruled out; 15 s signalling timeout identified as the cost mechanism. Trigger not yet proven. |
