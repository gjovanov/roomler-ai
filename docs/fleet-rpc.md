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
`--grep <pat>` / `-n <lines>` / `--source daemon|service|installer|panic`;
`installer` is the per-Machine MSI's own steps on Windows, #3230), never a path
guess. The daemon resolves its OWN log file — the path depends on the service
role, the running user, and a UTC-dated suffix, and a Windows host carries
two decoy `roomlerd.log*` files (the SCM supervisor's and the updater's).
Path archaeology has produced a false "the agent's logging is dead" verdict
twice (2026-08-07 and 2026-08-09); the verb existed both times.

**What reaches that log.** The daemon keeps lines by `RUST_LOG` when its environment
sets one, and otherwise by its default: `roomlerd` and `tunnel_core` at INFO,
everything else at WARN (`crates/agent-core/src/logging.rs:411`). Either way it then
keeps INFO from two targets that no shipped unit names
(`crates/agent-core/src/logging.rs:494`, #1928):

| Target | Its INFO lines |
|---|---|
| `roomler_localapi` | The LocalAPI listeners coming up and going down, and the person socket's up and down (FR-27 P11). Its per-connection lines are DEBUG and stay off. |
| `roomler_node_core::logging` | One: #3438's "this console has no window", at a worker's start. |

A `RUST_LOG` that already decides about a target is used as written: it has a
directive for the target, for a module under it or for any prefix of its name
(`EnvFilter` matches targets by prefix), or a bare level of `info` or finer. So
`RUST_LOG=debug` still logs both at DEBUG, and `roomler_localapi=off` keeps the crate
silent. An empty or wholly invalid `RUST_LOG` is left alone too: `EnvFilter` then
logs ERROR from every target. Two directives are added whatever the spec says:
`turn::client::relay_conn=error` (`crates/agent-core/src/logging.rs:449`) and
`tunnel_core::overlay::sockbuf=info` (`crates/agent-core/src/logging.rs:460`).

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

**The prompt names the asking org** (#2815). `rc:rpc.exec` carries the org's
display name, `tenant_name`, as `rc:request` does for a session. `authorize`
returns it from the tenant row that gate 1 already reads. On a multi-org
device the exec prompt then says "On behalf of «org»" by the same rule as a
screen-control prompt ([multi-org.md](multi-org.md) §8b). Before, it named the
enrollment's label, which on the primary enrollment is literally `primary`.

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
  S->>S: gates 1–3, exec_pending {request_id, command, run_as, claim_by}
  S->>W: rc:rpc.exec {request_id, …, via_service: true}
  W->>W: the person's exec_enabled, then consent
  W->>V: doorbell {request_id} (a pipe, no authority at all)
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
| An anchor that no administrator wrote through `roomlerd anchor` | the service reads the anchor only from `HKLM\SOFTWARE\Roomler\ExecService`, a key that SYSTEM or Administrators own and no other account may change, read or create (`machine_key.rs`, #3737); no file is read as one ([Where the anchor lives](#where-the-anchor-lives-3737)) |
| Squat the doorbell's pipe name | the worker refuses a pipe SYSTEM does not own; the service's `first_pipe_instance` refuses to share the name |
| Read SYSTEM's output | it is posted from the service to the server over HTTPS |
| An http origin, to rewrite the claim on the path | the anchor accepts https only (loopback excepted, for tests) |

### Anchoring a device

From an **elevated** prompt on the device:

```
roomlerd anchor create --server https://roomler.ai --token <enrollment-token> --device <device id> [--ssh system]
roomlerd anchor status            # what is anchored (never the credential)
roomlerd anchor exec on|off       # the anchor's half of gate 4
roomlerd anchor ssh off|console_user|system   # FR-105: roomler SSH through this service too
roomlerd anchor remove            # this machine stops offering SYSTEM exec
```

- **The anchor's SSH ceiling** (FR-105, from 0.4.136) is **off** by default, so
  no anchor gains SYSTEM SSH by updating. `system` lets roomler SSH sessions run
  through the same service as SYSTEM (session 0) or as the signed-in user;
  `console_user` allows only the latter. `anchor status` then prints the
  service's own SSH host key fingerprint. How the session gets there, and why
  nothing the signed-in user writes can widen it:
  [`roomler-ssh.md`](roomler-ssh.md) § Through the device's SYSTEM service.

- The device is **named**, not derived: on an attended install the enrollment
  belongs to the user who made it, and `machine_id` hashes that user's config
  path, so "re-enroll this machine" from another account mints another device.
- The enrollment token is single use and burned only once every check passes
  (the org is not archived, the admin who minted the token may still enroll
  devices there, live device, same org, Windows, host name matches the device
  unless `--force`).
- The anchor lives in the registry key `HKLM\SOFTWARE\Roomler\ExecService`
  (SYSTEM and Administrators only, #3737; [below](#where-the-anchor-lives-3737)).
  The worker learns from the service, every 30 s, which device it is anchored
  for, and offers the `exec-system` capability on that connection only. No
  restart is needed either way.

#### Where the anchor lives (#3737)

The anchor is a registry key, `HKLM\SOFTWARE\Roomler\ExecService` (the 64-bit
view), and nothing else. The service reads it through `exec_service::store`
(`agents/roomlerd/src/exec_service.rs`) and judges it with
`crates/agent-core/src/machine_key.rs`.

| Value | Holds | Written by |
|---|---|---|
| `Anchor` | the anchor: server origin, org, device, the service's credential, `exec_enabled`, the SSH ceiling (JSON, as `anchor.json` held it) | `roomlerd anchor create`, `exec` and `ssh`, elevated, and Setup's elevated anchor step (FR-104); the SYSTEM service when it renews its credential, and once at an upgrade (the copy below) |
| `SshHostKey` | the service's SSH host key (FR-105, OpenSSH PEM) | `anchor create` and `anchor ssh` when the ceiling is above off; the SYSTEM service whenever the device is anchored and the key holds none, whatever the ceiling (its endpoint publisher makes one) |
| `LegacyCopy` | what the one-time copy did: `at`, `outcome` (`pending`, `copied`, `none` or `refused`; `removed` after `anchor remove`), `why`, `agent_id`, `anchored_at`, and `earlier`, the outcomes it had before. No secret | the SYSTEM service at its first start on a build with #3737; `anchor create` when it is not there; `anchor remove` updates it |

- **What the service takes.** It reads the key only when SYSTEM or
  Administrators own it (TrustedInstaller counts), no rule in its DACL lets
  another account change it or read its values, and its parent,
  `HKLM\SOFTWARE\Roomler`, lets no other account change it, counting what the
  parent hands down to a key made below it later. Only SYSTEM and
  Administrators may create a key below `HKLM\SOFTWARE`, and a key never moves
  from one parent to another. Both keys are opened themselves
  (`REG_OPTION_OPEN_LINK`): a registry link at `Roomler` or `ExecService` is
  refused, never followed, by the reader and by every writer.
- **What the writer makes.** The key is created with exactly
  `O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)`, below a parent the writer creates
  owned by Administrators (`O:BA`, the DACL what `HKLM\SOFTWARE` hands down),
  whoever creates it: a policy that gives what an administrator makes to the
  administrator's own account would otherwise leave a parent the check
  refuses. `anchor create` gives an existing parent that its caller's own
  account owns to Administrators, by handle, when that alone stands in the
  way; a parent that fails for any other reason is left as it is.
- **A key that fails the check** is made again by a writer: deleted, through
  the handle it was judged on, and created again with that exact owner and
  DACL, never sealed where it stands. Deleting it makes every handle opened
  while it was loose fail, so nothing written through one later, or put in
  the key before, is read as the key's. Its `LegacyCopy` is read first and
  written back first, so the copy stays shut. The steps run in that order, so
  a stop at any of them leaves no key, or a sealed key that holds nothing of
  the loose one but its `LegacyCopy`. A key with a key below it, which
  roomlerd never makes, is not made again: delete it from an elevated prompt.
  The SYSTEM service's copy never makes a key again. Every write is read back
  through the service's own check, so an anchor the service would not use is
  an error at `anchor create`, not a device that quietly stays unanchored.
- **No file is read as an anchor**, and no switch makes one be. Builds before
  #3737 kept the anchor in `%ProgramData%\roomler\roomler\exec-service\anchor.json`
  and the host key beside it. Only the one-time copy reads them, once.

**The one-time copy.** The SYSTEM service's first start on a build with #3737
runs it before the doorbell opens (`store::copy_legacy_once`). The command line
and the worker never do.

```mermaid
flowchart TD
  S[service start] --> M{LegacyCopy value?}
  M -- yes --> D[doorbell up; the anchor comes from the key only]
  M -- no --> W[write LegacyCopy = pending]
  W -- fails --> F[logged; nothing copied; the next start tries again] --> D
  W --> K{"ROOMLERD_EXEC_ANCHOR_COPY=0?"}
  K -- yes --> R
  K -- no --> A{Anchor value there already?}
  A -- yes --> N[LegacyCopy = none] --> D
  A -- no --> L{anchor.json in exec-service?}
  L -- no --> N
  L -- yes --> V{"the FR-94 reader, no link on the way, the exact owner and DACL anchor create wrote, https, valid"}
  V -- pass --> C[SshHostKey, then Anchor; LegacyCopy = copied] --> D
  V -- fail --> R[LegacyCopy = refused, and why] --> D
```

- The marker is written before anything is read, so nothing is ever copied
  without it, and a start that finds it copies nothing: nothing that appears
  in the machine folder after it is copied. A start that cannot write it, or
  cannot make the key (its parent fails the check, say), copies nothing, and
  the copy runs again at the next start, until it has recorded an outcome.
- It takes the folder and the file only with the owner and DACL every build
  gave them (`O:BA`, a protected DACL of SYSTEM and Administrators alone), the
  host key under the same rule, and an anchor at an https origin that is not
  on this machine: `localhost`, a name below `.localhost`, a loopback or an
  unspecified address is a lab's origin. The files are read below the handle
  the folder was judged on, so the folder judged is the folder read.
- It writes the anchor as this build's writer would, the keys a newer build
  added kept, and leaves the files where they are.
- The service log says what it did, once: `exec-service: one-time anchor copy`,
  with `outcome=`.

**What a refused anchor shows.** An anchor the copy refused, or a key that fails
the check, leaves the device unanchored as far as the service is concerned:

- the service's health answers `anchored: false` with a `problem` that says why
  (for a refused copy, the reason and the command to anchor again; for a copy
  that started and did not finish, `pending`, the same command), and the
  worker logs `exec-service: the SYSTEM service's anchor is not used` with it;
- the worker stops offering `exec-system` at its next heartbeat, so commands
  take the worker path (`ran_as=worker`), or are refused with `SystemRequired`
  where the org requires SYSTEM;
- `roomlerd anchor status` (elevated) prints the reason;
- the device page still reads *Anchored*: that is the server's record.

The way back is `roomlerd anchor create … --force` from an elevated prompt, or
Setup with an anchor-capable token. `anchor remove` retires the report: the
marker then reads `removed`, with when, and keeps what it said before in
`earlier`.

**Rollback.** A build before #3737 reads only the file, which the copy left in
place, and its credential is still live: a renewal never revokes the earlier
credential of the same anchor. `anchor create` and `anchor remove` on a build
with #3737 delete the file and the host key beside it, so a rollback after
either finds no anchor and takes the worker path, rather than offering an
anchor the server no longer accepts. They delete by handle, and only where an
earlier build left the files: `exec-service` is opened itself (a junction or a
symbolic link there, or on the way to it, is refused) and must have exactly
the owner and DACL `anchor create` gave it, which no account but SYSTEM or an
administrator can give a folder; each file is opened below that folder's
handle, never through a link, and deleted through its own handle only when it
is a plain file with exactly the owner and DACL `anchor create` gave it.
Anything else is left as it is, and logged. The `exec-service` folder itself
stays. After a downgrade, remove an anchor with the newer build, or run
`anchor remove` again after the upgrade: an older build's `anchor remove`
changes only the file, so the upgrade finds the key's anchor again.

**Kill switch.** `ROOMLERD_EXEC_ANCHOR_COPY=0` in the service's own
`Environment`: the first start copies nothing and records `refused`, "switched
off". Anchor the device again to recover. `ROOMLERD_EXEC_VIA_SERVICE=0` and
`rc.exec_via_service = false` stop the service path as before.

An uninstall leaves the key, as it leaves `%ProgramData%\roomler`
([`installation.md`](installation.md#uninstall-every-platform)).

#### The wizard anchors, with the token it installs with (FR-104)

Roomler Setup anchors a machine-wide install (**Daemon — system** or **Daemon —
machine**) itself, so nobody needs to open an elevated prompt afterwards:

- **The token says so.** In the enrollment dialog, on the Windows tab for a
  machine-wide scope, an administrator ticks *Also let administrators run
  commands as SYSTEM on this device*. The server mints the token with an
  `anchor` claim (`POST …/agent/enroll-token {"anchor": true}`,
  `MANAGE_AGENTS`) and logs that choice with the token's `jti`.
- **One token, two uses, one device.** Enrolling records the token's `jti` on the
  device row (`enrolled_with_jti`). `POST /api/agent/anchor` then accepts an
  `anchor` token a second time, for exactly that device, once: it claims
  `"{jti}:anchor"` in `used_tokens` (`exec_service.rs` anchor route). Any other
  device, and a second anchor, is refused. A token without the claim is single
  use across both purposes, as before.
- **The wizard never holds the service's credential.** After the enrollment it
  runs `roomlerd anchor create --server … --token <the same token> --device
  <the id it just enrolled> --force` in an **elevated child**
  (`updater::spawn_elevated`, a second administrator prompt). That child posts,
  receives and writes the credential, as an administrator running it by hand
  would. `--force` because the wizard enrolled this very host a moment before.
- **A declined prompt or a failure leaves the install complete** and unanchored.
  The Done page names why and carries the command to anchor later.
- **The script and manual paths** take the same token. The dialog then shows the
  anchor command to run from the same Administrator PowerShell.

```mermaid
sequenceDiagram
  participant A as admin (dashboard)
  participant W as wizard (Medium)
  participant E as roomlerd anchor (elevated child)
  participant S as server
  A->>S: enroll-token {anchor: true}
  S-->>A: token (claims anchor = true)
  A->>W: the token
  W->>S: POST /api/agent/enroll (claims jti)
  S-->>W: agent id, agent token (row.enrolled_with_jti = jti)
  W->>E: second administrator prompt, then anchor create --token --device
  E->>S: POST /api/agent/anchor (claims "jti:anchor", this device only)
  S-->>E: the service's credential
  E->>E: write the anchor key (SYSTEM + Administrators)
```

⚠️ **Whoever holds an anchor-capable token can anchor the device it enrolls,**
and so speak for that device's SYSTEM service (fake "ran as SYSTEM" results, on
that one device). That is FR-94's delegation for every enrollment token, made
narrower. The dialog says to give such a token only to the person installing as
an administrator; for a managed fleet, IT installs the Background service.

**One live anchor per device.** The service's credential carries the anchor's
id, and the server accepts it only while that is the device row's current
anchor. Anchoring again revokes the earlier one; so does
`DELETE /api/tenant/{tid}/agent/{id}/exec-anchor` (`MANAGE_AGENTS`), without
removing the device. The service renews its credential under the same anchor
well before its year runs out.

```mermaid
stateDiagram-v2
  [*] --> Unanchored
  Unanchored --> Anchored: roomlerd anchor create (elevated)
  Anchored --> Anchored: renew, same anchor_id
  Anchored --> Anchored: anchor create again, new anchor_id
  Anchored --> Unanchored: DELETE exec-anchor
  Anchored --> Removed: the device is removed (the anchor ends)
  Removed --> Unanchored: the same machine re-enrolls
```

| Event | The service's credential | Where a command goes |
|---|---|---|
| `roomlerd anchor create` | minted, carrying a new `anchor_id` | the service, once the device advertises `exec-system` (within about 75 s, no restart) |
| renewal | minted again under the same `anchor_id` | unchanged |
| `roomlerd anchor create` again | every earlier `anchor_id`'s credential refused (401) | the service holding the new anchor |
| `DELETE …/exec-anchor` | refused (401) | the worker, or refused where the org requires SYSTEM |
| `roomlerd anchor remove` on the device | gone from the key (and any file an earlier build kept); the server's record stays | the worker once the device stops advertising `exec-system`, or refused where the org requires SYSTEM |
| the device is removed | ended: the removal clears the anchor (401, "deleted", meanwhile) | nowhere: the device is gone |
| the same machine re-enrolls | **still refused** (401): the revived row has no anchor | the worker, until an administrator anchors it again |

⚠️ **Removing a device ends its anchor** (#2133). Re-enrolling the same machine
(the same `machine_id`) revives the removed row in place, as a new device
(`AgentDao::revive`, #3627): no anchor, and no exec policy either, since every
policy is back at its closed default
([`device-credentials.md`](device-credentials.md#a-revived-device-starts-from-a-new-devices-settings-3627)).
`AgentDao::soft_delete` also clears `exec_anchor` in the same update that
tombstones the row. The anchor file still on the machine is then
useless, since its credential names an anchor the server no longer records. The
device's commands go to the worker until an administrator runs
`roomlerd anchor create` again. Re-enrolling a device that was **never
removed** (a re-run wizard, `roomlerd re-enroll`) keeps its anchor. An
ephemeral enrollment's row is deleted outright (FR-51), so nothing of it can
come back either.

**Routing needs both halves:** the server's record of the anchor, and the
device's `exec-system` word. Either alone sends the command to the worker, as
before. `rc.exec_via_service = false` on the server turns the SYSTEM path off
everywhere; `ROOMLERD_EXEC_VIA_SERVICE=0` on a device withdraws its word.

The whole decision, in the order the code takes it (`agent_exec.rs`
`authorize`, then the dispatch; `exec_service.rs` `routes_via_service`). Every
refusal is audited:

```mermaid
flowchart TD
  R["a command: UI, CLI, bulk, or a device's rc:rpc.request"] --> G{"gates 1–3, and the originating device"}
  G -- no --> D1["refused"]
  G -- yes --> V{"via the service? Windows, anchored on the server,<br/>exec-system advertised on this pod, rc.exec_via_service on"}
  V -- no --> Q{"Windows, and the org requires SYSTEM?"}
  Q -- yes --> D2["refused: system_required"]
  Q -- no --> C{"run_as = console_user?"}
  C -- yes --> L{"Linux or macOS, and this connection<br/>advertises console-user? (FR-97 P5a)"}
  L -- no --> D3["refused: run_as_unavailable"]
  L -- yes --> PU["rc:rpc.exec run_as = console_user: the daemon drops<br/>to the person at its screen, ran_as = console_user"]
  PU --> T2{"someone at the screen?"}
  T2 -- no --> D6["refused on the device: nobody is signed in"]
  C -- no --> W["the process holding the agent socket runs it<br/>(Windows: the worker, ran_as = worker)"]
  V -- yes --> P["exec_pending row; rc:rpc.exec via_service to the worker"]
  P --> K{"the person's exec_enabled, consent,<br/>then the anchor's exec_enabled"}
  K -- no --> D4["refused on the device"]
  K -- yes --> A{"run_as"}
  A -- system --> S["SYSTEM in session 0, ran_as = system"]
  A -- console_user --> T{"someone signed in at the console?"}
  T -- yes --> U["that person's own token, ran_as = console_user"]
  T -- no --> D5["refused: nobody is signed in"]
```

**Gate 4 has two halves on this path**, and both must say yes: the person's
`exec_enabled` (the worker checks it before it rings, exactly as before) and the
anchor's (the service checks it before it claims).

**The account.** A Windows device's execution policy chooses who the service
runs a command as: **SYSTEM** (the default) or **the signed-in user**
(`run_as: console_user`), whom only SYSTEM can reach (`WTSQueryUserToken`), and
which is refused when nobody is signed in. Both need the service, so
`console_user` on a device that cannot take that path is refused
(`run_as_unavailable`) rather than run as the worker, which in SystemContext
mode is SYSTEM, the opposite of the policy. A policy `PUT` that does not name
`run_as` keeps the stored one: a stale client saving the policy must not turn
the signed-in user back into SYSTEM.

**Off Windows (FR-97 P5a)** the same policy means the person at the device's
screen: logind's seat0 on Linux, the owner of `/dev/console` on macOS. No
service is involved: the root daemon drops to them itself, by the drop a `named`
SSH account takes, and refuses when nobody is at the screen
([`roomler-ssh.md`](roomler-ssh.md) § `console_user` on Linux and macOS). The
server says so in the frame (`rc:rpc.exec`'s `run_as`), and it is the one frame
field an older agent must never be sent, because it would ignore the field and
run the command as root:

| Where | What is checked | Source |
|---|---|---|
| The policy `PUT` | `console_user` off Windows needs the agent's stored `console-user` word | [`agent_exec.rs:965`](../crates/modules/fleet/src/agent_exec.rs#L965) |
| Every command (`authorize`) | the connection on this pod advertises `console-user`, else `run_as_unavailable` | [`agent_exec.rs:361`](../crates/modules/fleet/src/agent_exec.rs#L361) |
| The push itself | the hub refuses a frame carrying `run_as` to a connection without the word, whoever the caller | [`hub.rs:3206`](../crates/modules/fleet/src/hub.rs#L3206) |
| The device | `run_as` is honoured through `exec_service::run_as_of`; an unknown word is refused before the consent prompt | [`signaling.rs:5924`](../agents/roomlerd/src/signaling.rs#L5924) |

The word is kept per connection, never read from the stored row, for the reason
`ssh-grant-ack` is: after a rollback the row still promises what the running
agent does not do.

**An org can require SYSTEM.** With `exec_require_system` on (workspace
Settings, `PUT /api/tenant/{tid}/exec-settings`), a Windows command that cannot
take the SYSTEM path is refused as `system_required` instead of running as the
signed-in user.

Every `exec_audit` row that ran records the server's own routing decision as
`ran_as`: `system` or `console_user` through the service, `console_user` for a
Linux or macOS daemon told to drop to the person at its screen (FR-97 P5a),
`worker` otherwise.
That is what the audit view shows. The result the caller gets carries the
device's claim instead, and only the service makes one: a worker's answer has
no `ran_as`. The audit row is authoritative.

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
- ⚠️ **A command can outlive its answer, and the daemon's stop must not wait
  for it** (#2718). On Windows, tokio reads a command's stdout and stderr
  through its blocking pool, and a read stays in flight until every holder of
  the pipe is gone. That includes a process the command started and left
  running, such as `roomlerd self-update`'s installer and watcher, or
  `start /b`. Dropping the runtime waited on such a read with no end. Under a
  service stop that meant the host's 8 s budget and then a terminate, on every
  exec-driven update. Under the daemon's own restart it meant 71.6 s offline,
  measured, until the watchdog forced `exit(2)`. The daemon now shuts its runtime down
  within `runtime_exit::SHUTDOWN_BUDGET` (3 s) and logs `runtime shut down at
  its budget` when it leaves such a read behind. A command here runs in no job,
  on purpose: it has to outlive the daemon it stopped. A Roomler SSH command is the
  opposite ([`roomler-ssh.md`](roomler-ssh.md) § What outlives a session).

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
the caller's org (`crates/modules/fleet/src/socket.rs:1170` `resolve_exec_target`:
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
- Output streaming **on the RPC wire** (the engine
  itself streams for Roomler SSH since FR-89; the `rc:rpc.result` row and its
  audit sample would need a different shape), scheduled probes, and an
  `exec_policies` selector collection shaped like `overlay_policies`.
