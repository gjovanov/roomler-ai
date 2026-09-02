# FR-63 — One delay-based rate controller for remote desktop

**Issue:** [#1243](https://github.com/gjovanov/roomler-ai/issues/1243) · **Status:** proposed · **Plan:** `immutable-doodling-neumann` (approved 2026-09-02)

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

## Acceptance criteria

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
