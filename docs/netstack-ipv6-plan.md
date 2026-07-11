# Netstack IPv6 — implementation plan

Status (2026-07-11):

- **Phase A shipped** (PR #85) — dual-stack netstack internals: smoltcp
  `proto-ipv6`, `IpAddr`/`SocketAddr` API widening, the derived-v6 ULA
  (`fd72:6f6f:6d6c::/48`, pinned), dual-addressed iface, ICMPv6 echo, v6
  direct-pair tests. Inert.
- **Phase B shipped** — v6 *routing* on both surfaces, with one refinement
  over the plan below: instead of installing a per-peer v6 `/128` in
  `router.rs`, the router **unmaps a derived-ULA destination to its embedded
  v4 at packet-extraction time** (`Router::dst_of_ip_packet`) and routes on
  the existing v4 table — zero per-peer v6 state, zero `install_peers`
  change. Inbound needed nothing (boringtun's `WriteToTunnelV6` was already
  handled). OS-TUN parity: `SystemTun::up` assigns the derived v6 `/96`
  (Linux `ip -6 addr replace`, Windows `netsh`, best-effort; macOS deferred
  like the v4 per-peer routes). Proven by
  `bridge_v6_tcp_echo_and_ping_over_wireguard` (v4-only peer install; TCP +
  ICMPv6 round-trip over the derived v6 through a real WG pair).
- **Phase C shipped** — dual-stack surfaces. SOCKS front: a genuine
  ATYP=IPv6 target dials over v6 when it's a derived-ULA address (v4-mapped
  unwraps; other v6 → instant host-unreachable); names keep resolving to v4
  (universal — v6 is derived from it). MagicDNS answers `AAAA` with the
  derived v6 (default-on; `ROOMLER_AGENT_DNS_AAAA=0` reverts to A-only — the
  mixed-fleet escape hatch, since an old peer's OS doesn't own its derived v6
  and v6 toward it blackholes; happy-eyeballs apps fall back). `roomler ping`
  accepts v6 literals and `-6/--ipv6` resolves a name to the peer's derived
  v6 (`Request::Ping.prefer_v6`, serde-default so old daemons/clients
  interop).
- **Phase D shipped** — v6 visibility, with one refinement: **no wire/DB
  `overlay_ip6`** (derivation makes it redundant and it would be a
  consistency liability). The overlay runtime derives + publishes
  `self_ip6`/`overlay_ip6` into the LocalAPI view (`NodeStatus`/`PeerInfo`,
  serde-default), so `roomler status`/`peers`, the tray Devices view, and —
  via a TS mirror of the derivation (`ui: deriveOverlayV6`) — the admin
  overlay-nodes table all show both addresses with zero server change.

The original plan follows. The netstack (userspace smoltcp TCP/IP stack,
feature `overlay-netstack`) was IPv4-only when written. This document plans
dual-stack (IPv4 + IPv6) support: what changes, in what order, and the
decisions that make it tractable.

## Why (and why it can wait)

The overlay already works fully on IPv4 — a `100.64.0.0/10` CGNAT range (~4M
addresses, far beyond current scale). IPv6 is **parity + future-proofing**, not a
capacity need:

- **Tailscale parity.** Tailscale is dual-stack (each node gets a `100.x` v4 and
  a `fd7a:115c:a1e0::/48` ULA v6). A "Tailscale-alternative" is expected to be
  dual-stack eventually.
- **v6-preferring / v6-only apps.** Some services bind v6 first (or only); some
  corp networks are v6-internal. A userspace mesh that can't carry v6 can't reach
  those.
- **Not urgent.** Nothing is blocked on it today. Treat this as a deliberate,
  incremental effort, gated behind the existing `overlay-netstack` feature and
  inert until a node actually has a v6 address.

## Current v4-only shape (what this plan generalizes)

All in `crates/tunnel-core/src/overlay/` unless noted:

- **`netstack.rs`** — smoltcp iface with **`proto-ipv4` + `medium-ip`** (see the
  crate's `smoltcp` feature list in `crates/tunnel-core/Cargo.toml`). One IPv4
  CIDR on the iface (`Netstack::start(self_ip: Ipv4Addr, prefix, mtu)`). Public
  API is IPv4-typed throughout: `NetstackHandle::{connect(SocketAddrV4),
  listen(u16), udp_bind() -> NsUdpSocket, ping(Ipv4Addr, timeout)}`;
  `NsTcpStream.peer: SocketAddrV4`; `NsUdpSocket::{send_to,recv_from}` over
  `SocketAddrV4`; the poll loop's `sockaddr_v4_of(IpEndpoint) -> Option<SocketAddrV4>`
  and `echo_reply_seq` (parses **`Icmpv4`** only). One `icmp::Socket` bound to a
  per-instance random ident for echo.
- **`netstack_socks.rs`** — `resolve_overlay_host(&OverlayView, host) ->
  Option<Ipv4Addr>` (literal v4 → IPv4-mapped-v6 unwrap → peer name / first DNS
  label). SOCKS5 CONNECT + UDP-ASSOCIATE dial `SocketAddrV4`. ATYP=IPv6 is
  accepted only as an IPv4-mapped address; a genuine v6 is rejected.
- **Overlay addressing / routing** — each node carries a single IPv4 overlay
  address. `OverlayNode` / `NetmapPeer` (`crates/remote_control/src/...` +
  `signaling.rs`) expose `overlay_ip: String` (one v4). The WG crypto-router
  (`overlay/router.rs`) maps `overlay_ip -> wg_public_key`. MagicDNS
  (`overlay/dns.rs`) answers A records from a `name -> Ipv4Addr` map.
- **Surfaces** — `roomler ping` (`localapi.rs` + agent `localapi_state.rs`
  `resolve_overlay` + `overlay.rs` `NsPinger`) and the tray Ping button
  (`agents/roomler-agent-tray`) are all `Ipv4Addr`-typed.

## Key decision that shrinks the work: derive v6 from v4

**Assign each node's overlay IPv6 deterministically from its overlay IPv4** —
embed the 32-bit v4 in a fixed ULA `/96` (Roomler's own ULA prefix, e.g.
`fd7a:...::/96`, chosen once). So `100.64.0.5` → `fd7a:<roomler>::100.64.0.5`
(the v4 as the low 32 bits, à la IPv4-mapped but in a ULA).

Consequences:

- **No server allocation change.** A node self-derives its own v6 from the v4
  the server already assigns; it derives every *peer's* v6 from the peer's
  published v4. The `overlay/router.rs` install step adds the derived v6 `/128`
  → the same `wg_public_key` as the v4, so v6 packets route to peers **with no
  netmap/wire change**.
- Publishing `overlay_ip6` in the netmap/models becomes **optional** (nice for
  MagicDNS AAAA + admin UI clarity, but not required for routing) → deferrable to
  a later phase.
- Backward compatible: an old (v4-only) agent simply has no v6 and answers no v6
  traffic; v4 keeps working unchanged. v6 reachability needs *both* ends on the
  new code — an additive capability, never a regression.

(Independent, non-derived v6 allocation with a server-side table remains an
option if we ever want v6 without a v4, but derivation is the pragmatic default.)

## Phases

### Phase A — netstack internals, dual-stack (inert in prod)

1. **smoltcp**: add `proto-ipv6` to the `smoltcp` features in
   `crates/tunnel-core/Cargo.toml`. On `medium-ip` there is **no NDP/RA** (no L2),
   so v6 is simpler than on a real NIC — just addressing + ICMPv6.
2. **Generalize the netstack API** from `Ipv4Addr`/`SocketAddrV4` to
   `IpAddr`/`SocketAddr` (`std::net`): `connect`, `udp_bind`/`NsUdpSocket`,
   `ping`, `NsTcpStream.peer`, `Control::{Connect,Ping,UdpBind}`,
   `sockaddr_v4_of` → `sockaddr_of` (handle `IpAddress::Ipv6`). Mechanical but
   touches every signature; smoltcp's `IpEndpoint`/`IpAddress` are already v4-or-v6.
3. **Dual-address the iface**: `Netstack::start` assigns both the v4 CIDR and the
   derived v6 `/128` (+ the ULA on-link prefix so peers are on-link, mirroring how
   the v4 network prefix makes peers on-link today).
4. **ICMPv6 echo**: add a second icmp socket for v6 (smoltcp's `icmp::Socket`
   handles both; bind a v6 ident) and an `Icmpv6Repr`-based `echo_reply_seq6`.
   Wrinkle: **ICMPv6's checksum covers a pseudo-header** (src+dst addrs), so
   `Icmpv6Repr::emit` needs the addresses — unlike `Icmpv4Repr::emit`. `do_ping`
   picks v4/v6 by target family. `auto-icmp-echo-reply` already answers both, so a
   netstack host stays pingable over v6 too.
5. **Tests**: mirror `direct_pair_{tcp_echo,udp_echo,icmp_ping}` on a v6 ULA pair
   (two netstacks cross-linked, v6 CONNECT/UDP/ping round-trips). Keep all v4
   tests green.

Ships behind `overlay-netstack`, inert until Phase B routes v6.

### Phase B — routing (client-side only)

- On netmap apply (`overlay/runtime.rs` `install_peers`), for each peer derive
  its v6 from its published v4 and add the v6 `/128` → peer pubkey in
  `overlay/router.rs` alongside the v4. Self-derive our own v6 for the iface.
- After Phase B, a v6 packet to a peer's derived address rides the same WG
  carrier as v4 (WireGuard is L3-opaque; only the router table needs the v6 key).

### Phase C — surfaces (dual-stack UX)

- **SOCKS front** (`netstack_socks.rs`): `resolve_overlay_host -> IpAddr`; route
  a genuine ATYP=IPv6 overlay target; DOMAIN resolves to v4 or v6 (prefer v4 for
  compatibility, fall back to v6). UDP-ASSOCIATE the same. Reuse the SOCKS UDP
  header's native v6 ATYP.
- **MagicDNS** (`overlay/dns.rs`): answer AAAA from a `name -> (v4, v6)` map;
  keep A working.
- **`roomler ping` + tray**: `Ipv4Addr` → `IpAddr` in `localapi.rs`
  (`Request::Ping` already takes a string target; `resolve_overlay` returns
  `IpAddr`), the agent `NsPinger`, and the tray `cmd_ping`. The tray button can
  target the v6 (or offer both).

### Phase D — optional server/UI

- Add `overlay_ip6` to `OverlayNode`/`NetmapPeer` + the join/netmap wire (behind
  a version gate) so the UI shows v6 and MagicDNS doesn't have to derive. Admin
  device list shows both addresses. Only needed if we want explicit (non-derived)
  v6 or UI parity; derivation covers routing without it.

## OS-TUN (`overlay-l3`) parity

The netstack is one overlay surface; the OS TUN (`SystemTun`) is the other. For
consistent dual-stack, the OS-TUN path (`overlay/tun.rs` + `runtime.rs`) should
also assign the derived v6 to the `roomler0` device and install the v6 route.
This is a **sibling effort** to Phases A–C (same derivation, different plumbing:
`ip -6 addr add` / Windows `netsh` vs smoltcp iface addrs). Recommend doing it
alongside Phase B so both surfaces gain v6 together; otherwise v6 is
netstack-only.

## Risks & wrinkles

- **API churn.** `SocketAddrV4 → SocketAddr` ripples through every netstack
  signature + the SOCKS front + ping + tray. Large but mechanical diff; stage it
  as its own commit within Phase A so the v6 *behavior* changes are reviewable
  separately from the type widening.
- **ICMPv6 pseudo-header checksum** (see A.4) — the one non-mechanical netstack
  difference from v4.
- **Interop.** v6 reachability needs both ends upgraded; always keep v4 as the
  universal fallback so a mixed fleet never regresses.
- **ULA prefix choice** is a one-way decision (baked into every derived address).
  Pick a random `fd00::/8` ULA `/48` once and pin it as a constant, documented
  here. (Tailscale uses `fd7a:115c:a1e0::/48`; choose our own.)
- **Value/urgency.** Confirm demand before spending the OS-TUN + surfaces effort;
  Phase A alone (netstack v6, tested, inert) is cheap insurance and can land well
  ahead of a real need.

## Recommended sequencing

1. **Phase A** — netstack v6 internals + tests, behind `overlay-netstack`, inert.
   Self-contained, ~1 PR, no prod impact. Good first cut.
2. **Phase B** (+ OS-TUN parity) — derive + route v6 on both surfaces.
3. **Phase C** — SOCKS/DNS/ping/tray dual-stack.
4. **Phase D** — optional server/UI, only if explicit v6 or UI parity is wanted.

Field-validate at Phase B/C the same way v4 was (throwaway netstack node on the
live overlay, `roomler ping <peer-v6>` + a v6 SOCKS CONNECT). The autonomous
field-test recipe is in the `project-netstack` memory.
