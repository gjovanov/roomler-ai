# FR-72 — MagicDNS without an OS port

**Issue:** [#1382](https://github.com/gjovanov/roomler-ai/issues/1382) · **Status:** proposed · **Opened:** 2026-09-05

## Goal

MagicDNS resolves on every enrolled host, including one where another process
already owns `:53`. Today a single squatter on `0.0.0.0:53` removes the feature
entirely, silently, for the lifetime of that host.

## The failure, measured

CORPLAP-3 (Windows, corp-managed, AnyConnect), `roomlerd 0.4.66`:

```
roomler status
  magicdns   <suffix> (resolver DOWN, AAAA on, upstream 1.1.1.1:53)

Resolve-DnsName <peer>.<suffix>   ->  NO ANSWER
ping            <peer>.<suffix>   ->  host not found
```

The chain is four steps, and **every one of them is behaving as designed**:

1. `Get-NetUDPEndpoint -LocalPort 53` → `0.0.0.0 <- svchost`. A Windows service
   holds the wildcard.
2. Our bind fails. Measured directly on the host, as the daemon identity:

   ```
   bind 100.65.4.30:53            -> SocketException AddressAlreadyInUse (10048)
   bind 100.65.4.30:53 + SO_REUSEADDR -> AddressAlreadyInUse (10048)
   ```

   ⚠️ **`SO_REUSEADDR` does not rescue it.** A wildcard bind takes the port on
   every local address, so binding the specific overlay address is refused too.
3. `dns.rs:70` (`UdpSocket::bind(cfg.bind)`) returns the error, `dns_bound` is
   false, and `runtime.rs:2339` then **deliberately declines** to steer the OS,
   saying so at WARN (`runtime.rs:2345`): *"resolver did not bind :53 — NOT
   steering the OS (would blackhole the magic domain); names resolve via SOCKS
   only"*. That gate is correct and must stay — Windows NRPT is registry-global,
   so pointing it at a dead resolver would break the magic domain host-wide.
4. `Get-DnsClientNrptPolicy -Effective` → no roomler rule. Names do not resolve.

🔑 **Nothing here is a bug.** Each step is right; the feature is simply
unavailable, and the honest `resolver DOWN` in `roomler status` is the only
thing that makes it visible at all. This FR is about removing the dependency
that makes step 2 fatal.

## ⚠️⚠️ There is a SECOND blocker, and P1 alone does not clear it

Measured on the same host later the same day, after a restart that **won** the
`:53` race:

```
Get-NetUDPEndpoint -LocalPort 53   ->  100.65.4.30   <- us; the bind SUCCEEDED
                                       0.0.0.0

Get-DnsClientNrptRule              ->  OK count=1  .grox.roomler.ai -> 100.65.4.30
Get-DnsClientNrptPolicy -Effective ->  OK count=0

Resolve-DnsName <peer>.<suffix>    ->  NO ANSWER
```

Both queries were run **without** `-ErrorAction SilentlyContinue`, so `count=0`
is a genuinely empty table rather than a swallowed error.

So even with the bind succeeding and the NRPT rule **written and stored**, the
effective table is empty and the OS still does not send us the queries. It is
not the GPO-override theory either — the unfiltered policy is also `0` and there
is no GPO NRPT key. Why the rule is not applied is **not yet established**.

🔑 **Consequence for this FR's scope, stated plainly: P1–P3 fix the bind, and on
this host that is necessary but NOT sufficient.** Interception removes the
dependency on owning an OS port; it does nothing about an NRPT rule that fails
to take effect. A phase for the steer half is therefore part of the goal, not an
afterthought, and the acceptance criteria are written against *resolution
working*, not against *the resolver binding*.

⚠️ The failure is also **intermittent** — the same host reported `resolver DOWN`
and `active` hours apart, decided by who wins the `:53` race at daemon start. Any
verification must state which of the two states it measured.

## ⚠️ The obvious fix does not work, and this was measured

The operator's instinct was *"use another port, just like roomler SSH"*. Serving
on another port is fine; **steering the OS at another port is not**, on the one
platform that has the problem:

```powershell
Add-DnsClientNrptRule -Namespace '.test' -NameServers '1.2.3.4:5399'
Get-DnsClientNrptRule  ->  stored NameServers: []      # the port was DROPPED
```

NRPT nameservers are IP addresses; there is no port syntax, and the cmdlet
discards `ip:port` **silently** rather than refusing it. So an alternate port
alone leaves the OS with nothing to point at.

🔑 **But the SSH precedent is exactly right — its transferable part is
interception, not the port number.** Roomler SSH hit the identical problem
(`docs/roomler-ssh.md`): binding `overlay:22` fails with EADDRINUSE wherever
`sshd` holds `0.0.0.0:22`. It was not solved by moving to 2222 — 2222 is for
*coexistence during migration* — it was solved by intercepting the packets
**below the OS** in `split_tun::SplitTun`, so no OS socket is needed at all.
The same move is available here, and it is the design below.

## Key design — serve MagicDNS from the netstack, bind nothing

Divert UDP destined for `<self overlay ip>:53` out of the WireGuard bridge and
answer it in-process, exactly as SSH diverts TCP `:22`.

```
mesh ─▶ WgDevice ─decrypt─▶ SplitTun ──┬── dst == self_ip && udp && dport == 53
                                       │        → Netstack → the resolver
                                       └── everything else → the OS TUN
```

Two halves already exist and one does not:

| piece | state |
|---|---|
| in-process UDP sockets | **exists** — `netstack.rs:270` `NetstackHandle::udp_bind`, the SOCKS UDP-ASSOCIATE backend |
| the resolver itself | **exists** — `dns.rs`, and it is transport-agnostic apart from its `bind: SocketAddr` (`dns.rs:44`) |
| a UDP arm in SplitTun | **missing** — `split_tun.rs:84` defines `IPPROTO_TCP` and `:234` passes every non-TCP packet straight through |

The OS-facing contract does not change: NRPT still names the overlay IP, still
implies port 53, and the resolver still answers on `<overlay ip>:53`. The only
difference is that the socket lives in our netstack rather than the kernel, so
a squatter on `0.0.0.0:53` becomes irrelevant.

## Phases

| # | Phase | Kill switch |
|---|---|---|
| P1 | A UDP arm in `SplitTun` (`dst == self_ip && proto == UDP && dport == 53`), serving the existing resolver over an `NsUdpSocket` | `overlay_dns_intercept`, default **OFF** |
| P2 | The steer gate reads *"the resolver is reachable"* rather than *"the OS bind succeeded"*, so NRPT is installed when P1 is serving | inherits P1's switch |
| P3 | DNS over **TCP** `:53` for truncated answers — reuses SplitTun's existing TCP arm rather than adding one | inherits P1's switch |
| P4 | **The steer half** — establish why a written NRPT rule is not in the effective table on the affected host, and make the guard **verify its own write** (`Get-DnsClientNrptPolicy -Effective`) rather than assume it. Without this, P1–P3 leave the OS still not sending us the queries | reporting-only first (#1363) |
| P5 | Field-verify on the affected host, in **both** `:53`-race outcomes; only then consider the default | the switch itself |

## Acceptance criteria

- [ ] On a host where `0.0.0.0:53` is held, `Resolve-DnsName <peer>.<suffix>`
      returns the peer's overlay A record, and `ping <peer>.<suffix>` succeeds.
- [ ] `roomler status` reports the resolver as serving, and the reported state
      matches what `Resolve-DnsName` actually does — the #1363 lesson.
- [ ] A host where the OS bind **succeeds** is byte-identical with the switch
      off and on (no regression on the 99% case).
- [ ] Non-DNS overlay traffic is untouched: a throughput and an RTT sample
      across the switch, on the same pair, are within noise.
- [ ] The `dns_bound=false` → no-steer gate still holds when the netstack path
      is also unavailable — the host must never get an NRPT rule pointing at
      nothing.
- [ ] Field-verified on the affected host, with the before/after in this doc.

## Open decisions

1. **Is this Windows-only in practice?** Linux hosts run systemd-resolved on
   `127.0.0.53:53`, which does not collide with a bind on the overlay address;
   no Linux or macOS host in the fleet has ever reported `resolver DOWN`. If it
   is Windows-only, P1 could be `#[cfg(windows)]` and the blast radius shrinks
   accordingly. **Measure before deciding** — one `resolver DOWN` sweep.
2. **Fragmented UDP.** SplitTun's TCP arm sends non-first fragments to the OS so
   it can reassemble (`split_tun.rs`, the first-fragment check). A large EDNS0
   answer can fragment. Simplest honest rule: intercept only unfragmented
   datagrams and let anything else pass to the OS, which then fails the same way
   it does today — no worse, and never a half-diverted flow.
3. **Whether to keep the OS bind at all** when it succeeds, or always serve from
   the netstack for one code path everywhere. One path is simpler; two paths
   keep the proven behaviour on the hosts that already work.
4. Does the desktop companion's DNS section need to distinguish the two
   transports, or is "serving" enough?

## Out of scope

- Making the *upstream* resolver reachable — this is about the local listener,
  not about which forwarder answers.
- The `os_steer_active` precedence question (an NRPT rule written but outranked
  by a VPN's own). Unobserved; see #1363, which was withdrawn for lack of an
  instance.
- Overlay IPv6 / AAAA behaviour — #1342 establishes that nothing in the fleet
  opens an overlay-v6 socket, so it is not a driver here.

## Field-verification log

| date | build | host | result |
|---|---|---|---|
| 2026-09-05 | 0.4.66 | the affected corp laptop | Baseline, **bind-lost arm**: `resolver DOWN`; `Resolve-DnsName` and `ping <name>` both fail; `0.0.0.0:53` held by svchost; bind refused `AddressAlreadyInUse` with and without `SO_REUSEADDR`; NRPT drops `ip:port` silently |
| 2026-09-05 | 0.4.66 | same host, later | **bind-won arm**: we hold `100.65.4.30:53`, status says `active`, the NRPT rule is stored (`count=1`) — and the **effective table is empty** (`count=0`, no error), so `Resolve-DnsName` still returns nothing. Second blocker; see #1363 |
| — | — | fleet sweep | Every other host (2 Linux servers, a macOS/Asahi relay, two Windows laptops, this dev box) reports `active`, and the one checked resolves correctly — the failure is specific to the corp-managed host, not to the feature |

## Related

- `docs/roomler-ssh.md` — the same EADDRINUSE problem, and the interception
  answer this FR generalises.
- #1363 — withdrawn; the status line proved honest on this host, and the
  measurement above is why.
- #1342 — the same host's route war; unrelated cause, same host.
