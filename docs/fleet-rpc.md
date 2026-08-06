# Fleet RPC — remote command execution

Run a command on a trusted device from the CLI or the web UI, get its output
back, and leave an audit trail. Built for the case where a device is
misbehaving and the person who can diagnose it is somewhere else.

## Why it exists

The daemon already answers a rich diagnostic protocol — `status`, `peers` with
per-carrier debug, netstack `ping`, `config`, `tail-log` — but only to a caller
on the same host (`tunnel_core::localapi`, an ACL-gated named pipe / unix
socket). Everything needed to explain a bad overlay carrier is one pipe away
from someone standing at the machine and unreachable from anywhere else.

That made every hypothesis in a remote investigation cost a human round trip:
"please run this in an elevated pwsh and paste the output." Fleet RPC removes
that loop.

## Transport

Commands ride the agent's **existing control WebSocket**, not the overlay.

That is deliberate: the diagnostics this exists for are most needed exactly
when the mesh is broken, so a transport that depends on the mesh would be
unavailable in the case that motivated the feature. An overlay-direct path
(lower latency, works when the *server* is unreachable) is a later phase, and
selection will prefer it only when a carrier is actually up.

```
CLI:  roomler → local daemon (LocalAPI) → rc:rpc.request → API pod
                                                             │
UI:   browser → POST …/agent/{id}/exec ──────────────────────┤
                                                             ▼
                                   [gates 1–3 + audit + mint request_id]
                                                             │
                                            PodBus (if the agent is elsewhere)
                                                             ▼
                                   hub.send_to_agent(rc:rpc.exec) → device
                                                             │
                                   rc:rpc.result ────────────┘
```

Wire variants live in `crates/remote_control/src/signaling.rs`:

| Tag | Direction | Purpose |
|-----|-----------|---------|
| `rc:rpc.exec` | server → agent | run one bounded command |
| `rc:rpc.cancel` | server → agent | kill it and its process tree |
| `rc:rpc.result` | agent → server | the answer (always sent) |
| `rc:rpc.request` | agent → server | the `roomler exec` leg |
| `rc:rpc.response` | server → agent | its answer |

**Capability gate.** An agent that predates the feature drops an unknown
`ServerMsg` tag in its `Err(e) => debug!` branch. For `Goodbye`/`UpdateNow`
that is harmless; here a caller is *blocked on the answer*, so silence reads as
a hung device. `AgentCaps.rpc` must contain `exec`, and the server answers
`412` when it doesn't. **This is the rule for any new `ServerMsg` a caller
awaits.**

## The four gates

A command runs only if all four allow it. Each is owned by a different party,
so no single compromise is sufficient.

| # | Gate | Owner | Default |
|---|------|-------|---------|
| 1 | `TenantSettings.remote_exec_enabled` | org owner (`MANAGE_TENANT`) | **off** |
| 2 | `permissions::EXEC_DEVICE` (`1 << 27`) | role admin (`MANAGE_ROLES`) | not in `DEFAULT_ADMIN` |
| 3 | `Agent.exec_policy` (`ExecMode::Off`) | fleet admin (`MANAGE_AGENTS`) | **off** |
| 4 | agent-local `exec_enabled` config key | whoever holds the machine | **off** |

Gates 1–3 are evaluated by `routes::agent_exec::authorize`. Gate 4 is enforced
on the device and comes back as an `error` on the result, which is why a
refusal is a `200` with an error body rather than a transport failure.

Design notes worth keeping:

- **`ExecPolicy` is separate from `AccessPolicy`.** That one grants
  screen-view. "May watch your screen" must never be the same checkbox as
  "may run a root shell".
- **`EXEC_DEVICE` is not in `DEFAULT_ADMIN`**, though `VIEW_EXEC_AUDIT` is: an
  admin should see every command the fleet ran without silently gaining the
  power to run one. `REMOTE_CONTROL` is not equivalent — it is consent-gated,
  visible to whoever is at the machine, and runs as the interactive user.
- **Gate 4 is the one that survives a compromised server.** Everything else
  lives on the control plane. Clearing the key resets it to `false`, so
  `roomler config set exec_enabled` with no value can never turn it on.
- **`can_originate`** must be set on a device before its CLI may drive commands
  at *other* devices. Without it, compromising any enrolled laptop would
  inherit its owner's exec rights fleet-wide.

## Privilege

Commands inherit the daemon's identity: **SYSTEM** under a perMachine Windows
install, **root** under systemd. That is required for the diagnostics that
matter (`Get-NetFirewallRule`, `netsh`, route tables, service state), and the
admin UI says so in those words on both the console and the policy dialog.

There is no command blocklist. A blocklist on a root shell is false security
and trivially bypassed; the real mitigations are the four gates, the audit
trail, and the existing watchdog + SCM auto-restart if someone stops the
service out from under themselves.

## Bounds

Server-clamped, then re-enforced agent-side so a forged frame can't ask for an
unbounded run (`models::exec_limits`):

- timeout: 30 s default, 300 s max — the agent answers its own timeout
- output: 256 KiB default, 1 MiB max, **combined** across both streams, with an
  explicit `truncated` flag
- 4 concurrent commands per device; beyond that it refuses rather than queues
  (a caller on a deadline wants a fast "busy", not a slow timeout)
- no stdin, no interactivity, no streaming

Output is swept for the agent token(s), `Bearer …` headers and JWT-shaped
strings **before it leaves the host** — results are persisted in `exec_audit`
for 90 days, so a secret a command happens to echo would otherwise long outlive
the session.

## Audit

`exec_audit`, 90-day TTL, one row per **attempt** — including refusals. A
denied exec is the interesting one: without the row, someone probing which
devices will run things for them leaves no trace. Rows carry the caller, the
device, the origin device (CLI leg), the full command, the deny reason if any,
a capped output sample, and a SHA-256 of the full redacted output so a
truncated sample still ties to what ran.

## Using it

### CLI

```bash
roomler exec CORPLAP-1 -- Get-NetRoute -AddressFamily IPv4
roomler exec neo16 --shell pwsh --timeout 60 -- Get-NetAdapter
roomler exec CORPLAP-1 --json -- ipconfig /all

# canned evidence bundles
roomler diag host CORPLAP-1
roomler diag pair NEO16 CORPLAP-1
```

Exit status mirrors the remote command's, so `roomler exec` composes in a
script: a refusal or failure is non-zero here too, never a silent 0.

The diagnostic bundles live in the **CLI**, not the agent, so a new probe is a
CLI release rather than a fleet-wide agent rollout. That inversion is the whole
argument for shipping free-form exec before a typed verb catalog.

### Web UI

- **Device console** — device menu → Diagnostics → *Device console*
- **Execution policy** — device menu → Access → *Execution policy*
- **Org switch + audit** — workspace Settings

### Turning it on for a device

```
1. org owner:   Settings → allow remote command execution
2. fleet admin: device → Execution policy → accept remote commands
3. on the box:  roomler config set exec_enabled true
                (then restart the Roomler service)
4. role admin:  grant EXEC_DEVICE to whoever should be able to run things
```

Steps 1–2 and 4 are server-side; step 3 must be done by someone with access to
the machine, on purpose.

## Deferred

- **Overlay-direct transport.** `serve_connection` over `NsTcpStream` on a
  netstack port, with server-minted grants pushed to the target (the
  `OverlayRelayGrant` pattern), so there is no key distribution. Adds P2P
  latency and server-independence.
- **Typed verb catalog.** Native cross-platform `NetDiag` / `LogGrep` /
  `FirewallRules` verbs promoted from whichever shell bundles prove themselves,
  plus exposing the existing `status` / `peers` / `ping` / `config` /
  `tail-log` LocalAPI verbs remotely.
- Run-as-interactive-user (the `system_context/` `CreateProcessAsUser`
  machinery already exists), output streaming, scheduled probes, and an
  `exec_policies` selector collection shaped like `overlay_policies`.
