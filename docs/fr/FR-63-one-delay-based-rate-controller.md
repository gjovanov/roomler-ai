# FR-63 — One delay-based rate controller for remote desktop

**Issue:** [#1243](https://github.com/gjovanov/roomler-ai/issues/1243) · **Status:** in progress (B-opener shipped, default OFF) · **Plan:** `immutable-doodling-neumann` (approved 2026-09-02)

**Parent/siblings:** FR-59 (the regression that motivated the arc). This is one of three FRs from that plan (FR-62 encoder apply path, FR-63 the controller, FR-64 the data path).

## Goal

Replace eight estimators of one quantity — occupancy AIMD, blocked-send goodput, the FR-35 learner and rate memory, FR-59's floor relief / arrival clamp / drain / seed contradiction / remembered-rate opener, the FR-15 age loop — composed with "the lower of" rules in `RateGovernor::pre_encode_tick` (`agents/roomlerd/src/encode/governor.rs:482`) and `tick_viewer_window` (:708), with **one** pure, delay-based controller that runs in **shadow** for a release before it drives anything, and is verified against a deterministic simulator replaying recorded field traces rather than against the fleet.

## Key design

- `encode::ratectl::Controller` (pure, explicit `Instant`): a GCC-shaped state machine (over-use / hold / under-use) on the viewer's delay trend, loss, and blocked sends; decreases to `0.85 × measured`, **holds while the age level is elevated** (not the growth derivative — the lesson FR-59 paid for twice), bounded pauses on constrained paths only, **never on direct**; slow-start then additive increase; the remembered rate as a 10-s soft cap; `fps == 0` windows are no evidence.
- `RateDomain { floor, ceiling }` as the single source of pins (a second 1.5 M pin cannot exist as a constant), `PathClass` and the remembered rate as priors; `QualityRung { fps_cap, long_edge }` executed only at settles.
- Viewer feedback v2 (`rc:decodestat`, all optional): `frames_lost`, `frames_rx`, `delay_slope_ms_s` (from the per-frame pairs `rc-hop-stats.ts` already holds), `age_p95_ms`.
- **B0** `encode/sim.rs` (test-only): `PipeSim` (token bucket, stalls, loss, RTT), `EncoderSim`, `ViewerSim` producing decodestat-shaped windows; a CSV trace-replay format extracted from `agent_logs` by session; four fixtures (airport hotspot, CORPLAP-1 DERP, LAN Wi-Fi burst, fast pair misremembered slow) with settling-time / steady-state / no-standing-queue assertions.
- **B1** shadow beside the governor at the live positions, heartbeat fields `shadow_target_bps / shadow_state / shadow_measured_bps / shadow_disagree_pct / shadow_pauses / shadow_reason`; `scripts/rc-shadow-report.py` over a week of `agent_logs`. Flip criterion: ≥ 20 constrained + 20 direct sessions, shadow ≤ live in ≥ 90 % of elevated-age windows, all fixtures green.
- **B2** `ratectl = shadow|live|off`; **B3** retire the switches in batches on the registry table and delete `aimd.rs`, `ceiling_learn.rs`, `LinkLoop`/`AgeLoop`, the P1/P3/P4/P6 arms.

## Phases and status

| Phase | What | Kill switch | Status |
|---|---|---|---|
| **B-opener** | Slow-start for the session opener — `encode::slow_start` (pure law) + the governor cap on the opening ceiling **and floor** | `rate_slow_start` (default **OFF**) | shipped **inert** in 0.4.55 (#1262, #1265 capped only the ceiling, which `set_ceiling` raised back to the flat 1.5 M floor); fixed in 0.4.56 (#1275) and **field-verified engaging** — AC0a done, AC0b open |
| B0 | `encode/sim.rs` + CSV trace replay, four fixtures | n/a (test-only) | not started |
| B1 | `ratectl` shadow beside the governor, `shadow_*` heartbeat fields | `ratectl = shadow` (default) | not started |
| B2 | Flip to `live` on the flip criterion | `ratectl = shadow\|live\|off` | not started |
| B3 | Retire the eight estimators and their switches | `ratectl = off` for one release | not started |

**B-opener** was not in the original plan; it was added because the field
evidence on #1243 showed the opener over-driving from *both* directions on
the same host in one day — the remembered `6_134_627` (6287 ms of paint) and
the nominal `2_550_000` into a path measured at `213_180` (1550 ms, six
windows to recover). Both are commitments made before any evidence exists,
which the full controller would also have to solve; shipping the opener first
is cheap, is independently testable, and only ever *removes* an unevidenced
commitment — a proven floor (FR-59 P8's remembered-slow-pair open) still wins.

### The constrained cell (why this was blocked, and now is not)

`constrained` is measured from the nominated ICE pair
(`detect_constrained_transport`, `peer.rs:1384`), so the constrained posture
exists only on a path that actually relays. Field-measured 2026-09-03:
CORPLAP-1 on 0.4.55 took a **direct** pair (`tgt=12096000`, `c=false`, paint
age 11-42 ms), so slow-start could not engage and the A/B could not run —
the arc's constrained measurements had all been opportunistic, taken whenever
one laptop's corporate VPN happened to be up.

`ice_relay_tcp` (a config key since #1271; `ROOMLERD_ICE_RELAY_TCP` before
that) pins ICE to a real TURN relay, which is what virtual-desktop mode
already used. Both arms of an A/B then run on **one host, one build, one
encoder, minutes apart**, differing in exactly one flag. It is a test pin and
must be cleared afterwards.

### The cell must be able to FAIL, not merely be constrained

Field-measured 2026-09-03, and the reason AC0b is still open. With
`ice_relay_tcp` alone the cell was genuinely constrained — `host=0 srflx=0
prflx=0 relay=4 relay_tcp=true`, `c=true` in every heartbeat, and the relays
were the **public coturn** (`relay_addr_is_fast_local` was never in play, so
`local_turn` is not the lever) — and arm A still could not be made to fail:

```
13:09:32 tgt=2550000 c=true age=Some(37)      <- the field constant, opening
13:09:36 tgt=2737500 c=true age=Some(30)      <- the AIMD CLIMBS
...      tgt=3000000                          <- reaches the cap and stays
```

Flat age, no queue growth, no coarsening, no skips. A 2.55 Mbps opener into a
relay carrying ~3 Mbps is not an over-drive; the original field case needed a
~213 kbps pipe.

🔑 **An arm that cannot fail is not a baseline.** A constrained *posture* is
necessary and nowhere near sufficient — the pipe has to be thin enough that
the opening commitment is actually wrong.

`relay_max_kbps` (a config key since #1276; default 3000) is `bitrate_cap` on
a constrained path. Raising it to `12000` against the same real coturn gives a
genuine ~4× over-drive, because a 1920×1200 pair resolves ~12 Mbps: the pipe,
the encoder and the relay are all real, and the only thing changed is **what
the encoder believes it may use** — which is exactly the mistake this phase
exists to stop. Clear both pins when the measurement is done.

## Acceptance criteria

- [x] AC0a — **the ramp engages and opens where it says.** Field-verified 2026-09-03 on 0.4.56, CORPLAP-1: `FR-63 slow-start armed open_bps=300000 ceiling_bps=2550000`, against an arm-A opener of `2_550_000` on the same host and build — an 8.5× smaller opening commitment. `slf=Some(600000)` on the first window with `gp=None` proves the **floor descent** did it, since the P1 relief cannot fire without a measurement; on 0.4.55, which capped only the ceiling, the opener was pinned at the flat 1.5 M and the ramp was inert.
- [ ] AC0b — **the ramp removes the harm.** Not yet measured: it needs an arm A that actually fails. See "the cell must be able to fail" below.
- [ ] AC1 — All four fixtures green in `cargo test -p roomlerd --lib` on the default build.
- [ ] AC2 — One release of shadow logs across the fleet meets the flip criterion, report attached.
- [ ] AC3 — Field A/B (`scripts/rc-ab.sh`) on CORPLAP-1 (QSV, corp VPN) and CORPLAP-2 (real relay): paint age p90 ≤ 0.4.50's and no window with a standing queue while the encoder tracks (FR-62).
- [ ] AC4 — FR-59 AC4 (< 600 ms sustained on the airport-class link) closes here.
- [ ] AC5 — The eight replaced estimators and their six switches are deleted; `ratectl = off` restores the prior behaviour for one release.

## Open decisions

- Whether the viewer computes the delay slope (recommended; the data is there) or sends per-frame pairs.
- Follower (multi-viewer) folding: worst follower's `rx_bps/queue_ms` into the same window.

## Out of scope

- The encoder apply path (FR-62, must land first); ICE path selection (FR-64); encoder-bound pacing/downscale (kept).

## Related

FR-59 #1163, FR-35 #922, FR-15, FR-1 #767, FR-62, FR-64.
