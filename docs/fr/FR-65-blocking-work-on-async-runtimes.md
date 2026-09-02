# FR-65 — Blocking work on async runtimes: measure first, then remove

**Issue:** [#1255](https://github.com/gjovanov/roomler-ai/issues/1255) · **Status:** proposed · **Parent:** the FR-62/63/64 rate-control arc (plan `immutable-doodling-neumann`)

## Goal

Find and remove synchronous vendor/OS calls that run on tokio worker threads in
`roomlerd`'s hot paths — **driven by measurement, never by inference**. The
deliverable that matters most is the instrument: today nothing measures a stall,
so stalls are found by accident, months late.

## Why now — the evidence FR-62 produced

A QSV encoder open costs **~340–390 ms at maxrate ≥ 1.5 Mbps and 1.3–2.0 s at
≤ 1 Mbps** (CORPLAP-1, Iris Xe, 0.4.51, identical in both `low_power` modes).
`peer.rs` ran that open **inline on the pump thread** for constrained sessions —
and constrained is exactly the band where those targets live. Every landed rate
change on a slow-link QSV session froze the pump for ~2 s. #1254 fixed it for the
**rate** path only.

🔑 **The reason it went unnoticed for months is the whole thesis of this FR.**
The pump already measures `avg_capture_ms`, `avg_scale_ms`, `avg_encode_ms`,
`avg_send_ms` — and the 2 s stall appeared in **none** of them, because the
apply/rebuild phase was untimed and because a per-heartbeat *average* dilutes or
hides a single outlier. It became visible only when `set_ms` was instrumented
for FR-62 A0, for an unrelated reason.

⚠️ `roomlerd` is **one process**: overlay, DERP, tunnels, the WS control plane
and RC share runtimes. A long synchronous call taxes all of them, which is why an
encoder stall is not only an RC problem.

## Key design

### P0 — the stall watch (blocks every other phase)

A per-iteration wall-clock watch on the pump loop, threshold-gated.

- Two `Instant::now()` per iteration (~20–40 ns each) against a 16.7 ms frame
  budget at 60 fps. **Not** `tracing` spans — span entry/exit allocates and
  dispatches per phase per frame, which is the cost profile this FR exists to
  remove.
- Reuse the phase timers the pump already computes; **add the missing
  `apply_ms`** (the rate/dims apply + rebuild), which is precisely the phase the
  2 s stall hid in.
- Track **max** beside **avg** per phase (one compare per phase per iteration).
  An average over a 2 s heartbeat cannot represent a 2 s outlier.
- On `total > stall_warn_ms` (default **250 ms**) emit ONE `WARN` carrying the
  per-phase breakdown, session id, `constrained`, encoder name and target.
- ⚠️ **Rate-limit the WARN** (≤1 per 2 s per session): a stall storm that floods
  its own log becomes a second performance problem.
- Kill switch `pump_stall_watch` (env `ROOMLERD_PUMP_STALL_WATCH`, default **on** —
  it is near-free, and its absence is what cost us the months above).

**Generalisation is deliberately deferred**: P0 ships pump-local. If P3 needs the
same primitive for the overlay loops it moves to a shared crate then — not
speculatively.

### P1 — dims / chroma / backend rebuilds off-thread

The direct twin of #1254, and the largest remaining known win.

1. It fires **exactly when the session is already struggling** — the downscale
   tier engages on a constrained/congested session, the same conditions that put
   the target ≤ 1 Mbps, i.e. the 1.3–2.0 s band. The pump blocks for ~2 s at the
   moment it is trying to recover.
2. The background-swap machinery **structurally cannot carry it today**:
   `rebuild_spec(&self, bps: u32)` carries only a bitrate, and `adopt_rebuilt`
   returns `false` for a geometry change, logging *"background rebuild stale
   (dims/backend changed) — discarded"*. So a dims change not only runs inline,
   it **throws away any in-flight rate swap**.

Design: extend `RebuildSpec` with geometry (w/h/chroma/backend); `adopt_rebuilt`
compares against the **currently desired** geometry rather than the geometry at
request time, so a swap is stale only if the desire moved again. Adoption stays
**quiet-gated on constrained** (the #1254 invariant — adopting mid-motion on a
thin pipe is the 2026-08-27 regression). Kill switch `bg_rebuild_dims`.

⚠️ Ordering hazard: while the replacement opens, the pump keeps producing at the
OLD geometry. The viewer must never see a geometry change before its
`video_info`; the adoption path already clears `video_info_sent` and bumps
`send_epoch`, but this needs an explicit test, not an assumption.

### P2 — the per-frame encode call

**Confirmed mechanism, unquantified harm.** `FfmpegEncoder::encode()` is
`async fn` in signature only — its body is `self.encode_sync(&frame)` with zero
`.await`, and `spawn_blocking` appears nowhere in `encode/` except in comments.
So FFmpeg `send_frame`/`receive_packet` run **directly on the tokio worker** that
polls the pump. Measured on CORPLAP-1: `avg_encode_ms` **8.57 ms** plus
`avg_capture_ms` **4.16 ms** ≈ 13 ms synchronous per frame.

P2 **starts with the measurement, not the fix**:

- A **runtime canary** — a task that wakes every 10 ms and records its own
  lateness. 🔑 The canary is the right instrument because it measures the
  *symptom* (other tasks not getting scheduled) rather than a proxy for it, and
  it costs a timer.
- Optionally tokio `RuntimeMetrics` (`worker_total_busy_duration`, injection
  queue depth) as corroboration.

Only if starvation is demonstrated: move encode to a **dedicated encoder thread**
fed by a channel — **not** `spawn_blocking` per frame, which adds a handoff every
frame and can grow the blocking pool without bound. ⚠️ A dedicated thread adds a
queue hop, so the A/B must show frame **age** does not rise, not merely that the
stall count fell.

### P3 — overlay guard cadence and rekey storms

A tax rather than a block, in the same family. #1237 fixed the sibling-adapter
cause of the ~100 `net-change — revalidating direct carrier` events/min (each
force-rekeying **every** peer and demoting real carriers); the general guard
cadence is unexamined. Route-table enumeration and eviction are synchronous OS
calls (`GetIpForwardTable2` / netlink).

Measure first: guard waves/min, per-enumeration duration, forced rekeys/min — at
idle and across a VPN flap — with the canary watching for starvation. Then
coalesce/debounce net-change bursts into one revalidation and bound the
per-peer rekey rate. ⚠️ **Debounce, never disable**: the guard exists so a
hostile VPN cannot self-wedge the host, and its worst-case reaction time must
stay within today's SLA.

### P4 — the caps-probe respawn

Observed on neo16: a `caps-probe` **child process every ~10 s for nearly three
hours**. `detect()` is meant to cache in a `OnceLock`, so this is a bug — a
process spawn plus a driver enumeration on a repeating timer. Measure the spawn
rate per host, find the trigger (unshared cache vs. genuine re-detection), fix.
⚠️ Fleet-wide counts via `roomler exec` are a **biased sample** (only exec-enabled,
reachable hosts) — say so rather than generalising.

### P5 — the remaining synchronous calls, triaged by measurement

Inventory (`config::save`'s fsync under the daemon-wide write lock, WFP rule
application, any `Command::new(..).output()` on a runtime thread), ranked by what
the canary says actually hurts. Fix only what measures.

## Explicitly NOT in scope

- **The DataChannel send path.** `avg_send_ms` was **0.013 ms** — it is not a
  bottleneck and must not be "optimised".
- **The SCTP stalls.** Those were a correctness bug (the FORWARD-TSN
  bundled-chunk drop), already fixed by the vendored patch. Do not conflate a
  parser defect with a blocking-design problem.
- The rate-control architecture itself (FR-63), the coarsen-ladder/clamp trap
  (FR-62 A4), and which path ICE selects (FR-64).

## Acceptance criteria

- [ ] **AC1** — the stall watch ships and its overhead is **measured** at < 1 % of
      the frame budget by an A/B with it disabled, on real hardware. An instrument
      that costs what it measures is worthless.
- [ ] **AC2** — a synthetic stall is caught and attributed to the correct phase.
- [ ] **AC3** — no pump iteration exceeds 250 ms across a downscale-tier change on
      a constrained QSV session; recorded **before and after** P1.
- [ ] **AC4** — the encode-scheduling question is answered with canary data; if
      starvation is shown it is fixed, and the canary confirms, with frame age
      unchanged or better.
- [ ] **AC5** — overlay guard waves and forced rekeys/min are measured at idle and
      under a VPN flap, and bounded; no rekey storm.
- [ ] **AC6** — caps-probe spawns at steady state are 0/hour on a measured host.
- [ ] **AC7** — every fix carries a before/after from the **same** instrument on
      this issue. A fix with no measurement does not count as shipped.

## Open decisions

- Whether the stall watch generalises to a shared crate (decide in P3, from need).
- Dedicated encoder thread vs. leaving encode on the worker if the canary shows no
  starvation — P2's measurement decides, not preference.
- Whether `apply_ms` belongs in the heartbeat permanently or only under the watch.

## Risks

- The instrument perturbs what it measures → AC1 exists precisely for this.
- Off-thread work adds handoffs → every phase A/Bs **latency**, not just stalls.
- Debouncing the route guard could delay a legitimate heal → bound it and keep the
  worst-case reaction time.

## Related

FR-62 #1242 (produced the measurements that motivated this; #1254 fixed the rate
path), FR-63 #1243, FR-64 #1244, #1237 (the sibling route war).

## Field-verification log

_(empty — every entry must carry a before/after from the stall watch.)_
