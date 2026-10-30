# FR-35: The constrained ceiling learns the pair — grow the relay cap on delivery evidence, remember it per peer

Status: **P1 + P2 + P2b shipped (0.4.21 / 0.4.23); P3 (opener growth, opener grace, held NVENC increases) + P3b (grace-until-drained, measured growth, stable==0 write-back) + P3c (stuck detector so a hot seed cannot freeze the opener, + applied_bps self-correction) shipped 0.4.27 → 0.4.30 (#986/#988/#996/#1001/#1004). Field: IDR pulses gone; opener sharp from frame 1 with a healthy seed; no spurious freeze on a fast path (maxGap <130 ms at the 8 M ceiling). REMAINING: the stuck detector needs a path that cannot carry the seed to exercise — handed to the operator as a neo16→overlay A/B (100.65.4.2 reseeded to the 7 M froze-value).** (2026-08-30). Tracking issue: `FR-35` (#922).

**Re-read on the current code (2026-10-27/28 sessions on agent 0.4.177, 0.4.178 and 0.4.181;
the FR-35 surface — `ceiling_learn.rs`, `rate_memory.rs`, `governor.rs`, `goodput.rs`,
`encode/mod.rs` — is byte-identical from `agent-v0.4.177` through `agent-v0.4.185` and master; verified
2026-10-29).** Code-complete; the learner + memory surface was reworked since by FR-59 P3/P6/P8,
FR-70 P1 and FR-79 V2/V4 (carrier-keyed memory, measured-not-max write-back) — see
`docs/rate-control.md` § The ceiling learner / § The write-back and the seed. Field (read-only
`roomler exec` of CORPLAP-2's own daemon log and `rate_memory.json`, seven sessions on
`100.65.4.2|relay:derp/tcp`): **P1 + P2 are live** — every session opened at 0.85 × the remembered rate
(4.40–4.76 M ⇒ opener maxrate 3.74–4.05 M vs the 2.55 M cold), the learner stepped on windows that
carried ≥ 70 % of the ceiling (to 5.15 M in the one 79 s session with motion; one to three times in
the 11–19 s sessions; never on the idle one) and the ceiling never passed 5.15 M under `hi` 8 M;
`send_stalls=0` in every heartbeat of the four sessions read in full. **Three corrections to the first
(unverified) read of these sessions:** (1) none of them ran 0.4.179; (2) the CARRIED gate counts bytes
*written* to the data channel and does not read the FR-79
verdict, so one step per session can land inside the opener burst (10-28 07:57:43, 0.36 s before the
grace ended, after a 164 ms blocked send); (3) the pair is not "settling at a sustainable ~5 M" — the
memory sits at 4.4–4.8 M because an **idle** session writes its own seed back as evidence
(`stable_bps()` is a target hold, not a measurement) and V4's `damp` takes half of that gap (−7.5 %
per idle session), while a proving session moves it a tenth of its gap (+0.2 %). The 09:23 session
carried 6–7.6 Mbps windows with sub-ms send waits. **Open for close:** AC1 (the write-back question
above, then a driven measurement), AC2/AC6 (a relay-locked pair — none reachable from neo16 on
2026-10-29, CORPLAP-1/2/3 all ride `direct` — plus a kill-switch daemon restart for AC6), AC3 (a
60 s drag), AC4 (a ~2 Mbps relay-locked host on the current code).
Child of the RC-quality program. Follows FR-31 (every opening and repair number on an NVENC relay
session is proportional to `maxrate`, and nothing ffmpeg exposes changes that) and the parked
measured-rate line (#678: a sender cannot see capacity it is not using). Operator's directive
2026-08-29: the relay ceiling first; a vendored-ffmpeg patch only if this fails.

## What is in force today

- `relay_max_bps()` = **3 Mbps** (`agents/roomlerd/src/encode/mod.rs:489`, env `RELAY_MAX_KBPS`),
  applied as the encoder's `maxrate` cap on every constrained session
  (`encoder.rs` ↔ `rate_profile::ffmpeg_maxrate_bps_scaled`, `peer.rs:3133`), opened at 85 % =
  **2.55 Mbps**, and as the AIMD's ceiling (`governor.rs:264-283`); the send-queue budget derives
  from the same constant (`peer.rs:4652`).
- The AIMD (`encode/aimd.rs`) climbs additively (+ceiling/16 ≥ 150 kbps per 5 s of quiet) but
  **never above that ceiling**; it decreases ×0.85 on a full send channel, on a send-wait stall
  (#818) and on viewer-age excess (FR-15 P2). The governor's measured-ceiling clamp (stage 1) is
  **direct-only** and only ever lowers.

## The measurement (P0, 2026-08-29, the REAL pair `neo16 → CORPLAP-2`, DERP relay)

`ROOMLERD_FFMPEG_MAXRATE_KBPS=25500` on CORPLAP-2 (machine env + scheduled restart), viewer on neo16
with the FR-31 harness, one 20 s synthetic drag; CORPLAP-2's own heartbeats read afterwards.

| what | today (cap 2.55 Mbps) | cap 25.5 Mbps |
|---|---|---|
| opening | 5.6 KB max-QP keyframe, desktop repaired by inter frames over ~1.2 s | 83 B keyframe (black first capture) + **the whole desktop as one 680 KB frame at +0.3 s**, 1.06 MB in the first three frames |
| delivered rate during the drag | ≤ 2.55 Mbps | **3.3–10.6 Mbps per second**, no inter-message gap > 136 ms |
| agent `goodput` (stage-0 estimator, which never fired at 2.55 Mbps) | `None` | **3.6 → 5.8 → 7.5 → 10.9 Mbps** as the AIMD climbed 9.6 → 11.2 → 12.8 Mbps |
| `send_wait` | ~0.05 ms | ~0.05 ms up to ~11 Mbps; **one 7.9 s stall at 12.8 Mbps** (viewer age 6 986 ms, +556 backpressure skips) → AIMD cut to 4.1 Mbps; a second session opened at 18.8 Mbps and stalled 310 ms at once |

Reading: this path **sustains ~6–9 Mbps**, absorbs short bursts far above that, and chokes above
~12 Mbps. The nominal 3 Mbps is 2–3× too low for it; the 10× value that makes the opening keyframe
crisp is not sustainable. Other corp relays in the same fleet measured ~2 Mbps (CORPLAP-1, CORPLAP-3,
2026-08). **A constant cannot be right for both — the cap must be learned per pair, and it may only
ever climb during a session** (FR-31: any NVENC rate change *down* forces a starved keyframe that
replaces the picture).

## Design

**P1 — growth above the nominal, gated by the signals that predicted the stall.** The constrained
AIMD ceiling becomes `clamp(nominal, hi)` with `hi = relay_max_hi_kbps` (default **8 000**;
`0` = nominal only = today). Above the nominal the additive increase additionally requires, over
the trailing 10 s: no send-wait stall (`send_stalls` unchanged), `send_wait_max` < 20 ms, and —
when the viewer reports age — age ≤ 1.5 × the learned floor. Steps stay +ceiling/16 (≥ 150 kbps)
per 5 s settle. Decreases are unchanged, plus one new rule from the measurement: a send wait
> 1 s is a **hard stall** and cuts to ×0.5 rather than ×0.85 (the 7.9 s stall was answered by
three ×0.85 steps over 4 s). All of it lives in the pure controller/governor (`aimd.rs`,
`governor.rs`, `rate_profile.rs`) — unit-testable on this box; no wire change.

**P2 — rate memory per peer.** The highest target that held ≥ 10 s with no decrease is the
session's *stable rate*. On session end the agent persists `{peer overlay ip → stable_bps, at}`
(`rate_memory.toml` in the data dir; entries expire after 7 days), and the next session on that
pair **opens at 0.85 × stable, clamped `[nominal, hi]`** — so the opening keyframe and the repair
speed scale with what the pair proved, not with a fleet constant. A pair never seen opens at the
nominal (today). Nothing here ever lowers the nominal.

Kill switches: `relay_ceiling_learn` (tribool, default on; off = today's fixed ceiling and no
memory) and `relay_max_hi_kbps` (config-surface + env). Direct sessions are untouched.

**Not done, deliberately:** raising the nominal 3 Mbps (the ~2 Mbps relays are real); an active
startup bandwidth probe (measures burst capacity, which this measurement shows is *not* the
sustainable rate — 18 Mbps instantaneous vs ~8 sustained — and costs bytes on exactly the thin
links); the goodput estimator as the learning input (its down-fast EWMA crashed on relay
lumpiness, governor.rs:256-262 — the AIMD's own stable target is the robust signal).

## Acceptance criteria

Measured with the FR-31 harness on `neo16 → CORPLAP-2`, N ≥ 5 sessions after ≥ 1 learning session,
shipped defaults:

- [ ] opening keyframe ≥ 2.5× today's bytes (≥ 14 KB at 1920×1200, today 5.6 KB) and first-light
      sharpness ≥ 70 % of the 8 s steady state (today 47 %)
- [ ] time-to-crisp ≤ 0.6 s (today 1.2 s) — needs a relay-locked pair and a driven instrumented
      viewer session (a cold-vs-seeded A/B); not measurable from the agent-side heartbeat. No pair
      from neo16 relayed on 2026-10-29 (CORPLAP-1/2/3 all `direct`)
- [~] a 60 s drag: no send wait > 1 s, viewer-age p99 not worse than the FR-18 numbers, and the
      ceiling never above `hi` — **the ceiling half is met**: never above 5,146,238 across seven
      sessions (10-27/28, CORPLAP-2 `relay:derp/tcp`, 0.4.177–0.4.181) and bounded by construction
      (`ceiling_learn.rs:237`, `.min(hi)`); **the drag itself was never driven**, so the send-wait
      and viewer-age halves are unexercised — what was seen without a drag (four sessions read in
      full): `send_stalls=0`, `send_wait_max_ms` ≤ 163.9 (the opener), paint age p50 66–68 /
      p95 103–117 / max 775 ms. The FR-18 numbers to beat under drag (0.4.10, CORPLAP-3 relay):
      `send_wait` p99 200 ms / max 1 778 ms, viewer age max 2 185 ms — the undriven sessions sit
      under them, but that is not the same test
- [ ] a ~2 Mbps relay (CORPLAP-3, `av1_qsv`): `target_bps` never exceeds the nominal unless every growth
      gate passed — read from its heartbeats; the session must not get worse than today
      (0.4.21, 2026-08-29: no learner steps, `learned_ceiling_bps=0`, target ≤ 3.0 Mbps, 65–78 ms).
      The 0.4.21 control predates the FR-59/70/79 rework and CORPLAP-3 rides `direct` on 2026-10-29;
      a current-code control also has to read the CARRIED gate against the opener burst (see the
      2026-10-28 field row: one step inside the grace)
- [x] the learned rate survives a daemon restart and expires after 7 days (unit-tested; the file is
      `…\config\systemprofile\AppData\Roaming\roomler\roomler\data\rate_memory.json` for the SYSTEM
      service). **Restart survival field-read 2026-10-29** on CORPLAP-2: the seed chain 4,750,677 →
      4,759,961 → 4,402,963 → 4,411,567 was written at one session's end and read back at the next
      across the 13:41Z (0.4.177 → 0.4.178), 18:01Z, 22:56Z (10-27) and 06:56Z (10-28, → 0.4.181)
      service restarts; the file at that path holds carrier-keyed entries with `at_unix`. The 7-day
      expiry is the unit test the criterion names (`rate_memory.rs`:
      `round_trips_through_the_file_and_seeds_fresh_entries_only`, `record_replaces_and_prunes`); not
      observed in the field
- [ ] `relay_ceiling_learn=false` restores today's numbers byte-for-byte — needs a kill-switch A/B that
      restarts a relay-locked host's daemon (config `relay_ceiling_learn` applies on restart): an
      operator-scheduled run on a host nobody is using
- [x] **Docs** — the ceiling learner (P1) and the per-peer memory (P2) are documented in
      `docs/rate-control.md` (§ The ceiling learner / § The write-back and the seed) with a mermaid of
      the learner's per-window decision, a gate/anchor table, what CARRIED and DECREASE actually
      measure, the config rows and the 2026-10-27/28 field rows, and the page's `docs/README.md` row
      names the section (#3013; operator rule, 2026-09-05: docs before close)

## Open decisions

- **Growth speed — DECIDED 2026-08-30 (P3).** Sessions in the field last seconds and are mostly idle, so
  drag-evidence growth never reaches the crisp opener; and the FR-31 verdict forbids a boost-then-step-down
  (the step-down IDR replaces the crisp picture). P3 therefore grows the memory from evidence every session
  has for free: **the opening burst is a burst probe** — `bytes` sent and `max_send_wait` seen while the
  opener drains — and a session that saw no decrease records `max(stable, growth_target)`
  (`rate_memory::opener_growth_target_bps` + `record_session`). **P3b (#996)** shaped the target from the
  0.4.28 field runs: a burst that **queued** (`max_send_wait ≥ 100 ms`) measured the pipe, target =
  75 % × `bytes / max_send_wait` (a 451 KB opener that waited 802 ms says 4.5 Mbps ⇒ 3.38 M); a burst that
  **never queued** (a 238 KB opener with a 0 ms wait — it fit SCTP's send buffer) proves only "not slower
  than this", so the target is a bounded **×1.5 step on the opener's own maxrate** (the next, larger opener
  then queues and measures) — the first implementation read it as 95 Mbps and wrote the memory straight
  to `hi`. Both are capped at `hi`; a 2 Mbps pipe stays at the nominal; a decrease still lowers. The grace
  holds until the send ledger is EMPTY (≥ 2 s, ≤ 6 s): a fixed 2 s ended while a 451 KB tail was still
  draining and the AIMD took the decrease the grace exists to prevent.
  Two companions, both found on the seeded field run: an **opener grace** (soft send-wait stalls and
  backpressure skips inside the first 2 s are the opener draining, not congestion — they had cut a 5.95 Mbps
  session ×0.85 within 1.8 s) and **held NVENC increases on a constrained session** (an in-place
  reconfigure is a starved IDR; the AIMD's climb back produced one visible pulse per 5-s step — increases
  now flush through the existing spaced quiet arm, coalesced; decreases land at once and anchor the spacing).

- `hi` default: 8 000 kbps from this one pair; the second corp pair (CORPLAP-3) will say whether the
  gates keep it at the nominal there. Revisit with two pairs of data.
- Whether the learned rate should also seed the queue budget (`constrained_queue_budget_bytes`
  is derived from the nominal today). Probably yes, in P2.

## Out of scope

The carrier (FR-33 surfaces the capture); the ffmpeg-side keyframe budget (FR-31, refuted
levers); the direct path's measured clamp.

## Field log

| date | build | note |
|---|---|---|
| 2026-08-29 | agent 0.4.18 (CORPLAP-2), env `FFMPEG_MAXRATE_KBPS=25500`, web `v20260829-40e8fc071129` | P0: the table above. Two daemon restarts on CORPLAP-2 (apply, restore) via a scheduled task; env cleared afterwards. |
| 2026-08-29 | agent 0.4.21 (CORPLAP-2 `av1_nvenc`, CORPLAP-3 `av1_qsv`), web `v20260829-0da90b766dc0` | **First field run.** P1 steps logged and gated on CORPLAP-2: `3.0 → 3.19 → 3.39 Mbps` (marquee), `3.0 → 3.19 → 3.39 → 3.60` (window drag, carried 2.1–2.8 Mbps at 66–80 ms); ≈3 steps per minute of sustained drag. P2 seeds: next session opened at `maxrate = 0.85 × remembered` (3.386 → 2.879, 3.598 → 3.059). Opening keyframe on the seeded sessions: 8.0 / 8.4 / 22.9 / 19.5 KB vs 5.7–6.4 KB baseline — but two later seeded sessions at 2.6 Mbps opened at 5.9 KB, so the opener also tracks screen content and the criterion is not yet met. **Defect**: a 14-s idle session wrote its own seed back as the "stable rate" (3.598 → 3.059) — the memory decays 15 % per idle session; fixed in P2b (`record_session`: a lower value needs a decrease). Age gate 1.5× vetoed the evidence windows (66–80 ms over a 43 ms floor) → 2×. CORPLAP-3 negative control clean. |
| 2026-08-30 08:19–08:21 UTC | agent 0.4.26 (CORPLAP-2 `av1_nvenc`, DERP relay), web `v20260830-6a166a1ec9f7` | **Seeded-opener A/B, same screen a minute apart** (memory hand-seeded at 7 Mbps from P0). Baseline 3.0 Mbps: a ~0 KB black keyframe, the desktop as ONE 221 KB inter frame at +176 ms, first painted sample already at steady sharpness, one keyframe in 12 s. Seeded 5.95 Mbps: 30 KB keyframe (50 % sharpness) → 172 KB P → first light 200 ms earlier, 90 % of steady within ~190 ms — but a **×0.85 decrease inside 1.8 s** (`send_wait_max 367 ms`, 73 backpressure skips — the opener draining) and then **three AIMD climb-back steps at 5-s spacing, each an in-place NVENC reconfigure = a starved IDR** (166 KB at +0.3 s, 222 KB at +5.3 s; sharpness dipped to 91 % at +5.4 s). Also measured: the opener burst drained at ≈ 8 Mbps — the P0 number, now free. ⚠️ Windows PowerShell's `Set-Content -Encoding UTF8` writes a BOM that serde rejects (the memory read as EMPTY until rewritten BOM-free). |
| 2026-10-27 09:23, 15:27, 16:31 UTC | agent **0.4.177** (09:23) and **0.4.178** (15:27, 16:31) — CORPLAP-2 `av1_nvenc` 1920×1200 over `relay:derp/tcp` (DERP rtt 106–258 ms); read 2026-10-29, read-only `roomler exec` of the host's log | **Current-code re-read, corrected** (the first read of these sessions, made 10-27, labelled them 0.4.179; the host took 0.4.178 at 13:41Z and 0.4.179 at 18:01Z — service log). **09:23 (79 s, motion then idle):** seed 4,750,677 ⇒ opener 4,038,075; opener burst 496,887 B at 0 ms wait; steps 4.04 → 4.29 (09:23:26.0) → 4.56 (:31.1) → 4.84 (:37.2) while the 2 s heartbeats carried 6.2 / 6.3 / 4.4 / 7.6 Mbps at 32–53 fps; then 45 s of an idle desktop at 0.28–0.35 Mbps / 18 fps with the ceiling held at 4.84 M (not carried ⇒ no step); 09:24:25 the FR-59 P3 viewer bound clamped `target_bps` 4,843,519 → 4,116,975 (90 % of a 4.57 M arrival report) with `learned_ceiling_bps` unchanged — a `set_ceiling` clamp counts as no decrease (`aimd.rs:188`), so the learner's 10 s clean window did not restart; the AIMD climbed back +302,719 per 5 s and the learner stepped to 5.15 M at 09:24:34.8 on a 6.4 Mbps window. `send_stalls=0`; `send_wait_max_ms` 52.444 in the heartbeat holding the opener's tail and the first step, 0.057–0.50 in all 36 others; paint age p50 68 / p95 117 / max 130 ms over a 45–50 ms floor. Write-back `Some(4843519) → kept 4759961` (+0.2 %, `ALPHA_UP`). **15:27 (130 s, idle):** seed 4,759,961 ⇒ opener 4,045,966; a 405-byte opener, then 0.16–0.25 Mbps at 18 fps (`max_qp` 14); no step, no decrease; write-back `Some(4045966) → kept 4402963`: **the session's own seed written back as evidence, −7.5 %** (`stable_bps()` = the target hold, V4 `ALPHA_DOWN`). **16:31 (13 s):** seed 4,402,963 ⇒ opener 3,742,518; one step to 3.98 M; `None → 4402963` (nothing held 10 s). The memory survived the 13:41Z service restart between the first two sessions (AC5). |
| 2026-10-28 07:57, 12:38, 16:46, 16:46 UTC | agent **0.4.181** (service started 06:56Z) — same pair; read 2026-10-29 | Four more sessions, all seeded 4,402,963 ⇒ opener 3,742,518. **07:57 (19 s):** opener 697,527 B that blocked a send **163.9 ms**; the first step 3.74 → 3.98 M logged at 07:57:43.33 — **0.36 s before "opener grace over" (07:57:43.69)**: the CARRIED gate counts bytes written to the channel (`governor.rs:1004`) and `on_window` does not read the FR-79 verdict (`governor.rs:1388`), so the opener burst was the carrying window; two more steps to 4.49 M on 4.7–7.3 Mbps windows; `None` written. **12:38 (11 s), 16:46:11 (13 s):** one step each, `None`. **16:46:31 (90 s):** steps 3.74 → 3.98 → 4.22 → 4.49 M during 12 s of motion (3.1–7.4 Mbps), then 60 s idle at 0.35–0.77 Mbps with no step; `send_wait_max_ms` ≤ 0.57, `send_stalls=0`, paint age p50 67 / p95 111 ms (one 438 ms first report); write-back `Some(4489010) → kept 4411567` (+0.2 %). `rate_memory.json` on 2026-10-29: `100.65.4.2\|relay:derp/tcp` = 4,411,567 at `at_unix` 1793206081 (2026-10-28T16:48:01Z), plus three bare-address entries from browser-viewer sessions. **Reading for AC1:** the memory is held at 4.4–4.8 M by the write-back's dynamics (an idle session −7.5 %, a proving session +10 % of its gap), not by a measured limit — these sessions carried 6–7.6 Mbps windows at sub-ms send wait and P0 measured 6–9 M sustained. No daemon restarted, no input driven; the operator's console was signed in. On 2026-10-29 the pair rides `direct`, so no relay read is possible until it relays again. |
