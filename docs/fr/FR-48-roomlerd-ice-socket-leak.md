# FR-48 — roomlerd leaks WebRTC-ICE UDP sockets on the relay node

**Issue:** gjovanov/roomler-ai#1086 · **Surfaced by:** FR-19 box 886 relay-node socket census ([#805](https://github.com/gjovanov/roomler-ai/issues/805#issuecomment-5484369356)) · **Status:** proposed (investigation)

## Goal
Eliminate a slow UDP-socket accumulation in `roomlerd`: ephemeral WebRTC-ICE host-candidate sockets that are freed only on a control-WS reconnect, not when their `RTCPeerConnection` ends. FR-19's F6 census box cannot pass while this leaks, so this FR is its prerequisite.

## Field evidence (0.4.33, `scw-m2-asahi`)
Hourly `ss -H -uanp | grep -c roomlerd`, 24 samples over 22 h:
- From a reconnect-reset baseline of **16** the count climbed **monotonically ~+3/hour to 64 over 15 h** (no downward fluctuation), reset on a control-WS reconnect, then climbed again.
- Session-1 on **0.4.23** had OSCILLATED (16↔62↔10↔15↔0), no monotonic climb → the leak correlates with **0.4.23→0.4.33**.
- All sockets are ephemeral `0.0.0.0:<random-high-port>` **UNCONN** (only 1 is mDNS `:5353`) = the WebRTC-ICE host-candidate signature (CLAUDE.md socket-leak note).
- **NOT org-relay-specific** — it climbed while the org relay was OFF and the operator idle. General `roomlerd`, not FR-19.
- Bounded by the ~15 h reconnect reset (does not exhaust between reconnects) — but a real leak: the 2026-08-22 incident showed this class can exhaust the ephemeral range and take host DNS down.

## Design / investigation
The 2026-08-22 fix added `close()` + a `Drop` net to `tunnel_core::transport::webrtc_dc::TunnelPeer` (mirroring `AgentPeer`) because *"an `RTCPeerConnection`'s UDP sockets are owned by tasks the ICE agent spawned, not by the struct, so an `Arc` drop leaves every one live."* This leak is the **same class on a different path** — one that creates an `RTCPeerConnection` / gathers ICE and is dropped without `close()`. Paths to audit (P1):
- **carrier direct-upgrade probes** — the "relentless re-upgrade" ladder re-attempts direct for DERP-locked peers periodically; a socket leaked per attempt would give exactly the observed ~+3/h, org-independent. **Prime suspect.**
- org-relay reachability probes; STUN/srflx refresh; the caps-probe child;
- any `AgentPeer`/`TunnelPeer` construction on an **error path** that returns before `close()` (the 2026-08-22 fix noted `run_tunnel_session` has many `?` early returns).

## Phases
| Phase | What | Kill switch |
|---|---|---|
| **P0** | reproduce + characterize via the census — **DONE** (evidence above) | n/a |
| **P1** | find the uncovered `close()` path — RUST_LOG on a fleet node to catch the periodic socket-open + a code audit of the paths above | n/a |
| **P2** | add explicit `close()` + `Drop`; re-run the census and show it flat over 24 h | revert the close() |

## Acceptance criteria
- [ ] The leaking `RTCPeerConnection`/socket-owning path is identified with a `file:line`.
- [ ] It gets an explicit `close()` + a `Drop` net (mirroring `AgentPeer`/`TunnelPeer`), with a test.
- [ ] The relay-node UDP socket census is **flat over 24 h** on the fixed build — which closes **FR-19 F6**.

## Out of scope
- Org relay (its sockets are raw UDP, not the cause). FR-19 F6 is the *consumer* of this fix, not this FR.

## Field-verification log
| date | version | finding |
|---|---|---|
| 2026-08-31 | 0.4.33 | **Census caught it.** +3/h monotonic climb (16→64 over 15 h), reset on reconnect; ephemeral UNCONN sockets (1 mDNS); 0.4.23→0.4.33 correlation; org-independent. [#805 finding](https://github.com/gjovanov/roomler-ai/issues/805#issuecomment-5484369356). |
