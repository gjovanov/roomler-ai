# FR-72 — One MagicDNS resolver per daemon

**Issue:** [#1382](https://github.com/gjovanov/roomler-ai/issues/1382) · **Status:** P1 shipped, field verification pending · **Opened:** 2026-09-05

> ⚠️ **Re-aimed 2026-09-05, same day, after the verification overturned the
> original premise.** This FR opened as *"MagicDNS without an OS port"* — the
> theory that a third party owning `0.0.0.0:53` was killing the feature, to be
> fixed by intercepting DNS below the OS the way roomler SSH intercepts `:22`.
> That theory was wrong in an instructive way and the history is kept below,
> because the wrong turn is the most useful part of the record.

## Goal

MagicDNS resolves on every enrolled host, for as long as the daemon runs — not
only until the first WebSocket reconnect.

## The bug

`OverlayRuntime::run` is scoped to **one WS session**. It spawned the resolver
with `tokio::spawn(dns::run(...))` and **discarded the JoinHandle**, while
`dns::run` serves until its socket errors. So the task outlived the runtime that
created it and kept `<self overlay ip>:53` bound. Every reconnect then spawned
another resolver, which lost the bind **to its own dead predecessor**.

Field-measured on a corp laptop, two starts fourteen seconds apart inside one
daemon lifetime:

```
19:52:48  INFO magicdns: resolver up               bind=100.65.4.30:53
19:53:02  WARN magicdns: bind failed; resolver off bind=100.65.4.30:53  AddressAlreadyInUse
```

Three consequences, all observed:

1. **MagicDNS dies at the first reconnect** and stays dead until the process
   restarts. On a host whose overlay churns — a corp VPN reaping routes, say —
   reconnects are frequent, so this is the normal state there, not a rare race.
2. **`roomler status` reports whichever runtime's flag it reads**, so the same
   host showed `resolver DOWN` and `active` hours apart. That intermittency was
   the most misleading symptom (see #1363).
3. **The survivor answers from the DEAD session's name map**, so names NXDOMAIN
   while the port looks healthy — bound, owned by `roomlerd`, serving nothing
   useful.

## ⚠️ Why it looked like a port-ownership problem, and was not

The original premise came from one measurement, read wrongly:

```
bind 100.65.4.30:53                 -> AddressAlreadyInUse (10048)
bind 100.65.4.30:53 + SO_REUSEADDR  -> AddressAlreadyInUse (10048)
```

`0.0.0.0:53` was held by a Windows service at the time, so that was blamed. It
was not the cause: **the probe was colliding with `roomlerd`'s own resolver.**
The daemon binds `100.65.4.30:53` successfully while that service holds the
wildcard throughout — a wildcard bind never blocked us.

🔑 The lesson worth keeping: *"another process owns the port"* and *"we own the
port twice"* produce the identical `AddressAlreadyInUse`, and only the owning
PID tells them apart. Check the owner before designing around the error.

## Rejected, with the measurement that rejected it

| approach | why not |
|---|---|
| **SplitTun UDP interception** (the original P1–P3) | Solves third-party port ownership, which does not happen here. It is real engineering against a problem we do not have, in the packet path of every overlay host. |
| **An alternate port** (the first instinct) | NRPT nameservers carry no port: `Add-DnsClientNrptRule -NameServers 'ip:port'` **silently stores an empty list**. Measured. |
| **Writing the Group-Policy NRPT store** (Tailscale's `writeAsGP`) | It *works* — a GP rule plus `gpupdate` took the effective table 0 → 1 on the affected host — but it is not needed: the **local** rule works there too, and writing into a GPO-owned key means re-asserting it against every policy refresh. |
| **A DNS-manager fallback ladder** | Designed for a policy environment we then failed to reproduce. Local NRPT works on the host that motivated it. Revisit only with a host where it demonstrably does not. |

⚠️ An earlier root cause — *"an empty policy NRPT table suppresses local rules"* —
is **refuted**. It rested on one working host against one broken host, and the
broken one's local rule became effective after a policy refresh.

## What shipped (P1)

`OverlayRuntime::run` keeps the resolver's `JoinHandle` and aborts it in the
teardown, beside `inbound`, `tun_reader`, `outbound_pump` and the srflx
keepalive — every other task there was already stopped; this one was simply
missed. Aborting drops the socket, so the successor binds cleanly.

Locked by `dns::tests::an_aborted_resolver_releases_its_port_for_the_next_one`,
whose middle assertion is the point: **a second resolver must NOT bind while the
first lives.** Without that negative control the test would pass on the broken
code and prove only that binding works.

## Acceptance criteria

- [x] A second resolver cannot bind while the first holds the port, and can once
      it is aborted (unit).
- [ ] On a host that has reconnected at least once, `roomler status` reports
      `active` **and** `Resolve-DnsName <peer>.<suffix>` answers with the peer's
      overlay address.
- [ ] Exactly **one** `magicdns: resolver up` per daemon lifetime, and **no**
      `bind failed; resolver off`, across several reconnects.
- [ ] The port is owned by the live daemon and answers a direct query
      (`Resolve-DnsName … -Server <overlay ip>`).
- [ ] No regression on a host that was already healthy.

## Open

- **#1363** — `roomler status` said `active` over a resolver that had lost its
  own bind race. ⚠️ Verifying the NRPT effective table would **not** have caught
  this one; the check has to be *does our resolver answer*. That issue is the
  reporting half and is still open.
- Whether the same spawn-and-forget shape exists elsewhere in `run()`. The
  teardown aborts four tasks explicitly, which is exactly the pattern that hides
  a fifth.

## Out of scope

Interception, the GP store, and the fallback ladder — all recorded above with
the measurement that ruled each one out, so a future reader does not re-derive
them. Overlay IPv6 / AAAA behaviour is #1342's.

## Field-verification log

| date | build | host | result |
|---|---|---|---|
| 2026-09-05 | 0.4.66 | corp laptop | Baseline, **bind-lost arm**: `resolver DOWN`, no resolution |
| 2026-09-05 | 0.4.66 | same host | **bind-won arm**: `active`, NRPT rule effective and correct, still no resolution — resolver bound but not answering |
| 2026-09-05 | 0.4.66 | same host | Root cause: two `magicdns` starts 14 s apart in one lifetime, second `AddressAlreadyInUse`. GP-store experiment run and fully reverted (test key removed, task unregistered, policy refreshed) |
| — | — | fleet | Every other host reports `active` and resolves — they simply had not reconnected since their last restart |
