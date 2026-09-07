# Rate control — how the remote-desktop stream spends its bits

How a session decides **bitrate, quality, frame rate, and resolution**, and why
(since rc.445) it never changes resolution mid-motion. Companion to
[encoders.md](encoders.md) (which encoder runs) — this doc is about what that
encoder is told to do. History and field evidence at the bottom.

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
| **Byte-budget queue gate** (rc.442) | bytes in flight vs `constrained_queue_ms` (450 ms) of the relay ceiling | skip producing a frame | every loop iteration |
| **Viewer-rate divisor** (`encode::viewer_rate`) | browser's decoded-fps + struggling report | send every Nth frame | 1 s windows |
| **Encode pressure + auto tier** (`encode::encode_pressure`) | avg encode ms | maxrate factor; long-edge cap when encode-bound | 2 s heartbeat |
| **Goodput estimate** (`encode::goodput`, rc.453) | busy-period throughput from the send task | *none yet* — observed and reported only | folded on the 1 s window |

Two rules keep them from stepping on each other:

- **Rebuild-bound bitrate applies are motion-deferred** (rc.445). NVENC
  reconfigures maxrate in place; QSV/AMF must rebuild the encoder — a
  blocking open. The pump holds AIMD applies while any real frame encoded
  ≥4 KB in the last 1.2 s (the motion clock; caret/keystroke deltas at
  0.5–3 KB never hold it) and flushes once quiet — the rebuild then stalls a
  static image nobody can see, and its first-frame IDR doubles as the
  post-motion refresh.
- **A rebuild bumps the send epoch** (rc.445): the send task discards
  queued frames from the previous encoder, so the fresh IDR ships
  immediately instead of behind up to 450 ms of obsolete motion.

Rebuilds also reuse the session's **proven encoder name** first instead of
re-walking the vendor cascade (a failed tiered open of an absent vendor's
encoder costs 100–300 ms), and open at `min(ceiling, AIMD target)` so the
governor's forced reapply cannot trigger an immediate second rebuild.

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
| `ROOMLERD_RELAY_MAX_KBPS` | 3000 | The constrained-transport ceiling clamp |
| `ROOMLERD_SMOOTH_MAX_EDGE` / `RELAY_MAX_EDGE` | 1024 / 1280 | Rung sizes when `priority_res_cap` is on |
| `rate_prior_decay` | on | FR-70 P1 — a remembered rate standing in for a pipe measurement DECAYS toward the band on clean windows (×1.25 per 10 s from a seed, ×1.1 from a measurement, one step down per two pushed-back windows) instead of holding the floor relief and the queue budget at the memory for the whole session; heartbeat `prior_bps`. Off = FR-59 P8 verbatim |
| `transit_classify` | on | FR-71 T1a — classify every viewer window as `clear` / `overproduced` / `transit-stalled` / `viewer-late` / `unknown` from the M0 age split plus the sender's own window (queue vs budget, gate skips, blocked sends, worst send wait; a report gap counts as `transit-stalled` only after the viewer has reported once); heartbeat `pipe_state` + `pipe_states` counters + `pipe_gap_stalls` (how many of the `transit-stalled` windows were report gaps rather than a split verdict — the two are held apart because the hold would act on both). **Shadow only**: nothing acts on the verdict until T1b's `transit_hold`. Off = no verdict (`pipe_state=None`) |
| `transit_hold` | **off** | FR-71 T1b — act on a `transit-stalled` window: the opener's ramp neither steps nor ends, the FR-15 age loop does not fire (and its over-streak resets, so the backlog's own elevated windows need two fresh ones to fire), the FR-59 P3 clamp is neither armed nor released, the rate prior does not move; the FR-59 P4 drain still runs. The AIMD's per-frame additive increase is untouched. Heartbeat `transit_holds`. Default off for one release, flipped on the shadow's evidence (FR-63's rule); needs `transit_classify` |

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
