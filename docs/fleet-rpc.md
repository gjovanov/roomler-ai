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

**Reading a remote log: always `roomler exec <host> -- roomler logs`** (with
`--grep <pat>` / `-n <lines>` / `--source daemon|service|panic`), never a path
guess. The daemon resolves its OWN log file — the path depends on the service
role, the running user, and a UTC-dated suffix, and a Windows host carries
two decoy `roomlerd.log*` files (the SCM supervisor's and the updater's).
Path archaeology has produced a false "the agent's logging is dead" verdict
twice (2026-08-07 and 2026-08-09); the verb existed both times.

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

Commands run privileged on purpose: the diagnostics that matter
(`Get-NetFirewallRule`, `netsh`, route tables, service state) need it, and the
admin UI says so in those words on both the console and the policy dialog.
**Root** under systemd, and on Windows **SYSTEM through the device's
service** once an administrator has anchored the device (FR-94,
`docs/fr/FR-94-fleet-rpc-system-on-windows.md`).

### Who runs the command

| Device | Without an anchor | Anchored (FR-94) |
|---|---|---|
| Linux (systemd), supervised Mac | root | n/a: already root |
| Windows, SystemContext mode | SYSTEM, in the console session (the worker is SYSTEM) | SYSTEM in session 0, through the service |
| Windows, attended install, an admin signed in | **that user**, elevated, session 1 (#2021) | SYSTEM in session 0, through the service |
| Windows, attended install, a standard user signed in | **that user**, Medium integrity: no admin rights | SYSTEM in session 0, through the service |
| Windows, nobody signed in | offline: no worker holds the socket | offline, the same (D3, the service owning the socket, is out of scope) |

Before FR-94, Fleet RPC ran in whatever process held the agent socket, and on
an attended Windows install that is the **worker in the console session**,
running as the signed-in user. A corporate laptop's commands then acted as the
employee (their Kerberos/NTLM identity on every share), and a standard user's
had no admin rights at all.

### The SYSTEM path

```mermaid
sequenceDiagram
  participant A as admin (roomler exec / UI)
  participant S as server (any pod)
  participant W as worker (session 1, the user)
  participant V as service (session 0, SYSTEM)
  A->>S: POST /api/tenant/{tid}/agent/{id}/exec
  S->>S: gates 1–3; exec_pending {request_id, command, run_as, claim_by}
  S->>W: rc:rpc.exec {request_id, …, via_service: true}
  W->>W: the person's exec_enabled, then consent
  W->>V: doorbell {request_id} (a pipe; no authority at all)
  V->>V: the anchor's exec_enabled
  V->>S: POST /api/agent/service/rpc/{request_id}/claim (the anchor's credential)
  S-->>V: the command (single use, this device only, inside the claim window)
  V->>V: run as SYSTEM: same timeout, output cap, redaction, tree kill
  V->>S: POST …/rpc/{request_id}/result
  S->>A: 200 {outcome, ran_as: "system"}
```

⚠️ **The invariant: nothing the signed-in user can write may decide what
SYSTEM runs.** The worker is the user's process and its config is the user's
file, so the service takes nothing from it but a request id. The command comes
from the server, fetched with a credential the user cannot read, from an
origin the user cannot change, and the output goes back to the server, never
to a local pipe. What each attempt to get around that meets:

| Attempt | What stops it |
|---|---|
| Ring the doorbell with a made-up or stale request id | the claim: the server has nothing pending under it for this device (404, audited `claim_refused`) |
| Point the user's config at another server | the service never reads it: its origin is in the anchor |
| Plant an anchor before an admin makes one (`%ProgramData%` lets users create folders) | the service trusts the file only if it and its directory are owned by SYSTEM or Administrators and grant nobody else write (`exec_service.rs` `check_private`) |
| Squat the doorbell's pipe name | the worker refuses a pipe SYSTEM does not own; the service's `first_pipe_instance` refuses to share the name |
| Read SYSTEM's output | it is posted from the service to the server over HTTPS |
| An http origin, to rewrite the claim on the path | the anchor accepts https only (loopback excepted, for tests) |

### Anchoring a device

From an **elevated** prompt on the device:

```
roomlerd anchor create --server https://roomler.ai --token <enrollment-token> --device <device id>
roomlerd anchor status            # what is anchored (never the credential)
roomlerd anchor exec on|off       # the anchor's half of gate 4
roomlerd anchor remove            # this machine stops offering SYSTEM exec
```

- The device is **named**, not derived: on an attended install the enrollment
  belongs to the user who made it, and `machine_id` hashes that user's config
  path, so "re-enroll this machine" from another account mints another device.
- The enrollment token is single use and burned only once every check passes
  (live device, same org, Windows, host name matches the device unless
  `--force`).
- The anchor lives in `%ProgramData%\roomler\roomler\exec-service\anchor.json`
  (Administrators-only). The worker learns from the service, every 30 s,
  which device it is anchored for, and offers the `exec-system` capability on
  that connection only. No restart is needed either way.

**One live anchor per device.** The service's credential carries the anchor's
id, and the server accepts it only while that is the device row's current
anchor. Anchoring again revokes the earlier one; so does
`DELETE /api/tenant/{tid}/agent/{id}/exec-anchor` (`MANAGE_AGENTS`), without
removing the device. The service renews its credential under the same anchor
well before its year runs out.

**Routing needs both halves:** the server's record of the anchor, and the
device's `exec-system` word. Either alone sends the command to the worker, as
before. `rc.exec_via_service = false` on the server turns the SYSTEM path off
everywhere; `ROOMLERD_EXEC_VIA_SERVICE=0` on a device withdraws its word.

**Gate 4 has two halves on this path**, and both must say yes: the person's
`exec_enabled` (the worker checks it before it rings, exactly as before) and the
anchor's (the service checks it before it claims).

**An org can require SYSTEM.** With `exec_require_system` on (workspace
Settings, `PUT /api/tenant/{tid}/exec-settings`), a Windows command that cannot
take the SYSTEM path is refused as `system_required` instead of running as the
signed-in user.

Every `exec_audit` row that ran records `ran_as`: `system` through the service,
`worker` otherwise. The result the caller gets carries the device's own
`ran_as` beside it.

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
- no stdin, no interactivity, no streaming — **on this wire**. The engine has a
  streaming entry point beside `run_fed`, `ExecEngine::run_streamed` (FR-89),
  with no ceiling and no wall clock; Roomler SSH's `ssh <node> 'cmd'` uses it
  ([`roomler-ssh.md`](roomler-ssh.md) §3). `rc:rpc.exec` keeps every bound
  above, because its answer is one persisted `exec_audit` row, and the two
  paths share the identity model, the permit, the redactor and the tree kill —
  only the shape of the output differs.

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
roomler exec winhost-a -- Get-NetRoute -AddressFamily IPv4
roomler exec devbox --shell pwsh --timeout 60 -- Get-NetAdapter
roomler exec winhost-a --json -- ipconfig /all
roomler exec "Office PC" -- hostname      # a dashboard display name works too

# canned evidence bundles
roomler diag host winhost-a
roomler diag pair DEVBOX winhost-a
```

Exit status mirrors the remote command's, so `roomler exec` composes in a
script: a refusal or failure is non-zero here too, never a silent 0.

The target is a device name or a hex agent id, resolved by the server within
the caller's org (`crates/modules/fleet/src/socket.rs:1014` `resolve_exec_target`:
hex id → exact name → case-insensitive name). A dashboard **display name** is
turned into the hex id by the CLI before the request leaves — only when no
device name matches it, and refused when two devices share it
(`docs/device-naming.md` § Display names on the command line). The server never
sees a display name and gates the hex id exactly as it would a typed one.

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
- Run-as-interactive-user as a per-device policy (FR-94 P4: the service can
  already run `console_user`; the policy field and its UI are what is left),
  output streaming **on the RPC wire** (the engine
  itself streams for Roomler SSH since FR-89; the `rc:rpc.result` row and its
  audit sample would need a different shape), scheduled probes, and an
  `exec_policies` selector collection shaped like `overlay_policies`.
