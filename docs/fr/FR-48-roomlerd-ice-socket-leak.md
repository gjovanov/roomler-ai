# FR-48 — roomlerd leaks unconnected ephemeral UDP sockets on the relay node

**Issue:** gjovanov/roomler-ai#1086 · **Surfaced by:** FR-19 box 886 relay-node socket census ([#805](https://github.com/gjovanov/roomler-ai/issues/805#issuecomment-5484369356)) · **Status:** **P1 answered, P2 fix landed — awaiting fleet release + a flat 24 h census**

## Goal
Eliminate a slow UDP-socket accumulation in `roomlerd` — measured at **~+6.5 sockets/hour, tracking uptime**, on every overlay node including ones with no remote-control traffic at all. FR-19's F6 census box cannot pass while this grows, so this FR is its prerequisite.

**The cause (field-traced 2026-09-01, 0.4.42):** the sockets are **glibc resolver sockets from `getaddrinfo`**. The STUN-vantage resolvers in `overlay/direct.rs` re-resolved `coturn-<region>.roomler.ai:3478` on **every** carrier/srflx probe pass via `tokio::net::lookup_host`; each lookup runs `std`'s blocking resolver on a blocking-pool thread, and glibc retains that thread's nameserver socket in its `_res` state. **P2 caches the resolved vantages behind a 300 s TTL.**

> ⚠️ **Everything in the two sections below is the ORIGINAL hypothesis and is now DISPROVEN.** It is kept because the dead ends are the useful part of the record — but do not act on it. It attributes the sockets to unclosed `RTCPeerConnection`s / WebRTC-ICE host candidates; that was falsified by a control host with zero RC traffic that leaks at the same rate, and "unconnected `0.0.0.0:*`" turned out to be **normal** for webrtc-rs (it uses unconnected sockets by design). The `16` baseline quoted below is also wrong — a fresh daemon holds **41**.

## Field evidence (0.4.33, `scw-m2-asahi`) — ⚠️ SUPERSEDED, see the log at the bottom
Hourly `ss -H -uanp | grep -c roomlerd`, 24 samples over 22 h:
- From a reconnect-reset baseline of **16** the count climbed **monotonically ~+3/hour to 64 over 15 h** (no downward fluctuation), reset on a control-WS reconnect, then climbed again.
- Session-1 on **0.4.23** had OSCILLATED (16↔62↔10↔15↔0), no monotonic climb → the leak correlates with **0.4.23→0.4.33**.
- All sockets are ephemeral `0.0.0.0:<random-high-port>` **UNCONN** (only 1 is mDNS `:5353`) = the WebRTC-ICE host-candidate signature (CLAUDE.md socket-leak note).
- **NOT org-relay-specific** — it climbed while the org relay was OFF and the operator idle. General `roomlerd`, not FR-19.
- Bounded by the ~15 h reconnect reset (does not exhaust between reconnects) — but a real leak: the 2026-08-22 incident showed this class can exhaust the ephemeral range and take host DNS down.

## Design / investigation — ⚠️ SUPERSEDED (the suspects below were all falsified)
The 2026-08-22 fix added `close()` + a `Drop` net to `tunnel_core::transport::webrtc_dc::TunnelPeer` (mirroring `AgentPeer`) because *"an `RTCPeerConnection`'s UDP sockets are owned by tasks the ICE agent spawned, not by the struct, so an `Arc` drop leaves every one live."* This leak is the **same class on a different path** — one that creates an `RTCPeerConnection` / gathers ICE and is dropped without `close()`. Paths to audit (P1):
- ~~carrier direct-upgrade / `public-dial` probes~~ — **RULED OUT (P1, 2026-09-01, [#1086 comment](https://github.com/gjovanov/roomler-ai/issues/1086#issuecomment-5485353447))**: `overlay/carrier_plane.rs` binds these **once per engine** behind `bind_gate`, stores them in `st.binds`, and handles the one re-bind race cleanly (aborts the new tasks, drops the new socket `Arc`s). Not a per-attempt leak. Remaining suspects: WebRTC-ICE (`peer.rs`/`webrtc_dc.rs` error paths), a QUIC/`quinn` endpoint, an STUN/srflx-refresh socket. ⚠️ the "WebRTC-ICE" attribution is a *signature* match, NOT confirmed — a headless relay does little WebRTC.
- org-relay reachability probes; STUN/srflx refresh; the caps-probe child;
- any `AgentPeer`/`TunnelPeer` construction on an **error path** that returns before `close()` (the 2026-08-22 fix noted `run_tunnel_session` has many `?` early returns).

## Phases
| Phase | What | Kill switch |
|---|---|---|
| **P0** | reproduce + characterize via the census — **DONE** (evidence above) | n/a |
| **P1** | find the path — **DONE via bpftrace**, no restart and no log-level change needed. Not an unclosed `RTCPeerConnection` at all: the sockets are **glibc resolver sockets from `getaddrinfo`**, reached through `tokio::net::lookup_host` in the STUN-vantage resolvers | n/a |
| **P2** | **cache the resolved vantages** (`STUN_DNS_TTL`, 300 s) so the periodic probe cycle stops re-resolving on every pass; re-run the census and show it flat | set `STUN_DNS_TTL` to 0 (every lookup misses ⇒ prior behaviour), or revert the commit |
| **P3** | *(follow-up, not in P2)* the same stack shows a **blocking `to_socket_addrs` running inside a task poll** — a latency hazard on a node with degraded DNS, independent of the socket count | n/a |

## Acceptance criteria
- [x] The socket-owning path is identified with a `file:line`. *(bpftrace, 0.4.42: `__socket ← __res_context_send ← getaddrinfo ← to_socket_addrs ← tokio task poll`; sites `crates/tunnel-core/src/overlay/direct.rs:1132/:1320/:1395`, all in the STUN-vantage resolvers. ⚠️ The original wording assumed an unclosed `RTCPeerConnection` — that framing was **wrong**; these are glibc resolver sockets, and WebRTC/RC was falsified by a control host with zero RC traffic.)*
- [x] The fix has a test. *(`stun_hostport_strips_scheme_and_query_for_every_form` locks the now-shared scheme-stripper — it is both the lookup key and the cache key, so a drift would desync them; `a_failed_lookup_is_not_cached` locks that a DNS failure is retried rather than remembered for a whole TTL.)*
- [ ] The relay-node UDP socket census is **flat over 24 h** on the fixed build — which closes **FR-19 F6**. *(needs the fix released to the fleet; the trackers are already in place on jupiter/mars/asahi.)*

## Out of scope
- Org relay (its sockets are raw UDP, not the cause). FR-19 F6 is the *consumer* of this fix, not this FR.

## Field-verification log
| date | version | finding |
|---|---|---|
| 2026-08-31 | 0.4.33 | **Census caught it.** +3/h monotonic climb (16→64 over 15 h), reset on reconnect; ephemeral UNCONN sockets (1 mDNS); 0.4.23→0.4.33 correlation; org-independent. [#805 finding](https://github.com/gjovanov/roomler-ai/issues/805#issuecomment-5484369356). |
| 2026-09-01 | 0.4.41/42 | **Scoping: three hypotheses falsified, then the path found.** Peer churn (19 min dead flat through the most churn-heavy window), `roomler exec` (10 execs, zero growth) and TURN re-allocation (4 events/day vs ~11 sockets/2 h) were each killed by measurement. **RC/WebRTC killed by a control host**: `mars` has ZERO RC heartbeats and 92 sockets at 10.1 h vs the relay host's 86 at 7.5 h *with* a permanent 3 Mbps session. Cross-host scan gave the clean rate — jupiter **36 @ 1.5 h**, zeus **35 @ 1.5 h**, mars **92 @ 10.3 h**, same role and same 15 peers ⇒ **~+6.5/h tracking UPTIME, not peers**. ⚠️ Two of my own measurements were wrong and are retracted: the assumed fresh-daemon floor of 16 (it is **41**, measured after the 0.4.42 auto-update restart), and "unconnected `0.0.0.0:*` is a leak signature" (webrtc-rs/ICE uses unconnected sockets **by design**). ⚠️ A 15-min window showed mars "flat" and nearly produced a *plateau* conclusion — at 6.5/h that window expects +1.6, i.e. noise. |
| 2026-09-01 | 0.4.42 | **P1 ANSWERED + P2 shipped.** `bpftrace` on jupiter (no restart, no log change) traced every `AF_INET/AF_INET6 + SOCK_DGRAM` creation to `__socket ← __res_context_send ← __res_context_query ← getaddrinfo ← std…lookup_host ← <(&str,u16) as ToSocketAddrs>::to_socket_addrs ← tokio task poll` — **glibc resolver sockets**, from `tokio::net::lookup_host` in `overlay/direct.rs:1132/:1320/:1395` (the STUN-vantage resolvers, re-resolving `coturn-<region>.roomler.ai:3478` on **every** probe pass with no caching). Each lookup lands on a blocking-pool thread whose glibc `_res` retains a nameserver socket. **P2** adds `lookup_host_cached` + a single shared `stun_hostport` behind a 300 s TTL, caching only successful non-empty results. ⚠️ **Filter by address family FIRST**: unfiltered, the dominant `socket()` caller is `sysinfo::Networks::refresh → getifaddrs`, which is **AF_NETLINK** and invisible to `ss -uanp` — chasing it would have been the wrong function. ⚠️ `overlay` is **off by default**, so a bare `cargo check`/`clippy` compiles none of this — the first green run was vacuous; use `--features overlay-l3`. |
