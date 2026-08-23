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
`ROOMLER_AGENT_*` env twins; restart required.

| Key | Default | Meaning |
|---|---|---|
| `priority_res_cap` | off | Restore the pre-rc.445 dial resolution rungs (A/B only) |
| `smoother_rate_pct` / `balanced_rate_pct` | 70 / 85 | Dial ceiling factors (30–100) |
| `constrained_queue_ms` | 450 | Send-queue byte budget, ms of the relay ceiling; 0 = unbounded |
| `constrained_hrd_pct` | 200 | HRD window for relay sessions, % of maxrate. ⚠ sub-100 is per-host experiment only — a window smaller than a forced IDR makes Intel AV1 **error and hang** (rc.442 incident) |
| `constrained_cq_relief` | 4 | CQ softening at a sub-native rung on relay — only reachable via explicit picks / restored rungs |
| `idle_refine_settle_constrained_ms` | 1200 | Up-flip settle on relay when a rung exists |
| `gpu_scale` / `scale_threads` | on / 1 | HW-downscale Phase A/B levers (only active when something scales) |
| `ROOMLER_AGENT_RELAY_MAX_KBPS` | 3000 | The constrained-transport ceiling clamp |
| `ROOMLER_AGENT_SMOOTH_MAX_EDGE` / `RELAY_MAX_EDGE` | 1024 / 1280 | Rung sizes when `priority_res_cap` is on |

## Field history (why it is shaped this way)

| Release | Change | Field driver |
|---|---|---|
| rc.436–441 | HW downscale (CPU resampler rework, GPU scale-before-readback), deliverable refine-Up | Smoother's 1024 rung cost 26–45 ms CPU Lanczos on Iris Xe |
| rc.442 | Signed CQ bias (relief), byte-budget queue gate, settle 2000→1200 ms | 9 fps motion equilibrium; drag-start freeze = 0.5–1 MB queue; 4–5 s crystallize |
| rc.443 | HRD trim reverted; stale-pipeline eviction; encode-error ladder | Intel AV1 rejects + hangs on an over-budget forced IDR; a hung pump zombied the shared pipeline ("no video after 4 attempts") |
| rc.445 | **No-flip motion**: dial rungs off, dial ceiling factors, motion-deferred QSV bitrate, send-epoch flush, proven-encoder fast path | The remaining ~1 s mid-drag freeze measured as the flip's blocking encoder open (865/654 ms) + mid-motion ladder rebuilds |
| rc.446 | Deferral motion clock on any ≥4 KB frame | Light motion (GDI + AV1 window moves at 5–30 KB) slipped under the significance floor and let ladder rebuilds through mid-burst |

## The measured-rate closed loop

The remaining constants (dial percentages, 450 ms budget, relay clamp) are
open-loop: they key off a NOMINAL relay clamp while the variable that matters
is what the session actually delivers. The AIMD only watches send-channel
occupancy and SCTP absorbs the mismatch, so it parks at the ceiling and never
learns the pipe — a field capture shows `target_bps=3000000` constant across a
session delivering 1.75 Mbps.

### Stage 0 — measured, reported, not yet consumed (rc.453)

`encode::goodput::GoodputEstimator`, owned by the governor, folded on the
existing 1 s viewer-window tick, reported in the heartbeat as `goodput_bps`
and `goodput_samples=(accepted, rejected)`. **Read it against `target_bps` on
the same line — the gap between them is the open-loop error.**

The hard part is that *a fast sample is not evidence*. Handing a frame to SCTP
is not delivering it: with buffer headroom a frame serialises in microseconds,
which computes to an absurd rate and means only "at least this fast". So the
measurement is taken across a **busy period** — an unbroken stretch where the
send task always had another frame waiting — and only when that stretch lasted
≥ 300 ms. A period that long can only end when the pipe drains, so its
bytes-over-time *is* the drain rate. Shorter periods are **discarded, not
down-weighted**, so no amount of idle traffic can bias the estimate upward.

Two consequences, both intentional:

- On an unbound link no period qualifies, so the estimate stays `None`,
  everything falls back to the nominal band, and **direct sessions behave
  exactly as before, by construction**. `accepted=0` with a large `rejected`
  is the healthy signature there, not a wiring fault.
- On relay sessions the at-rest polish traffic (~1.75 Mbps) keeps periods
  alive, so the estimate survives an idle viewer.

The EWMA is asymmetric — down fast (α 0.5), up slow (α 0.1): a VPN throttling
mid-session is worth believing at once, one lucky burst is not proof the pipe
grew. Confidence decays to `None` after 60 s without a qualifying sample, so a
stale number can never outlive the conditions that produced it.

### Stages 1–2 — planned

`B = min(nominal, 0.85 × G)` becomes the effective budget, turning the ceiling,
queue budget, HRD window (with a per-codec keyframe floor — the rc.442 lesson)
and settle into measured quantities, behind a `measured_rate` kill switch with
the current constants as the confidence-`None` fallback. Then the tunables are
deleted. ⚠️ Measurement may only ever LOWER the clamp: the clamp also protects
the TURN path.
