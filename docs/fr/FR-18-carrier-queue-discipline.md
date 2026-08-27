# FR-18: Carrier queue discipline

Status: **in progress** (2026-08-27). Tracking issue: `FR-18` (#801). Child of FR-1;
the lever that matters most for corp-VPN hosts.

## Why this is the priority — the LAN-relay test came back negative

Before committing to queue work, we tested whether a LAN-local DERP relay could serve
the two problem hosts, since all six machines share one desk and one Wi-Fi
(`192.168.68.0/24`). Method: a controlled TCP listener on neo16 (`192.168.68.126:47443`)
with an explicit inbound firewall rule, verified reachable from neo16 itself on both
loopback and its LAN address, then probed from each VPN host over Fleet RPC.

| host | VPN client | LAN route | result |
|---|---|---|---|
| CORPLAP-3 | Cisco AnyConnect | `192.168.68.0/24 → 10.138.80.1` (VPN gw) **metric 1**, beating the on-link WLAN route at metric 256 | unreachable — every LAN target, including the router |
| CORPLAP-1 | Check Point Endpoint | clean: only the on-link WLAN route | reachable on paper, **times out in practice** |

The listener recorded only neo16's own two self-tests; neither VPN host ever arrived.
neo16 holds live ARP entries for both machines (`192.168.68.106`, `.119`), so L2 is fine
and the AP is not isolating clients — the block is inside each VPN client. AnyConnect
enforces "no local LAN access" by hijacking the LAN prefix in the routing table; Check
Point enforces it in its endpoint firewall with the routes left intact.

Both are corporate policy on managed devices. **Working around them would be VPN policy
evasion and is out of scope.** The consequence is the load-bearing one: for these hosts
the DERP relay is **structural, not a fallback** — so the carrier's queue discipline is
not one lever among several, it is the whole game.

## Problem

A video frame crosses three stacked buffers on its way out, and the first two are ours:

| buffer | depth | at 3 Mbps |
|---|---|---|
| DERP outbound queue (`transport/derp.rs`, `OUTBOUND_QUEUE = 512`) | 512 frames ≈ 660 KB | ≈ **1.8 s** |
| kernel TCP send buffer on the DERP WS (no `SO_SNDBUF` cap) | autotuned, often MBs | 0.5–2 s |
| SCTP reliable+ordered queue above (FR-17 #799) | unbounded | seconds under loss |

Measured consequence: `send_wait_max_ms` of **10 263 ms** on CORPLAP-1 (hevc_qsv), 4 740
(vp9_qsv), 1 870 (h264_qsv), 907 on CORPLAP-3 (av1_qsv) — the time ONE frame spent inside the
DataChannel send call, while encode ran 8–12 ms and our own `bytes_inflight` held tens
of KB. The agent is healthy; it cannot hand bytes to the wire.

Two specific defects, both ours:

1. **The queue drops the wrong end.** `send_to` uses `try_send`, which discards the
   **newest** frame on overflow. For a loss-tolerant real-time carrier that is
   backwards: under sustained overload we retain 512 stale frames and throw away the
   fresh ones, so the receiver is served old data and the queue never refreshes.
2. **The queue is sized in frames, so its latency contribution scales with the
   reciprocal of the rate.** 512 frames is 33 ms at 160 Mbps and 1.8 s at 3 Mbps — and
   3 Mbps is exactly where these hosts live.

A datagram carrier that buffers instead of dropping converts loss into latency. That is
the bufferbloat pathology, one layer below where we have been looking.

## Design

**A. Deadline-aware dropping.** Stamp each frame on enqueue; the WS writer discards any
frame older than `derp_queue_max_age_ms` (default 100 ms) instead of sending it. This
bounds our layer's latency contribution **in time**, independent of rate, and it is
self-correcting: under overload the writer discards the stale head quickly and reaches
fresh frames, which is drop-oldest semantics without a custom queue structure.

**B. Cap the kernel send buffer** on the DERP TCP socket (`socket2::SockRef`, the
pattern already used in `driver.rs`). Without it, a frame the app just decided to send
can still sit for seconds in a kernel buffer the age check cannot reach — the app-level
discipline would simply drain into a deeper queue below it.

**C. Count both drop causes** (`dropped_stale`, `dropped_full`) and surface them, so the
next reader can tell "the carrier shed load" from "the carrier stalled".

Kill switch `derp_queue_max_age_ms = 0` restores the pre-FR-18 behaviour.

⚠️ Deliberately NOT drop-oldest via a custom queue: the age check achieves the same
effect at the consumer with no new data structure and no change to the mux's ownership
model. Revisit only if measurement shows the discard loop itself is a cost.

⚠️ This does not fix the base RTT (86–210 ms for a same-desk pair, because the relay is
on the internet). It fixes the queue *on top of* that RTT, which is where the seconds
are.

## Acceptance criteria

- [ ] On the CORPLAP-1 ↔ CORPLAP-3 relay pair under drag: `send_wait` p99 < 250 ms
      (was: 10 263 ms max) and no `send_wait` sample above 1 s.
- [ ] `dropped_stale` becomes non-zero under load while delivered fps does NOT fall —
      shedding stale frames must replace waiting, not replace delivery.
- [ ] Direct transports unchanged (a direct pair's `dropped_stale` stays ~0, since the
      queue never fills).
- [ ] The kernel send buffer cap is verified applied (`getsockopt`), not merely
      requested.
- [ ] Field: the FR-1 age pill on a relayed pair no longer shows multi-second
      excursions.

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.9 | LAN-relay test negative (above); baseline `send_wait` numbers recorded; FR filed. |
| 2026-08-28 | 0.4.10 | **Partial pass.** Operator: "it works much much smoother now." Same host, same pair, split by emitting version: CORPLAP-3 `send_wait` **p99 2 995 → 200 ms**, max 7 826 → 1 778 ms, p50 unchanged (0.158 → 0.164 — the gate costs nothing at steady state), viewer age max 4 087 → 2 185 ms, skips 2 → 58 (the intended trade). CORPLAP-1 on 0.4.10: `send_wait` max 1.66 ms. **Not closed**: max is still 1 778 ms (target: no sample > 1 s) and age excursions are still multi-second; residual belongs to the layers this FR does not touch (SCTP HOL, FR-17 #799) plus the FR-15 floor bug, observed live here as `viewer_age_floor_ms = 2` on a 44 ms-per-leg path. ⚠️ `dropped_stale` could NOT be evaluated — the counter was added without a reader (`stale_drops()` has no consumers); that instrumentation gap must close before the shed-vs-deliver criterion means anything. |
