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

**Re-read 2026-10-31, fleet on 0.4.191.** The surface is identical from `agent-v0.4.190` through
`agent-v0.4.191` and master; 0.4.188 differs by one `pub mod bench` line, and 0.4.187 also lacks
FR-63's shadow controller, which no live decision reads. No box moved. Each open criterion is
measured or named for what stops it. **AC1** and **AC2** need a relayed NVENC pair and a driven
viewer, which means an operator window on CORPLAP-2; the memory stands at 3,379,312 (an opener
1.13× cold), and the replay in the field log shows that counting only carried holds reaches about
1.9× at best, against the 2.5× AC1 asks. **AC3** stays `[~]`: natural motion read, no drag. **AC4**
waits on CORPLAP-3 relaying. **AC6** needs a hardware encoder, on a relay, on a host whose daemon
may restart; a VM cannot prove it. FR-63 B3 deletes this code, so new learner behaviour is a
question for the operator, not a phase of this FR.

**Driven 2026-10-31 evening, on 0.4.191, CORPLAP-2 on its natural relay. This replaces the
paragraph above for AC1, AC2, AC3 and AC6.** **AC3 is met. AC1 and AC2 are `[~]`:** their numbers
pass, but on H.265 4:4:4, this pair's default cell now, a cold opener passes them too, so the run
does not show that the learned rate improved the opener. Which reading counts is the operator's
call. This paragraph also called the note above wrong, the one saying 2.5× needs a 7.5 M memory, on
the grounds that the keyframe does not follow `maxrate` in a straight line
([the A/B](#the-opener-ab-2026-10-31)); the next paragraph withdraws that. **AC4** was waiting on
CORPLAP-1's RC rows and **AC6** on a restart of jupiter's daemon; both were read on 2026-11-02. The
memory stood at 4,326,752 at 22:41 after three writes, one of them an evidence of 13.04 M, above
`hi`, from a 45 s session ([field log](#field-log)); that belongs with FR-79's open hold question.

**Read 2026-11-02, fleet on 0.4.197. This replaces the paragraph above for AC4 and AC6, and corrects
it on the keyframe.** **AC6 is `[~]`:** the kill-switch A/B ran on jupiter, and off restores
everything the switch governs, to the byte where the two arms can be compared
([the A/B](#the-kill-switch-ab-2026-11-02)). At load, bytes cannot tell on from off on `h264_vaapi`,
so whether that meets "byte-for-byte" is the operator's call. **AC4 stays `[ ]`:** CORPLAP-1's RC
rows no longer stand in the way, and no ~2 Mbps relay exists to read. **AC1 and AC2 stay `[~]`, with
one correction:** the 10-31 arms were not level. The opener's keyframe follows `dc_unopen_drops`, the
frames encoded and thrown away before the video channel opens
([what moved it](#what-moved-the-openers-keyframe-2026-11-02)). At equal drops it follows `maxrate`,
as FR-31 found, so the note that a 2.5× opener needs a 7.5 M memory holds again at equal drops. A
cold opener still passes AC1's and AC2's named numbers. The pump's behaviour is
[#3334](https://github.com/gjovanov/roomler-ai/issues/3334), not this FR's. **A defect found on the
way:** an idle relay session can write a few kbps into the memory, and the key then keeps itself low
([field log](#field-log)). The fix is with FR-79; the fleet count is one natural key at or under
1 Mbps, and no natural session opened from one in 7 days.

Child of the RC-quality program. Follows FR-31 (every opening and repair number on an NVENC relay
session is proportional to `maxrate`, and nothing ffmpeg exposes changes that) and the parked
measured-rate line (#678: a sender cannot see capacity it is not using). Operator's directive
2026-08-29: the relay ceiling first; a vendored-ffmpeg patch only if this fails.

## What is in force today

- `relay_max_bps()` = **3 Mbps** (`agents/roomlerd/src/encode/mod.rs:498`, env `RELAY_MAX_KBPS`),
  applied as the encoder's `maxrate` cap on every constrained session
  (`encoder.rs` ↔ `rate_profile::ffmpeg_maxrate_bps_scaled`, `peer.rs:3267`), opened at 85 % =
  **2.55 Mbps**, and as the AIMD's ceiling (`governor.rs:264-283`); the send-queue budget derives
  from the same constant (`peer.rs:4803`).
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

- [~] opening keyframe ≥ 2.5× today's bytes (≥ 14 KB at 1920×1200, today 5.6 KB) and first-light
      sharpness ≥ 70 % of the 8 s steady state (today 47 %) — **measured 2026-10-31, 0.4.191; which
      reading counts is the operator's call.** Five sessions after the learning drag with nothing
      overridden, on a dense screen: 23.1–23.3 KB, first light 104–107 %. **Against the numbers it
      names, 14 KB and 70 %, this passes. As a test that the learned rate improves the opener, it
      shows nothing,** because a cold opener passes as well: 23.4–63.3 KB and 106–107 % on the same
      screen and cell, 13.6–37.8 KB and 89–107 % on AV1. Against a cold opener on the same cell, the
      pair's keyframe is 1× or less. The criterion was also written on AV1, not on today's default
      H.265 4:4:4 cell ([the A/B](#the-opener-ab-2026-10-31)). **2026-11-02: those arms were not
      level.** The pair's sessions rode the overlay and landed 5–19 `dc_unopen_drops`, cold's 2–7, and
      more drops mean a smaller keyframe ([what moved it](#what-moved-the-openers-keyframe-2026-11-02)).
      At equal drops the rate does scale the keyframe: 1.08× at the pair's 1.13× `maxrate`, 1.68–2.29×
      at the 4.55 M entry's 1.78×. This does not move the box: a cold opener still passes the named
      numbers, and at this memory the drops move the keyframe more than the rate does
- [~] time-to-crisp ≤ 0.6 s (today 1.2 s) — **measured 2026-10-31, with the cold-vs-seeded A/B
      this criterion asks for; the same two readings as AC1.** Time-to-crisp is FR-31's, ≥ 90 % of
      steady. **Against the number it names, this passes:** 0 ms in every H.265 session of every arm,
      at most 185 ms on AV1. **As a test that the learned rate improves it, it shows nothing:** a cold
      opener passes too, at 155 ms at worst, and August's 1.2 s did not reproduce. The memory's
      effect shows only nearer the top: on AV1 on the wallpaper, 97 % of steady took 994–1,312 ms
      cold, 756–948 ms at the pair's rate and 302–743 ms at 4.55 M. The drops in AC1's note were not
      level here either
- [x] a 60 s drag: no send wait > 1 s, viewer-age p99 not worse than the FR-18 numbers, and the
      ceiling never above `hi` — **met 2026-10-31**, 0.4.191, CORPLAP-2 on `relay:derp/tcp`: a real
      Explorer window dragged by its title bar for 60 s, 1,136 pointer moves, 2.94 Mbps mean, 4.35
      peak. Inside the drag `send_wait_max_ms` stayed ≤ 0.383 and paint age ≤ 132 ms. Across the
      session's 95 heartbeats: send wait max 0.787 ms, age p99 195 ms, `send_stalls=0`, no
      backpressure skip. The FR-18 numbers (0.4.10, CORPLAP-3 relay): `send_wait` p99 200 ms / max
      1,778 ms, viewer age max 2,185 ms. The learner stepped eight times, 3.00 → 4.87 M, under `hi`
      8 M, and bounded by construction (`ceiling_learn.rs:237`, `.min(hi)`). The load was the
      screen's, not the pipe's: QP sat at 15 in all but two of the drag's heartbeats and the ceiling
      climbed past the load, so the drag did not push this pair to its limit. Without a drag the week's 24 sessions
      had one heartbeat near a second (824.7 ms) and one past FR-18's age max, 3,364 ms (2026-10-30
      11:48, the stand-in session FR-79 reads on #1524)
- [ ] a ~2 Mbps relay (CORPLAP-3, `av1_qsv`): `target_bps` never exceeds the nominal unless every growth
      gate passed — read from its heartbeats; the session must not get worse than today
      (0.4.21, 2026-08-29: no learner steps, `learned_ceiling_bps=0`, target ≤ 3.0 Mbps, 65–78 ms).
      The 0.4.21 control predates the FR-59/70/79 rework and CORPLAP-3 rides `direct` on 2026-10-29;
      a current-code control also has to read the CARRIED gate against the opener burst (see the
      2026-10-28 field row: one step inside the grace). **2026-10-31:** still open. CORPLAP-3 rode
      `direct` in all 41 of its sessions with a carrier this week, so the named host gives no read.
      CORPLAP-1's five relayed sessions are recorded in the field log; nothing shows that pipe is
      ~2 Mbps. On the opener: 1 of the 19 sessions that stepped on the CORPLAP-2 pair took its first
      step inside the grace, that same 2026-10-28 session. **2026-10-31, evening:** a viewer can now
      put CORPLAP-3 on a relay from its own tab, but that gives a relay, not a ~2 Mbps pipe, so it
      would not answer this. CORPLAP-1 (`hevc_qsv`, with no `av1_qsv`) rides `relay:derp/tcp` and is
      the nearest candidate. It showed two RC rows still `active` (21:18 and 21:51) that nobody here
      can account for, so it was left alone. **2026-11-02, read only:** those rows no longer stand in
      the way, and nothing was cleared. The 21:51 row closed by itself at 23:22:45
      (`controller_hangup`). The 21:18 row is an orphan: still `active`, its heartbeats stop at
      21:35:31. What is missing is the pipe. CORPLAP-3 has 61 direct rows and no relayed one in 7 days,
      and CORPLAP-1's last rows are direct. Those two CORPLAP-1 sessions are the nearest read
      ([field log](#field-log)): the gates held at the nominal on the session that struggled and opened
      on the healthy one, on one relay, 16 minutes apart. Nothing shows a ~2 Mbps pipe, so this does
      not tick it
- [x] the learned rate survives a daemon restart and expires after 7 days (unit-tested; the file is
      `…\config\systemprofile\AppData\Roaming\roomler\roomler\data\rate_memory.json` for the SYSTEM
      service). **Restart survival field-read 2026-10-29** on CORPLAP-2: the seed chain 4,750,677 →
      4,759,961 → 4,402,963 → 4,411,567 was written at one session's end and read back at the next
      across the 13:41Z (0.4.177 → 0.4.178), 18:01Z, 22:56Z (10-27) and 06:56Z (10-28, → 0.4.181)
      service restarts; the file at that path holds carrier-keyed entries with `at_unix`. The 7-day
      expiry is the unit test the criterion names (`rate_memory.rs`:
      `round_trips_through_the_file_and_seeds_fresh_entries_only`, `record_replaces_and_prunes`); not
      observed in the field
- [~] `relay_ceiling_learn=false` restores today's numbers byte-for-byte — **driven 2026-11-02 on
      0.4.197, jupiter on a pinned relay ([the A/B](#the-kill-switch-ab-2026-11-02)); whether this meets
      "byte-for-byte" is the operator's call.** **Met:** off, a session opens at 2,550,000 on a key
      whose memory held 2,989 bps, holds `target_bps` at exactly 3,000,000 under load where on climbed
      to 5.84 M, and neither reads nor writes the memory. At equal `dc_unopen_drops` the arms match to
      the byte: at 2 drops, 4 sessions each way, a 158,708 B keyframe and a 436,580 B opener. **Missing:**
      `h264_vaapi` delivered 10.7 Mbps under 2.55, 3.0 and 5.84 M alike, so bytes at load cannot tell
      on from off on this cell, and no cell that follows `maxrate` was run, because no such host has a
      daemon that may restart. The August numbers were a different host, encoder and screen, so off is
      compared with the current code's cold path. **2026-10-31: a VM or a container with no GPU cannot
      answer this.** The learner exists only in `media_pump_ffmpeg_dc` (`peer.rs:6472`, the one caller
      of `relay_max_hi_bps()`), the VP9-444 pump passes `hi = 0` (`peer.rs:4291`), and the FFmpeg pump
      needs a hardware encoder. So on a host with no GPU the switch reads the same on and off, and a
      pass there proves nothing
- [x] **Docs** — the ceiling learner (P1) and the per-peer memory (P2) are documented in
      `docs/rate-control.md` (§ The ceiling learner / § The write-back and the seed) with a mermaid of
      the learner's per-window decision, a gate/anchor table, what CARRIED and DECREASE actually
      measure, the config rows and the 2026-10-27/28 field rows, and the page's `docs/README.md` row
      names the section (#3013; operator rule, 2026-09-05: docs before close)

## The opener A/B (2026-10-31)

The memory key is the nominated remote address (`peer.rs:6298`, `rate_memory.rs:65`). So a viewer
that offers one TURN address lands on that address's entry, with nothing changed on the device.
`coturn.roomler.ai` has three addresses, and CORPLAP-2's memory held a different state for each,
all from real sessions. That gave three openers on one held screen, in interleaved rounds, each
session's `seed_bps` and `maxrate_bps` confirmed from the host's own log:

| screen · cell (steady sharpness) | cold, 2.55 M | the pair, 2.85–2.87 M | a 10-26 entry, 4.55 M |
|---|---|---|---|
| wallpaper · H.265 4:4:4 (3.23–3.25) | 25.1–60.0 KB, 98–100 % | 25.2–27.3 KB, 97–99 % | 42.7–60.0 KB, 99–100 % |
| wallpaper · AV1 4:2:0 (3.36–3.40) | 16.6–32.1 KB, 91–95 % | 17.6–20.5 KB, 92–95 % | 39.98 KB ×3, 94 % |
| dense text · H.265 4:4:4 (6.14–6.19) | 23.4–63.3 KB, 106–107 % | 23.2–23.4 KB, 102–104 % | 32.9–69.2 KB, 105–107 % |
| dense text · AV1 4:2:0 (6.14–6.19) | 13.6, 37.8 KB; 89, 107 % | 14.9, 15.1 KB; 87, 90 % | 52.6, 86.5 KB; 104, 109 % |

Each cell is the first keyframe and first-light sharpness, three sessions per arm, two on dense AV1,
all 1920×1200 with display match off in the tab. H.265 4:4:4 is the shipped default here
(`useRemoteControl.ts:3620` returns `sharper`). AV1 was forced in the tab, because it is the
cell the criteria were written on and the one this pair rode all week. Dense text is an Explorer
window maximized on `C:\Windows`. First light above 100 % means no blur, not a sharper picture: the
metric counts edges, and compression ringing on text most likely adds to them.

~~**The keyframe does not follow `maxrate` in a straight line.**~~ **Withdrawn 2026-11-02: this
spread is mostly the drop count, which this read did not look at
([what moved it](#what-moved-the-openers-keyframe-2026-11-02)).** On H.265 the same 2.55 M gave 23 to
63 KB, and 60.02 KB came back to within 6 bytes at two different maxrates, which points at a limit
this read did not find. On dense AV1, 1.6× the maxrate gave 3.5–5.8× the keyframe. **The whole opener
is small:** 215–524 KB in its 2 s grace with display match off, about 0.9–2.1 Mbps, under even the
cold cap, which is why the ceiling matters so little here. **The August numbers did not reproduce:** no cold opener on
either screen or cell came near 5.6 KB and 47 %. That screen is gone, so this is not like for like,
and nothing here says what changed.

**Shipped defaults, unfiltered:** H.265 4:4:4 opens at 1920×1200, then display match can reopen the
encoder at 1920×1080 within 3 s. Seven such sessions before the drag gave 19.9–25.3 KB, except one
that opened on a blank 675 B frame with the picture 1 s later in a 47 KB keyframe. Natural ICE also
chooses: 2 of 24 unfiltered sessions took a TURN key rather than the pair's, so read each session's
`seed_bps`. This is two screens on one idle desktop and a few sessions per arm.

## What moved the opener's keyframe (2026-11-02)

The FFmpeg DC pump encodes before the video channel can carry anything. A frame with nowhere to go
is dropped and counted in `dc_unopen_drops`, and the drop re-arms a forced keyframe, so that the
first frame the browser receives is an IDR (`peer.rs:9722`, `:9728`). The encoder therefore makes one
keyframe per frame until the channel opens, and the browser gets only the last. `idr_count` at the
first heartbeat reads `dc_unopen_drops + 1` on both hosts below, plus one more where a session had a
second keyframe, except after a display-match reopen, which resets the counter. Neither A/B looked at
this count when it ran. Read back from `agent_logs`, it accounts for most of the spread they reported.

**jupiter, `h264_vaapi`, one held screen, 19 cold sessions.** The bytes repeat exactly at each count,
with the switch on or off:

| `dc_unopen_drops` | sessions, on | sessions, off | first keyframe (B) | `opener_bytes` | first channel opened after the encoder |
|---|---|---|---|---|---|
| 1 | 6 | 0 | 119,989 | 502,293 | 36–63 ms |
| 2 | 4 | 4 | 158,708 | 436,580 | 68–116 ms |
| 3 | 3 | 0 | 115,149 | 298,956 | 137–178 ms |
| 5 | 0 | 1 | 55,849 | 176,855 | 269 ms |
| 8 | 0 | 1 | 55,849 | 62,738 | 529 ms |

This answers why 55,849 B came twice with the switch off and never in 13 cold sessions with it on.
Those two connects were late before the switch had any part in them: the encoder opened 519 and
2,440 ms after the offer, against 213–419 ms in the other 17, and off's four 2-drop sessions opened it
at 215–318 ms, inside on's range. The viewer's box was at 96 % CPU during the 8-drop session; it was not
read for the other. With the switch on, a load session reached the same class: 54,942 B at 5 drops,
against 120,044 B at 1 drop on that screen (54,371 B at 4 drops off).

**CORPLAP-2, the 10-31 A/B, re-read session by session.** Each entry is `drops: first keyframe in
bytes`, from the sessions in [the table above](#the-opener-ab-2026-10-31):

| screen · cell | cold, 2.55 M, TURN | the pair, 2.85–2.87 M, overlay | a 10-26 entry, 4.55 M, TURN |
|---|---|---|---|
| wallpaper · H.265 4:4:4 | 2: 60,028 · 3: 35,750 · 5: 25,129 | 5: 27,253 · 8: 25,249 · 8: 25,261 | 3: 60,025 · 3: 60,022 · 5: 42,733 |
| wallpaper · AV1 4:2:0 | 3: 32,060 · 4: 22,308 · 7: 16,580 | 6: 20,537 · 9: 18,101 · 10: 17,628 | 5: 39,982 · 5: 39,976 · 5: 39,981 |
| dense text · H.265 4:4:4 | 2: 63,284 · 3: 32,743 · 4: 23,435 | 9: 23,285 · 10: 23,362 · 12: 23,226 | 3: 69,168 · 9: 36,806 · 22: 32,866 |
| dense text · AV1 4:2:0 | 3: 37,845 · 7: 13,557 | 11: 14,898 · 19: 15,103 | 3: 86,539 · 4: 52,563 |

- **More drops, a smaller keyframe, down to a floor.** On one `maxrate`: 63,284 B at 2 drops and
  23,435 B at 4; 37,845 B at 3 and 13,557 B at 7. Past about 5 drops it moves by less than 20 %.
- **At equal drops the keyframe follows `maxrate`, as FR-31 measured.** At 1.78× the rate: 1.68× and
  1.70× on wallpaper H.265, 2.11× on dense H.265, 2.29× on dense AV1. At 1.58–1.59×: 1.57× and 1.58×.
  At 1.13×: 1.08×.
- **The floor is near where FR-31 put it on AV1.** Dense AV1 at 7 or more drops is 1.25–1.28 ×
  `maxrate`/fps, against FR-31's 1.1–1.3, and wallpaper AV1 1.47–1.56×. H.265 on these screens floors
  higher, at 1.7–2.4×.
- **The repeats have an ordinary reason.** 39.98 KB ×3 was three sessions at 5 drops. 60.02 KB came at
  2 drops on 2.55 M and at 3 on 4.55 M, and is most likely that screen's keyframe at the cell's
  constant quality, before `maxrate` bites. This read cannot prove that.
- **The arms were not level.** The overlay's sessions landed 5–19 drops, against 2–7 for cold. "The
  pair's keyframe is 1× or less" compared carriers and drop counts along with rates.
- **It shows on screen where the keyframe is small.** Dense AV1 at 2.55 M: first light 107 % at 3 drops,
  89 % at 7. jupiter at 2 drops: first light 95 %, 90 % of steady at 0 ms. At 5 drops: 68 %, 1,784 ms.

**FR-31 recorded this counter as not causal,** on sharpness across runs at 4–16 drops. That range is on
or near the floor above, so the two reads agree, and this one adds what happens below it.

That each forced keyframe uses up the encoder's rate budget, leaving less for the next, is the likely
reason. These reads did not measure the budget, and nothing held the counts: they fell as they fell,
over a few sessions per arm, on two screens and one host per encoder.

**Not built, and not this FR's.** [#3334](https://github.com/gjovanov/roomler-ai/issues/3334) has the
mechanism, the numbers and two untested fixes. It is not learner behaviour, so FR-63 B3 leaves it in
place, and an agent change there waits for FR-63's read.

## The kill-switch A/B (2026-11-02)

jupiter, agent 0.4.197, `h264_vaapi`, 1600×900, viewer Chrome on neo16, the relay pinned in the tab to
one TURN address over UDP, one seeded screen, a scroll in the load arms and no other input.
`relay_ceiling_learn=false` went in at a daemon restart at 21:11:00 and came out at one at 21:17:00,
each gated in the same call on no open session. The figures come from the host's own log in
`agent_logs`, tied to the viewer's by order and by `opener_bytes`.

| arm | switch | memory on the key | sessions | opens at | first keyframe (B), by drops | `opener_bytes` | rate |
|---|---|---|---|---|---|---|---|
| cold | on | none | 13 | 2,550,000, 30 fps | [the table above](#what-moved-the-openers-keyframe-2026-11-02) | the same table | 2.55 → 2.7375 → 2.925 M |
| load | on | none | 2 | 2,550,000 | 5: 54,942 · 1: 120,044 | 2.70, 2.78 MB | learner 3.0 → 6.21 M in 60 s, 12 steps; target to 5.84 M |
| seeded | on | 3,322 → 2,690 bps | 5 | **1280×720, 15 fps**, floor 200 kbps | 32,321, then 41,396 at 1.1–1.4 s, all 5 | 90,561–93,807 | first light 60–65 %, 90 % of steady at 2.7–3.8 s |
| off | **off** | 2,989 (not read) | 6 | 2,550,000, 30 fps | the same table | the same table | as the cold arm |
| off, load | **off** | 2,989 (not read) | 1 | 2,550,000 | 4: 54,371 | 2.71 MB | target 2.55 → 2.7375 → 2.925 → **exactly 3,000,000** for the last 72 s; `learned_ceiling_bps=0`, no learner line |

- **Off opens and holds the fixed numbers.** It opens at 2,550,000 and 30 fps on a key whose memory
  opened the on arm at 720p15, and under load the target stops at exactly 3,000,000 where on climbed to
  5.84 M.
- **Off reads and writes no memory.** 7 sessions: no seed line, no learner line, no V4 line, and the
  memory file's hash unchanged.
- **On matches off to the byte at equal drops.** At 2 drops, 4 sessions each way: the keyframe, the
  opener, and `bytes_written` of 436,430, 551,109 and 553,449 at the first three heartbeats. Two of the
  off sessions' heartbeats fell one frame earlier, 76–78 B short of those. Per-second delivery at the
  viewer, kbps: `2621,876,883,37,10,10` off, `2621,876,882,37,10,10` on.
- **The revert brings it back.** The next two sessions opened seeded, with keyframes equal to the
  earlier seeded ones to the byte. The config file hashed to its pre-test bytes right after the key was
  cleared, and a daemon start then moved only `last_run_unhealthy`.
- **jupiter was left as found.** The key reads default, the daemon is up on 0.4.197 with 17 peers, the
  row is online, and the memory file was restored at 21:18:49 to its pre-test bytes (one entry from
  09-08, expired, not read). Nine of the cold sessions ran after that.

**What it could not compare:**

1. **Delivered bytes do not follow `maxrate` on this cell in this range.** The same scroll delivered
   10.7 Mbps under 2.55 M, under 5.84 M and under the off arm's 3.0 M, on `rc_mode=VBR` on a Raphael iGPU,
   where `b:v` is meant to be the cap (`docs/encoders.md:1004`). Only the slow-link open changes the
   bytes. So at load the target, the ceiling and the memory tell on from off, and the bytes do not. A
   cell that follows `maxrate` (NVENC, as in P0) would show it in bytes. None was run, because no such
   host has a daemon that may restart.
2. **The August numbers.** A different host and encoder, a screen that is gone, and a build from before
   FR-59, FR-70 and FR-79. Off is compared with the current code's cold path, which opens at the numbers
   in [What is in force today](#what-is-in-force-today).
3. **The switch does not cover all of FR-35.** The hard stall's ×0.5 (`governor.rs:991`), the opener grace
   (`peer.rs:7049`) and the held increases stay on. `relay_ceiling_learn` governs the ceiling and the
   memory, which is what [the Design](#design) says it does.

**For the operator:** A, accept this: the switch restores everything it governs. B, ask for a cell that
follows `maxrate` first. The recommendation is A, because FR-63 B3 deletes this code.

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
| 2026-10-31 | fleet on 0.4.191, prod `hosted-20261031-28dfd72`; bounded `agent_logs` and `remote_sessions` reads, one RC session | **No relayed pair from neo16 today.** Candidate probe in both connected browsers: Chrome 16 host + srflx, Edge one mDNS + srflx, so a browser no longer relays every session by itself. One RC session to a fleet server on a virtual desktop (no input) rode srflx ↔ host at 33 ms, unrelayed; the `relay=true` rows those servers logged on 10-29/30 did not reproduce from this browser, and that desktop is too empty for AC1 anyway (a 6.9 KB keyframe, steady sharpness 0.14). Carrier census of the last 7 days: CORPLAP-2 `derp` ×24 (last 10-30 19:43), `turn` ×5, `direct` ×13; CORPLAP-1 `turn` ×5; CORPLAP-3 `direct` ×41. The named pair still relays, just not today. |
| 2026-10-25 → 10-30, read 2026-10-31 | agents 0.4.171–0.4.188, CORPLAP-2 `av1_nvenc`, `100.65.4.2\|relay:derp/tcp` | **The pair's week, 24 sessions and 5,645 heartbeats, and a replay of the memory.** `damp` reproduces every logged `kept` to within 2 bps. The memory fell 5,906,129 → 3,379,312 over 18 writes. All 18 lie within 0.2 % of a rung, 0.85 × memory × (17/16)^k: k = 0 ×6, 1 ×2, 2 ×3, 3 ×4, 4 ×2, 9 ×1. A write moves the memory −7.50 / −4.84 / −2.02 / +0.20 / +0.83 % at k = 0…4 and +4.7 % at k = 9, a mean of −2.98 % here, so a session raises the memory only if it climbs at least 3 rungs and then holds 10 s. **Only 6 of the 18 held a target that a 2 s window carried at ≥ 70 %.** Replay of the same sessions, each rule as end value with the logged evidence / end value had the same rungs been climbed from the higher opener (an upper bound: CARRIED gets harder at a higher ceiling): a window at the held target carried (6 writes) 4.13 / 5.79 M; the session's peak window ≥ 70 % of the evidence (11 writes) 3.78 / 5.51 M; only rungs above the seed (7 writes) 4.23 / 6.25 M. Openers from the upper bounds are 1.93×, 1.84× and 2.08× cold, against 1.13× today. **Reading for AC1:** the hold question matters, nearly doubling the opener at best, but no rule reaches the 7.5 M a 2.5× opener needs, and the week's highest learned ceiling was 6,397,901 (a session that opened at 5.02 M). **Health:** max `send_wait_max_ms` 824.7, no heartbeat ≥ 1 s, `send_stalls` ≤ 3 per session, ceiling under `hi` throughout. **Steps:** 58 in 19 sessions; one session's first step fell inside the grace (10-28 07:57). **Natural motion for AC3:** 10-30 12:23:00–12:25:19, 0.4.187, 139 s at 2.61 / 2.74 / 3.78 / 7.57 Mbps (p10/p50/p90/max), send wait ≤ 1.128 ms, age ≤ 122 ms, ceiling 3.37 → 4.30 M. Not a drag: the load sat under the ceiling. |
| 2026-10-30, read 2026-10-31 | agents 0.4.187 ×3 and 0.4.189 ×2, CORPLAP-1 `hevc_qsv`, relayed | **A sibling relay, recorded for AC4. Not the named host, and nothing here shows a ~2 Mbps pipe.** Five sessions of 2–286 s, peak 2 s windows 1.59–3.15 Mbps. Four: no learner step, `learned_ceiling_bps=0`, target ≤ 3.0 M. The fifth (22:16, 29 s) stepped three times to 3.60 M, at 18.6, 23.7 and 28.8 s with the opener grace over at 0.9 s, so no step rode the opener. Send wait ≤ 45.4 ms, `send_stalls=0`, age p95 ≤ 98 ms, 890 backpressure skips in one session (02:16). |
| 2026-10-31 | code at master; two days of the vmtest org's heartbeats | **Why a VM cannot prove AC6, and the lab legs that went with it.** The plan was a kill-switch A/B on a throwaway VM on a forced relay, plus shaped lab legs for AC3 and AC4. Reading the code first stopped it: `relay_max_hi_bps()` has one caller, in `media_pump_ffmpeg_dc` (`peer.rs:6290`), the VP9-444 pump passes `hi = 0` (`peer.rs:4153`), no software FFmpeg encoder exists, and the heartbeats agree: 142 VP9-444 rows from guests with no GPU, FFmpeg rows only on NVENC. On such a host `relay_ceiling_learn` reads the same on and off. The one throwaway with a hardware encoder would be a second daemon on a fleet host, against "one daemon per enrolled machine", so that was decided against (hub, 2026-10-31). No VM or container was started. |
| 2026-10-31 evening | 0.4.191, CORPLAP-2 `hevc_nvenc` and `av1_nvenc`, `100.65.4.2\|relay:derp/tcp`, Chrome on neo16; bounded `agent_logs` reads, no exec, no device change | **AC1, AC2, AC3 driven: [the A/B](#the-opener-ab-2026-10-31).** **The drag** (session 22:24:43–22:27:42, 95 heartbeats): seed 3,379,312, opener 2,872,415, learner 3.19 → 3.39 → 3.60 → 3.82 → 4.06 → 4.32 → 4.59 → 4.87 M in 60 s on 2.1–3.8 Mbps heartbeats, all figures in AC3. After the drag, 14 sessions on the key at seed 3,528,631 gave 22.6–27.5 KB, first light 97–102 %, time-to-crisp 0 ms. **The memory, three writes in one night:** the drag `Some(4872507)` → 3,528,631 (+4.4 %); a session that opened and maximized an Explorer window `Some(3187500)` → 3,358,065 (−4.8 %); a 45 s session that closed one `Some(13044940)` → 4,326,752 (+28.9 %), six times the drag's move on an evidence above `hi`. That one came from `pipe_bps` (`peer.rs:8988`, `governor.rs:634`), which nothing clamps. Below a memory of about 3.53 M the opener sits under the plan's 3 M, so an idle session has no `stable_bps()` (`ceiling_learn.rs:266`) and writes nothing: an 8 min idle session read `evidence=None`. That is where the week's decay stopped. **AC6's host:** jupiter on a pinned relay, 22:44:59, no input, read in AC6. **Wrong turns:** g1's six sessions read as a pass until the agent side showed the cell had moved to `hevc_nvenc` and every session shared one seed, so none was cold. A marquee drag delivered 0.31–0.36 Mbps and a drag on the docked Sticky Notes moved nothing, so neither counts. A synthetic press needs `setPointerCapture` shimmed on `Element.prototype`, not on `.video-frame`. The arrow in the extension's screenshots is its own pointer at the viewport centre, not the host's cursor. Display match set off once stays off in that page until a reload. |
| 2026-11-02, 20:59–21:23 UTC | 0.4.197, jupiter `h264_vaapi` 1600×900, Chrome on neo16, the relay pinned in the tab; one config change and two gated daemon restarts under the standing grox consent, reverted and read back; bounded `agent_logs` reads, `mongodb-0` restarts 1 before and after | **AC6: [the A/B](#the-kill-switch-ab-2026-11-02). The keyframes: [what moved them](#what-moved-the-openers-keyframe-2026-11-02).** **How an idle relay session poisoned the key, each step read on jupiter.** (1) The second load session delivered 10.7 Mbps and went idle at 21:06:21. At 21:06:32 the viewer reported `link_congested=true link_ceiling=Some(8540)`, age 216 ms over a 17 ms floor: windows of queue growth (`viewer_rate.rs:578`), with the viewer's box at 96 % CPU. (2) `governor.rs:1343` takes an arrival rate measured while the queue grows as the path's limit, though the sender was idle. `viewer_rate.rs:542` warns that `rx_bps` on a static desktop is "a few KB/s of keepalive deltas". (3) Target 6.2 M → 200 kbps, `FR-59 P6 abandoned=6209663 measured=None`, an encoder rebuilt at 200 kbps. (4) At the session's end `pipe_bps` (`governor.rs:634`) gave `evidence=Some(3322) kept=3322`, adopted outright as first evidence (`rate_memory.rs:155`). (5) The next sessions read `seed=3322` and opened on the slow-link profile: a 32 KB keyframe, 90 % of steady at 2.7–3.8 s. (6) It keeps itself alive. With no measurement `pipe_bps` falls to `prior.stand_in_bps()` (`governor.rs:648`), so each session wrote the seed back, 3322 → 3322 → 2989 → 2989 → 2989 → 2690, refreshing `at_unix` each time, so the 7-day expiry never arrives. The comment at `peer.rs:2122` says that must not happen. The first load session, which held the same load for 88 s and ended before step 3, wrote nothing, and a cold key with no prior writes nothing. **Fleet count, the 7 days to 2026-11-02:** 27 agents with a non-direct RC row, of 87 with rows; 304,999 log docs in daily slices, no day truncated, no exec. 86 natural writes: CORPLAP-2 77, MacBook-1 8, CORPLAP-1 1. One of the 86 is at or under 1 Mbps, a CORPLAP-2 key at 84,195 bps, written 10-30 12:22:06 and expiring 11-06 12:22 unless revisited. It followed a congested window (`link_ceiling=10863`, age 4,988 ms over a 698 ms floor, last heartbeats 0.14–0.61 Mbps), so it is not shown to be idle-triggered. No natural session opened from a low key; the five that did are this test's. The fix is with FR-79. **Side findings.** Idle refine keyframes (214 KB) drew the ×0.85 cuts at +68 s and +83 s that stopped a hold reaching `STABLE_HOLD`. Three sessions with a main-thread stall in the viewer at connect came up `relay=false` on a `Prflx` remote, at the direct profile (60 fps, `maxrate` 27.54 M, keyframes 485,803–570,961 B), with a relay-only policy in the tab and `prflx-host` in the viewer's own stats. Whether those bytes went through the relay was not established, so they count in neither arm. **Wrong turns.** An empty server list with the relay policy gives a session with no video that takes the direct profile (21:00:46–21:01:01; its rows stay in `remote_sessions`). A TURN URL with `?transport=udp` is not the UDP :3478 entry. A main-thread stall did not raise the drop count: 1,855 ms to first video still gave 1 drop and 119,989 B. `roomler exec` has no stdin, and `pkill -f <path>` in an exec kills its own shell, whose argv holds the path, so the rest of the script silently does not run. |
| 2026-10-31, read 2026-11-02 | 0.4.191, CORPLAP-1 `hevc_qsv` 1920×1200, relayed; bounded `agent_logs` and `remote_sessions` reads, no exec, nothing cleared | **The nearest read for AC4 on current code. Not the named host, and nothing shows a ~2 Mbps pipe.** **21:18:09–21:35:31 (17 min):** no learner step, `learned_ceiling_bps=0` in all 502 heartbeats, target ≤ 3.0 M from the moment it knew it was relayed. Its first 6 s ran `constrained=false` at 36.72 M, because the pair was not nominated within 3 s. 8 `send_stalls`, send wait max 654 ms, age p50/p95/max 163/266/1,070 ms, delivered p50/p99/max 0.84/1.75/2.72 Mbps. The viewer was struggling too (`frames_skipped_decode` above 1,000, reported 5–6 fps), so this does not prove a thin pipe. **21:51:13–23:22:45 (91 min):** opened cold at 2.55 M, six steps 3.0 → 4.32 M between 22:00:00 and 22:30:01, delivered p50/p99/max 1.45/2.13/3.01 Mbps, send wait p99 0.337 ms and max 1.279 ms, 0 stalls, age p50/p99 58/135 ms. The target was at the ceiling before each step. CARRIED is per 1 s window (`governor.rs:1057`), so the 2 s heartbeats confirm it for only 3 of the 6. Write-back `Some(4316131)`, kept as first evidence: a memory 1.4× the highest 2 s window it ever delivered, which is FR-79's open hold question again. **Reading:** the gates held at the nominal on the struggling session and opened on the healthy one, on one relay, 16 minutes apart. |
