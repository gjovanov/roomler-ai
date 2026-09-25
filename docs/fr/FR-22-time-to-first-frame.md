# FR-22: Time-to-first-frame — connecting sometimes takes 10–15 s

Status: **parts 1 + 3 + 3b shipped; field-verified 2026-09-25 (prod `0.4.101`) — the healthy
signalling band shows no slowdown attributable to #821 (AC4 `[~]`) and the stall is localized to the
negotiating phase on flap-prone control-WS hosts (AC5 `[~]`); both criteria stay open pending
a persisted-marks trace, so the FR does not close.** Tracking issue: `FR-22` (#819).
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
- [~] The healthy path is not slowed: p50 TTFF unchanged within noise. **Signalling half
      met; paint-inclusive half unproven.** Field-verified 2026-09-25 on prod `0.4.101`
      (§ Field-verification): the server-observed signalling band — consent EXCLUDED,
      `consent_prompted`→`session_started`, mined from `remote_audit` (90 d) — shows **no
      slowdown attributable to #821**. The numbers are not "unchanged": p50 **116 ms → 143 ms**,
      p90 216 → 293 (n 6503 before / 1809 after), a shift that sample sizes this large do not
      put down to chance. But the aggregate is confounded by fleet composition (corporate and
      WSL hosts carry the later tail), the last 7 d read p50 118 (n 49), and #821 only arms a
      *client-side* timeout that never fires on a healthy connect, so it has no mechanism to
      slow one. A per-host before/after would settle it. ⚠️ MISSING: a **paint-inclusive** TTFF
      (through `first_frame`) p50 baseline. TTFF is browser-only and **not persisted** —
      `agent_logs` holds **zero** browser rows because the `/api/log/browser` route was
      never wired — so no pre-#821 TTFF distribution exists and none can be reconstructed;
      a current self-measurement was blocked by the automation tab being a hidden background
      tab (no `requestAnimationFrame` ⇒ viewer-paint timing invalid, standing rule).
- [~] The stall's ROOT CAUSE is identified from the new instrumentation and recorded here.
      **Phase and leading mechanism identified from recorded audit; a single-attempt
      end-to-end log trace is still owed.** Field-verified 2026-09-25 on prod `0.4.101`
      (§ Field-verification): the server-visible stall is the **negotiating** band
      (offer→answer, i.e. `consent_granted`→`session_started`), consent EXCLUDED (consent
      was tiny on every outlier), **34 of 8312 started sessions ≥ 3 s (0.41 %)**, ~17 ≥ 8 s,
      clustered on the flap-prone control-WS hosts (CORPLAP-1/2/3, NEO16, NEO16-WSL) while
      direct/LAN hosts stay p99 < 630 ms — which points at a **half-open / flapping agent
      control WS delaying offer delivery**, NOT the media carrier (which sits AFTER the
      answer) and NOT consent. ⚠️ MISSING: (a) no single stall could be traced through BOTH
      ends' logs — every recorded outlier predates the pod-log (~21 h) and `agent_logs`
      (7 d) windows while `remote_audit` persists 90 d, and the browser marks that name the
      phase per-attempt are not persisted at all; (b) the **requesting**-phase stall is
      unquantifiable from records (a request that never reaches a hub leaves no row).
      Closing it needs the marks persisted (wire `/api/log/browser`) or a live catch —
      proposed as **part 4** below. Remains open; not `[x]`.

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
| 2026-09-25 | prod `0.4.101` | **AC4 field read.** Server-observed signalling band (consent excluded) shows no slowdown attributable to #821, though the aggregate moved: p50 116 → 143 ms, p90 216 → 293 (n 6503/1809). Last 30 d p50 142, last 7 d p50 118 / p99 204 / max 252 (n 49) — no stalls in the recent window at all. AC4 → `[~]`: signalling half met; paint-inclusive TTFF has no recorded baseline (browser marks unpersisted) and could not be self-measured (hidden automation tab). |
| 2026-09-25 | prod `0.4.101` | **AC5 field read.** The server-visible stall is the **negotiating** band; 34/8312 started sessions ≥ 3 s (0.41 %), ~17 ≥ 8 s, clustered on flap-prone control-WS hosts, consent excluded. Leading mechanism: a half-open agent control WS delaying offer delivery — not the carrier, not consent. AC5 → `[~]`: phase + mechanism localized; single-attempt end-to-end trace and requesting-phase quantification still owed (explanatory logs aged out; marks unpersisted). |

## Field-verification — 2026-09-25 (prod `0.4.101`)

Run by the FR-backlog worker from the dev box: `remote_audit` / `remote_sessions` /
`agent_logs` mined via the prod MongoDB (through the mars deploy session), the live carrier
roster read from the local daemon (`roomler peers`). **No fix was under test** — parts 1/3/3b
already shipped — so this is a *measurement + root-cause* pass, not a fix A/B. The AC4 "before"
is the genuine pre-#821 distribution from the 90 d audit window (#821 merged
2026-08-28 10:46 Z; the split is at 12:00 Z that day).

### What the server can and cannot see

The eight browser marks (#821) and where each one is observable:

```mermaid
flowchart LR
  ws[ws_ready] --> turn[turn_ready] --> probes[probes_ready] --> req[request_sent]
  req -->|"REQUESTING — 4 s bound #821"| sc[session_created]
  sc -->|"CONSENT — human-paced, EXCLUDED"| rdy[ready]
  rdy --> off[offer_sent]
  off -->|"NEGOTIATING — 15 s bound — THE STALL"| ans[answer]
  ans --> pc[pc_connected] --> dc[dc_open] --> ff["first_frame = TTFF"]
  classDef srv fill:#dbeafe,stroke:#2563eb,color:#1e3a8a;
  classDef br fill:#f3f4f6,stroke:#9ca3af,color:#374151;
  class sc,rdy,ans srv;
  class ws,turn,probes,req,off,pc,dc,ff br;
```

Only three marks have a server record, via `remote_audit` (`event.kind`, 90 d TTL):
`session_created` ≈ `consent_prompted`, `ready` ≈ `consent_granted`, `answer` ≈
`session_started` (stamped when the hub forwards the SDP answer — `crates/modules/fleet/src/hub.rs:1242`,
`crates/remote_control/src/audit.rs:152`). Everything blue is server-visible; everything grey
is **browser-only and unpersisted**:

- ⚠️ **The `/api/log/browser` ingest route exists but is unwired** — no UI uploader ships to
  it (`crates/modules/fleet/src/agent_log.rs:160`, and the route's own `// punted to rc.59`
  comment), and `agent_logs` holds **0** browser-source rows. So the TTFF number and the
  `stalled waiting for <mark>` verdict live only in the live browser (console + snackbar)
  and are gone the moment the tab closes.
- ⚠️ The **carrier** phases (`pc_connected`, `dc_open`, `first_frame`) sit *after* the
  server's `session_started` and dominate a relayed connect (the 2026-08-29 read on a
  relayed pair: `answer` +214 ms but `pc_connected` +1.2 s, `dc_open` +0.3–0.8 s,
  `first_frame` +0.4 s of a 2.7 s TTFF). The server is blind to all of it.

### AC4 — no slowdown attributable to #821 in the healthy signalling band

Negotiate band = consent EXCLUDED (`consent_granted`→`session_started`), the only part of
the wait #821 shares a code path with. Consent (`consent_prompted`→`consent_granted`) is
reported separately and excluded, exactly as the snackbar excludes it.

| window | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| **before #821** (< 08-28 12:00 Z) | 6503 | **116 ms** | 216 | 439 | 17 604 |
| **after #821** | 1809 | **143 ms** | 293 | 2 777 | 21 625 |
| last 30 d | 2141 | 142 ms | 283 | 2 777 | 21 625 |
| last 7 d | 49 | 118 ms | 177 | 204 | 252 |
| consent band, before | 6503 | 43 ms | 84 | 12 846 | 29 276 |
| consent band, after | 1809 | 46 ms | 215 | 25 232 | 202 386 |

p50/p90 are **not** flat across the boundary: 116 → 143 / 216 → 293, and with n 6503 and 1809
that shift is real, not chance. What it is not is evidence against #821. The last 7 d (n 49)
read 118 / 177, back at the "before" level, so the aggregate moves with which hosts happened
to connect in a window. A per-host before/after, not this aggregate, is what would show a
change in the healthy path. The p99 rose (439 → 2 777) but that is the negotiating stalls #821 deliberately did **not**
touch (AC3's caveat) plus sample composition, not a slowdown of the healthy path — and #821
only arms a client-side `setTimeout` that never fires on a healthy connect. The consent band's
huge p99/max (up to 202 s) is a human taking their time on a Prompt-mode device — correctly
excluded from the connect verdict.

Per host (negotiate band, last 30 d) — the p50/p90 are small **regardless of carrier**
(the offer→answer exchange rides the signalling WS, not the media path); the multi-second
tails land only on the flap-prone hosts:

| host (display name) | current carrier | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| CORPLAP-3 | relay (derp/tcp) | 482 | 157 | 287 | **6 065** | **21 625** |
| CORPLAP-2 | relay (Check Point) | 338 | 217 | 325 | 1 101 | 9 138 |
| CORPLAP-1 | direct (was VPN) | 315 | 202 | 409 | **5 351** | **18 784** |
| NEO16-WSL | relay (derp/tcp) | 56 | 171 | **1 191** | **8 215** | 12 667 |
| NEO16 | direct | 107 | 118 | 201 | 1 407 | 3 951 |
| MacBook-1 | direct | 190 | 108 | 160 | 305 | 627 |
| Apple-Asahi | direct | 116 | 79 | 112 | 187 | 276 |
| mars / zeus / jupiter | direct / relay | 9 / 5 / 3 | 66 / 59 / 56 | — | — | 105 / 60 / 67 |

⚠️ **What AC4 still lacks is a paint-inclusive TTFF baseline.** Browser timing did not exist
before #821 and is not persisted after it, so a pre/post TTFF-through-`first_frame`
distribution cannot be built from records; and the current self-measurement was blocked —
the Claude-in-Chrome automation tab was a **hidden background tab** (`document.hidden = true`,
`hasFocus() = false`), where `requestAnimationFrame` does not run and viewer-paint timing is
invalid (the 2026-09-07 standing rule). Hence AC4 is `[~]`, not `[x]`.

### AC5 — the stall is in the negotiating phase, on flap-prone control-WS hosts

Decomposing every started session (n 8312) into consent vs negotiate bands, then taking the
consent-excluded outliers:

- **34 sessions (0.41 %) have a negotiate band ≥ 3 s; ~17 (0.2 %) ≥ 8 s; range 6–21 s.**
  Consent on those outliers was tiny (26–800 ms) — the delay is **not** someone approving a
  prompt.
- They cluster on **CORPLAP-1, CORPLAP-2, CORPLAP-3, NEO16, NEO16-WSL** (and one
  home device) — every one a host whose control WS runs through a TLS-inspecting corporate
  middlebox or a WSL NAT that is documented to keep half-open sockets ACKing after the
  upstream leg dies. Direct/LAN hosts (MacBook-1, Apple-Asahi, mars/zeus/jupiter, the vmtest
  VMs) stay p99 < 630 ms.
- A dense **2026-09-02** burst spans several agents at once (a signature more consistent with
  a server-side event — a pod roll / cross-pod split — than an independent per-host fault;
  though 09-02 was also a heavy self-driven RC-testing day, so some is reconnect churn).

**Reading:** the dominant server-observable FR-22 stall is in the **negotiating** phase —
the agent is slow to *receive the offer or return the answer*. Because this sits **before**
`pc_connected`, it is **not** the media carrier (a relayed carrier only costs time after the
answer, and the carrier does not touch the signalling WS the offer travels on). Consent is
excluded. The remaining explanation that fits the host clustering is a **half-open / flapping
agent control WS**: the hub pushes the offer into a socket that still ACKs but whose upstream
leg is dead, and it is not delivered until the agent's receive-liveness deadline forces a
reconnect (the same class as the "GREEN but `agent_offline`" note in `CLAUDE.md`; agents
≥ rc.293 self-heal in ≤ ~2 min). That is a **candidate localized by phase + host**, not a
mechanism observed end-to-end — see the gap below.

⚠️ **Why this is `[~]` and not `[x]`:**

1. **No single stall could be traced through both ends' logs.** The `remote_audit` rows live
   90 d, but the pod log is ~21 h (a pod restart 6 h ago; `Exit Code 137` = OOM/limit) and
   `agent_logs` (agent source) is 7 d (oldest 2026-09-18). Every recorded outlier is older
   than 7 d — and there are **zero** negotiate stalls in the last 7 d — so the logs that would
   name the mechanism have aged out. A stall's phase is in the browser marks, which are not
   persisted at all. To trace one you must first make the evidence outlive the event.
2. **The requesting-phase stall is invisible to records.** A request that never reaches a hub
   (lost frame, dead WS, cross-pod before the row is written) creates no `remote_audit` row,
   so the requesting-vs-negotiating split the spec describes can only be quantified from the
   browser marks — again, unpersisted.

### Proposed part 4 (not built this run) — persist the connect marks

Wire the RC viewer to POST each completed/abandoned attempt's marks to the existing
`/api/log/browser` route, tagged with `session_id`, so a stall's phase (the missing mark) is
mineable server-side **within the same retention window as the pod/agent logs**. This is the
one change that lets AC5 close on recorded evidence instead of a rare live catch — the marks
already exist (`ui/src/composables/rcConnectTiming.ts`); only the uploader is missing. It is
a UI change whose payoff is a *future* stall, so it is not field-verifiable inside one run and
is left for the operator to schedule. A live catch is the alternative, but at 0.2 % and zero
in the last 7 d it needs hundreds of connects and a foreground browser tab.
