# Overlay NAT-traversal cascade

> Cross-ref: the L3 overlay mesh is part of the remote-control / tunnel
> subsystem ([`docs/remote-control.md`](./remote-control.md)). This doc covers
> how two overlay nodes pick a WireGuard **carrier** — and, in particular, how
> a NAT'd node reaches another without a relay hop. The Windows-firewall piece
> is [`docs/overlay-wfp.md`](./overlay-wfp.md); the exit-node routing on top of
> a carrier is [`docs/overlay-exit-nodes.md`](./overlay-exit-nodes.md).

## The carrier cascade

Every overlay peer link rides one **carrier** — the transport the WireGuard
datagrams travel over. The runtime picks the best one available, in priority
order, and demotes to the next tier if it can't establish:

| Tier | Carrier | When it wins | Flag (default) |
|---|---|---|---|
| LAN direct | UDP on the shared interface socket | peer shares one of our /24s | `ROOMLER_NODE_OVERLAY_DIRECT` (**on**) |
| **A** direct-to-public | UDP via an unbound egress socket | peer's NIC holds a public IP | `ROOMLER_NODE_OVERLAY_PUBLIC_DIRECT` (**off**) |
| **C** srflx hole-punch | UDP via the **punch socket** | both ends NAT'd (not both symmetric) | `ROOMLER_NODE_OVERLAY_SRFLX` (**on** since rc.200) |
| **D** single-relay | ONE coturn allocation + a raw dialer, QUIC-over-TURN | nothing direct works | `ROOMLER_NODE_OVERLAY_RELAY_SINGLE` (**on** since rc.200) |
| **D′** both-allocate relay | two coturn allocations (raw / QUIC) | single-relay off, or a mixed-capability pair | always available (fall-through) |

LAN direct and the relay predate this work (rc.131–rc.135; the relay is the
original path). **Phases A / C / D** are the NAT-traversal cascade. **C (srflx
punch) and D (single-relay) shipped default-ON in agent rc.200** after being
field-proven in a mars↔zeus netns NAT lab (cone↔cone → direct punch 0% loss
~0.6 ms; sym↔sym → single-relay 0% loss ~1.3 ms). A is still default-OFF
(public-on-NIC is rare and its own field arc). Each gate takes
`0`/`false`/`no`/`off` to disable. Single-relay needs the QUIC carrier
(`ReadyLink.single_relay` forces it): a raw relay carrier discards the recv
source, so an anchor can't reply to a symmetric dialer's coturn-observed port —
only quinn's server consumes it.

The relay always works but is the worst option: it adds a hop's latency, a
coturn dependency (dies on UDP-blocked / TLS-inspecting corp nets), and — for
the exit-node feature — a cross-NAT **hairpin** that never carried in the field.
Getting off the relay is the whole point.

## What each direct tier needs

- **LAN direct** — the peer advertised an `ip:port` sharing one of our /24s.
  Reliable L2, no NAT games. One socket per interface (bound to the interface
  IP + `IP_UNICAST_IF`-pinned on Windows) so a full-tunnel VPN can't steal the
  egress.
- **Phase A (direct-to-public)** — the peer's NIC holds a **public** IP (bare
  metal / Hetzner, not 1:1-NAT). A NAT'd client dials it directly; WireGuard
  endpoint-roaming + the exit-side *accept* path (below) complete the handshake.
  No STUN needed — the public side has no NAT filter to open.
- **Phase C (srflx hole-punch)** — **both** ends are NAT'd. Each learns its own
  public mapping via STUN (server-reflexive = "srflx"), advertises it, and the
  two dial each other simultaneously so both NATs' filters open. This is the
  classic UDP hole-punch, described in detail below.
- **Phase D (relay)** — last resort; see "Relay" below.

## Phase C: the hole-punch, precisely

Two facts about this codebase's WireGuard make the punch simpler than a generic
ICE agent:

1. **WG *is* the punch burst.** A direct carrier initiates bilaterally
   (`install_ready` / `add_direct_peer` with `initiate=true`), and boringtun
   retransmits the handshake INIT every ~5 s for ~90 s. So both ends are already
   firing INITs at each other on a tight cadence — no separate "punch packet".
2. **The netmap fan-out *is* the rendezvous.** When a node trickles its srflx
   (`rc:overlay.srflx`), the server fans a netmap delta to every peer within
   WS-delivery skew (~sub-second). NAT mappings/filters live ≥30 s, so the two
   ends don't need a shared clock — first attempts are naturally near-
   synchronous, and the periodic re-upgrade tick (below) closes any larger skew.

So Phase C is **not** a rendezvous protocol. It's five concrete pieces:

### 1. Dial from the socket that owns the advertised srflx ("the punch socket")

The load-bearing rule. A srflx mapping is created by *the socket that sent the
STUN query*. If we advertise the mapping from socket **S** but then dial the
peer from a different socket **P**, our INITs from P open a *different* mapping
the peer never dials, and the peer's INITs to our S-mapping hit our NAT's filter
(S never sent toward the peer) — so **both directions fail** on anything
stricter than full-cone.

Fix: `gather_srflx` returns each candidate paired with the socket it was
gathered on; the runtime records the first as `DirectCtx.punch` and
`install_srflx_direct` dials the peer's srflx **from that socket**. Now our
outbound INIT (a) rides the mapping we advertised and (b) opens our filter
toward the peer's srflx — exactly what port-restricted-cone needs. (Phase A
keeps dialing via the arbitrary-egress `public_sock`: the public peer has no
filter, so the mismatch is harmless there.)

### 2. Keep the srflx fresh (demux-routed STUN keepalive)

A UDP NAT mapping expires on an idle node (30 s – 5 min). A gather-once srflx
goes stale for a peer that joins later. The keepalive task (`run_srflx_keepalive`,
`ROOMLER_NODE_OVERLAY_SRFLX_KEEPALIVE_SECS`, default 20, `0` = off) re-runs a
STUN Binding on the punch socket every interval — both holding the mapping open
and detecting a change.

The punch socket's `recv_from` is owned by the overlay's demux loop, so the
keepalive can't read replies directly. The demux forwards any datagram that
carries the STUN magic cookie **and is not WireGuard-shaped** to a STUN sink the
keepalive drains. (The two wire shapes are disjoint: WG's 4-byte little-endian
type header leaves bytes 1..4 = 0, a STUN Binding message always has 0x01 in
byte 1 — so a WG data packet whose index bytes collide with the cookie is still
routed as WG.)

The STUN target is **pinned** once at startup (re-resolved only after several
failures): the fleet resolves `coturn.roomler.ai` to several workers, and an
unpinned target would make every DNS rotation look like a mapping change and fan
a network-wide re-trickle every tick. Re-trickle happens **only** when the punch
mapping actually changes; a STUN outage retains the last-known advert.

### 3. Time out a punch that never establishes

A failing punch sends no *data*, so it's invisible to the relay-fallback health
sweep (which watches the `tx`/`rx` data counters), and boringtun stops even
keepalives once the 90 s attempt expires — the carrier would zombie forever. A
lock-free `handshake_complete` flag (`PeerStats.handshake`, latched in
`process_inbound` the instant a session establishes) lets the sweep tear down a
srflx/public carrier that hasn't handshaken within its deadline (**srflx 12 s**,
**public-direct 30 s**) and fall back to relay. Once the handshake latches, the
normal data-traffic health check governs the established link.

### 4. Skip a punch that can't work (NAT-type probe)

Symmetric↔symmetric can never punch (neither can predict the other's per-
destination port). At startup each node probes its NAT mapping type — STUN the
punch socket against **two distinct targets**; same public mapping ⇒ `cone`
(punchable), different ⇒ `symmetric` — and advertises it (`OverlaySrflx.nat`,
surfaced as `NetmapPeer.srflx_nat`). A dialer skips the srflx tier **only** when
**both** ends are symmetric; any `cone`/unknown side still attempts (an unknown
stays optimistic — the tight deadline bounds a wasted try).

### 5. Retry without waiting for a netmap

A lapsed cooldown otherwise only takes effect on the next netmap; a quiet mesh
would never re-attempt direct after a fallback. A re-upgrade tick (~every 30 s)
re-runs the tier evaluation over the current netmap, retrying a lapsed direct
tier and driving punch convergence at large install skew.

### The accept side

Both A and C rely on the exit/peer *accepting* an inbound INIT from a source it
couldn't know in advance (a NAT'd dialer's mapping). The demux forwards an
unknown-source WG **handshake INIT** to the runtime, which **cryptographically
authenticates** it (a throwaway `Tunn` performs the full Noise-IK validation —
`parse_handshake_anon` alone proves only a *claimed* key) before installing or
re-pointing the peer onto the arriving socket + source. An authenticated INIT
that traversed both NATs is proof the pair can reach each other, so it bypasses
the srflx cooldown and clears that peer's strikes.

## Cooldowns and the never-poison rule (CC1)

Each direct tier (LAN / public / srflx) has its **own** failure cooldown, so a
routinely-missing punch can never poison the proven LAN or public-NIC path. LAN
and public escalate to a 24 h session-sticky deny after 2 failures (a false /24
— a VPN client pool — or an unreachable "public" endpoint). srflx uses a
**shorter 15 min** deny after 3 failures: a punch reflects the *current* NAT
pair, which changes when a host roams, so a day-long deny would wrongly outlive
it.

## Phase D: the relay (and why DERP, eventually)

The relay is coturn TURN, optionally upgraded to QUIC-over-TURN. It always works
for the **single-relay** case (peer → coturn relayed addr → allocation owner),
which the fleet's remote-desktop media already proves.

The open problem is the exit-node / cross-NAT case where **both** ends allocate
and coturn must hairpin between two of its own allocations — it never carried in
the field. The mutual permission bootstrap (PR #124, the stray `\x00` from both
ends) was necessary but not sufficient; a raw-UDP single-relay dialer also fails
for **symmetric** NAT (coturn sees a different source port than STUN reported).
The planned fix is a **DERP-style** pubkey-keyed relay: both peers dial *out*
(NAT-agnostic for any NAT type, including symmetric), the relay maps
`pubkey → connection` and forwards ciphertext. It reuses the existing
QUIC-over-TURN plumbing and sidesteps every coturn trap (permissions, the 300 s
refresh, srflx-must-be-permitted, symmetric port mismatch, the hairpin, worker
pinning). Phase C serves cone↔cone directly; symmetric pairs are what Phase D
must cover.

## NAT lab (for field-validating Phase C)

The direct-tier failure modes only reproduce behind real NATs. The lab uses two
throwaway libvirt VMs on the **mars** utility host (never a prod cluster node),
each behind its own nftables NAT gateway:

- `masquerade` ≈ endpoint-independent mapping + address-and-port-dependent
  filtering ≈ **port-restricted cone** — the punchable case.
- `snat --random-fully` ≈ **symmetric**.
- low `conntrack` UDP timeouts force the stale-srflx case; `conntrack -F` forces
  a mid-punch mapping rotation.

Server = prod `roomler.ai` (already wire-capable); test daemons run the branch
build via `--config` with `ROOMLER_NODE_OVERLAY_SRFLX=1` set only on them. The
matrix: cone↔cone punches ≤ ~10 s (both `Direct`, zero coturn allocations for
the pair); cone↔symmetric attempts once then relays; symmetric↔symmetric skips
up-front to relay; install skew converges via the re-upgrade tick; stale-srflx
re-trickles; and a same-LAN pair / Phase A path / flag-unset default stay
unchanged. See the P5 VM-field-test recipe for the enrollment + systemd
mechanics.
