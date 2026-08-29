# FR-33: A VPN that captures the LAN prefix should say so — surface LAN capture in `status`, `why` and the RC path pill

Status: **designed, not implemented** (2026-08-29). Tracking issue: `FR-33` (#905).
Sibling of FR-9 (LAN relay diagnosis) and FR-31 (opening keyframe). Spec on master up front; the
design is known.

## The measurement that motivates it

2026-08-29: the operator reported `neo16 → PC55331` as a *quality regression*. It was not — the
pair had been relay-locked for **five days** because PC55331's Check Point Endpoint VPN carries
`192.168.68.0/25` + `192.168.68.128/25` via the VPN adapter (`Ethernet 3`, `172.30.245.31/19`)
at metric 1, beating the on-link `/24`. neo16's LAN handshakes arrive (PC55331's LAN socket
shows `rx=9155`) and are accepted as probes; PC55331's replies leave through the VPN and die.
Both ends logged `direct probe did not handshake within deadline; kept relay` ~45×/hour for
120 h, and every surface the operator reads said only `upgrading`, `relay`,
`blocked_by: penalty`.

Getting from that to the cause took `Find-NetRoute` on the device over Fleet RPC, a Mongo
histogram over both agents' logs, and the AnyConnect precedent from FR-9. A daemon that already
samples the effective route by LOOKUP (`crates/tunnel-core/src/overlay/netstate.rs:122` —
`GetBestRoute2` / `ip route get` / `route get`, precisely because "a corp capture rarely touches
`/0`") can answer this in one more lookup.

## Goal

When a host's own LAN prefix is routed through an interface other than the one that owns the
address, every place an operator reads a carrier verdict names the capture — so a relay on a LAN
pair is attributed to the VPN in seconds, not days, and is never again hunted as an encoder
regression. **Detect and surface only.** Routing around the capture is VPN policy evasion and
stays out of scope (operator's standing rule).

## Key design

1. **Detection (netstate, all three OSes).** For every LAN interface address in
   `NetSnapshot.ifaces` (`netstate.rs:134`), look up the route to another address inside the
   same prefix — a known peer LAN endpoint from the netmap when one exists, else the prefix's
   broadcast address. If the selected interface ≠ the owning interface, record
   `LanCapture { prefix, owner_ifref, via_ifref, via_name }`. Sampled where the default-route
   lookup already runs (`sample_snapshot`, `netstate.rs:438`): one lookup per LAN interface per
   snapshot. Onset and clear each produce ONE `NetDelta` with a one-line `summary`
   (`netstate.rs:160`) — the "LOG every silent drop" rule — never a per-snapshot line.
2. **`roomler status`** gains a line next to `srflx`, using the same shape
   (`agents/roomler-cli/src/localclient.rs:1237`):
   `lan         CAPTURED — 192.168.68.0/24 leaves via "Ethernet 3" (Check Point Virtual
   Network Adapter For Endpoint VPN Client); direct on the LAN is impossible while it does`
   and `lan         clear` otherwise. Wire: `NodeStatus.lan_capture: Option<LanCaptureStatus>`
   (`crates/localapi/src/lib.rs:89`), populated the way `srflx` is
   (`agents/roomlerd/src/localapi_state.rs:371` ← `runtime.rs:3164`). Optional field: an old
   CLI against a new daemon, or the reverse, prints nothing extra.
3. **`peers --json why`**: `BlockedBy::LanCaptured` (`path.rs:1310`) when the LAN tier is refused
   while a capture is active on *this* host — a fact about this host, the way
   `PeerRelaysInstead` is a fact about the peer. `explain` (`path.rs:913`) resolves it in the
   same order `eligible` tests, so the text can never disagree with the verdict.
4. **RC path pill**: `rc:video-info` (`agents/roomlerd/src/peer.rs` ~3043, `video_info_sent`)
   carries an optional `transport_reason` string set by the *agent* from its own capture state;
   the viewer renders `relay · VPN captures the host's LAN`. Optional field — old viewers ignore
   it, old agents omit it.
5. Kill switch `overlay_lan_capture_probe` (default on; the probe is a read-only lookup).

## Phases

| phase | scope | kill switch |
|---|---|---|
| P1 | detection + `NetDelta` onset/clear lines + `roomler status` line (Windows first; Linux/macOS via the existing `ip route get` / `route get` backends) | `overlay_lan_capture_probe=false` |
| P2 | `BlockedBy::LanCaptured` in `peers --json why` | same |
| P3 | `rc:video-info.transport_reason` + the viewer pill | same (agent side omits the field) |

## Acceptance criteria

- [ ] `roomler exec pc55331 -- roomler status` prints the `CAPTURED` line naming `Ethernet 3`
      while its VPN is up; `roomler status` on neo16 prints `lan         clear`
- [ ] PC55331's `roomler peers --json` for neo16 reads `blocked_by: lan_captured`
- [ ] the RC pill on `neo16 → PC55331` reads `relay · VPN captures the host's LAN`
- [ ] the daemon log carries ONE onset line and ONE clear line across a VPN connect/disconnect
      cycle on PC55331 (no per-snapshot spam)
- [ ] no change on hosts without a capture (neo16, rozalina-2, the cluster nodes): status line
      `clear`, `why` unchanged, pill unchanged
- [ ] `overlay_lan_capture_probe=false` removes the line, the reason and the pill text

## Open decisions

- Whether a detected capture should also **pause the LAN probe cadence** (today ~45 failed
  probes/hour, each a zero-disruption shadow probe — cheap, and P8 deliberately never escalates
  the LAN penalty under make-before-break). Default: no — "heuristics may detect; they never
  decide". Revisit only with a cost measurement.

## Out of scope

Bypassing the capture; the relay ceiling; FR-31's encoder work.

## Field log

| date | build | note |
|---|---|---|
| 2026-08-29 | 0.4.17/0.4.18 (PC55331), 0.4.16 (neo16) | Motivating case above; `Find-NetRoute -RemoteIPAddress 192.168.68.126` on PC55331 → `Ethernet 3`, `NextHop 172.30.245.30`, `DestinationPrefix 192.168.68.0/25`; `Get-NetAdapter` names the Check Point adapter. |
