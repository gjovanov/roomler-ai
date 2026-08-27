# FR-15: Relay age feedback — close the rate loop with the viewer's own clock

Status: **proposed** (evidence collected 2026-08-27, first field session of the FR-1 P7
age pill). Tracking issue: `FR-15` in gjovanov/roomler-ai/issues. Child of FR-1; the
successor to FR-10's open decision ("relay drainage needs a signal SCTP doesn't give us").

## Problem

On constrained transports the sender runs **open-loop at the nominal relay clamp**
(~2.5–3 Mbps). When the path's true capacity dips below that — corp TLS middlebox
throttling, TCP RTO stalls on the DERP leg, relay-server backpressure — the excess bytes
queue in layers **below every agent counter** (WG-over-DERP socket, the DERP server's
TCP toward the viewer's egress), and the viewer watches frames get progressively older.
Nothing on the agent can see it; the FR-1 P7 age pill can.

## Evidence (2026-08-27, first age-pill field read — operator + CORPLAP-3 heartbeats + carrier probes)

- **neo16 → CORPLAP-3** (relay, 85 ms): age starts ~60 ms, **climbs to ~120 ms and the
  sluggishness starts with the climb**. +60 ms at 3 Mbps ≈ 22 KB — matches the agent's
  observed inflight excursions (max 26 KB). Mild: path ≈ nominal, queue small but felt.
- **CORPLAP-1 → CORPLAP-3** (both corp-VPN, relay): age **>200 ms sustained, spikes >1000 ms**.
  During that exact session (14:38–14:41Z, AV1 @ 3 Mbps) CORPLAP-3's agent pipeline was
  EMPTY — `bytes_inflight` avg 1.5–9 KB / max 26 KB, `send_wait_max` <10 ms, zero
  skips, `settle_kf_suppressed` active — while the overlay pair's RTT probe bounced
  **111 → 196 → 382 ms**. A 26 KB agent queue cannot make 1000 ms of age: the backlog
  is in the DERP/TCP path, invisible to the sender.
- Same-day agent-side exonerations (encode ~12–15 ms, thrift on) rule out the FR-10
  lump class and the encode pipeline.

## Design

**A. Age over the existing feedback wire.** The viewer already reports
`rc:decodestat {fps, struggling}` once per stats window (rc.188). Add `age_ms` (the
window's average paint-age) and `age_min_ms` (the window's minimum — the path-floor
tracker). Old web sends no age → agent sees absent → loop stays off (back-compat).

**B. Constrained age-loop in the governor.** The agent learns the session's age FLOOR
(min over recent windows — same min-filter logic as the clock probe: the floor is the
path's propagation+decode cost, not queue). When `age_avg − floor` exceeds a slack
(~60–75 ms) for ≥2 consecutive windows, treat it as over-rate: **step the constrained
ceiling down ×0.85 per window** while the excess persists, floor at the relay area
floor (1.5 M). Recovery rides the existing AI climb (+ceiling/16 per 5 s) back to the
nominal clamp. Mirrors AIMD, but keyed on the only end-to-end signal the relay path
cannot hide. Kill switch `relay_age_feedback` (default ON), constrained-only —
direct already has the measured ceiling + byte gate (and FR-14 will own its episodic
holes).

**C. Heartbeat exposure.** `viewer_age_ms` + `viewer_age_floor_ms` in the pump
heartbeat, so field verification reads the loop server-side (`agent_logs`), not just
on the viewer's screen.

Deliberately NOT in scope: shrinking `constrained_queue_ms` (450 ms) — the evidence
shows the queue is mostly BELOW the agent, so tightening the agent's own budget
mostly adds skips without moving the age; revisit only if post-loop residuals point
at the agent again. Also not in scope: intra-refresh (spread-I) encoding — the right
long-term answer to residual IDR cost on thin pipes, but a separate arc.

## Acceptance criteria

- [ ] neo16 → CORPLAP-3: age plateaus ≤ ~90 ms during sustained drag (was: climbs to 120);
      no felt sluggishness onset.
- [ ] CORPLAP-1 → CORPLAP-3: no >1000 ms age excursions; sustained age within ~2× that pair's
      floor; heartbeats show `target_bps` stepping DOWN during the viewer's age spikes
      (the loop visibly reacting).
- [ ] A struggling/absent-age viewer (old web) leaves relay behaviour byte-identical
      to 0.4.7.
- [ ] Direct transports untouched (loop gated `constrained`).

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.7 | Baseline: the two readings above; FR filed. |
