# Warm relay — a UDP relay leg that survives the corporate VPN (C4 design)

> **Scope.** The guaranteed-UDP floor for flow-lifecycle networks: one
> standing TURN/UDP allocation, established whenever UDP works and kept
> alive forever, so that when the VPN comes up and kills all *fresh* UDP,
> the relay tier keeps a grandfathered UDP leg instead of degrading to
> TURNS/TCP head-of-line blocking.
>
> Status: **DESIGN — no code yet.** Finalize against two field inputs due
> after the next VPN-on window on winhost-a: (1) W5 phase-2 — does the
> grandfathered DIRECT-socket mapping survive VPN connect? (2) how does
> QUIC-over-TURN behave when only the anchor's client leg is TCP?
>
> Companion docs: carriers and tiers in
> [`overlay-communication.md`](./overlay-communication.md); the rendezvous
> mechanics this builds on in the W6 arc (rc.364–367: permission
> visibility, multi-IP permits, `pick_anchor_relay_endpoint`).

---

## 1. The network this is for

Check Point-class corp VPNs (winhost-a, field-proven 2026-08-14) block
**every fresh outbound UDP flow** — any port, any destination, including
DNS — while **grandfathering flows that already exist** in the session
table, as long as ~20–25 s keepalives keep them assured. Consequences
today, on a host that connects the VPN:

- STUN dies → srflx NONE → the host becomes the universal relay ANCHOR.
- Its per-pair TURN allocations are created *after* pairing demand, i.e.
  post-VPN → fresh UDP → blocked → the tiered allocator falls to
  **TURNS/TCP:443**. The relay leg is TCP: loss on that leg stalls every
  peer's packets behind retransmit (the 374 ms ping-spike class).

The W5 stable-port work already applies the flow-lifecycle insight to the
**direct** socket. C4 applies the same insight to the **relay** leg.

## 2. Core mechanism

**One standing "warm" TURN allocation per host** (per org is unnecessary —
see §5 sharing):

1. **Establish when UDP works**: at boot on a friendly network, or in any
   VPN-off window. The W5 SEEKING/ESTABLISHED srflx task already knows
   exactly when UDP egress starts working — its NONE→SOME transition is
   the trigger to (re)establish the warm allocation.
2. **Keep alive forever**: TURN allocation refreshes (and permission
   refreshes) ride the same 5-tuple and double as the CP keepalive. The
   flow stays in the session table across VPN connect.
3. **Survive credential expiry without losing the flow**: ephemeral
   use-auth-secret credentials expire, and coturn validates them on
   refresh. Before expiry, request fresh creds and **re-allocate on the
   SAME socket** — same 5-tuple, flow stays grandfathered. The relayed
   address `R` changes on re-allocation → re-advertise (rare, ~hourly).
4. **Use as the anchor's rendezvous**: when this host is the single-relay
   ANCHOR and holds a live warm allocation, hand pairs the warm
   `(conn, R)` instead of allocating per-pair. Peers dial `R` exactly as
   today — post-#453 the dialer positively identifies coturn-IP
   endpoints, so advertising the warm `R` through the existing endpoint
   union is safe and picked correctly.

## 3. The honest limitation

A warm allocation **also needs a UDP-working window to establish**. A host
that *boots inside* the VPN never gets one — for that cohort TURNS/TCP
remains the true floor, unchanged. C4's claim is narrower and still worth
it: a host that was EVER on friendly UDP (nightly at home, dock/undock,
lunch-break hotspot) carries its UDP relay leg into the VPN.

## 4. Why one allocation must be shared (and how)

RFC 5766: **one allocation per 5-tuple.** A single warm socket cannot hold
per-pair allocations, so the warm allocation is shared by every pair that
needs this host as anchor:

- **Permissions**: per-IP, already multi-target since rc.365
  (`extra_permission_targets`); open one per dialer as pairs form.
- **Inbound demux**: data indications carry the from-address, but
  identity comes from WireGuard itself — feed warm-allocation inbound
  into the **carrier plane's receiver-index/auth demux**, exactly like a
  direct plane sock. The multi-org plane already routes by cryptographic
  identity, not source; the warm allocation becomes one more attached
  sock (a "relay-backed plane carrier").
- **Outbound**: per-peer `send_to(peer_srflx)` through the shared conn —
  the raw relay path unchanged, just over a shared allocation.
- **QUIC**: a single quinn server endpoint on the warm allocation can
  accept MANY dialer connections — but today's per-link
  `QuicPeer`-wraps-the-conn shape breaks on a shared conn (two wrappers
  would steal each other's datagrams). **v1 ships RAW WG over the warm
  allocation**; QUIC-over-warm is v2, gated on an endpoint-per-allocation
  refactor (one accept loop, connections handed to links).

## 5. Protocol + server surface

The server grants TURN creds per pair today (`OverlayRelayRequest`). The
warm manager needs a **pair-less grant**:

- `ClientMsg::OverlayWarmRelayRequest {}` →
  `ServerMsg::OverlayWarmRelayGrant { ice, ttl_secs }`.
- ⚠️ New `ServerMsg` ⇒ **hello capability flag** (fleet rule): agents
  advertise `supports_warm_relay`; the server never sends the grant to an
  agent that didn't.
- Server side is a thin re-use of the existing ephemeral-cred mint; no
  coturn changes (a warm allocation is an ordinary allocation).

## 6. Staging

1. `overlay_warm_relay` config key (tribool, default **OFF**) + full
   config-surface plumbing.
2. **Stage 1 — establish + keep alive only** (no pairing use): field-gate
   on winhost-a = the allocation survives a VPN connect (refresh keeps
   succeeding over the grandfathered flow; `relay warm R=<addr> age=<h>`
   in status). Measurement-only, like every stage-1 in this program.
3. **Stage 2 — anchor prefers the warm allocation** for new pairs when
   live; per-pair allocation stays as fallback. Field-gate: VPN-on
   winhost-a pairs read `relay:turn/udp` (not `tcp`), and the disco
   `rtt_tail` p95 for those pairs drops to the UDP baseline.
4. **Stage 3 — QUIC-over-warm** (the endpoint refactor), only if stage-2
   raw shows the TCP-leg HoL is otherwise still visible end-to-end.

## 7. Interactions

- **W5 phase-2 (grandfathered direct mapping)**: if the direct-socket
  mapping survives VPN connect, srflx stays SOME and the host may not
  even need the anchor role — the warm relay then serves only the pairs
  whose other end can't punch. The two mechanisms are the same idea at
  two tiers; neither replaces the other.
- **Multi-org**: the plane is process-wide; ONE warm allocation serves
  every org (identity comes from WG auth at demux). Adverts go out
  per-org as usual.
- **PoP choice**: warm allocation pins to the nearest/healthiest PoP at
  establish time, NOT per-pair (`pair_key`) — dialers just dial the
  advertised `R` (post-#453 they no longer assume same-worker). Busy
  steering applies at establish/re-allocate time.
- **Failure honesty**: if the warm allocation dies mid-VPN (idle timeout
  missed, coturn restart — the 2026-08-12 worker outage class), fresh
  UDP is blocked and it CANNOT re-establish until the next VPN-off
  window; the host falls back to TURNS/TCP per-pair allocations exactly
  as today, and `status` must say `warm relay: LOST (VPN up — cannot
  re-establish until UDP returns)` rather than pretending.
