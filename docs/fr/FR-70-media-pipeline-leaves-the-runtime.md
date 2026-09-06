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
~~`T1` transport classification~~ (→ **FR-71 #1362**, 2026-09-05) · `P1` priors decay + visible overrides.

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

## M1 — the encoder thread, as designed (2026-09-05)

Anchors verified against master `b4d12871`.

**What the pump is today.** `media_pump_ffmpeg_dc` (`agents/roomlerd/src/peer.rs:4545–7244`,
27 await points) runs on a tokio worker and does everything in one loop: the
budget gate and rate applies (`:5763`, `:6639`, `:6677`, `:6740`, `:6991`), pacing
(`:5799`), capture (`:5813`, already a handoff to capture's own thread), the
encoder open (`:6237`, a `spawn_blocking` since FR-65), keyframe requests
(`:6326`, `:6339`, `:6348`, `:6976`), the background rebuild's adoption
(`:5410`, `adopt_rebuilt`), **the encode itself as
`tokio::task::block_in_place(|| enc.encode_sync(&frame))` (`:6759`)**, and the
hand-off to the dedicated send task (`:6892`). Scaling is inside the encoder
(`encode/ffmpeg/encoder.rs`), not a stage of the loop. The encoder is therefore
driven from whichever worker thread the pump task happens to be polled on, held
there by `block_in_place`, and every `enc.*` call is a direct method call from
the decision that wanted it.

**M1's shape: the encoder gets a thread, the decisions stay where they are.**
`encode::thread::EncoderThread` owns the `VideoEncoder` on one dedicated OS
thread per session (named `rc-enc-<session>`; MF/COM and QSV affinity satisfied
by construction) and serves a bounded command channel:

| command | reply | replaces |
|---|---|---|
| `Encode(Arc<Frame>)` | `Result<Vec<EncodedPacket>>` over a oneshot | `block_in_place(encode_sync)` `:6759` |
| `SetBitrate(bps)` | none (fire-and-forget, in order) | the five `set_bitrate` sites |
| `RequestKeyframe` | none | the four `request_keyframe` sites |
| `Adopt(RebuiltEncoder)` | `bool` | `adopt_rebuilt` `:5410` |
| `Open(params)` / `Reopen(params)` | `Result<()>` | the `spawn_blocking` open `:6237` — the open happens ON the encoder thread, so M2's replacement encoder is built there too |
| `Probe` (name, `supports_dynamic_bitrate`, `reconfig_forces_idr`) | a cached `EncoderCaps` snapshot | the `enc.name()` / capability reads (`:6266`, `:6629`, `:7190`) |

The pump loop keeps its structure and every decision site; the only change at
each site is that a method call becomes a send. `Encode` is awaited (the loop
still consumes one frame at a time — cadence, pacing and the budget gate are
unchanged), which is why M1 changes NO behaviour: the same frame, the same
decision, the same packet, one thread hop later (~10 µs, measured before
merge). The worker thread is never blocked again, so every other task on the
runtime (signalling, the send task, the heartbeats, the control DC) stops
sharing a thread with a 5–30 ms encode.

**Kill switch** `media_thread` (config tribool + `ROOMLERD_MEDIA_THREAD`),
**default OFF for one release**: off = today's `block_in_place` loop verbatim.
Both paths share one `EncoderHandle` enum so the pump has exactly one call
site per operation and the switch is a constructor choice, not a second loop.

**Split.** M1a — the thread, the command channel and the switch, with the
pump otherwise untouched; the B0 simulator is not involved (nothing about rate
changes). M1b — the VP9-444 pump on the same handle. M1c — the measurement on
all three CORPLAP hosts after a release with the switch on: `iter_ms_max`,
`pump_stalls`, `apply_ms_max` (FR-65's counters) and the age split, against
the same session shape with the switch off — **the gate is "unchanged or
better", and a regression on any host keeps the default off**.

**Status.** M1a built 2026-09-05 ([#1375](https://github.com/gjovanov/roomler-ai/pull/1375)):
`encode::thread::EncoderThread<E: EncoderOps>` (generic, unit-tested on the
default build with a fake — in-order service on the named thread, the maxrate
mirror exact after the await, destruction on the owning thread at drop, an
encode error is the encoder's not the thread's, a dead thread fails the next
command instead of blocking), `encode::ffmpeg::EncoderHandle` (`Inline` /
`Threaded`, one call site per operation), fourteen pump sites rewired from a
method call to a send, kill switch `media_thread` default off. A spawn
failure hands the encoder back so the handle falls back to inline. Verified
with the feature on in WSL against the vendored FFmpeg tree (rustc 1.95's
diagnostic renderer panics on a pre-existing dead-code warning there — on
master too — so `--message-format=short` is the form that answers). M1b built
the same day: the VP9-444 pump on the same handle — which had run its whole
libvpx encode (BGRA→YUV included) on the runtime worker with **no
`block_in_place` at all** — through a generalised
`encode::thread::EncoderHandle<E>` whose inline path keeps each pump's own
behaviour verbatim (`EncoderOps::INLINE_BLOCK_IN_PLACE`), plus
`EncoderHandle::with(f)` for the operations the trait does not name
(`set_speed`); nine sites rewired, the same switch. Both shipped in
`agent-v0.4.69` with the switch off.

**M1c — first point (2026-09-05 18:11–18:18 UTC, CORPLAP-1 on 0.4.69).** Two
back-to-back sessions, hevc_qsv 1920×1200 on the same relay path (the host's
corp VPN was up), the host at its lock screen with mouse motion only, the
switch flipped between them by `roomler config set media_thread …` + a detached
restart; session B logged `FR-70 M1: encoder handed to its own thread …
threaded=true`.

| steady state (heartbeats 4+) | A: switch off (`6a9c5b65`) | B: switch on (`6a9c5c6d`) |
|---|---|---|
| heartbeats | 32 | 28 |
| `avg_encode_ms` avg / max | 11.19 / 11.94 | 10.89 / 12.11 |
| `avg_capture_ms` | 3.31 | 3.15 |
| `iter_ms_max` avg over windows / worst | 29.0 / **90.0** | 18.2 / **50.4** |
| windows with `iter_ms_max` > 50 ms | 5 | 1 |
| `pump_stalls` / `send_stalls` / `apply_ms_max` | 0 / 0 / 0 | 0 / 0 / 0 |
| the open, `open_ms` (unchanged by M1) | 414 | 485 |

Encode and capture averages equal within noise (the channel hop is invisible
at this scale); the loop's worst pass per window came down — the worst steady
window 90 → 50 ms, the >50 ms windows 5 → 1 — which is the runtime worker no
longer carrying the encode. **Unchanged or better on this host.** One host, one
short session per arm, low motion: the gate wants the same on CORPLAP-2 and
CORPLAP-3 before the default flips. The device was returned to the default.

**M1c — the other two hosts (2026-09-05 18:30–18:38 UTC, both on 0.4.69,
sessions driven concurrently from neo16, switch off then on with a detached
restart between; both switch-on sessions logged `threaded=true`).**

| steady state (heartbeats 4+) | CORPLAP-3 off (`6a9c5fb3`) | CORPLAP-3 on (`…bf574`) | CORPLAP-2 off (`6a9c5fcd`) | CORPLAP-2 on (`…bf57d`) |
|---|---|---|---|---|
| encoder · path | av1_qsv · direct, 60 fps target, 9.7 Mbps, an unlocked desktop | same | av1_nvenc · relay (corp VPN), 6.8 / 5.8 Mbps, lock screen | same |
| heartbeats | 44 | 45 | 31 | 39 |
| `avg_encode_ms` avg / max | 13.64 / 17.85 | 13.51 / 16.17 | 10.83 / 11.96 | 10.97 / 12.33 |
| `avg_capture_ms` | 4.81 | 3.26 | 3.36 | 3.31 |
| `iter_ms_max` avg / worst | 27.3 / 51.2 | 21.9 / 32.1 | 56.3 / **785.7** | 37.2 / 84.3 |
| windows with `iter_ms_max` > 50 ms | 2 | 0 | 4 of 31 | 8 of 39 |
| `pump_stalls` / `send_stalls` | 0 / 0 | 0 / 0 | 0 / 1 | 0 / 1 |
| the open, `open_ms` (unchanged by M1) | 632 | 684 | **2919** | **2846** |

CORPLAP-3 — the encode-bound host — is better on every counter with the switch
on. CORPLAP-2 is better on the averages and on the worst pass (786 → 84 ms)
and equal on encode/capture, but its count of >50 ms windows went the other
way (4 of 31 → 8 of 39) on a relay path whose off-arm carried a 786 ms
outlier; that one counter on that one host is not a clear regression and not
a clean pass either. **Verdict across the three hosts: no host regressed on
averages or worst pass; one counter on CORPLAP-2 is ambiguous.** The default
stays off for this release as the gate says; a longer CORPLAP-2 pair (or a
week of the fleet with the switch on per device) decides the flip. Both
devices were returned to the default. ⚠️ Side finding, unrelated to M1:
CORPLAP-2's encoder open takes **2.8–2.9 s** (av1_nvenc) on every session —
finding 1's worst instance so far, and M2's strongest case.

**The longer CORPLAP-2 pair (2026-09-05 18:41–18:54 UTC, ~4.5 min each,
same relay path, lock screen with mouse motion):**

| steady state (heartbeats 4+) | off (`6a9c6245`) | on (`6a9c640b`) |
|---|---|---|
| heartbeats | 133 | 144 |
| `avg_encode_ms` avg / max | 10.84 / 12.16 | 10.63 / 12.41 |
| `avg_capture_ms` | 1.78 | 2.59 |
| `iter_ms_max` avg over windows | 30.5 | 36.6 |
| windows with `iter_ms_max` > 50 ms | 29 of 133 (22 %) | 25 of 144 (17 %) |
| windows > 100 ms | 0 | 2 — both the lock-screen click burst (121 budget-gate skips in that window, 957 ms) and its tail |
| the open | 2910 ms | 2898 ms |

The ambiguous counter resolves in the switch's favour over a longer run (22 %
→ 17 % of windows over 50 ms); the two >100 ms windows on the on-arm are the
burst the click produced, which the off-arm had inside its excluded opening
heartbeats. Encode and capture equal within noise again. **The gate is met on
all three hosts: `media_thread` flips to on for 0.4.70** (this PR), with
`media_thread = false` per device as the way back; the fleet's
`iter_ms_max` / `pump_stalls` in `agent_logs` after the roll is the check
that the three short pairs did not mislead.

**0.4.70 — the same-day fleet read (2026-09-05, released 19:46 UTC, read
20:05–20:42 UTC).** One trap first, because it would have made the read
unfalsifiable: all three CORPLAP hosts still carried an explicit
`media_thread = false` in `config.toml`, left behind by the M1c pairs (the
off-arm was written last on each; CORPLAP-1 also kept `transit_hold = false`
from FR-71's field session). An explicit key beats the built-in default, so
the flip would have changed nothing on exactly the hosts being read and
"no regression" would have been measured on the inline encoder. The keys
were cleared with `roomler config clear` on all three before any reading;
CORPLAP-3 was restarted on the default, CORPLAP-1 and -2 took it at the
restart of the operator's own dashboard-triggered update. `threaded=true` in
every session's log is the proof the default is what ran. The rule this
leaves: a default flip's field check starts by proving the key is absent on
every host, not by reading counters.

Before = the operator's own sessions on 0.4.69 (inline), 597 heartbeats
each; after = the operator's own sessions on 0.4.70 on CORPLAP-1/-2
(597 and 570 heartbeats) and a driven 3-minute session on CORPLAP-3
(185), steady state = heartbeats 4+:

| | CORPLAP-1 (hevc_qsv, relay) | CORPLAP-2 (av1_nvenc, relay) | CORPLAP-3 (av1_qsv, direct) |
|---|---|---|---|
| `avg_encode_ms` before → after | 9.63 → 10.01 | 11.17 → 10.34 | 12.35 → 12.48 |
| `avg_capture_ms` | 2.64 → 3.36 | 1.40 → 1.39 | 2.37 → 2.63 |
| `iter_ms_max` avg over windows | 21.2 → 21.9 | 27.8 → 23.9 | 15.2 → 17.1 |
| worst steady window | 90.6 → 120.6 | 81.1 → 84.0 | 67.5 → 67.2 |
| windows > 50 ms | 8 (1 %) → 11 (2 %) | 130 (22 %) → 93 (16 %) | 2 (0 %) → 4 (2 %) |
| pump stalls after the opening | 0 → 1 | 0 → 0 | 0 → 0 |
| the session-start open | 534 ms | 2915 ms | 711 ms |

Encode and capture equal within noise on all three; the host M1c had called
ambiguous (CORPLAP-2) is the one that moved most, 22 % → 16 % of windows
over 50 ms on a 20-minute natural session — the same direction as its long
pair. CORPLAP-3's four windows over 50 ms are the +10 s start burst (67 and
63 ms, eight budget-gate skips) and two at 52 ms. CORPLAP-1's one steady
stall is the number worth keeping: at +20 s a single pass took 120.6 ms
with `capture_ms=0.345 encode_ms=0.0 … other_ms=120.273 dominant=None` —
nothing in the frame path ran, the encoder was on its thread, and the whole
pass sat in the un-instrumented residual. That is not the encoder (M1's
job, done) and not a rebuild (M2's); it is the loop body still living on a
runtime worker with everything else that runs there, which is M3's case.
**Verdict: the default holds; no host needed the way back
(`media_thread = false`); the three short pairs did not mislead.** M2a and
FR-71's gap counter rode `agent-v0.4.71` (20:37 UTC the same evening);
M2a's field gate on CORPLAP-2 is the next read.

## M2 — make-before-break, as designed and built (2026-09-05)

**What the measurement says.** The encoder open is the largest stall left in
the frame path once M1 is in: 414–685 ms on CORPLAP-1 and CORPLAP-3, and
**2.8–2.9 s on every session on CORPLAP-2** (av1_nvenc). Today every
*rebuild* pays it again with a frozen picture: `need_rebuild` fires whenever
the resampled frame's dims differ from the encoder's (`peer.rs`, the
`encoder_dims` compare before the open), and the plan moves the resolution
often — priority rungs, the relay cap, idle refine, the soft cap, a transport
flip — so a session sees several. The open itself already runs off the
worker (`spawn_blocking`, FR-65); what M2 removes is the *wait*.

**The two halves of the open.** At session start there is nothing to serve
meanwhile: the pump spawns when the answer is sent, the open starts on the
first captured frame and overlaps the ICE/DTLS setup that is already in
flight — on CORPLAP-2 the viewer's own breakdown reads
`pc_connected:+644 dc_open:+237 first_frame:+2689`, i.e. the 2.9 s open
*is* the first-frame latency and the setup it could hide behind is ~0.9 s.
That half is the encoder's cost, not the pump's, and M2 leaves it. The
other half — every rebuild after the first — is the pump's, and M2a takes
it.

**M2a — the dims make-before-break.** The rate swap already had the shape
(FR-62 P3's `bg_rebuild`: `rebuild_spec` → `open_rebuilt` on a blocking
thread → `adopt_rebuilt` between frames), but only at the encoder's own
dims — `adopt_rebuilt` refused anything else, because a stale rate swap must
not drag the session back to dims it has left. M2a generalises it:

- `FfmpegEncoder::rebuild_spec_at_dims(w, h, bps)` — the same backend, fps,
  cq and chroma at the new dims; `adopt_rebuilt` accepts a dims change (the
  packets' codec still cannot change under a live decoder, so backend and
  chroma must match), and the *pump* now guards the stale rate swap by
  comparing `RebuiltEncoder::dims` with `encoder_dims` before adopting.
- In the pump: when `need_rebuild` fires with a live encoder, the replacement
  opens in the background at the new dims while the effective target stays
  **pinned** to `built_target` — the target the live encoder was built for —
  so the capturer's cap and the resampler keep producing frames the live
  encoder can take. The frame that revealed the change is at the new dims
  already, so it is the one frame skipped. When the open completes the
  replacement is adopted between frames (its first frame is an IDR, the
  send epoch bumps, `video-info` is re-announced), the pin lifts and the
  plan's target takes over. A failed or refused open, a target that moved
  again, or an encoder that was never live falls through to the inline open
  exactly as before — there is no second loop and no new switch.
- Heartbeat: `dims_swaps`; per swap a log line with the dims and how long
  the old encoder kept serving (`FR-70 M2: dims swap adopted — the picture
  never froze`).

**Gate.** On a session with several resolution moves, the picture keeps
updating through each (frames keep flowing at the old dims while the open
runs) and `dims_swaps` counts them; `open_ms` appears in the heartbeat only
for the session's first open. The VP9-444 pump has no background rebuild
and keeps its inline re-open (its libvpx open is milliseconds). **Released
in `agent-v0.4.71` (2026-09-05 20:37 UTC), merged as #1388.**

**First field contact (2026-09-06 01:06 UTC, CORPLAP-3 on 0.4.71, sole
viewer, av1_qsv, direct): the gate FAILED, and the failure is the finding.**
Two things about driving it first. The priority dial moves no dims
(`priority_relay_cap`'s dims-caps are off by default), and "Fit to local
viewport" at a DPR-2 stage asks for 2064×1234 — more than the host has —
so it clamps to native; what moves dims is `rc:resolution`, which the
viewer re-sends on a browser-window resize (a 1100 px window asked for
1598×738, planned as 1180×738). The trigger did fire: `FR-70 M2: dims
change — opening the replacement in the background`. But the same pass had
already handed the NEW dims to the capture backend — the Phase-B
`set_output_cap` sat above the resample and the make-before-break
decision — so the very next frame arrived at 1180×738, the pinned target
could only pass it through, `need_rebuild` was true again with a swap in
flight, and the inline re-open ran anyway: a 628 ms `STALL` at 01:06:28.4,
`encoder (re)built`, and the replacement — whose own open finished 20 ms
later — dropped unadopted. `dims_swaps` stayed 0. Net behaviour on 0.4.71:
exactly pre-M2a, plus one wasted background open per dims change. **Fix**:
the capture backend's cap is handed over AFTER the make-before-break
decision and the inline open (the spawning pass leaves by `continue` and
never reaches it; every later pass sees the pinned target), so the
capturer keeps producing the live encoder's dims until the swap adopts and
the plan's target takes over. The gate re-runs on the release carrying
that: a window-resize-driven session on a 0.4.72+ host reads
`dims_swaps ≥ 1`, the `dims swap adopted` line, and no `open_ms` after the
session's first heartbeat.

**Second field contact (2026-09-06 11:34 UTC, CORPLAP-3 on 0.4.73 — the
cap-order fix in — sole viewer, av1_qsv, direct): half a pass, and the
other half names the next bug.** Driver: Fit mode, then the stage element
shrunk and restored from the page (`document.querySelector('.video-frame')
.style.width = '900px'`, later `'700px'`; the viewer's `ResizeObserver`
re-sends fit on each change) — the dropdown's option clicks do not land
reliably through the browser extension, and `resize_window` cannot shrink
a maximized window. **Downward moves work as designed**: native → 1214×758
and native → 944×590 each logged `dims change — opening the replacement in
the background` and, 300 ms later, `dims swap adopted — the picture never
froze` (`open_ms=288` and `285`, off the frame path; the adoption itself
cost `apply=16.6 ms` in-path), heartbeats with `open=0.0`, no `STALL`,
`dims_swaps=2`. **Upward moves (small → native) still froze**: the
trigger fired both times, then `encoder (re)built` + a 410 / 392 ms
`STALL` — the inline path, the replacement dropped. Cause: on the pass
where the plan moved to Native the frame still carried the old cap, so
`need_rebuild` was false and the `!need_rebuild` branch refreshed
`built_target` from the plan's NEW target although the live encoder was
still 1214×758; the cap block then lifted the cap, the next native frame
spawned the open, and the pass after that pinned to the corrupted
`built_target` (Native) — native frames against a 1214 encoder,
`need_rebuild` with a swap in flight, inline. **Fix**: `built_target` is
set only where an encoder actually comes to exist — the inline open (from
the target that produced the frame) and the adoption (from the target the
replacement was opened for, now carried in `PendingDimsOpen`) — and never
refreshed on a no-rebuild pass. The one native frame that arrives before
the pin restores the cap is CPU-downscaled by the resampler, which is why
the downward direction never showed the flaw. Gate criterion unchanged,
now in both directions.

**Third field contact (2026-09-06 12:23 UTC, CORPLAP-3 on 0.4.74 — both
fixes in — same driver): every intended move adopts, and the frame-driven
trigger is shown to be the wrong shape.** Four moves, four `dims swap
adopted — the picture never froze` (1920→1214, 1214→1920, 1920→944,
944→1920; `open_ms` 290 / 295 / 327 / 285 off the frame path, adoption
17.9 / 16.7 ms in-path, `dims_swaps=4`). The downward moves are clean.
But 7 ms after each UPWARD adoption a new `dims change` fires — back
toward the small dims — followed by two inline re-opens (`STALL` 387 and
416 ms, heartbeat `open=685 stalls=2`). What happens: the frames still in
flight after the adoption were captured under the OLD (small) cap; the
trigger read a frame's dims as the plan and opened a replacement toward
those stale dims, `continue`d past the cap block, and on the next pass the
same stale frame with a swap now pending took the inline path, whose
`built_target` then pointed at the pinned Native while the encoder was
small again — one more spawn, one more inline. The pattern says the
trigger must not be frame-driven at all: a frame's dims are evidence of
what the capturer WAS told, not of what the plan wants. **Fix**: the
make-before-break is decided from the plan — a live encoder built for
`built_target` that the plan has moved away from gets its replacement
opened at the dims the plan's target will produce
(`resample::target_dims`, the resampler's rule stated without a frame and
locked to it by a test), and the target is pinned from that same pass, so
the capture cap follows the pin and never runs ahead of the swap; a frame
whose dims match neither the encoder's nor the pin's while a cap change
is still propagating is stale and is skipped (bounded: three frames per
cap change, then the inline open self-heals a backend that rounds the
box differently). The inline path is left for what it was always for: the
session's first encoder, a backend that cannot rebuild off the frame path,
a host whose native dims changed. Gate criterion unchanged: a resize-driven
session on the release carrying this reads `dims_swaps` equal to the
number of moves, one `dims swap adopted` line per move, and no `open_ms`
and no `STALL` after the session's first heartbeat.

**What M1 does not do**, on purpose: no `Plan` (M3), no in-loop decision
moves (M3), no make-before-break (M2 — but it becomes a `Open` on the same
thread while the current encoder keeps serving `Encode`, which is the whole
reason the open belongs there), no controller change (M4).

## Acceptance criteria

- [ ] **AC1** — capture/scale/encode run on a dedicated thread; the async runtime
      shows no media-path blocking under a canary that records its own lateness.
- [ ] **AC2** — `open_ms` disappears from the frame path: first-frame latency
      drops by the measured open (0.29–0.96 s), before/after on the same host.
- [ ] **AC3** — no rate or geometry decision remains in the media loop.
- [ ] **AC4** — heuristics 34 → ≤ 10, kill switches 11 → ≤ 4, estimators 8 → 1,
      each retirement gated on a counter measured fleet-zero.
- [x] **AC5 (attribution half)** — a repeat of finding 4 is attributable
      without reading source. **Met by M0's `age_split`, field-verified
      2026-09-05.**
- [ ] ~~**AC5 (response half)** — a transit stall does not cut the rate~~ —
      **moved to FR-71 (#1362) on 2026-09-05**, per the open question below;
      it closes there.
- [x] **AC6 (rate half)** — an unmeasured prior cannot hold a session at the
      floor. **Field-verified 2026-09-04 on 0.4.64** on the pair that failed:
      the prior decayed to `None` at 106 s and the target reached 3.9 Mbps with
      nothing measuring the pipe; the same build with `rate_prior_decay=false`
      reproduced the 200–285 kbps pin for three minutes. See the log.
- [x] **AC6 (visible half)** — an overridden `user_target` is visible to the
      operator. **Field-verified 2026-09-05** on web `v20260905-1c2753684ab9`:
      the pill names the slow-link cap and its remembered rate, and the
      Resolution setting says what caps the session and what lifts it. See the
      log.
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
| 2026-09-05 08:57 UTC | **web `v20260905-1c2753684ab9` + agent 0.4.65** | the visible half: CORPLAP-1 pinned to the relay (`ice_relay_tcp`), the viewer's public address seeded to 200 k so the profile engages, session `6a9bd981` | **PASS.** Pill: `1280×800 · slow link (remembered 200 kbps) · native 1920×1200`. Resolution setting: *"This session is capped at 1280×800: the path was remembered 200 kbps when it opened (slow-link profile). It re-evaluates on the next session once the link has carried more. Agent native 1920×1200."* Agent log for the same session: `reason="slow-link-cap"`. ⚠️ The memory key for a relay-pinned agent is the VIEWER's public address (`37.63.112.129`), not a relay's — seeding the seven known keys engaged nothing until the session's own write-back named it. |
| 2026-09-05 09:00 UTC | **agent 0.4.66** (M0), same web | the age split: the viewer auto-reconnected after the self-update, relay-pinned, session `6a9bda3f` | **PASS.** Every heartbeat: `viewer_age_ms=Some(45)` split as `AgeSplit { sender_ms: Some(0.08–0.12), transit_ms: 43–44, viewer_ms: 1–2 }` — the fused 45 ms is the relay round trip, with the browser and the send queue each contributing ~1 ms. M0's gate (attributable to a plane without reading source) is met on a live path; the pre-M0 windows of the same day read `age_split=None`, as designed. |

Device left clean after both: `ice_relay_tcp` cleared, memory restored from the pre-test backup, daemon restarted on 0.4.66.

⚠️ Two things the run taught that the plan did not know: ICE nominates a
**different pair on every reconnect** here (the two overlay host pairs and two
public relays, in five sessions), so a cell keyed on one pair's memory is not
reproducible on demand without seeding every constrained key; and a session
started six seconds after the update task fired ran on the OLD binary until the
installer restarted the service — read the version at the session, not at the
task.

**AC6 status**: both halves field-verified — the rate half on 0.4.64
(2026-09-04), the visible half on web `v20260905-1c2753684ab9` (2026-09-05).

## Open decisions

- Whether the media thread owns the scaler too, or scale stays with capture.
- ~~Whether `PipeState` classification lives agent-side or needs a viewer-side
  report change (finding 4 needs arrival data the agent does not have today).~~
  **Moved to FR-71 (#1362)**; the arrival data exists since M0.
- ~~Whether P1's decay replaces the FR-35 learner or bounds it.~~ **Answered by
  P1: bounds.** The learner only ever lifts a ceiling and was never the pin; the
  pin was the memory's use as a stand-in measurement, and that is what decays.

## Out of scope

The encoder apply path itself (FR-62 — measured dead for QSV, and M2 makes it
irrelevant rather than fixing it); ICE path selection (FR-64).

## Related

FR-62 #1242, FR-63 #1243, FR-64 #1244, FR-65 #1255, FR-59 #1163, FR-1 #767.
