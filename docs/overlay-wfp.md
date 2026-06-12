# Overlay WFP firewall override (Windows)

> Cross-ref: the L3 overlay mesh is part of the remote-control / tunnel
> subsystem ([`docs/remote-control.md`](./remote-control.md)). This doc
> covers one Windows-specific, security-sensitive piece: how the agent
> makes the overlay survive a Group-Policy-locked Windows Defender
> Firewall by programming the Windows Filtering Platform (WFP) directly.

## The problem

The Tailscale-style L3 overlay (feature `overlay-l3`, default-OFF) brings up
a Wintun virtual NIC named **`roomler`** and routes per-tenant overlay IPs
(`100.64.0.0/10`) over WireGuard, relayed through coturn when direct
hole-punching fails. On a clean host this works both directions.

On a **corporate host whose Defender Firewall is controlled by Group
Policy**, unsolicited *inbound* packets to the `roomler` adapter are
dropped. The relay, the WireGuard handshake, and the routing are all fine —
the peer's packets reach the host's TUN — but the host won't *answer*
inbound, so the reverse direction fails. Worse, a local
`New-NetFirewallRule` has no effect: the GPO sets
`AllowLocalFirewallRules=False`, and the firewall can't be disabled.

Field-observed on PC50045 (2026-06-12): PC50045→NEO16 worked, NEO16→PC50045
timed out, with relay/WG/routing all proven healthy.

## How Tailscale solves it (and so do we)

Defender Firewall rules — including GPO ones — are just **filters** in the
Windows Filtering Platform, living in the low-weight MPSSVC sublayers.
Tailscale survives locked-down hosts by **programming WFP directly** from
its LocalSystem service instead of adding Defender rules. The agent already
runs as a LocalSystem Windows service, which is exactly the privilege WFP
writes require, so it does the same.

On overlay bring-up (`overlay::tun::SystemTun::up`, after the `roomler`
adapter exists), the agent:

1. Opens a **dynamic** WFP engine session
   (`FWPM_SESSION_FLAG_DYNAMIC`) — every object it adds is auto-removed by
   the Base Filtering Engine when the handle closes or the process exits.
   No persistent/boot-time rules, robust to a crash.
2. Adds a provider + a **sublayer at weight `0xFFFE`** — above the MPSSVC
   firewall sublayer (~weight 2), so it's arbitrated first.
3. Adds four **hard-permit** filters (one per ALE layer:
   `ALE_AUTH_RECV_ACCEPT_V4/V6`, `ALE_AUTH_CONNECT_V4/V6`), each scoped by a
   single condition `FWPM_CONDITION_IP_LOCAL_INTERFACE == <roomler LUID>`.

The **hard permit** is the key: the filters carry
`FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT`, which clears the action-write right.
A hard permit in a higher-weight sublayer overrides a (hard) filter block in
a lower-weight sublayer — so it beats the GPO firewall's inbound drop. (A
plain *soft* permit, which is the WFP default and what Tailscale's published
demo uses, would lose to a GPO hard block — so we go further than the demo.)

This is **additive permit only** — no shields-up, no discard filters, never
touches any interface other than `roomler`.

### Identity (greppable in `netsh wfp show filters`)

| Object | Name | GUID |
|---|---|---|
| Provider | `Roomler Overlay` | `524f4f4d-4c45-5200-5052-4f5649444552` |
| Sublayer | `Roomler Overlay Permit (LUID-scoped)` | `524f4f4d-4c45-5200-5355-424c41594552` |
| Filters | `Roomler Overlay Inbound/Outbound Permit` | (auto-assigned per layer) |

## Limits — when this still won't work

A hard permit overrides a *filter* block, but **cannot** override:

- a **callout-driver veto** (some EDR / DLP / ZTNA agents enforce network
  policy via a kernel callout), or
- an **IPsec connection-security rule** (authenticated-inbound GPOs).

Nor can it run if the GPO has hardened the BFE security descriptors so even
LocalSystem can't add filters. In those cases the install fails (or succeeds
but is still vetoed downstream), and the only recourse is an **IT-managed
exception**: a domain firewall allow-rule scoped to the `roomler` adapter /
`100.64.0.0/10`, or a connection-security exemption.

The install is **best-effort**: a failure logs a WARN and the overlay still
comes up — it only matters on hosts where the firewall is the blocker.
Verify the actual outcome in the field with `netsh wfp show filters` (look
for the sublayer above) and a reverse-direction ping/curl.

## Disabling it

Set `ROOMLER_AGENT_WFP_PERMIT=0` (or `false`/`no`/`off`) to skip WFP
programming entirely — e.g. on a host where IT installed a managed
exception, or to silence an AV "firewall tampering" alert. Default is **ON**
whenever `overlay-l3` is active.

## Security note (for reviewers)

A high-weight, hard-permit sublayer from a non-Microsoft provider that
overrides a GPO firewall block is exactly the pattern some EDRs flag as
"WFP tampering". Mitigating properties:

- **LUID-scoped to `roomler`** — it cannot open the host's other
  interfaces; it only permits traffic on the overlay NIC the agent itself
  created.
- **Additive permit only** — never shields-up, never a discard/block.
- Runs as **LocalSystem** (the service privilege that makes BFE writable).
- **Break-glass disable** via `ROOMLER_AGENT_WFP_PERMIT=0` — an IT/security
  team can neutralize it without rebuilding.

Implementation: `crates/tunnel-core/src/overlay/wfp.rs` (raw `windows-sys`
0.61 FFI, gated `#[cfg(all(feature = "overlay-l3", windows))]`).
