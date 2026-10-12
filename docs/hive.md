# Hive — agent sessions

Hive runs coding agents on an organization's own enrolled machines and makes each
run a record the org can see. A member starts a session on a device, in a folder.
The device runs Claude Code headless as one of its own local accounts. The people
the owner invites watch the session in a chat room, and the ones allowed to drive
it prompt it and answer its approval cards, from a desk or a phone. The server
keeps the record and one stub per turn. What the agent and the people said and did
stays on the device.

> Spec, phases and the full field log: [`fr/FR-90-hive-agent-sessions.md`](fr/FR-90-hive-agent-sessions.md)
> (#1827). The whole design, including the replicaset, the vault, knowhow and the
> learning loop, none of which is built yet: [`roomler-hive-design.md`](roomler-hive-design.md).
> This page describes what is built: agent sessions through P1, and P2a's member
> store. Core memory has its own page, [`brain.md`](brain.md).

Nothing runs until three parties turn it on. The server's switch is `hive.tenants`
(§9). A person needs `HIVE_RUN`. A device runs nothing until its owner sets
`hive_enabled`, maps accounts and names folders. The Linux and macOS release builds
carry the `hive` feature since 0.4.122, and the Windows build from P1i-3 on (#1915),
so from the first Windows release cut after 0.4.123. A device whose owner never
turns sessions on keeps nothing for them: no data directory, no `hive.db`, no store
thread ([`store_wanted`](../agents/roomlerd/src/hive/supervisor.rs#L1017)).

```mermaid
flowchart LR
    B["browser<br/>(a member of the session's room)"]
    subgraph S["server: module hive"]
        R[("agent_sessions · agent_approvals<br/>hive_audit · the brain")]
        C["the session's Secret room<br/>notes, turn stubs, approval stubs"]
    end
    subgraph D["device: roomlerd with feature hive"]
        SUP["supervisor<br/>the device's own gates"]
        H["Claude Code, headless,<br/>as the mapped account"]
        ST[("hive.db<br/>the transcript")]
        SC["model sidecar<br/>on loopback"]
        TB["toolbelt<br/>approve"]
    end
    P["model provider"]
    B -->|"REST: start, stop, people, brain"| S
    S -->|"rc:hive.* on the control WS,<br/>metadata only"| SUP
    SUP -->|"rc:hive.state, turn, approval"| S
    SUP --> H
    H -->|"stream-json"| ST
    H -->|"a session token"| SC
    SC -->|"the device's key"| P
    H -->|"MCP over a socket or a pipe"| TB
    ST -.->|"viewer peer, data-only WebRTC:<br/>pages, live events, prompts, answers"| B
```

---

## 1. What a session is

A session is four things, and only the first two are on the server.

| Piece | Where | What it holds |
|---|---|---|
| the record | `agent_sessions` ([`model.rs:109`](../crates/modules/hive/src/model.rs#L109)) | who started it (`owner_id`) and who else drives it; the title and the room; the harness and its own session id, a UUID the server mints so every resume continues one conversation; the device, the folder as typed and the account the device says it mapped; the status, the fence, the brain revision, and `origin` (`started` or `adopted`) |
| the room | a `Secret` chat room at the path `hive-<session>`, bound to `{module: "hive", ref: <session>}` ([`room.rs:35`](../crates/modules/hive/src/room.rs#L35)) | notes the session authors when it starts, is refused or ends; one stub per turn; one stub per approval. Never what was asked or answered |
| the transcript | the device's replica store, `hive.db` ([`store.rs`](../crates/hive-node/src/store.rs)) | every event: prompts with their authors, assistant text, tool calls and their results (an output over 64 KiB keeps its head and tail), approvals, each turn's end with its cost, and Hive's own notes |
| the viewer peer | a data-only WebRTC peer from the device to one browser ([`view.rs`](../agents/roomlerd/src/hive/view.rs)) | the transcript's pages and its live tail one way; a driver's prompts and approval answers the other |

⚠️ **The server holds no transcript, and the frames are shaped so it cannot.**
`rc:hive.start`, `rc:hive.state`, `rc:hive.turn`, `rc:hive.approval`,
`rc:hive.manifest` and the replica frames (`rc:hive.replica.*`, P2c-3a) carry ids,
words, numbers, hashes and display names. Tests in
`signaling.rs` lock each field set, so a `prompt`, `tool` or `input` field would be
a deliberate edit there. The one Hive frame with text a model reads is
`rc:hive.memory`, the curated core memory ([`brain.md`](brain.md)). P0f's canary
test ([`hive_canary.rs`](../crates/tests/tests/hive_canary.rs)) sends a canary
through a prompt, a tool's output and a crashing harness's stderr, and finds each
in the device's store and in no Mongo document, object-store file, server log line
or frame the server sent the browser. It found one leak when it was written: the
`ended` detail carried the harness's last 400 bytes of stderr to the server. Those
are a note in the transcript now, and the server hears only how the harness ended
([`describe_end`](../agents/roomlerd/src/hive/supervisor.rs#L5210)). Since P2d-6 it
runs with the replicaset on and a second device, a member, which must hold every
canary too, carried to it over the replica stream (§12, P2d-6).

**Statuses.** The record mirrors what the device reports. Every transition a device
drives is a compare-and-set on the session's device (the authenticated socket's,
never a field of the frame), its fence and the statuses it may leave
([`dao.rs`](../crates/modules/hive/src/dao.rs)), so a stale frame, another device's
frame or one that crossed a stop on the wire changes nothing.

| Status | Means | Set by |
|---|---|---|
| `starting` | the start was sent; `accepted_at` says whether the device answered | the server |
| `idle` · `running` · `awaiting_approval` | the harness waits for a prompt · a turn runs · a tool call waits for a driver | the device's `rc:hive.state` |
| `stopping` | the owner ordered a stop, and the device has not confirmed it | the server |
| `ended` | over; `end_reason` says how: `stopped`, `exited`, `not_on_device`, `member_removed`, `device_removed`, `tenant_archived`, `hive_not_enabled`, `attribution_changed` | the device, or the server for a device it can no longer ask |
| `refused` | the device refused the start, and `refusal` names its gate | the device's `rc:hive.start_ack` |
| `lost` | the start was never answered and will not be sent again (`never_answered`) | the server |

**A turn** is one prompt and the harness's work on it, ended by the stream-json
`result`. The device writes prompts one at a time. One that arrives during a turn
waits, and at most 8 wait before the next caller is refused, because Claude Code
would queue the prompt itself and its `result` would then close the wrong turn's
stub. A turn's stub is posted when the prompt goes in and edited when the turn
ends, and a report for a turn older than the newest stub changes nothing
([`room.rs:241`](../crates/modules/hive/src/room.rs#L241)):

```
✅ Turn 3 — done · 5 steps · 42 s · $0.12 · asked by Alice
```

The cost is what that turn cost. Claude Code's `total_cost_usd` is its process's
running total, so the device subtracts what the process had spent at the previous
turn's end ([`supervisor.rs:4563`](../agents/roomlerd/src/hive/supervisor.rs#L4563)).

**The transcript** is a hash chain ([`chain.rs`](../crates/hive-node/src/chain.rs)).
Each event carries the session's `seq` (gapless from 1), the `fence` of the lease
that wrote it and `prev_hash`, the BLAKE3 hash of the event before it. It is stored
as its exact JSON text, so a member that cannot parse a newer kind still chains and
serves it. One writer thread owns the SQLite file
([`hive/store.rs`](../agents/roomlerd/src/hive/store.rs)), and every append is
checked against the chain's tip inside the transaction that writes its full-text
index row. The file sits in a directory locked to the daemon (`0700`, or SYSTEM and
Administrators on Windows): no local account reads anyone's transcript.

### The viewer peer

```mermaid
sequenceDiagram
    participant B as browser
    participant S as server (hive)
    participant D as device (roomlerd)
    B->>S: hive:view.open {session}
    Note over S: may read it? device online here with hive-view? rate
    S->>D: rc:hive.view.grant {grant, session, user, may_prompt, ttl_secs}
    D-->>S: rc:hive.view.grant_ack, or a refusal
    S->>B: hive:view.ready {grant, ice_servers}, only now
    B->>S: hive:view.offer
    S->>D: rc:hive.view.offer {sdp, ice_servers}
    D-->>S: rc:hive.view.answer, then rc:hive.view.ice
    S-->>B: the answer, then the candidates
    B-->>D: data-only WebRTC, one DataChannel named hive
    Note over B,D: hello, page, follow, prompt, answer
    B->>S: hive:view.renew, at half the TTL
    S->>D: rc:hive.view.renew, once the right to read is checked again
```

| Rule | Why | Where |
|---|---|---|
| The browser dials only after the device confirmed the grant | a device that would refuse never gets an offer (FR-83's rule) | server [`view.rs:490`](../crates/modules/hive/src/view.rs#L490) `on_device_frame` |
| A grant's TTL is relative (`ttl_secs`, 10 minutes) and renewed at half-life, and each renewal checks again that the viewer may read | a device with a skewed clock cannot expire a fresh grant, and a member taken out of the room loses the view within one TTL | `view_limits`, [`hive.rs:91`](../crates/remote_control/src/hive.rs#L91) |
| Whoever may not read the session, or is in an org the pillar does not serve, is told `not_found` | the answer a bogus id gets | server [`view.rs:191`](../crates/modules/hive/src/view.rs#L191) `open` |
| The server allows 4 grants per browser connection and 20 opens a minute per (user, session); the device serves 8 viewers per session and 32 per device | | server [`view.rs:67`](../crates/modules/hive/src/view.rs#L67), device [`view.rs:263`](../agents/roomlerd/src/hive/view.rs#L263) |
| The device serves a grant on its primary enrollment, with `hive_enabled` on (or `hive_adopt`, for an adopted session), for a session it holds, live or in its store | | device [`view.rs:263`](../agents/roomlerd/src/hive/view.rs#L263) `view_grant` |
| One actor per grant owns the peer, its timers and its channel, and is the one place `close()` runs | ⚠️ a dropped WebRTC peer frees nothing: its ICE sockets belong to tasks the stack spawned, and leaked peers once ate a host's whole ephemeral port range | device [`view.rs:400`](../agents/roomlerd/src/hive/view.rs#L400) |
| Each JSON message travels as binary frames of at most 60,000 bytes: `[v1][id u32][part u16][parts u16][payload]` | one SCTP message over 65,536 bytes is silently lost | [`framing.rs`](../agents/roomlerd/src/hive/framing.rs) |
| `follow` subscribes to the store's live feed first, then catches up from the store | an event that lands in between is in one or the other: never lost, never sent twice | device [`view.rs:817`](../agents/roomlerd/src/hive/view.rs#L817) |
| TURN credentials are minted per grant, with a 12 h TTL | the credential bounds the allocation's whole life, and the configured 600 s would cut a relayed viewer at ten minutes | server [`view.rs:78`](../crates/modules/hive/src/view.rs#L78) |
| ICE is set up as remote control's browser-facing peer: the overlay interface kept out, `.local` candidates resolved by the OS, `ICE_RELAY_TCP` honoured | | device [`view.rs:540`](../agents/roomlerd/src/hive/view.rs#L540) |

The browser half is [`useHiveViewer.ts`](../ui/src/composables/useHiveViewer.ts).
It opens on the last 300 events, keeps at most 1,000 while it follows the newest
(the oldest are a "load earlier" away), and closes its peer on every way out. The
transcript draws each tool call by its tool (a command, an edit as a diff, a file
range, a search) with its result folded under it, and assistant text through
`renderMarkdown`, the one XSS boundary. Everything else is text interpolation
([`HiveTranscript.vue`](../ui/src/components/hive/HiveTranscript.vue)).

---

## 2. Starting one, and who may drive

```mermaid
sequenceDiagram
    participant B as browser (the starter)
    participant S as server (hive)
    participant D as device (roomlerd)
    participant H as Claude Code
    B->>S: POST /api/tenant/{tid}/hive/session {device_id, folder, title?}
    Note over S: a member? the device live in the org? the org not archived?<br/>HIVE_RUN? online here with hive? at most 10 starts a minute
    S->>S: core memory rendered, the room opened, the record at fence 1
    S->>D: rc:hive.memory, when the device advertises hive-memory
    S->>D: rc:hive.start {session, harness_session, fence, folder, user, address}
    Note over D: the primary org? hive_enabled? an account in hive_accounts?<br/>the folder inside hive_roots? capacity? a harness?
    D->>H: launch, as the account
    D-->>S: rc:hive.state idle
    D-->>S: rc:hive.start_ack {account}, or {refused}
    S-->>B: accepted, refused or pending, within 10 s
```

**The server's gates**, in order ([`routes.rs`](../crates/modules/hive/src/routes.rs)):
membership first, so a stranger learns nothing about devices; the device, live in
this org, and a 404 otherwise, as for a bogus id; the org not archived; `HIVE_RUN`;
the device connected here and advertising `hive`; and the per-(user, device)
ceiling of 10 starts a minute, last, so a refusal is attributable to an identity
that passed the others. From the archive check on, a refusal is a `200` that names
the reason, and it is audited in `hive_audit`. That is the exec convention: the row
is what remains when someone probes which devices will run things for them.

`HIVE_RUN` (bit 32) is in no managed role below `ADMINISTRATOR`, pinned by
`no_managed_role_below_administrator_seeds_a_root_shell`
([`role.rs:408`](../crates/db/src/models/role.rs#L408)). Running a session is remote
code execution on a device, gated like exec and SSH.

**The device's gates**, in order ([`decide_and_launch`](../agents/roomlerd/src/hive/supervisor.rs#L1529)).
Each fails closed, and none takes an answer from the server: the start names who is
asking and where, never which account.

| # | Gate | A refusal |
|---|---|---|
| 1 | the frame came on the primary enrollment's connection: a secondary org's admin must not start sessions on a device that org merely borrows | `hive_disabled` |
| 2 | `hive_enabled` | `hive_disabled` |
| 3 | the harness asked for is Claude Code | `other` |
| 4 | a start already running at this fence is answered `accepted` again and launches nothing: reconcile re-sends a start whose answer was lost | — |
| 5 | the daemon is not about to restart for an update, and not stopping | `other`, "start the session again in a minute" |
| 6 | `hive_accounts` maps the starter: by user id first, then by the address the server sent, which is a proven one or the `.invalid` placeholder `users.email` holds otherwise, and that matches nothing ([`gates.rs:106`](../agents/roomlerd/src/hive/gates.rs#L106)) | `no_account` |
| 7 | a session this device hosted resumes as the account it ran as, or not at all: its history is in that account's home | `no_account` |
| 8 | the folder resolves, symlinks and `..` followed, inside a `hive_roots` entry; an empty list means nowhere ([`roots.rs:32`](../crates/hive-node/src/roots.rs#L32)) | `folder_not_allowed` |
| 9 | fewer than `hive_max_sessions` (4) sessions run | `at_capacity` |
| 10 | the store is open, and the sidecar is bound when a key helper is set | `launch_failed` |
| 11 | the launch: the account resolves (uid 0 refused), a harness is found, the account can execute the toolbelt's relay, and the process starts | `no_account` · `harness_missing` · `launch_failed` |

Only a process that started is `accepted`. Each refusal word names a different gate
because each has a different fix, and the words are decoded leniently: an unknown
word from a newer device is still a refusal (`other`), never "accepted"
([`hive.rs:412`](../crates/remote_control/src/hive.rs#L412)). On Windows
`no_console_user` joins them (§3).

**The device's keys** ([`config.rs:384`](../crates/agent-core/src/config.rs#L384)).
Every one is the device owner's, read once when the daemon starts (a change needs a
restart), and none can be pushed:
`the_device_owned_refusals_are_not_pushable`
([`models.rs:5672`](../crates/remote_control/src/models.rs#L5672)) asserts that no
Hive key appears in a serialised `DesiredConfig`. The user-facing reference is
[`configuration.md`](../ui/docs/content/reference/configuration.md).

| Key | Default | Meaning |
|---|---|---|
| `hive_enabled` | off | run agent sessions here |
| `hive_accounts` | empty, so nobody | `{"<user id or proven address>" = "<local account>"}` |
| `hive_roots` | empty, so nowhere | the folders sessions may run in, checked on the resolved path |
| `hive_max_sessions` | 4 | sessions at once |
| `hive_harness` | unset | Claude Code's path; unset, the default locations (§3) |
| `hive_api_key_helper` | unset: no model access | the command the DAEMON runs to print the provider's key (§5) |
| `hive_api_workspace_id` | unset | sent as `anthropic-workspace-id`, for a key that is not scoped to a workspace |
| `hive_update_wait_secs` | 1800 | how long an update waits for running turns; `0` never waits (§7) |
| `hive_core_memory` | off | show the org's core memory to the sessions here ([`brain.md`](brain.md)) |
| `hive_adopt` | off | let this device's people adopt their terminal sessions; off on Windows whatever it says (§8) |
| `hive_allow_passwordless_sudo` | off | macOS: run sessions as an account whose `sudo` needs no password, which gives them root; off, such a start is refused (§3) |
| `hive_replica` | off | hold copies of the sessions this device's owner runs on their other devices (§12, P2c) |
| `hive_archive` | off | offer this device as the org's archive replica; only ever with `hive_replica` |
| `hive_store_quota_mib` | unset: the disk's | the most this device keeps as a replica, in MiB |

### Who may drive

A session's room has members, and a session has drivers
([`access.rs`](../crates/modules/hive/src/access.rs)). The owner names both. What
anyone writes in the room is ordinary chat. Only a driver's "ask the agent" reaches
the harness, over that driver's own viewer peer.

| Who | Reads it | Prompts it, answers its approvals | Stops it, names its drivers |
|---|---|---|---|
| its owner | ✅, even out of its room | ✅ while it is live | ✅ |
| a driver the owner named | ✅ | ✅ while it is live | — |
| another member of its room (a reader) | ✅ | — | — |
| anyone else, in the org or not | — (a 404, as for a bogus id) | — | — |

| Rule | Why | Where |
|---|---|---|
| Naming a driver is the owner's alone, and the person must hold `HIVE_RUN`. At most 16 besides the owner, held by the update's own filter | driving runs code on the device as the session's account, since a driver answers approvals too | [`participants.rs:196`](../crates/modules/hive/src/participants.rs#L196) |
| A change ends the person's open views (`role_changed`), and the view they reopen is minted afresh | a grant carries `may_prompt` as it was minted | server [`view.rs:624`](../crates/modules/hive/src/view.rs#L624) |
| ⚠️ The device decides whom it lets act as its account. A driver other than the starter prompts and answers only when the device's own `hive_accounts` maps them to the account the session runs as; otherwise the view is read only, and `hello` says why (`driving_refused`). The starter is recognised by id, from the start order, never through the map | the gate that survives a wrong server (decision 8). A server before P1c-2 sends no address, and the starter must not be locked out of their own session | [`drives_here`](../agents/roomlerd/src/hive/supervisor.rs#L2029) |
| The model reads who asked, as `[Name] text`. A slash command goes as typed, and the name loses brackets and control characters and is cut to 64 characters | several drivers can share a session, and a display name must not be able to close the label | [`launch.rs:316`](../crates/hive-node/src/launch.rs#L316) `attributed_prompt` |
| A stub's `prompted_by` and an approval's `answered_by` are believed only when they name a driver | both are the device's word | [`room.rs:241`](../crates/modules/hive/src/room.rs#L241), [`room.rs:319`](../crates/modules/hive/src/room.rs#L319) |
| A member removed from the org drives nothing anywhere, and their views end at once | rejoining the org must not hand back a seat nobody offered again | [`hooks.rs:154`](../crates/modules/hive/src/hooks.rs#L154) |
| Every change is a note in the room, and audited (`participant`) | who may prompt the agent is for everyone there to see | |

---

## 3. Who runs it, on each platform

### Linux and macOS

The daemon runs as root, and a session runs as the account `hive_accounts` maps the
starter to, through the one privilege path that exec, SSH and the PTY share, as an agent session
runs it: [`exec::apply_session_run_as`](../agents/roomlerd/src/exec.rs#L853). The
account is resolved in the parent (`getpwnam_r`, `getgrouplist`), uid 0 is refused
([`exec.rs:955`](../agents/roomlerd/src/exec.rs#L955)), and the child only calls
`setgroups`, `setgid` and `setuid`, then checks that they took.

⚠️ **A session holds no administrator group** (decision 13), as Windows denies its
sessions the Administrators group:
- It holds none of the account's administrator groups
  ([`exec.rs:1028`](../agents/roomlerd/src/exec.rs#L1028)): root's own, the sudoers groups
  (`wheel`, `sudo`, macOS's `admin`) and those whose socket or device is root by another
  door (`docker`, `lxd`, `incus`, `libvirt`, `disk`). An account whose primary group is one
  is refused ([`exec.rs:1093`](../agents/roomlerd/src/exec.rs#L1093)). The kernel then
  grants the session nothing by those groups: no docker or libvirt socket, no disk
  device, no write into macOS's `/Applications` (`root:admin 775`).
- On Linux the harness also starts with `no_new_privs`
  ([`exec.rs:1150`](../agents/roomlerd/src/exec.rs#L1150)), so nothing it runs gains a
  privilege by exec: no `sudo`, whatever sudoers says of the account.

| A session, as an account whose `sudo` needs no password | Linux | macOS |
|---|---|---|
| holds `sudo`, `wheel` or `admin` | no | no: a write into `/Applications` is refused |
| `sudo` by a rule that names the account | refused, by `no_new_privs` | **would be allowed**, so the start is refused |
| `sudo` by a rule that names a group (`%admin`, `%sudo`) | refused, the same way | **would be allowed**, so the start is refused |

⚠️ **On macOS the groups cannot stop `sudo`.** macOS has no `no_new_privs`, and its `sudo`
reads the account's groups from the directory, never from the process. Field-measured on
macOS 15.7.7 (sudo 1.9.13p2), with `%admin ALL=(ALL) NOPASSWD: ALL` the only passwordless
rule: a session without the admin group got `sudo-rc=0`, while its write into
`/Applications` was refused. `id` misleads on a Mac too: it reads the directory, so it
lists `admin` in a session that does not hold it.

So a Mac refuses to START a session as an account whose `sudo` needs no password
(decision 15, [`hive/sudo.rs`](../agents/roomlerd/src/hive/sudo.rs)), unless its owner sets
`hive_allow_passwordless_sudo`:

```mermaid
flowchart TD
    S["a start (or a resume after a restart), on a Mac,<br/>past every other gate"] --> A{"hive_allow_passwordless_sudo?"}
    A -->|on| L["launch"]
    A -->|off| Q["as root: sudo -l -U &lt;account&gt;<br/>(LC_ALL=C, 10 s)"]
    Q -->|"a NOPASSWD rule, or !authenticate"| R1["refused: passwordless_sudo"]
    Q -->|"every rule asks, or no sudo at all"| L
    Q -->|"no answer, or one it cannot read"| R2["refused: launch_failed, in words"]
```

⚠️ The check asks **as root**, never as the account. Listing another account's rules
authenticates nobody: in the field the root listing left only directory lookups in the
unified log. Asked as the account (`sudo -n -l`), it would fail on every Mac whose `sudo`
asks for a password, the default, and have PAM try to authenticate the account at every
start (`pam_sm_authenticate(): … Error obtaining the authtok`). A Mac nobody changed lists
`(ALL) ALL`, which asks, so it starts sessions as before.

The daemon never writes into a tree the account owns. It starts `/bin/sh -c` with a
fixed script ([`WRAPPER`](../agents/roomlerd/src/hive/supervisor.rs#L108)) and
positional arguments, never interpolated, and that script runs as the account:
`umask 077`, make the session's config directory, copy core memory in where nothing
is yet, `cd` into the folder, and `exec` Claude Code. The environment is cleared
first and rebuilt from `HOME`, `USER`, `LOGNAME`, `PATH` (the account's
`~/.local/bin` and `~/.cargo/bin` before the system's) and `LANG`, then Hive's own
variables, so none of the daemon's `ROOMLERD_*` knobs reaches a session. The harness
leads its own process group, so a stop reaches the tools it started.

| What | Where |
|---|---|
| the session's config directory (`CLAUDE_CONFIG_DIR`) | `~<account>/.roomler/hive/<session>/claude`, with the project name pinned to `hive-<harness uuid>` (`CLAUDE_CODE_PROJECT_DIR_NAME`), so the history and the auto-memory live under one `projects/hive-<uuid>/` whatever the folder is ([`launch.rs:129`](../crates/hive-node/src/launch.rs#L129)) |
| the runtime directory, the daemon's | `/run/roomler-hive/<session>` on Linux, `/var/run/roomler-hive/<session>` on macOS, root's and `0755`: `settings.json` and `mcp.json` (root's, `0644`), `toolbelt.sock` (the account's, `0600`), and `memory/` |
| the harness, with `hive_harness` unset | the first executable of `~/.local/bin/claude`, `/usr/local/bin/claude`, `/opt/homebrew/bin/claude` (macOS) and `/usr/bin/claude` ([`supervisor.rs:3899`](../agents/roomlerd/src/hive/supervisor.rs#L3899)) |

**The launch is rebuilt in full every time**, because a resume restores neither
`--settings` nor `--mcp-config` ([`launch.rs:183`](../crates/hive-node/src/launch.rs#L183)):

```
claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages
       --session-id <uuid>                      (or --resume <uuid>)
       --settings <runtime>/<session>/settings.json
       --mcp-config <runtime>/<session>/mcp.json --strict-mcp-config
       --permission-prompt-tool mcp__roomler__approve
       --permission-mode default
       --disallowedTools AskUserQuestion
```

| Flag or setting | Why |
|---|---|
| `--permission-mode default` | ⚠️ unset, a `-p` run that fetches no feature flags, which is every run behind the sidecar, starts in `auto`, where a classifier rather than a person decides what runs. P1a's contract probe found it |
| `--strict-mcp-config` | a repository's `.mcp.json` cannot shadow `roomler`, the server that decides what runs |
| `--disallowedTools AskUserQuestion` | it would reach the permission tool as a tool call whose answer must carry the person's choices, which no card asks for; the model asks in its reply instead |
| `settings.json` holds `permissions.deny: ["Read(//proc/**)"]`, `sandbox.autoAllowBashIfSandboxed: false` and no `apiKeyHelper` ([`launch.rs:364`](../crates/hive-node/src/launch.rs#L364)) | the Read tool runs inside the harness, so `/proc/self/environ` would hand the model its environment; a sandbox that comes on, ours or the user's, must not take Bash out of the approvals; nothing in the session can print the provider's key |
| `--resume` exactly when Claude Code's own history exists (`projects/hive-<uuid>/<uuid>.jsonl`), `--session-id` otherwise, for every launch ([`supervisor.rs:3622`](../agents/roomlerd/src/hive/supervisor.rs#L3622)) | Claude Code refuses both other ways round: "Session ID … is already in use", "No conversation found". The daemon only checks that the entry exists, and never reads it |
| ⚠️ no `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` | on Linux it sandboxes every command, and without bubblewrap and socat Bash then refuses everything. It was the real reason P0's sessions could not run `whoami` (P1a-3) |

### Windows

A Windows device runs a session as the user signed in at the console, at Medium
integrity, and as nobody else (design decision D2). There is no S4U and no
`LogonUser`: the daemon asks for no credential. With nobody signed in a start is
refused `no_console_user`. `hive_accounts` must map the starter to that user, as
`name`, `DOMAIN\name` or `.\name`, compared case-insensitively
([`hive_win.rs:397`](../agents/roomlerd/src/hive_win.rs#L397)); otherwise the start
is refused `no_account`, and the refusal names who is at the console.

The process that hosts sessions is the service's worker. Who the worker is depends
on how the device was installed, never on who connects to it:

```mermaid
flowchart LR
    SVC["the service<br/>SYSTEM, session 0"] -->|"installed with SystemContext:<br/>always"| WS["worker: SYSTEM,<br/>in the console session"]
    SVC -->|"installed without it"| WU["worker: the console user,<br/>elevated"]
    WS -->|"WTSQueryUserToken,<br/>filtered for a UAC administrator"| H["harness: the console user at Medium,<br/>in its own Job Object"]
    WU -->|"a restricted, Medium copy<br/>of its own token"| H
    WS --- D[("%ProgramData%\roomler\roomler\hive<br/>hive.db · hosted.json · run")]
    WU --- D
```

| Rule | Why | Where |
|---|---|---|
| ⚠️ A SystemContext worker is SYSTEM from the start and stays SYSTEM. With `ROOMLERD_ENABLE_SYSTEM_SWAP` on, every cycle of the service reads as if a controller were connected, so the worker is never swapped as one connects or leaves. Without SystemContext the worker is the console user throughout, elevated (`ROOMLERD_ELEVATE_WORKER`, on by default) | P1i-0 expected a remote-desktop connection to swap the worker and cut the running turns, from a gate 0.3.0-rc.7 retired. P1i-4 measured that it does not | [`win_service/supervisor.rs:869`](../agents/roomlerd/src/win_service/supervisor.rs#L869), [`:746`](../agents/roomlerd/src/win_service/supervisor.rs#L746) `decide_spawn` |
| Either way the session is the same person at Medium. A SYSTEM worker takes the console session's token; a worker that is the user takes a restricted, Medium copy of its own token: every administrator group deny-only, no privilege but traverse (FR-85's recorder rule) | a session never holds an administrator's rights on Windows | [`hive_win.rs:135`](../agents/roomlerd/src/hive_win.rs#L135) `console_user`, [`win_token.rs:209`](../agents/roomlerd/src/win_token.rs#L209) |
| `roomlerd hive-prep` does the wrapper's work as the user, in a Job Object of its own with a 30 s deadline: the config directory, core memory copied where nothing is, and the folder opened as the user | the daemon never writes into the user's profile | [`hive_win.rs:677`](../agents/roomlerd/src/hive_win.rs#L677) `prep`, [`:730`](../agents/roomlerd/src/hive_win.rs#L730) `run_prep` |
| The harness is created suspended, assigned to its own Job Object (`KILL_ON_JOB_CLOSE`, no breakaway), and only then resumed. It inherits its three standard handles and nothing else | none of its tools escapes the job, and no other session's pipe leaks into it | [`spawn_into_job`](../agents/roomlerd/src/win_service/supervisor.rs#L2293), [`JobObject`](../agents/roomlerd/src/win_service/supervisor.rs#L2176) |
| The environment is the user's own block with the session's variables laid over it by name, case-insensitively, and a NUL in a name or a value is refused | `Path` and `PATH` are one variable, and a NUL could smuggle in an entry of its own | [`merge_env_block`](../agents/roomlerd/src/win_service/supervisor.rs#L2589) |
| The harness is `hive_harness`, else the native installer's `%USERPROFILE%\.local\bin\claude.exe`, else npm's `%APPDATA%\npm\claude.cmd` run through `cmd.exe /d /v:off /s /c`. An argument holding one of cmd's metacharacters is refused, never escaped | npm's shim passes the line on again with `%*` inside an `IF (…)` block, and no quoting is right for both readers | [`hive_win.rs:523`](../agents/roomlerd/src/hive_win.rs#L523) `harness_command_line` |
| A stop closes the harness's stdin, then ends its job after the grace | no signal reaches a console-less process | [`hive/supervisor.rs:5198`](../agents/roomlerd/src/hive/supervisor.rs#L5198) |

---

## 4. Approvals over the toolbelt

Every tool call that Claude Code's read-only rules do not let through waits for a
driver. Claude Code calls its permission-prompt tool, `mcp__roomler__approve`, on the
session's toolbelt: a `roomler` MCP server, one per session, with one tool
([`toolbelt.rs`](../agents/roomlerd/src/hive/toolbelt.rs)).

Claude Code starts its MCP servers itself, as the session's account, over stdio. So
the server it starts is a relay, `roomlerd hive-mcp <socket>`, which pipes its stdin
and stdout to the session's endpoint. The relay is short-circuited at the top of
`daemon_main`, so it runs none of the daemon's start-up
([`main.rs:1098`](../agents/roomlerd/src/main.rs#L1098)), and the daemon speaks MCP on
the other end.

| | Linux, macOS | Windows |
|---|---|---|
| the endpoint | `<runtime>/<session>/toolbelt.sock`, handed to the session's account, `0600`, in a directory only the daemon writes | `\\.\pipe\roomler-hive-<session>-<nonce>`, made with `FILE_FLAG_FIRST_PIPE_INSTANCE` so a name someone holds is refused, never joined; remote clients rejected |
| who may connect | the kernel admits the account and root, and the daemon checks the peer's uid again on every connection | the DACL grants SYSTEM and Administrators, and the account `0x12008b`: read and write, but not `FILE_CREATE_PIPE_INSTANCE`, which would let it add an instance of its own and take the next connection. Each client's SID is checked again, and the relay opens the pipe with exactly that access, at the identification level ([`hive_win.rs:823`](../agents/roomlerd/src/hive_win.rs#L823), [`:944`](../agents/roomlerd/src/hive_win.rs#L944)) |

⚠️ **It is deliberately not the LocalAPI socket.** On a root daemon that one is
root-only, and most of its verbs trust the socket alone. A process running as the
session's account can reach the toolbelt too, and all it can do there is ask.

⚠️ **What fails, fails closed.** A relay that cannot reach its endpoint leaves Claude
Code without its permission tool. Every call that needs one then errors ("MCP tool
mcp__roomler__approve … not found") and the harness exits 1: nothing runs
unapproved. So a start whose account cannot execute the relay at all is refused
`launch_failed` at once, rather than dying at its first approval
([`executable_by`](../agents/roomlerd/src/hive/supervisor.rs#L4129)).

```mermaid
sequenceDiagram
    participant H as Claude Code (the account)
    participant D as roomlerd (toolbelt, viewer peer)
    participant S as server (hive)
    participant V as a driver's browser
    H->>D: tools/call approve {tool_name, input, tool_use_id}
    D->>D: approval_requested in the transcript, the id in hosted.json
    D->>S: rc:hive.approval {approval_id, turn, status open}
    S->>S: one agent_approvals record, one stub in the room
    S-->>V: in the app, and a web push to every driver in the room
    D-->>V: over the peer: the open approvals, and the call's input
    V->>D: over the peer: answer {approval, allow or deny, message?}
    D-->>H: allow with the input, or a denial with its reason
    D->>S: rc:hive.approval {status allowed, answered_by}
    S->>S: the stub edited, the record closed
```

| Rule | Why | Where |
|---|---|---|
| The server hears THAT a session waits, how it ended and who answered. `rc:hive.approval` carries an id, a turn, a word and a user id, and its field set is locked by test. The stub reads "🔐 Approval needed · turn 2 — open the session to answer", then "🔐 Approval · turn 2 — ✅ allowed by Alice" | which tool, and what it would do, travel only over the viewer peer (AC5) | [`room.rs:319`](../crates/modules/hive/src/room.rs#L319) |
| One `agent_approvals` record per (session, approval), held by a unique index, its end a compare-and-set on `open`; kept 90 days | a replayed frame posts no second stub | [`lib.rs:245`](../crates/modules/hive/src/lib.rs#L245) |
| The notification names the session and never the call: `Claude · <device> needs approval`, by web push to every subscription of every driver still in the room | a desk with the app open is not a phone in a pocket | [`room.rs:546`](../crates/modules/hive/src/room.rs#L546) |
| Only a driver's grant answers; a reader's `answer` is refused on the device | | device [`view.rs:864`](../agents/roomlerd/src/hive/view.rs#L864) |
| Unanswered for 25 minutes is a denial that says so. Progress goes to Claude Code every 60 s while a person decides, and the tool-call timeout in the MCP config is 30 minutes | what the model reads is our answer, never a transport error | [`toolbelt.rs:69`](../agents/roomlerd/src/hive/toolbelt.rs#L69) |
| At most 4 approvals are open per session, and a fifth is refused at once | Claude Code asks one at a time (measured), so more is not Claude Code | [`toolbelt.rs:75`](../agents/roomlerd/src/hive/toolbelt.rs#L75) |
| The harness letting go (`notifications/cancelled`, a closed relay), a stop, or the session's end withdraws an open approval | | [`toolbelt.rs:660`](../agents/roomlerd/src/hive/toolbelt.rs#L660) |
| A denial reaches the model as a failed tool result with our words verbatim, for example "Alice denied this: not now" | the model knows who said no, and why | |

---

## 5. The model sidecar

The provider's key never enters a session. The daemon runs the device's
`hive_api_key_helper` itself, as SYSTEM or root, and keeps what it prints. The
harness gets a loopback base URL and a token that only this sidecar honours
([`sidecar.rs`](../agents/roomlerd/src/hive/sidecar.rs)).

```mermaid
flowchart LR
    H["Claude Code, as the account<br/>ANTHROPIC_BASE_URL = 127.0.0.1:port/s/session<br/>ANTHROPIC_API_KEY = this run's token"] -->|"x-api-key: the token"| SC
    subgraph D["roomlerd, SYSTEM or root"]
        SC["sidecar: this session's token?<br/>does it run here at that fence?<br/>offline for at most 120 s?"]
        K["hive_api_key_helper prints the key<br/>cached 5 min, run again after a 401"]
    end
    SC -->|"x-api-key: the device's key,<br/>the body and anthropic-* unchanged"| P["provider"]
    P -->|"streamed back as it arrives"| SC
    K -.-> SC
```

| Piece | Rule | Why | Where |
|---|---|---|---|
| the token | 256 random bits, bound to (session, fence), minted just before the harness starts and taken back if it does not; each run revokes exactly its own token | a run that ends as the same session starts again here cannot take the new run's | [`sidecar.rs:89`](../agents/roomlerd/src/hive/sidecar.rs#L89) |
| the paths | exactly `POST /v1/messages` and `POST /v1/messages/count_tokens`; anything else is `404 not_found_error` | the device's key opens more than inference: the Files API holds every session's uploads under it, and a batch outlives its session and its fence | [`sidecar.rs:328`](../agents/roomlerd/src/hive/sidecar.rs#L328) |
| the fence | a call is forwarded only while the session runs HERE at the token's fence | a stopped session, or a stale primary after a move, spends nothing | [`sidecar.rs:128`](../agents/roomlerd/src/hive/sidecar.rs#L128) `sidecar_admit` |
| `offline_grace` | 120 s after the primary control connection closed, with no newer one in its place, calls are refused `503 overloaded_error`; they flow again on reconnect | a partitioned primary stalls instead of diverging. "Closed" is the agent's own verdict, so a half-open socket counts as up until `WS_RX_DEADLINE` (80 s) ends it, and the worst case is about 200 s | [`sidecar.rs:64`](../agents/roomlerd/src/hive/sidecar.rs#L64), the agent's [`signaling.rs:60`](../agents/roomlerd/src/signaling.rs#L60) |
| the forward | the headers unchanged but for `x-api-key` and `authorization` (the device's key swapped in), the hop-by-hop ones, and `anthropic-workspace-id`, set from `hive_api_workspace_id` when that is configured; a body of at most 32 MiB; the answer streamed as it arrives, `retry-after` and the rate-limit headers intact; TLS through the OS trust store and the proxy environment | an SSE turn is never buffered, and a TLS-inspecting middlebox the machine trusts still works | [`sidecar.rs:380`](../agents/roomlerd/src/hive/sidecar.rs#L380) `handle` |

The helper runs through `/bin/sh -c` on Unix and `cmd.exe /d /s /c "<helper>"`, by
its full path, on Windows, within 10 s. The first line it prints is the key, and
neither its output nor its errors' output is logged
([`sidecar.rs:218`](../agents/roomlerd/src/hive/sidecar.rs#L218)). With no helper a
session has no model access: its own config directory holds no login of its own.

---

## 6. Restart, resume, and what ends a harness

A restart still takes every harness down. Under systemd (`KillMode=control-group`)
a stop reaches the daemon and then every process in its unit. Under launchd, and for
a daemon nothing supervises, the daemon takes them down itself as it leaves
([`wind_down`](../agents/roomlerd/src/hive/supervisor.rs#L2889): SIGTERM to each
harness's process group, SIGKILL after 3 s). On Windows a harness lives in its
worker's Job Object and goes with it. What survives is the session: the next daemon
resumes it with the same Claude Code conversation.

```mermaid
sequenceDiagram
    participant D as roomlerd, going down
    participant F as hosted.json
    participant N as roomlerd, next start
    participant S as server
    Note over D,F: a launch writes the entry, and a turn writes its number before its stub
    D->>D: shutdown signalled, begin_shutdown, the record frozen
    D--xD: the harness dies with it: kept, nothing reported
    N->>F: read this enrollment's entries
    N->>S: the control WS comes up
    N->>N: a harness a crash left running is taken down, by pid and start time
    N->>N: every gate again, as the device is configured now
    N->>N: relaunch with --resume, the turn count carried on
    N->>S: idle, the cut turn interrupted, its approvals withdrawn
    N->>S: rc:hive.manifest, after the resume
```

| Rule | Why | Where |
|---|---|---|
| `hosted.json` sits beside the store, root's, `0600`, written whole. Per session it holds the launch's inputs (fence, account, the folder as asked, the starter and their address, Claude Code's session id), the turns begun, the turn in progress and who asked for it, its open approvals, and the harness's pid and start time | after a restart the device holds nothing else: the replay of its last reports lived in memory | [`hosted.rs`](../agents/roomlerd/src/hive/hosted.rs) |
| A new turn's number and a new approval are written before the frame that tells the server, and so is a turn's end; an approval's end is written after its frame | a reused turn number would have every later stub ignored as older than the newest, and a finished turn must never be reported cut. A withdrawal sent twice changes only an approval still open | [`supervisor.rs:4518`](../agents/roomlerd/src/hive/supervisor.rs#L4518) |
| From `begin_shutdown` on, the record is frozen, the toolbelt is ignored, and new prompts and starts are refused ("this device is restarting"). `begin_shutdown` runs the moment any shutdown is signalled: an update, a requested restart, a rollback, an OS stop | the teardown's frames go into closing connections; recorded, they would be lost twice. Field, 2026-10-08: an approval the teardown withdrew stayed "needed" | [`main.rs:3607`](../agents/roomlerd/src/main.rs#L3607), [`supervisor.rs:2851`](../agents/roomlerd/src/hive/supervisor.rs#L2851) |
| A harness that ends without a stop waits 1 s before its end counts, and if the daemon was told to stop by then the session is kept and nothing is reported | under systemd the daemon hears its stop a moment before the harness dies | [`supervisor.rs:2913`](../agents/roomlerd/src/hive/supervisor.rs#L2913) |
| The next daemon resumes at the first connection of its primary enrollment, once, and that connection's manifest waits for it, at most 60 s | a manifest sent first would leave the resuming sessions out, and the server would end every one. A device that never comes back online launches nothing | [`supervisor.rs:2683`](../agents/roomlerd/src/hive/supervisor.rs#L2683) |
| ⚠️ Every gate a start passes is passed again, as the device is configured now: `hive_enabled`, the starter mapped to the SAME account, the folder inside `hive_roots`, capacity, a harness. A refusal ends the session ("not resumed after the device restarted: …") and forgets it | an owner who turns sessions off, or remaps an account, and restarts must not find the session back | [`supervisor.rs:2716`](../agents/roomlerd/src/hive/supervisor.rs#L2716) |
| A recorded pid is signalled only while its start time still matches: the boot id and the `/proc` start time on Linux, `proc_pidinfo` on macOS, `GetProcessTimes` on Windows. Pids 0 and 1 never are | two harnesses on one history would both write it, and whatever holds that pid since must be left alone | [`procs.rs:30`](../agents/roomlerd/src/hive/procs.rs#L30), [`supervisor.rs:5161`](../agents/roomlerd/src/hive/supervisor.rs#L5161) |
| The cut turn is reported `interrupted`, naming who asked; its approvals are reported `withdrawn`; the transcript says the session resumed. The first turn of a resumed process reports no cost | Claude Code restores its running total only from a clean exit's `cost-state`, so where that process starts counting is unknown, and no number is better than a wrong one | [`supervisor.rs:3275`](../agents/roomlerd/src/hive/supervisor.rs#L3275) |
| A session resumed 3 times in a row, each time without outliving the resume by 2 minutes, is ended instead | a resume that takes the daemon down must not become a crash loop | [`supervisor.rs:219`](../agents/roomlerd/src/hive/supervisor.rs#L219) |
| A stop that arrives before the session resumed ends it with no launch | | [`supervisor.rs:2172`](../agents/roomlerd/src/hive/supervisor.rs#L2172) |

⚠️ Prompts waiting behind a turn are lost when a restart cuts it. An update waits the
turn out (§7); a crash does not. ⚠️ `hosted.json` is not synced to disk: a clean
stop, a restart and a crash all keep it, a power cut may lose its last change, and
then the manifest ends what could not be resumed.

### The manifest

On every connection of its primary enrollment the device sends `rc:hive.manifest`:
the sessions it runs NOW, ids and fences only
([`supervisor.rs:2645`](../agents/roomlerd/src/hive/supervisor.rs#L2645)). The server
ends each session it holds as running there that the list leaves out, as
`not_on_device` (or `stopped`, for one being stopped), tells its room and withdraws
its open approvals ([`agent_socket.rs:696`](../crates/modules/hive/src/agent_socket.rs#L696)).

- ⚠️ "Running there" means live and launched on the device's own word: its answer to
  the start (`accepted_at`), or a run state only the device reports (`idle`,
  `running`, `awaiting_approval`). The state usually lands before the answer, so a
  socket that drops between the two leaves `idle` with no `accepted_at`, and with
  `accepted_at` alone that session outlived its harness for ever
  ([`dao.rs:797`](../crates/modules/hive/src/dao.rs#L797)). A start the device has
  said nothing about stays reconcile's, which re-sends an unanswered start for 10
  minutes and an unconfirmed stop on every connection
  ([`agent_socket.rs:918`](../crates/modules/hive/src/agent_socket.rs#L918)).
- ⚠️ A device that comes back with an agent that runs no sessions ends what it ran:
  a connection the hub records without `hive` is given the empty manifest before
  anything pending is read. Field, 2026-10-10: a Windows device updated itself to
  0.4.123, whose build has no `hive`, and its accepted session stayed `idle` for
  ever. Only a definite "no `hive`" counts: a connection a newer one displaced
  decides nothing ([`agent_socket.rs:940`](../crates/modules/hive/src/agent_socket.rs#L940)).
- A list longer than 256 is no device's and changes nothing, and a build that runs
  Hive but predates the manifest sends none, which changes nothing either.
- The other way round: a device that reports it RUNS a session whose record is over,
  because its starter was removed while it was away, is answered with a stop
  ([`agent_socket.rs:745`](../crates/modules/hive/src/agent_socket.rs#L745)).

### What ends a harness

| Event | What happens to the harness | The session |
|---|---|---|
| the owner's Stop | Unix: stdin closed, SIGTERM to its process group, SIGKILL after 5 s. Windows: stdin closed, its job ended after 5 s | ends `stopped` |
| the harness exits or crashes | — | ends `exited`; a crash's last words on stderr stay in the transcript |
| a daemon restart, an update, a crash | it goes with the unit's control group (systemd), by `wind_down` as the daemon leaves, or with its worker's Job Object (Windows). One a crash left running is taken down by the next daemon, before the resume | kept, and resumed by the next daemon |
| Windows: the console session changes (a sign-in, a sign-out, a switch of user) | the service starts a worker in the new session, and the old worker's harnesses go | resumed by the next worker, if its gates still allow it |
| Windows: a remote-desktop connection | nothing: the worker is not swapped (P1i-4, measured) | goes on, a turn waiting at its approval included |
| the gates changed across a restart; three quick resumes | not launched | ends, saying why |
| the starter leaves the org, the device is removed, the org is archived, the org leaves `hive.tenants`, the device comes back without `hive` | stopped, if the device is still connected here | the server ends it: `member_removed` · `device_removed` · `tenant_archived` · `hive_not_enabled` · `not_on_device` |

---

## 7. The update hold

An update restarts the daemon, and every harness with it. So it waits for running
turns first.

```mermaid
flowchart LR
    U["an update is ready<br/>(periodic, or pushed)"] --> T{"agent turns running?<br/>(running or awaiting_approval)"}
    T -- no --> H["hold new prompts and starts,<br/>then look once more"]
    H --> I["install: the daemon restarts"]
    T -- "yes, within hive_update_wait_secs" --> W["wait, logged at once<br/>and every minute"] --> T
    T -- "yes, past it" --> C["install anyway:<br/>the turns are cut, with a warning"] --> I
    I --> R["the sessions resume"]
```

| Platform | Who installs | How the wait runs |
|---|---|---|
| Linux, Windows | the daemon's in-process updater | [`wait_for_agent_turns`](../agents/roomlerd/src/updater.rs#L278) runs before the installer is spawned, for the periodic check and a pushed update alike |
| macOS | the root update helper, `com.roomler.update`, never the daemon | before `installer(8)` the helper asks the root daemon, over its system LocalAPI socket, to hold (`HiveUpdateHold`). The daemon runs the same wait and answers `HiveUpdateHeld {waited_secs, cut}`, then refuses new prompts and starts until that connection closes ([`updater.rs:349`](../agents/roomlerd/src/updater.rs#L349), [`:2805`](../agents/roomlerd/src/updater.rs#L2805)) |

- It waits at most `hive_update_wait_secs` (30 minutes; `0` never waits), looks again
  every 5 s, and logs "update deferred — agent turns running" when it starts and once
  a minute.
- It holds a PUSHED update too: a person asked for the update, not for their agent to
  be cut. It is bounded, so a busy session cannot hold a device's updates back.
- Once no turn runs, new prompts and starts are refused BEFORE a last look, so none
  begins in the gap. One admitted before that still runs, and is waited for. An
  installer that fails to start releases the hold.
- ⚠️ The macOS hold is bound to the helper's connection, not to a timer: a helper
  that dies mid-wait releases it, and so does a failed install. The verb is refused
  to every peer but root, because a hold refuses every new prompt and start on the
  device ([`localapi/src/lib.rs:2503`](../crates/localapi/src/lib.rs#L2503)). A missing, older or
  silent daemon (the helper waits at most 2 h) gets the install it got before: the
  helper fails open.
- ⚠️ `roomlerd self-update`, the CLI, does not wait. It is an operator's explicit
  command, in another process.

---

## 8. Adopting terminal sessions

People already run Claude Code in a terminal. `roomler hive adopt` mirrors those
sessions into the device's store and lists each for its owner alone, read only
(decision 11). A terminal session passed none of Hive's start gates, so adopting has
gates of its own, each default-deny and each owned by a different party. It is Linux
and macOS only: on Windows `hive_adopt` reads off whatever the config says, so the
device never advertises `hive-adopt` ([`gates.rs:73`](../agents/roomlerd/src/hive/gates.rs#L73)),
and the CLI refuses.

| Gate | Owner | Default | A refusal |
|---|---|---|---|
| the hooks are installed (`SessionStart`, `Stop`, `SessionEnd`) | the person, in their own user-level Claude Code settings | not installed | nothing is mirrored |
| `hive_adopt` | the device's owner: a device key, never pushable, advertised as `hive-adopt` | off | nothing reaches the server; an offer from a connection without `hive-adopt` is dropped unanswered |
| the org is served | the server (`hive.tenants`) | — | `hive_not_enabled` |
| the account names ONE person, a member | the device's `hive_accounts`, resolved by the server | — | `no_account` · `ambiguous_account` · `not_a_member` |
| capacity and rate | the server: 16 live adopted sessions a device, 20 offers a minute | — | `at_capacity` · `rate_limited` |

```mermaid
sequenceDiagram
    participant T as Claude Code (a terminal, as the account)
    participant K as roomler hive hook (as the account)
    participant D as roomlerd (root)
    participant S as server (hive)
    T->>K: SessionStart {session_id, transcript_path, cwd}
    K->>D: the adopt socket: start {harness session, cwd}
    Note over D: hive_adopt on? the peer's uid from the kernel,<br/>its account, the hive_accounts keys for it
    D->>S: rc:hive.adopt {adopt_id, harness_session, keys, account, folder}
    S-->>D: rc:hive.adopt_ack {session_id, fence}, or {refused}
    T->>K: Stop, at every turn
    K->>D: the transcript's new lines, read as the account
    D->>S: rc:hive.turn, metadata only
    T->>K: SessionEnd
    K->>D: the rest, and the end
    D->>S: rc:hive.state ended
```

| Rule | Why | Where |
|---|---|---|
| The device sends EVERY `hive_accounts` key that maps to the account, and the server adopts only when they name exactly one person | the attribution is the device owner's statement, never a guess: an account two people share is `ambiguous_account`, never "the one of them who is a member" | [`adopt.rs:288`](../crates/modules/hive/src/adopt.rs#L288) `people` |
| The hook reads the transcript and sends its lines; the daemon never opens a path a hook names | a root daemon that opened a path a hook named would read any file the requester pointed it at | [`supervisor/adopt.rs:651`](../agents/roomlerd/src/hive/supervisor/adopt.rs#L651) |
| Who it is comes from the kernel, the socket's peer credentials, never from the hook; root's own sessions are refused | the hook runs as whoever ran Claude Code, and says nothing that is not checked | [`supervisor/adopt.rs:413`](../agents/roomlerd/src/hive/supervisor/adopt.rs#L413) |
| No drivers, the owner included: naming one is a 409. Readers by name, as for any session. The record has no title, only the folder's name | the terminal holds the harness, and a terminal's first prompt is content | [`access.rs:52`](../crates/modules/hive/src/access.rs#L52) |
| Only the person's conversation is mirrored: `user` and `assistant` lines, never a sub-agent's (`isSidechain`) or Claude Code's bookkeeping | the on-disk transcript is Claude Code's internal format, and a change in it must thin the mirror, never stop it | |
| A terminal killed outright never says `SessionEnd`, so the daemon records Claude Code's process (pid and start time) and ends a session whose process is gone, every minute. That process is the hook's nearest ancestor that is not a shell | Claude Code runs a hook through `/bin/sh -c`, and dash forks it, so the hook's parent is a dying `sh` (P1j-5's field run) | [`procs.rs:44`](../agents/roomlerd/src/hive/procs.rs#L44) `hook_terminal` |
| "Stop mirroring" ends the record and never the terminal; a `claude --resume` later is offered afresh, as a new record | the terminal is the person's: Roomler can only stop watching it | [`supervisor/adopt.rs:818`](../agents/roomlerd/src/hive/supervisor/adopt.rs#L818) |
| `adopted.json`, beside `hosted.json`, keeps what is mirrored and the records of the last 1,000 adopted sessions that ended, which the device still serves to their owner with `hive_enabled` off | a restarted daemon goes on mirroring where it stopped, and an owner can read a session that ended | |
| `hive-adopt` is advertised, and the socket exists, only while `hive_adopt` is on; a daemon started with it off removes a socket its predecessor left | the leftover file read as "this device adopts" (P1j-5) | [`supervisor/adopt.rs:383`](../agents/roomlerd/src/hive/supervisor/adopt.rs#L383) |
| `unadopt` takes out exactly what `adopt` put in, and gives back a file Claude Code wrote byte for byte | the person's settings, and other tools' hooks in them, are theirs | [`hive_hooks.rs`](../agents/roomler-cli/src/hive_hooks.rs) |

---

## 9. The org gate on the server

The module switch (`[modules] hive`) is all or nothing for a server, so it has a
second dial: `hive.tenants` (`ROOMLER__HIVE__TENANTS`), the organizations agent
sessions serve, as comma-separated ids ([`scope.rs`](../crates/modules/hive/src/scope.rs)).
Empty or `*` means every organization, which is what a self-hosted server wants.

⚠️ **The list is the switch.** A non-empty `hive.tenants` mounts the module by itself,
with no `[modules] hive`, and `/api/capabilities` says so
([`settings.rs:911`](../crates/config/src/settings.rs#L911) `Settings::hive_on`). A
hosted server sets only the list, because that form fails safe. An image from before
P1g never reads the list, so with `ROOMLER__MODULES__HIVE=true` beside it, promoting
such an image (a rollback, or another session's older tag) would open the pillar to
every organization on the server. With the list alone, every older image leaves it
off. Clearing the list is the kill switch.

```mermaid
flowchart LR
    R["a request for org X"] --> M{"a member of X?"}
    M -- no --> N["403 not_a_member<br/>(nothing about the gate)"]
    M -- yes --> G{"X in hive.tenants?<br/>(empty means every org)"}
    G -- no --> F["404: agent sessions are not<br/>available to this organization"]
    G -- yes --> H["the route"]
    D["a device of X connects"] --> G2{"X served?"}
    G2 -- yes --> REC["reconcile: re-send<br/>what it missed"]
    G2 -- no --> END["its sessions end (hive_not_enabled),<br/>and it is told to stop them"]
```

| Where | An organization agent sessions do not serve |
|---|---|
| every route | a `404`, after the membership check, so a non-member still gets `not_a_member` ([`routes.rs:237`](../crates/modules/hive/src/routes.rs#L237) `member_tenant`) |
| `GET /api/tenant/{tid}/hive` | the SPA's question: `{"enabled": true}`, or that 404 |
| the viewer | `hive:view.open` answers `not_found`, as for a session the caller may not read |
| a device connecting | nothing is re-sent; what it still runs for the org ends (`hive_not_enabled`) and it is told to stop ([`agent_socket.rs:895`](../crates/modules/hive/src/agent_socket.rs#L895)) |
| an adopt offer | `hive_not_enabled` |
| a malformed entry | logged, and it matches nothing: a typo shuts the org it meant, and never opens another |

⚠️ **The SPA fails CLOSED for this module alone.** Every other module's capability
gate fails open until `/api/capabilities` answers. `hive` is in `DEFAULT_OFF`
([`registry.ts:39`](../ui/src/modules/registry.ts#L39)), and the router also asks
whether the pillar serves the organization (`checkServed`) before it opens a Hive
page ([`router.ts:394`](../ui/src/plugins/router.ts#L394)).

Prod has served agent sessions to one test organization since 2026-10-09; every
other organization reads the 404.

---

## 10. Platform notes

### Windows: the directories' DACLs

The store, `hosted.json` and each session's runtime files live in the service's
machine-wide directory, `%ProgramData%\roomler\roomler\hive`
([`hive_win.rs:829`](../agents/roomlerd/src/hive_win.rs#L829)), which both kinds of
worker reach. Every directory there is owned by Administrators with a protected DACL,
set by [`dir_with_dacl`](../agents/roomlerd/src/hive_win.rs#L897) as it is made, or
again on one already there.

| Directory | SDDL | Means |
|---|---|---|
| `hive` (the store) and `hive\run` (the runtime root) | `O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x100080;;;AU)` | SYSTEM and Administrators, in full; Authenticated Users may examine the directory itself, and nothing more ([`hive_win.rs:804`](../agents/roomlerd/src/hive_win.rs#L804)) |
| `hive\run\<session>` | the same owner and first two grants, then `(A;OICI;0x1200a9;;;<account SID>)` | the session's account may also list, traverse and read it, for its settings, its MCP config and its core memory, and writes nothing ([`hive_win.rs:809`](../agents/roomlerd/src/hive_win.rs#L809)) |

- **The owner is set too.** An owner keeps `WRITE_DAC` whatever the DACL says, so a
  folder someone else made there first is taken over, never trusted. A link or a
  junction on the way, or anything but a plain directory in the place, is refused.
- ⚠️ **`FILE_READ_ATTRIBUTES | SYNCHRONIZE` (`0x100080`) for Authenticated Users,
  inherited by nothing.** Both directories lie on the way to every session's
  `…\hive\run\<session>\settings.json`. Claude Code examines each component of a path
  it is handed and refuses the file when one cannot be examined: "Refusing to read a
  file whose path could not be vetted". Field, 2026-10-09 (P1i-4): every session
  ended 1.1 s after its start with exactly that, because the two directories granted
  SYSTEM and Administrators alone, and a session's restricted token has its
  Administrators group deny-only. `SYNCHRONIZE` is needed beside
  `FILE_READ_ATTRIBUTES` because `CreateFileW` asks for it with every synchronous
  open, libuv's `lstat` and Rust's among them: `0x80` alone was still refused. The
  grant is a stat of the directory alone. It lists nothing, so the session ids stay
  unknown, and another session's directory stays closed, its own DACL granting only
  its account. The unit test runs as a restricted Medium copy of its own token, and
  fails on the old DACL
  ([`supervisor.rs:9698`](../agents/roomlerd/src/hive/supervisor.rs#L9698)).

### macOS: the `/var` link

The store's directory is locked to the daemon with no link on its way that someone
other than root could have made
([`untrusted_link`](../agents/roomlerd/src/hive/supervisor.rs#L1044)). On a Mac the
root daemon's data directory is under `/var/root`, and `/var` is the system's own
link to `/private/var`. While every link was refused, as FR-85's recording-folder
check refuses them, a Mac opened no store and refused every session "/var is a
symbolic link" (P1h-3, 2026-10-10). So a link is followed when root owns it, in a
directory that root owns and that nobody else may write to, since only root could
have made it. Any other link is refused, as before.

- ⚠️ The published 0.4.122 and 0.4.123 `.pkg`s predate that fix (#1927), so on a Mac
  they refuse every session `launch_failed`. A Mac needs a release that carries it.
- The runtime directory is `/var/run/roomler-hive`, which macOS clears at boot.
- launchd does not kill a harness's process group when the daemon leaves, so
  `wind_down` does it: SIGTERM, then SIGKILL after 3 s.

### Unix: the NGROUPS cap

The privilege path sets the account's supplementary groups from `getgrouplist`, and
`setgroups` refuses the whole list when it is longer than the system's
`NGROUPS_MAX`. That is 16 on macOS, where a test VM's `admin` account was in 17, and
every session on that Mac failed "starting the harness: Invalid argument (os error
22)". The list is now cut to `sysconf(_SC_NGROUPS_MAX)`, in order, the primary group
first, so the child holds fewer groups and never more
([`exec.rs:1010`](../agents/roomlerd/src/exec.rs#L1010) `within_ngroups_max`). Exec, SSH
and the PTY share the fix, since they share the path.

The cap is for `setgroups` alone. `sudo` never reads that list on a Mac, and a Linux
session cannot run `sudo` at all, so nothing is cut below the maximum. What a session
holds of the administrator groups is in §3.

---

## 11. What is field-verified

| Criterion | Linux | macOS | Windows |
|---|---|---|---|
| AC1: a prompt from the session's room comes back as a turn stub, its steps rendered over the viewer peer | ✅ | — | — |
| AC3: `whoami` prints the mapped account (on Windows the console user), never root or SYSTEM; `no_account`; `no_console_user` | ✅ | ✅ | ✅ at Medium integrity |
| AC4, first half: a primary cut off from the server stops calling the model after `offline_grace` | ✅ refused at 125 s, forwarded again after the reconnect | — | — |
| AC5: an approval answered from a phone; the stub holds no tool arguments | ✅ | — | — |
| AC6: a reader's message never reaches the harness | ✅ | — | — |
| AC7: an update waits for a running turn, and logs it | ✅ a real install, 85 s (and a dry run, 80 s) | ✅ a real install by the helper, 135 s | ✅ a real install, 140 s |
| AC8: a fact reaches the next session and not the running one ([`brain.md`](brain.md)) | ✅ | — | — |
| AC20: a session survives a service restart, a restart at an approval and a crash; a changed gate ends it | ✅ | ✅ | ✅ |
| AC20: a session survives an update's restart | ✅ 0.4.123 → 0.4.124, the codeword kept | not yet | not yet |
| AC21: an adopted session is its owner's alone, and read only | ✅ | — | not built |
| Decision 13: a session holds no administrator group and cannot `sudo` | ✅ `27(sudo)` gone, `sudo` refused | ⚠️ the group gone, `sudo` still allowed (§3) | — |
| Decision 15: a Mac refuses a start as an account whose `sudo` needs no password | — | ✅ refused under a rule by user and by group; started with the owner's key, and on a Mac whose `sudo` asks | — |

Linux ran on a throwaway stack (a local server, loopback TURN, a root daemon in WSL),
macOS and Windows on throwaway VMs enrolled in the test organization on prod
(decision 10). Still open: AC2 holds on one device in CI and stays unticked until
replicas exist; AC4's stale-fence half needs P2's promotion; and an update's own
restart is not field-run as a resume on any platform, because the Linux dry run
installs nothing and the only newer release could not resume a session on Windows or
macOS. Every run, its build and what it measured are in the spec's
[§8](fr/FR-90-hive-agent-sessions.md#8-field-verification-log).

In CI, [`hive_canary.rs`](../crates/tests/tests/hive_canary.rs) (AC2, on two devices
since P2d-6), [`hive_drivers.rs`](../crates/tests/tests/hive_drivers.rs) (AC6),
[`hive_memory.rs`](../crates/tests/tests/hive_memory.rs) and
[`hive_memory_off.rs`](../crates/tests/tests/hive_memory_off.rs) (AC8) drive a real
server and a real in-process device, each in a test binary of its own, because a
process holds one supervisor. The canary's second device, a member, is a process of
its own for the same reason. The device's own tests run on Linux, macOS and Windows
CI (`hive::`).

---

## 12. P2a: the member's store, and what P2 adds

Until P2 a session lives on the one device that runs it, and it is readable only
while that device is online. P2 gives each session a replicaset: the org's own
devices, each holding a full copy it could resume, with the server learning how fresh
each copy is from sequence numbers and hashes alone (spec §3b). P2a built the store's
half of a member. Nothing sends it anything yet: no frame, no server change, and no
caller outside tests until the join (P2c), the stream (P2d), the purge (P2g) and an
archive replica's search (P2i).

| Piece | What it does | Where |
|---|---|---|
| apply | keeps an envelope another member sent, byte for byte, through the same chain check, so the member ends on the primary's `(seq, hash)` | [`store.rs:436`](../crates/hive-node/src/store.rs#L436) `Store::apply` |
| the fence floor | per session, raised by the server's word before any event of a new fence exists, and only ever raised; an older fence is refused, the device's own appends included | [`store.rs:545`](../crates/hive-node/src/store.rs#L545), [`chain.rs:195`](../crates/hive-node/src/chain.rs#L195) |
| the divergent tail | an older fence's tail, set aside out of `events` and the index; never a tail that holds the floor's fence | [`store.rs:574`](../crates/hive-node/src/store.rs#L574) |
| blobs | content-addressed by BLAKE3, at most 64 MiB each, kept per session; bytes a peer sent under another name are kept under neither | [`store.rs:659`](../crates/hive-node/src/store.rs#L659) |
| purged ids | a purge removes the session's events, floor, tail and the blobs no other session holds, and keeps its id, so nothing takes the session back | [`store.rs:1010`](../crates/hive-node/src/store.rs#L1010) |
| the writer | each of these on the daemon's one writer thread, answered with the store's own error, so a caller can tell `purged` from a stale fence | [`hive/store.rs:90`](../agents/roomlerd/src/hive/store.rs#L90) `Member` |

⚠️ **The four tables are new, and `user_version` stays 1**
([`store.rs:49`](../crates/hive-node/src/store.rs#L49)). A daemon refuses a store a
newer schema wrote, and the updater's crash-loop rollback can put an older daemon on
a device at any time, so a new column on `events` would cost it every transcript.
`a_daemon_from_before_p2a_reads_what_p2a_wrote` runs P0a's own statements, frozen, on
a store P2a wrote.

⚠️ "Committed" is SQLite's WAL with `synchronous = NORMAL`: an acknowledged event
survives a daemon crash, not a power cut.

### P2b-1: what a checkpoint takes of the config directory

A member can resume a session only with Claude Code's own files: the history that
`--resume` reads byte for byte, the sub-agents' transcripts, the outputs too large
for the context, the auto-memory and `CLAUDE.md`. At each turn's end the primary
will take them as a **checkpoint** (P2b-3 runs it as the session's account), record
it in the chain as a `checkpoint` event, and stream it with the blobs it names. P2b-1
builds what decides what is taken, and how a member puts it back together
([`checkpoint.rs`](../crates/hive-node/src/checkpoint.rs)). Nothing takes one yet.

```mermaid
flowchart LR
    T["a turn ends"] --> C["take() as the account:<br/>the five allowlisted paths,<br/>never the config root"]
    C -->|"a checkpoint event:<br/>every file's len · hash · new chunks"| S[("the primary's store")]
    C -->|"blobs, BLAKE3-named"| S
    S -->|"events, then blobs (P2d)"| M[("a member's store")]
    M --> A["assemble(): each chunk's hash,<br/>its place, and the whole's hash"]
```

| Path, under the config directory | How Claude Code writes it | How a checkpoint takes it |
|---|---|---|
| `projects/hive-<id>/<id>.jsonl`, the history | appended to only | the bytes past the last checkpoint, up to the last whole line ([`checkpoint.rs:458`](../crates/hive-node/src/checkpoint.rs#L458)) |
| `projects/hive-<id>/<id>/subagents/`: `agent-<id>.jsonl` | appended to only | the same |
| the same directory: `agent-<id>.meta.json` | replaced by a rename | whole, when it changed ([`checkpoint.rs:520`](../crates/hive-node/src/checkpoint.rs#L520)) |
| `projects/hive-<id>/<id>/tool-results/` | each output written once | whole |
| `projects/hive-<id>/memory/` | the auto-memory | whole |
| `CLAUDE.md` | the session's own instructions (P1e) | whole |

The chain is the state. A file's `len` and `hash` in the last checkpoint are where
the next starts ([`checkpoint.rs:335`](../crates/hive-node/src/checkpoint.rs#L335)),
so nothing else is kept between turns. An appended file that shrank, or changed before
its last offset, is taken again from its first byte. A chunk at offset 0 tells a
member to start the file over, and an empty one is sent when no whole line has come
yet, so the old bytes never linger. Chunks are at most 4 MiB, cut at line ends where
one is within reach. One checkpoint lists at most 10,000 files and adds at most
512 MiB.

⚠️ **An allowlist, never the directory** ([`checkpoint.rs:394`](../crates/hive-node/src/checkpoint.rs#L394)).
The config directory's root holds a `peerToken` beside the harness's messaging socket
and a `machineID`, so nothing outside the five paths is opened. The harness id must be
a lowercase, hyphenated UUID, because it is part of every path.

⚠️ **A link is never followed** ([`checkpoint.rs:662`](../crates/hive-node/src/checkpoint.rs#L662)).
A link in the allowlist to the account's `~/.ssh` would copy a key to every member. A
link, or a name that is not UTF-8, is listed as skipped and taken nowhere.

A member's [`assemble`](../crates/hive-node/src/checkpoint.rs#L790) refuses what does
not add up: a missing blob, a blob that does not hash to its name, a chunk out of
place, or a whole that does not hash to the last checkpoint's. The event
([`event.rs:118`](../crates/hive-node/src/event.rs#L118)) is a flat object beside its
`kind`, and nothing in it goes into the full-text index.

### P2b-2: the workspace, through the account's own git

The folder a session works in is checkpointed as a git commit, by the account's own
`git` and never a git built into the daemon (decision 14). It uses plumbing only,
through a temporary index, so the person's index, branches and work tree stay as they
were ([`workspace.rs`](../agents/roomlerd/src/hive/workspace.rs)). P2b-3 will run it
inside `hive-checkpoint`, as the account; for now only its tests do.

```mermaid
flowchart TD
    F{"the folder"} -->|"a repository's top"| O["its own repository:<br/>the whole tree"]
    F -->|"inside a repository"| P["its own repository:<br/>the subtree at --show-prefix,<br/>its ignores honoured"]
    F -->|"in none"| S["a shadow: &lt;state dir&gt;/shadow.git,<br/>nothing written into the folder"]
    O --> I["GIT_INDEX_FILE=$gd/hive-&lt;sid&gt;.index<br/>(seeded from $gd/index) git add -A -- ."]
    P --> I
    S --> I
    I --> W["write-tree [--prefix] → tree"]
    W -->|"the last checkpoint's tree"| N["no commit, no pack"]
    W -->|"changed"| C["commit-tree --no-gpg-sign [-p last]<br/>update-ref refs/hive/&lt;sid&gt;/head"]
    C --> K["pack-objects --revs --thin:<br/>&lt;commit&gt; ^&lt;last&gt;, chunked as blobs"]
```

| Step | What it guards | Where |
|---|---|---|
| the repository | its own, or the one the folder is in; a shadow for a folder in none, and a refusal inside a git directory | [`workspace.rs:391`](../agents/roomlerd/src/hive/workspace.rs#L391) |
| the index | `$gd/hive-<sid>.index`, a copy of the person's: theirs is read, never written | [`workspace.rs:297`](../agents/roomlerd/src/hive/workspace.rs#L297) |
| the tree | `write-tree --prefix` for a folder inside a repository, so nothing outside the folder is taken and the repository's `.gitignore` still keeps a `.env` out | [`workspace.rs:444`](../agents/roomlerd/src/hive/workspace.rs#L444) |
| the commit | "Roomler Hive" as author, never the person; `--no-gpg-sign`, so a person's `commit.gpgSign` never waits on their key; the last checkpoint's commit as parent | [`workspace.rs:247`](../agents/roomlerd/src/hive/workspace.rs#L247) |
| the pack | thin against the last checkpoint, whole for the first; at most 512 MiB | [`workspace.rs:468`](../agents/roomlerd/src/hive/workspace.rs#L468) |
| every command | no hook (`core.hooksPath` names nowhere), no fsmonitor, no automatic gc, none of the caller's `GIT_*` environment | [`workspace.rs:872`](../agents/roomlerd/src/hive/workspace.rs#L872) |

⚠️ **The first checkpoint's commit has no parent.** Its pack holds the whole tree and
none of the person's history, because a member has nothing else to resolve a delta
against. Each later commit is the last checkpoint's child, and its pack is thin
against it, so a target applies the packs in order (`git index-pack --fix-thin`). The
person's `HEAD` is recorded beside the commit
([`WorkspaceSnap`](../crates/hive-node/src/checkpoint.rs#L207)), for a teleport into a
clone of the same repository (P2f).

⚠️ A turn that changed no file writes no commit and no pack. A workspace that cannot be
taken (no `git`, a pack over the limit, a folder inside a git directory) never fails the
checkpoint: the config directory is what a resume needs, so the workspace is skipped,
in words. The temporary index and `refs/hive/<sid>/*` stay in the person's repository
until the session ends or is purged (P2g).

### P2b-3a: `hive-checkpoint`, as the account

The daemon never opens a path in the account's tree, so a checkpoint is taken by a
child it starts as the account, `roomlerd hive-checkpoint <request>`, as the Unix
wrapper and Windows' `hive-prep` already run
([`checkpointer.rs`](../agents/roomlerd/src/hive/checkpointer.rs)). P2b-3a builds both
sides. P2b-3b calls it at a turn's end, for a session the server joined as replicated
(P2c).

```mermaid
sequenceDiagram
    participant D as the daemon (root / SYSTEM)
    participant C as hive-checkpoint (as the account)
    participant S as the store
    D->>D: the request into the session's runtime directory:<br/>dirs, n, turn, the last checkpoint
    D->>C: start it as the account (Unix: the session's privilege path,<br/>Windows: the console user, in a job)
    C->>C: take() the allowlist, and the workspace through git
    C-->>D: stdout: magic · header · blobs
    D->>D: read_frames (limits), then check() against the request
    D->>S: P2b-3b: the blobs, then the checkpoint event
```

| On stdout | What the daemon holds it to |
|---|---|
| `HIVECP1\n` | the magic, or nothing is read |
| u32 · JSON `{"checkpoint": …}` | at most 16 MiB; the request's `n` and `turn`; every path inside the allowlist, in order, taken the way the allowlist says, at most mode `0o777` ([`checkpointer.rs:201`](../agents/roomlerd/src/hive/checkpointer.rs#L201)) |
| u32 · then u64 · bytes per blob | each the very blob its chunk names, by length and hash, in order (each file's, then the pack's); not one more or fewer; each at most 4 MiB ([`checkpointer.rs:159`](../agents/roomlerd/src/hive/checkpointer.rs#L159)) |
| the workspace | git's ids only (40 or 64 hex digits), and a prefix that cannot step outside the repository |

⚠️ **What comes back is the account's word, and the agent runs as that account.** The
child is the daemon's own binary, but the account can reach it while it runs. So the
daemon takes nothing on trust. A path like `../.ssh/id_ed25519` or `sessions/1.key`,
a blob swapped for another, an extra or a missing one, or a `../` prefix is refused
before the store sees any of it. A member still checks each whole file again when it
assembles it.

The child gets 120 s ([`checkpointer.rs:304`](../agents/roomlerd/src/hive/checkpointer.rs#L304))
and may write at most the limits' sum. On Unix it is started through the session's
privilege path ([`checkpointer.rs:562`](../agents/roomlerd/src/hive/checkpointer.rs#L562)).
On Windows it runs as the user signed in at the console, whom `hive_accounts` must name
([`hive_win.rs:1056`](../agents/roomlerd/src/hive_win.rs#L1056)), in a job of its own.

⚠️ **An ended checkpoint takes its `git` with it.** On Unix the child leads its own
process group, and a timeout, too much output or an abandoned checkpoint signals the
whole group ([`child.rs:28`](../agents/roomlerd/src/hive/child.rs#L28)
`Group`). Until P2b-3b it signalled the leader alone (`start_kill`, `kill_on_drop`),
which left a `git add` hashing the folder behind it. TERM goes first, then KILL after
2 s, because git removes its lock files on TERM and never on KILL. The group is
signalled only while its leader is unreaped: after that its id may name someone else's
group.

### P2b-3b: a checkpoint at each turn's end

A replicated session checkpoints each turn's end
([`checkpointer.rs:400`](../agents/roomlerd/src/hive/checkpointer.rs#L400)). The checkpoint
comes after the turn's `result` and before the next queued prompt begins, so it holds
the files as the turn left them; nothing writes them while the harness waits for input.
The blobs go into the store first, then the `checkpoint` event, so a member that sees the
event can fetch every blob it names.

```mermaid
flowchart LR
    R["the turn's result"] --> Q{"replicated?"}
    Q -- no --> N["the next prompt,<br/>or idle"]
    Q -- yes --> G{"the daemon<br/>stopping?"}
    G -- yes --> N
    G -- no --> C["checkpoint_or_stop"]
    C -- "taken: the blobs,<br/>then the event" --> N
    C -- "failed: said once per reason,<br/>unless the daemon went down" --> N
    C -- "a stop: abandoned,<br/>its group TERMed" --> E["the session ends"]
    P["a prompt meanwhile"] -.-> W["waits in the queue"] -.-> N
```

| Piece | What it does |
|---|---|
| the switch | `StartOrder.replicated`: a session the server joined as replicated (P2c, which sets it from the wire). Kept in `hosted.json`, so a resume after a restart goes on checkpointing; absent from an older file, it reads off |
| where it runs | in the session's run loop, not inside the event that ended the turn ([`supervisor.rs:5001`](../agents/roomlerd/src/hive/supervisor.rs#L5001) `checkpoint_or_stop`), so the session still listens: a prompt sent meanwhile waits behind it, and a stop is a stop |
| a stop during one | abandons it: its child's group is TERMed and no checkpoint is recorded. The next one takes everything since the last one kept |
| where the next one starts | the last checkpoint, in memory; after a restart, read back from the store ([`store.rs:765`](../crates/hive-node/src/store.rs#L765) `last_of_kind`). A checkpoint an older daemon stored has no `kind` column, so the next one takes everything again: more bytes, never a wrong checkpoint |
| the request | `checkpoint.json` in the session's runtime directory, the daemon's own: made `0600` and handed to the account through its descriptor, never by a path ([`checkpointer.rs:529`](../agents/roomlerd/src/hive/checkpointer.rs#L529)); on Windows it takes the session directory's DACL |
| a failure | logged with the session and the number, and said once per reason in the transcript ("Checkpoint 3 was not taken: …"); the session goes on |
| the daemon going down | starts none, and says no failure: the child went down with it (systemd signals the whole cgroup at once), and the next daemon's next checkpoint takes what this one would have (P1d-2) |
| a lock left behind | a lock on the checkpoint's own index older than 300 s is cleared ([`workspace.rs:277`](../agents/roomlerd/src/hive/workspace.rs#L277)). A KILL or a power cut leaves one, and it would refuse every checkpoint after it for good; no checkpoint holds one longer than its 120 s |
| the room | a `checkpoint` event is one line: "💾 Checkpoint 2: 1 of 3 of the agent's files changed; the folder committed", or the folder unchanged, or not kept and why |

⚠️ Nothing sets `replicated` before P2c, so no device takes a checkpoint yet. Three
tests turn it on: `a_replicated_session_checkpoints_each_turns_end_and_no_other_does`
(the chain: the history from where the last checkpoint left it, the folder's commit on
the last one's, and the other side), `a_failed_checkpoint_is_said_once_and_the_session_goes_on`,
and `a_stop_is_heard_while_a_checkpoint_runs_and_a_prompt_waits_behind_it`.

⚠️ **Open before P2c turns it on: a folder too big to add.** A folder whose first `git
add` cannot finish within 120 s fails at every turn's end. The add that is ended writes
no index, so the next one starts over, and each such turn waits the full 120 s before
its next prompt. P2c must refuse such a folder up front, or bound the add.

⚠️ **The allowlist is the harness's.** What P2b-1 takes is Claude Code's own layout
(`projects/hive-<id>/…`, `CLAUDE.md`). Codex keeps its rollouts elsewhere
(`~/.codex/sessions`), so the Codex adapter (design §4, P7) brings its own allowlist;
until then only a Claude Code session is replicated.

### P2b-4a: `hive-materialize`, the config directory as the account

The reverse of `hive-checkpoint`. A promotion (P2e) or a teleport (P2f) puts the
session's last checkpoint back on the device that will run it, and the daemon never
writes a path in the account's tree. So it starts `roomlerd hive-materialize` as the
target's account and streams it the files
([`materializer.rs`](../agents/roomlerd/src/hive/materializer.rs)). P2b-4a builds both
sides for the config directory; P2b-4b adds the folder (below). Nothing calls it before P2e.

```mermaid
sequenceDiagram
    participant D as the daemon (root / SYSTEM)
    participant S as the store
    participant M as hive-materialize (as the account)
    D->>D: plan(): the last checkpoint's files, and for each the<br/>chunks since it last started over (chunks_of)
    D->>M: start it as the account (Unix: the session's privilege path,<br/>Windows: the console user, in a job)
    D->>M: stdin: magic · header (every file's path, len, hash)
    M->>M: refusal() and check_places(): every name and every place,<br/>before anything is written
    loop each file, blob by blob
        S-->>D: get_blob, held to its chunk
        D->>M: the bytes
        M->>M: staged beside where it goes, 0600, hashed as it arrives
    end
    M->>M: every file held to its entry, then all renamed into place,<br/>then what the checkpoint no longer lists removed
    M-->>D: stdout: {"files": N, "removed": M}
```

| Step | What it guards | Where |
|---|---|---|
| the plan | every path in the allowlist; each file's chunks from its last start-over, adding up to its entry. A history is streamed, never held whole | [`materializer.rs:186`](../agents/roomlerd/src/hive/materializer.rs#L186) · [`checkpoint.rs:740`](../crates/hive-node/src/checkpoint.rs#L740) `chunks_of` |
| the stream | each blob held to its chunk before it goes; a blob the store lacks ends the stream short, the child writes nothing, and the daemon's own word is the answer | [`materializer.rs:348`](../agents/roomlerd/src/hive/materializer.rs#L348) |
| the names | the allowlist again, in order. On Windows: no device name (`aux.md` is AUX, `COM1.log` is COM1), none of `<>:"\|?*` or a control character, no trailing dot or space, at most 259 characters where it goes. On Windows and macOS: no two names that differ only in case | [`materializer.rs:712`](../agents/roomlerd/src/hive/materializer.rs#L712) |
| the places | every directory on the way is one of its own, never a link to one, and no file goes where a directory is | [`materializer.rs:846`](../agents/roomlerd/src/hive/materializer.rs#L846) |
| the files | staged beside where they go, `0600` whatever mode the checkpoint's OS kept, and each held to its entry's length and hash; only then are they renamed into place | [`materializer.rs:592`](../agents/roomlerd/src/hive/materializer.rs#L592) |
| what goes | inside the allowlist, a file the checkpoint does not list is removed, never through a link and no deeper than a checkpoint looks; nothing outside it is touched (a login, the harness's own state) | [`materializer.rs:982`](../agents/roomlerd/src/hive/materializer.rs#L982) |

⚠️ **What comes in is another device's word.** A checkpoint was taken by whichever
member was primary, and this daemon only relays it. Unchecked, a path that climbs out
with `..` would be written as the target's account wherever it can write, its
`~/.bashrc` included. So the child holds every name to the allowlist again. A refusal at any step writes nothing, and
the files it staged so far are removed.

⚠️ **Windows' names are refused before anything moves.** A Linux session's memory can
hold `aux.md` or `a:b.md`, which Windows cannot. Two names that differ only in case
are one file on Windows and on a default macOS volume. P2f's prepare refuses such a
session up front through the same `refusal`.

⚠️ **A child that refuses is answered in its own words.** It stops reading, and the
feed then meets a closed pipe, which would otherwise be all that is said
([`child.rs:79`](../agents/roomlerd/src/hive/child.rs#L79)). On Unix the runner, its
process group and its time limit are the ones `hive-checkpoint` uses. On Windows a
thread of its own feeds the child's pipe
([`hive_win.rs:1086`](../agents/roomlerd/src/hive_win.rs#L1086)).

The files are written `0600` because a checkpoint taken on Windows records mode 0,
and restoring that on Linux would leave the account unable to read its own history.

### P2b-4b: `hive-materialize`, the folder as the account

The same child takes the session's folder back too, after the config directory's files
are staged ([`workspace.rs`](../agents/roomlerd/src/hive/workspace.rs)). The daemon
sends the packs that bring the last checkpoint's commit
([`materializer.rs:244`](../agents/roomlerd/src/hive/materializer.rs#L244)):
back from it along each `base` to a whole pack, past the turns that changed nothing,
and oldest first, so each thin pack finds its base in the repository already.

```mermaid
flowchart TD
    P["place_for(): the folder's own repository,<br/>or a shadow (a missing folder is made only at the switch)"] --> I["index-pack --stdin --fix-thin, pack by pack:<br/>git checks every object as it lands"]
    I --> T{"the commit's tree, as git hashed it,<br/>the checkpoint's?"}
    T -- no --> R["refused: nothing in the folder,<br/>no config file placed"]
    T -- yes --> N{"every name in the tree<br/>one this OS can hold?"}
    N -- no --> R
    N -- yes --> J{"judge(): the folder holds nothing,<br/>its HEAD's tree, the session's own last tree here,<br/>or already the checkpoint's?"}
    J -- no --> R
    J -- yes --> S["switch(): read-tree -m -u from its tree to the checkpoint's,<br/>through the session's temporary index"]
    S --> V{"the folder reads back as the tree?"}
    V -- no --> F["said, and the ref not moved"]
    V -- yes --> D["the session's ref = the commit;<br/>then the config files placed"]
```

| Step | What it guards | Where |
|---|---|---|
| the place | the folder's own repository, at its prefix, or a shadow in the session's state directory; a link where the folder goes is refused | [`workspace.rs:557`](../agents/roomlerd/src/hive/workspace.rs#L557) |
| the packs | each through `git index-pack --stdin --fix-thin`, which checks every object as it lands. Nothing else of the repository changes; the objects of a materialize refused later are referenced by nothing, and git's own gc takes them | [`workspace.rs:582`](../agents/roomlerd/src/hive/workspace.rs#L582) |
| the tree | the commit's tree, as git hashed it, must be the one the checkpoint names | [`materializer.rs:670`](../agents/roomlerd/src/hive/materializer.rs#L670) |
| the names | every path in the tree held to the same rules as the config directory's: Windows' names and lengths where it goes, and on Windows and macOS two names that are one there, files or the directories on their way (`Dir/a` beside `dir/b`) | [`materializer.rs:763`](../agents/roomlerd/src/hive/materializer.rs#L763) |
| the folder | only one it is safe to replace: holding nothing yet, exactly its own `HEAD`'s tree (a clean clone), exactly what this session last left in it (a former primary: moving back), or already the checkpoint's. Anything else is the person's own work, and is refused | [`workspace.rs:695`](../agents/roomlerd/src/hive/workspace.rs#L695) |
| the switch | git's own two-tree `read-tree -m -u`, through the session's temporary index: what the tree lacks is removed, what it changes is rewritten with the repository's line endings and filters, and an untracked file in the way is refused before anything is written | [`workspace.rs:744`](../agents/roomlerd/src/hive/workspace.rs#L744) |
| a folder inside a repository | the switch is between the whole repository's trees, the index's and the same with the folder's subtree swapped, so nothing outside the folder moves | [`workspace.rs:791`](../agents/roomlerd/src/hive/workspace.rs#L791) |
| the end | the folder read back as a checkpoint reads it must be the tree; only then is `refs/hive/<sid>/head` the commit, where the next checkpoint here starts | [`workspace.rs:744`](../agents/roomlerd/src/hive/workspace.rs#L744) |

⚠️ **The person's `HEAD`, branches and index are never touched**, as the checkpoint
side never touches them. In a clean clone the session's changes land in the work tree
as uncommitted changes, which is what they were where the checkpoint was taken.

⚠️ **A move keeps git's tree, not the bytes** (design §6.3). Line endings follow the
target's git configuration, so a Windows checkout with `core.autocrlf=true` lands with
CRLF. That is why the folder is held to the tree id, never byte for byte. A folder whose
git cannot give the tree back — a CRLF blob into a repository with
`core.autocrlf=input` — is said to, and its ref is not moved.

⚠️ **A switch that fails part way is not undone.** Every check runs before the folder is
touched, and git refuses an untracked file in the way before it writes. But a write that
fails part way (a full disk) leaves the folder between the two trees. The ref is not
moved, the config files are not placed, and the materialize says so.

### P2c-1: the device's gates for holding copies

Before any frame of the replicaset exists, the device's own refusals do (§3g). Three
keys, each the device owner's, default off, never pushable, and in the lock test's list
([`models.rs:5672`](../crates/remote_control/src/models.rs#L5672)):

| Key | What it is | Where |
|---|---|---|
| `hive_replica` | hold copies of the sessions the owner runs on their other devices. While it is on the device keeps a store, as an archive replica's container must although it runs nothing | [`config.rs:471`](../crates/agent-core/src/config.rs#L471) · [`supervisor.rs:1017`](../agents/roomlerd/src/hive/supervisor.rs#L1017) |
| `hive_archive` | offer the device as the org's archive replica; only ever with `hive_replica`, so alone it reads off | [`gates.rs:79`](../agents/roomlerd/src/hive/gates.rs#L79) |
| `hive_store_quota_mib` | the most the device keeps as a replica, in MiB; unset, the disk's | |

Two verbs join the wire's vocabulary, `hive-replica` and `hive-archive`
([`models.rs:663`](../crates/remote_control/src/models.rs#L663)). `hive` is the prefix of
both, and the three are different promises: running sessions is not holding copies of
other devices', and a replica is no archive. They are matched by equality and locked by
test, like `ssh` and `ssh-consent`.

⚠️ **A verb is advertised only with the frames it promises.** Advertising one says the
device understands its frames, and nothing answered a join before P2c-3b: a server that
saw `hive-replica` then would have waited for a `join_ack` that never came. P2c-3b
advertises both together with the join's handling, only while the keys are on.

⚠️ **A folder too big for one checkpoint is skipped at once**
([`workspace.rs:327`](../agents/roomlerd/src/hive/workspace.rs#L327)). Before `git add`
hashes anything, `git ls-files --others` walks what the index does not have yet,
without hashing and with ignores honoured, and stops at the first limit passed: 20,000
files or 512 MiB. The workspace is then skipped, in words that point at `.gitignore`.
Without it, a folder too big to add within the checkpoint's 120 s failed at every turn's
end, because an add that is ended writes no index and the next one starts over. That was
P2b-3b's open prerequisite for P2c.

### P2c-2a: the org's replica policy

Where an organization's sessions are copied is the organization's call (spec §3b,
"Membership and placement"): one document per organization in `hive_policies`, unique on
`tenant_id` ([`lib.rs:272`](../crates/modules/hive/src/lib.rs#L272)), written by an
`ADMINISTRATOR` and audited. Until one is written, every reader gets decision 2's
defaults ([`policy.rs:91`](../crates/modules/hive/src/policy.rs#L91)). The two routes,
`GET` and `PUT …/hive/policy` ([`lib.rs:207`](../crates/modules/hive/src/lib.rs#L207),
`docs/api.md`), answer **404** while the server's `hive.replicaset` is off
([`settings.rs:83`](../crates/config/src/settings.rs#L83),
[`policy.rs:247`](../crates/modules/hive/src/policy.rs#L247)) or `hive` does not serve the
organization. Placement (P2c-2b) is its reader.

```mermaid
sequenceDiagram
    participant A as an administrator
    participant S as the server
    participant D as hive_policies
    A->>S: GET …/hive/policy
    S->>D: the organization's policy
    D-->>S: none
    S-->>A: decision 2's defaults, revision 0
    A->>S: PUT …/hive/policy, revision 0
    S->>S: an ADMINISTRATOR, within bounds,<br/>each archive device live here and not ephemeral
    S->>D: insert, at revision 1
    D-->>S: stored
    S-->>A: revision 1
    Note over A,D: a second tab still holds revision 0
    A->>S: PUT …/hive/policy, revision 0
    S->>D: insert, at revision 1
    D-->>S: duplicate key on tenant_id
    S-->>A: 409, audited stale
```

| Field | What a write may set | Why |
|---|---|---|
| `replicaset.min` | 1 to 8; the primary counts | fewer copies than `min` is shown in the session's room, never accepted in silence |
| `replicaset.max` | `min` to 8 | |
| `replicaset.archive` | whether every archive replica the rules allow joins | |
| `replicaset.prefer` | `owner_devices`, once; any other word is refused | the one order decision 2 names. Refused rather than stored, so a word the server cannot place by fails at the write, not silently at every placement |
| `replicaset.restricted_tags` | trimmed, empties dropped, once each, at most 16 of 40 characters ([`agent.rs:931`](../crates/modules/fleet/src/agent.rs#L931)) | normalized as device tags are, so a tag compares equal to the one a device carries |
| `retention_days` | 0 to 3650; 0 keeps sessions for ever | P2g applies it |
| `archive_devices` | at most 16, each a live device of this organization and not ephemeral, each kept once ([`policy.rs:315`](../crates/modules/hive/src/policy.rs#L315)) | the administrator's half of an archive replica. The device's own `hive_archive` is the other half, read where placement runs, so either may come first |

⚠️ **A write names the revision it read** ([`policy.rs:169`](../crates/modules/hive/src/policy.rs#L169)).
A `PUT` replaces the whole policy, so two administrators editing at once would otherwise
each write over the other's change unseen. The first write is an insert, and the unique
index decides between two of them; every later one is an update filtered by
`{tenant_id, revision}` ([`policy.rs:193`](../crates/modules/hive/src/policy.rs#L193)).
Either way the loser gets **409** and reads again.

⚠️ **An ephemeral device is never an archive replica**
([`policy.rs:328`](../crates/modules/hive/src/policy.rs#L328)). FR-51 fixes `ephemeral`
at enrollment, and the reaper hard-deletes such a row once it goes quiet: it could keep
nothing for the retention, and never acknowledge a purge.

⚠️ **Only a device that is not there is a refusal.** A lookup that failed is the
database's answer: a **500**, never a `not_a_device` row in the audit blaming the caller.

Every attempt lands in `hive_audit` as `action: policy`
([`policy.rs:368`](../crates/modules/hive/src/policy.rs#L368)): `set`, or `refused` with
`no_permission`, `invalid`, `not_a_device`, `ephemeral` or `stale`.

A policy only ever says where copies **may** go. Whether a device holds one is still its
owner's `hive_replica` (P2c-1), which no policy and no server can turn on.

### P2c-2b: placement at a start

Placement chooses a session's members from the policy and the devices' stored rows. Its
rules are one pure function, `place`
([`placement.rs:253`](../crates/modules/hive/src/placement.rs#L253)), each rule a unit
test. With `hive.replicaset` on, the start route runs it before the record is written
([`routes.rs:425`](../crates/modules/hive/src/routes.rs#L425)) and keeps the result on the
record as `replicaset` ([`model.rs:327`](../crates/modules/hive/src/model.rs#L327)), which
the session view carries (`docs/api.md`). A placement is a plan: nothing reaches a member
before the join (P2c-3), and the member's own `hive_replica` decides whether it takes the
copy.

```mermaid
flowchart TD
    S["a session starts on its primary"] --> P{"hive.replicaset on?"}
    P -- no --> N["no replicaset:<br/>the primary alone, as in P1"]
    P -- yes --> R["the policy, or decision 2's defaults;<br/>live, permanent devices whose last hello offered hive-replica"]
    R --> T["restricted tags: those the session carries,<br/>and each policy tag its primary carries, in any case"]
    T --> A["archive replicas, in the policy's order:<br/>designated, offering hive-archive, archive on"]
    A --> O["then the owner's own devices, last seen first, until min:<br/>owner_user_id and enrolled_by both the owner"]
    O --> M["the members, never more than max;<br/>short, and why, when fewer than min"]
```

| Rule | Why | Where |
|---|---|---|
| A member offers: its last hello advertised `hive-replica`, and it is live, not ephemeral, and not the primary | the device owner's `hive_replica` is the gate that survives a compromised server; FR-51 hard-deletes an ephemeral row once it goes quiet | [`placement.rs:273`](../crates/modules/hive/src/placement.rs#L273), [`agent.rs:242`](../crates/services/src/dao/agent.rs#L242) |
| The owner's own devices are those whose `owner_user_id` **and** `enrolled_by` both name the session's owner | `owner_user_id` is reassignable with `MANAGE_AGENTS` alone: a device manager who hands someone a device must not receive that person's sessions on it | [`placement.rs:306`](../crates/modules/hive/src/placement.rs#L306) |
| Archive replicas come first, in the policy's order: designated, offering `hive-archive`, and only while `archive` is on | the administrator's half and the device owner's half | [`placement.rs:281`](../crates/modules/hive/src/placement.rs#L281) |
| The owner's devices fill to `min`, last seen first and ties by id; never more than `max` in all | decision 2, and the same answer whenever it is computed | [`placement.rs:319`](../crates/modules/hive/src/placement.rs#L319) |
| A restricted tag only ever takes a device out | a copy is everything the agent saw | [`placement.rs:274`](../crates/modules/hive/src/placement.rs#L274) |
| An adopted session is placed nowhere | decision 11; the adopt path records no replicaset at all | [`placement.rs:264`](../crates/modules/hive/src/placement.rs#L264) |

⚠️ **A tag is compared without case**
([`placement.rs:143`](../crates/modules/hive/src/placement.rs#L143)). A primary tagged
`Prod` is restricted by the policy's `prod`, and a device tagged `PROD` may hold its
copies. A comparison that assumed one casing would leave that session unrestricted: a
guard that assumes a casing is no guard.

⚠️ **Restricted tags stick.** The record keeps the tags the session carries, in the
policy's spelling. A later placement adds to them and never takes one away, so a session
that was restricted stays restricted, whatever its primary is tagged later.

⚠️ **Placement reads each device's stored row, not the pod's live connections.** A device
offline at the start is placed, and joins when it connects (P2c-3). A row can be stale:
it may still offer `hive-replica` after its owner turned the key off. That device refuses
the join itself.

⚠️ **Replication never blocks a start.** A policy or device read that fails is logged,
and the session starts on its primary alone, with no `replicaset`.

A session with fewer members than `min` says why in `short`: `restricted` when a device
that would hold a copy lacks a restricted tag, `no_device` when no other device offers.
The room's header shows it in P2j. Each placement lands in `hive_audit` as
`action: place`, `placed` or `short` with that word
([`routes.rs:484`](../crates/modules/hive/src/routes.rs#L484)).

P2c-2b places only at a start. Placement runs again when the policy or its archive
replicas change, when a member goes and when a device's tags or owner change (P2c-4a,
below), and a session from before P2 is placed when its primary connects (P2c-4b).

### P2c-3a: the join, and what a member says back

A member learns it holds a session from `rc:hive.replica.join`. The server learns what
each member holds from what the member says back: its answer, then the tip of its copy
(spec §3b, "What the server learns"). P2c-3a builds the frames and the server's half
([`replica.rs`](../crates/modules/hive/src/replica.rs)). The device's half is P2c-3b,
below: its gates on a join, the membership its store keeps, and its tips. A server
sends no join to a build that does not advertise `hive-replica`.

```mermaid
sequenceDiagram
    participant P as the primary
    participant S as the server
    participant M as a member
    S->>P: rc:hive.start, replicated
    P-->>S: rc:hive.start_ack
    S->>M: rc:hive.replica.join, session, fence, role
    Note over M: only on a connection advertising hive-replica,<br/>else it waits, pending, for one
    M-->>S: rc:hive.replica.join_ack, or refused and why
    M->>S: rc:hive.replica.tip, fence, seq, hash, checkpoint
    P->>S: rc:hive.replica.manifest, on every connection
```

| Frame | From | Says | Where |
|---|---|---|---|
| `rc:hive.start` gains `replicated` | the server | the session has members besides its primary: checkpoint each turn's end (P2b-3b). Absent, as from every earlier server, and a device that predates it ignores it | [`signaling.rs:3112`](../crates/remote_control/src/signaling.rs#L3112); the device takes it into its start order, [`signaling.rs:4253`](../agents/roomlerd/src/signaling.rs#L4253) |
| `rc:hive.replica.join` | the server | `{session_id, fence, role}`: hold a copy, as `archive` or `owner`; or, as `primary` (P2c-4b, below), checkpoint the session you run, which has members now. A role the device cannot name decodes as none, and it refuses | [`signaling.rs:3137`](../crates/remote_control/src/signaling.rs#L3137) |
| `rc:hive.replica.join_ack` | a member | joined, or `refused`: `replica_disabled`, `archive_disabled`, `secondary_org`, `purged`, `stale_fence` or `quota`. An unknown word is still a refusal | [`signaling.rs:831`](../crates/remote_control/src/signaling.rs#L831), [`hive.rs:592`](../crates/remote_control/src/hive.rs#L592) |
| `rc:hive.replica.tip` | a member, or the primary | `{session_id, fence, seq, hash, checkpoint?}`: where its copy ends, at most every 5 s and at each checkpoint | [`signaling.rs:858`](../crates/remote_control/src/signaling.rs#L858) |
| `rc:hive.replica.manifest` | each device, on every connection | every session it holds a copy of, its own included, each with its tip; at most 4,096 | [`signaling.rs:876`](../crates/remote_control/src/signaling.rs#L876), [`hive.rs:491`](../crates/remote_control/src/hive.rs#L491) |

| When | What the server does | Where |
|---|---|---|
| the primary accepts the start | sends each member that has not answered its join, in its role | [`agent_socket.rs:489`](../crates/modules/hive/src/agent_socket.rs#L489), [`replica.rs:189`](../crates/modules/hive/src/replica.rs#L189) |
| a device connects | sends it every join it has still to answer, if THIS connection advertises `hive-replica`. A member placed before P2c-3a has no state, and counts as pending | [`agent_socket.rs:373`](../crates/modules/hive/src/agent_socket.rs#L373), [`replica.rs:208`](../crates/modules/hive/src/replica.rs#L208), [`dao.rs:957`](../crates/modules/hive/src/dao.rs#L957) |
| a join answer arrives | the member is `joined`, or `refused` with the device's word | [`replica.rs:261`](../crates/modules/hive/src/replica.rs#L261), [`dao.rs:876`](../crates/modules/hive/src/dao.rs#L876) |
| a tip arrives, alone or in a manifest | kept on the member's entry: the fence, `seq`, hash and newest checkpoint, and when it was heard | [`replica.rs:303`](../crates/modules/hive/src/replica.rs#L303), [`dao.rs:931`](../crates/modules/hive/src/dao.rs#L931) |

Each member on the record now carries `state` (`pending`, `joined` or `refused`,
[`model.rs:374`](../crates/modules/hive/src/model.rs#L374)), the refusal, when it
answered, and its tip ([`model.rs:405`](../crates/modules/hive/src/model.rs#L405)), all
in the session view (`docs/api.md`). The primary is `joined` from its placement: it holds
the session by running it. It is `pending` only while it is owed its own join (P2c-4b).

⚠️ **A join goes only to a connection that advertises `hive-replica`**
([`hub.rs:1881`](../crates/modules/fleet/src/hub.rs#L1881)). Advertising says two things
at once: the device understands the join, and its owner lets it hold copies. Any other
connection receives nothing, and the join waits, pending, for one that does, as an
offline device's remote configuration waits.

⚠️ **What a member says is a claim, applied only where the record names that device.**
An answer from a device placement did not choose, or to another fence, matches nothing.
A tip from a member that refused is dropped: by its own word it holds nothing.

⚠️ **A hash that is not one is never stored**
([`replica.rs:306`](../crates/modules/hive/src/replica.rs#L306)): exactly 64 lowercase
hex characters ([`hive.rs:511`](../crates/remote_control/src/hive.rs#L511)), or the
report is dropped.

⚠️ **A start says `replicated` only when placement chose another member**
([`replica.rs:62`](../crates/modules/hive/src/replica.rs#L62)). With the switch off there
is no placement, so no start says it and nothing checkpoints; from P2c-4b a start sent
again says it only while the switch is on, too (below). The device keeps the flag
in its start order and its hosted record, so a restarted daemon still checkpoints the
session (P2b-3b).

### P2c-3b: the join, the device's half

A device answers a join through its own gates, keeps what it joined in its store, and
tells the server where each copy ends
([`supervisor/replica.rs`](../agents/roomlerd/src/hive/supervisor/replica.rs)).

```mermaid
flowchart TD
    J["rc:hive.replica.join, session, fence, role"] --> G1{"on the primary<br/>enrollment's connection?"}
    G1 -- no --> R1["secondary_org"]
    G1 -- yes --> G2{"the owner's hive_replica on?"}
    G2 -- no --> R2["replica_disabled"]
    G2 -- yes --> G3{"a role this build can name?<br/>an archive's hive_archive on?"}
    G3 -- no --> R3["other, or archive_disabled"]
    G3 -- yes --> G4{"the store open,<br/>and under hive_store_quota_mib?"}
    G4 -- no --> R4["other, or quota"]
    G4 -- yes --> T["one transaction: not purged, not below the floor,<br/>the floor raised to the fence, the membership kept"]
    T -- refused --> R5["purged, or stale_fence"]
    T -- kept --> A["rc:hive.replica.join_ack, joined"]
```

The gates run in that order ([`replica.rs:60`](../agents/roomlerd/src/hive/supervisor/replica.rs#L60)),
and the answer goes back on the connection the join came on
([`supervisor.rs:783`](../agents/roomlerd/src/hive/supervisor.rs#L783)). A join that
passes is one transaction in the store
([`store.rs:275`](../crates/hive-node/src/store.rs#L275) `Store::join`): the floor rises to
the join's fence, and a row in `memberships` keeps the role and fence
([`store.rs:114`](../crates/hive-node/src/store.rs#L114)). It is a new table, so
`user_version` stays 1 and a daemon the updater rolled back still opens the file. A purge
ends the membership with the rest ([`store.rs:1025`](../crates/hive-node/src/store.rs#L1025)).

| What the device says | When | Where |
|---|---|---|
| `rc:hive.replica.join_ack` | at once, joined or refused with its word | [`supervisor.rs:783`](../agents/roomlerd/src/hive/supervisor.rs#L783) |
| `rc:hive.replica.tip` | after each checkpoint of a replicated session it runs | [`supervisor.rs:5043`](../agents/roomlerd/src/hive/supervisor.rs#L5043), [`replica.rs:496`](../agents/roomlerd/src/hive/supervisor/replica.rs#L496) |
| `rc:hive.replica.manifest` | on every connection, after the P1b manifest: every membership and every replicated session it runs, newest first | [`supervisor.rs:2626`](../agents/roomlerd/src/hive/supervisor.rs#L2626), [`replica.rs:448`](../agents/roomlerd/src/hive/supervisor/replica.rs#L448) |

A tip is the chain's end, the event's fence, `seq` and hash, and the newest `checkpoint`
event's `seq` and hash. A member that holds nothing yet says `seq` 0 at its floor
([`replica.rs:201`](../agents/roomlerd/src/hive/supervisor/replica.rs#L201)): it joined,
and waits for the carrier.

The device advertises `hive-replica` while its owner's `hive_replica` is on and the store
opened, and `hive-archive` while `hive_archive` is on too
([`caps.rs:1644`](../agents/roomlerd/src/encode/caps.rs#L1644)). A secondary org's
connection strips both words, in its hello and in every heartbeat that re-announces the
caps ([`signaling.rs:5377`](../agents/roomlerd/src/signaling.rs#L5377)), so that org's
placement never picks this device.

⚠️ **Only a device with copies to tell about sends a replica frame**
([`replica.rs:433`](../agents/roomlerd/src/hive/supervisor/replica.rs#L433)). One whose
owner never turned `hive_replica` on, and that runs no replicated session, sends no
`rc:hive.replica.*` at all, so a server from before P2c-3a never receives a word it cannot
read.

⚠️ **The keys are read once, at the daemon's start**, like every `hive_*` key. Turning
`hive_replica` on is advertised, and answered, from the next start.

⚠️ **The quota is the store's own count of its pages**
([`store.rs:394`](../crates/hive-node/src/store.rs#L394)). The write-ahead log is folded
back at each checkpoint, and is not counted.

⚠️ **A joined member holds no copy yet.** The copy comes with the carrier and the stream
(P2d). Until then its tip is `seq` 0, and the server knows it as joined and empty.

### P2c-4a: placement decided again

A placement is not frozen at the start. When something it reads changes, the server
decides again for every live placed session of the organization
([`placement.rs:427`](../crates/modules/hive/src/placement.rs#L427)).

| What changed | Who tells hive | Where |
|---|---|---|
| a device's tags or owner | fleet's update route, once they are stored, through a new core hook, `agent_updated`; a failing holder is the route's 500, and a retry decides again | [`agent.rs:1042`](../crates/modules/fleet/src/agent.rs#L1042), [`hooks.rs:149`](../crates/core/src/hooks.rs#L149), [`hooks.rs:114`](../crates/modules/hive/src/hooks.rs#L114) |
| the policy, or its archive replicas | the policy write, after it is stored | [`policy.rs:354`](../crates/modules/hive/src/policy.rs#L354) |
| a member removed from the fleet | the removal cascade, hive first, while the row is still there: the device is named as gone | [`hooks.rs:103`](../crates/modules/hive/src/hooks.rs#L103) |

```mermaid
flowchart TD
    C["a tag, an owner, the policy, a removal"] --> P["place again, keeping what it holds:<br/>members that still qualify first, in their order"]
    P --> N{"for each device it chooses"}
    N -- "new" --> A["added pending, and joined<br/>once the primary runs the session"]
    N -- "leaving" --> B["pending again, and joined again"]
    N -- "a member already" --> K["kept as it is"]
    P --> L{"for each member it no longer chooses"}
    L --> X["leaving: its copy goes with the purge (P2g)"]
```

⚠️ **Deciding again moves no copy that need not move.** [`place`](../crates/modules/hive/src/placement.rs#L318)
keeps the members a session holds, in their order, before it takes another device,
active ones before leaving ones. Without that, tagging an unrelated device B would evict
member A from a session only because B was seen more recently. A smaller `min` keeps the
first members, and the rest leave.

⚠️ **A leaving member keeps its copy until the purge.** Its state is `leaving`
([`model.rs:387`](../crates/modules/hive/src/model.rs#L387)), and P2g sends it the purge
once `min` holds elsewhere: make-before-break. A join answer in flight when it was let go
does not take it back. If placement chooses it again, it is `pending` and is joined again;
its store already holds the membership.

⚠️ **Restricted tags still only grow.** Placement starts from the tags the session already
carries, so a primary losing `prod` does not release the session to untagged devices.

Each change that moved a member lands in `hive_audit` as `place` with outcome `replaced`.
The row names the session's owner, since the decision was the system's, not a person's.
A member that refused its join stays refused. A primary that started alone is told when
its session gains a member (P2c-4b, below).

⚠️ **Retained sessions are not placed again.** Placement decides again for live
sessions only, so an archive replica designated later receives the live ones it may
hold, never the ended sessions kept for the retention. The back-fill spec §3b asks for
needs a member to stream from, so it waits for the carrier (P2d).

### P2c-4b: the primary told, and sessions from before placement

A session that started alone was started without `replicated`, so its primary takes no
checkpoint. When placement later gives it a member (one of P2c-4a's changes, or its first
placement, below), the primary is told with the frame a member gets, in a third role:
`rc:hive.replica.join {role: primary}`
([`hive.rs:536`](../crates/remote_control/src/hive.rs#L536)). And a session placed
nowhere, from before P2 or started while the switch was off, is placed the first time
its primary connects with the switch on.

```mermaid
sequenceDiagram
    participant P as the primary
    participant S as the server
    participant M as the new member
    Note over P: runs the session alone:<br/>no checkpoint
    Note over S: placement gives it a member:<br/>a policy, a tag, a connection
    S->>S: the primary is owed its join (pending)
    S->>M: rc:hive.replica.join, role owner
    S->>P: rc:hive.replica.join, role primary
    Note over P: replicated from its next turn's end,<br/>and in its hosted record
    P-->>S: rc:hive.replica.join_ack: the primary is joined
    P->>S: rc:hive.replica.tip, after each checkpoint
```

| When | What the server does | Where |
|---|---|---|
| a session that started alone gains its first member | its primary is owed its join (`pending`), sent once the primary runs the session | [`placement.rs:555`](../crates/modules/hive/src/placement.rs#L555), [`dao.rs:1024`](../crates/modules/hive/src/dao.rs#L1024) |
| the primary connects | sends every primary's join it has not answered `joined`: `pending`, or `refused`, since a build that could not name the role may have learned it since | [`replica.rs:215`](../crates/modules/hive/src/replica.rs#L215), [`dao.rs:987`](../crates/modules/hive/src/dao.rs#L987) |
| the primary answers | `joined`, or `refused` with its word; an answer it was not owed changes nothing | [`dao.rs:876`](../crates/modules/hive/src/dao.rs#L876) |
| a device connects with the switch on | each live session it runs that was never placed is placed as at a start, and its primary is owed its join when placement chose a member besides it | [`agent_socket.rs:375`](../crates/modules/hive/src/agent_socket.rs#L375), [`placement.rs:609`](../crates/modules/hive/src/placement.rs#L609) |

The device answers the primary's join apart from a member's
([`supervisor/replica.rs:286`](../agents/roomlerd/src/hive/supervisor/replica.rs#L286)):

| Gate, in order | Refused with |
|---|---|
| the primary enrollment's connection | `secondary_org` |
| the daemon is not going down: its record would not keep the change, so the next daemon is asked | `other` |
| it runs the session at the join's fence | `stale_fence` when it runs a later one, `other` otherwise |

`hive_replica` is not asked: the primary holds no copy of another device's session, only
its own run.

⚠️ **The join turns a running session replicated, from its next turn's end.** The flag
is shared with the session's task ([`supervisor.rs:373`](../agents/roomlerd/src/hive/supervisor.rs#L373)),
which reads it at each turn's end. So every session now builds its checkpointer at
launch, and takes a checkpoint only while the flag is on.

⚠️ **It holds across a restart.** The hosted record keeps `replicated` (P2b-3b), so the
next daemon resumes the session replicated. The join writes the record, then the live
session, then the record again if it was not there yet. Three writers could lose it, and
none does:

| Writer | How the join's mark survives it | Where |
|---|---|---|
| the resume, which writes back the record as the daemon found it when it began | it keeps a `replicated` the record holds now. The server sends the primary's join at the very connection the resume runs for, so this is the usual order, not a race | [`supervisor.rs:2788`](../agents/roomlerd/src/hive/supervisor.rs#L2788) |
| the launch of a resumed session | it reads the record again once the session is live | [`supervisor.rs:3431`](../agents/roomlerd/src/hive/supervisor.rs#L3431) |
| a new session's first record | written from the live flag, under the record's lock | [`supervisor.rs:3464`](../agents/roomlerd/src/hive/supervisor.rs#L3464) |

⚠️ **A build from before P2c-4b cannot name the role.** P2c-3a and P2c-3b decode it as
none and refuse it: `other`, or `replica_disabled` while their `hive_replica` is off. An
older build drops the frame. The server keeps the refusal, so the room can say why the
session has no checkpoints, and asks again at every connection of that primary; updated,
it answers `joined`.

⚠️ **A primary its start told is owed nothing.** Its start said `replicated`, so it is
`joined` from its placement, and an answer to a join it was never sent changes nothing.

⚠️ **The switch off sends no replica frame, for sessions placed while it was on as
well.** Until P2c-4b only placement was gated: a session placed while `hive.replicaset`
was on, with the switch then turned off, still had its pending joins sent at every
connection and at its acceptance, and a start sent again still said `replicated`. A
primary's join turns checkpoints on, so a switch that let it through would not be a
kill switch. Now no join goes at a connection or an acceptance, and a start sent again
says `replicated` only while the switch is on
([`agent_socket.rs:370`](../crates/modules/hive/src/agent_socket.rs#L370)). Answers and
tips are still kept: they change no device. A device that already checkpoints goes on
until its session ends, since nothing turns checkpointing off.

Placement at a connection follows the rules of a start, and is audited as `place`, with
outcome `placed` or `short`, under the session's owner. A second connection places
nothing more: the record is written only where none is
([`dao.rs:1069`](../crates/modules/hive/src/dao.rs#L1069)).

In P2e the same frame becomes the server's word that lets a replicated session run after
a start, a restart or a reconnect (spec §3b): P2c-4b sends it when a session first has
members, and P2e at every connection.

### P2d-1: the stream's grants, the server's half

A member holds a copy, and the copy must come from somewhere: P2d carries it from the
source to the member, over QUIC between the two devices. The server never carries a byte
of it. P2d-1 builds the handshake on the server
([`stream.rs`](../crates/modules/hive/src/stream.rs)): a member asks, the server checks and
grants, the source confirms, and only then is the member told where to dial (FR-83). The
devices' half, the QUIC endpoint and the dial, is P2d-2.

```mermaid
sequenceDiagram
    participant M as member (dials)
    participant S as server (hive)
    participant P as source (the primary)
    M->>S: rc:hive.replica.want {session, fence, tip, path}
    Note over S: a joined member? the fence? the rung?<br/>the rate? the source online with hive-stream?
    S->>P: rc:hive.replica.grant {grant, member, path, token, ice_servers, ttl}
    P->>S: rc:hive.replica.ready {grant, cert_fingerprint, addrs} or refused
    S->>M: rc:hive.replica.dial {grant, source, token, cert_fingerprint, addrs}
    M-->>P: QUIC on the rung (P2d-2)
    M->>S: rc:hive.replica.candidate {grant, addr}, on the relay rung
    S->>P: rc:hive.replica.candidate, for the source to permit
```

| Frame | From → to | Says | Where |
|---|---|---|---|
| `rc:hive.replica.want` | member → server | `{session_id, fence, seq, hash, path, derp_pubkey?}`: stream me this session from my tip, on this rung | [`signaling.rs:886`](../crates/remote_control/src/signaling.rs#L886) |
| `rc:hive.replica.grant` | server → source | `{grant_id, session_id, fence, member_id, path, token, ice_servers, ttl_secs, member_derp_pubkey?}`: stand up an endpoint for this member | [`signaling.rs:3166`](../crates/remote_control/src/signaling.rs#L3166) |
| `rc:hive.replica.ready` | source → server | `{grant_id, cert_fingerprint, addrs, derp_pubkey?}`, or `{grant_id, refused, detail}` | [`signaling.rs:907`](../crates/remote_control/src/signaling.rs#L907) |
| `rc:hive.replica.dial` | server → member | `{grant_id, session_id, fence, source_id, path, token, cert_fingerprint, addrs, derp_pubkey?, ice_servers, ttl_secs}` | [`signaling.rs:3196`](../crates/remote_control/src/signaling.rs#L3196) |
| `rc:hive.replica.candidate` | member → server → source | `{grant_id, addr}`: the member's relayed address, for the source's TURN permission | [`signaling.rs:939`](../crates/remote_control/src/signaling.rs#L939), [`:3223`](../crates/remote_control/src/signaling.rs#L3223) |
| `rc:hive.replica.refused` | server → member | `{session_id, fence, path, reason, detail?}`: no stream this time, in the server's word or the source's | [`signaling.rs:3232`](../crates/remote_control/src/signaling.rs#L3232), [`hive.rs:822`](../crates/remote_control/src/hive.rs#L822) |
| `rc:hive.replica.close` | either way | `{grant_id, reason}`: the stream is over | [`signaling.rs:948`](../crates/remote_control/src/signaling.rs#L948), [`:3249`](../crates/remote_control/src/signaling.rs#L3249) |

**The ladder is the member's to climb** ([`hive.rs:768`](../crates/remote_control/src/hive.rs#L768)).
Each want names one rung, `direct`, `relay` or `derp`, and the source stands up one
endpoint for it. A member climbs down a rung when one does not carry, and every renewal
starts from `direct` again: never ratchet.

| Rung | The grant carries | The source answers |
|---|---|---|
| `direct` | STUN only, for a server-reflexive candidate | its host and reflexive addresses |
| `relay` | TURN credentials, minted for both sides with the grant as the session key, so both land on one coturn worker | its relayed address; the member's `candidate` gets it a permission |
| `derp` | the member's WireGuard key, from the want | its own key; no address, since DERP addresses by key |

The server's gates on a want, in order ([`stream.rs:268`](../crates/modules/hive/src/stream.rs#L268)):

| Gate | Refused with |
|---|---|
| the session is in the device's org, and the device a joined member of it, never its primary | `not_a_member`, the answer a session it cannot see gets too |
| the want's fence is the session's | `stale_fence` |
| a rung this build can name; on `derp`, the member's key; on `relay`, a TURN server configured | `path_unavailable` |
| 12 wants a minute per (member, session), counted after the gates above | `rate_limited` |
| the source, the session's primary in P2d, online here | `source_offline` |
| its connection advertises `hive-stream` | `source_unsupported` |
| it serves fewer than 32 streams | `busy` |

The source's own word comes back in its `ready`, and reaches the member as is:
`not_held`, `path_unavailable`, `busy`. A source that does not answer within 30 s is the
member's `no_answer`, and the source is told to drop the grant
([`stream.rs:429`](../crates/modules/hive/src/stream.rs#L429)). The relay's allocation alone
can take ~20 s on a hostile network (#1761), hence 30 s where the viewer waits 10.

⚠️ **The member dials only what the source said, and only once it said it**
([`stream.rs:466`](../crates/modules/hive/src/stream.rs#L466)). A `ready` must carry a
certificate fingerprint as one is written, IP literals and never names, at most 8 of
them, and on `derp` the source's key. Anything else is refused `other` to the member, and
the source told to close the endpoint (`malformed`).

⚠️ **A renewal comes up beside the grant it replaces.** A member holds at most two grants
of one session: the one it follows, and the renewal that carries before the first is let
go (make-before-break). A third want replaces the oldest, and both sides are told
`replaced`.

⚠️ **A member let go stops following at once** ([`placement.rs:571`](../crates/modules/hive/src/placement.rs#L571)).
When placement marks it `leaving`, its streams of that session end there, not at their
grant's end: a tag taken off must stop the copy following, whatever it holds until the
purge. A removed device's streams end too, as member or source
([`hooks.rs:97`](../crates/modules/hive/src/hooks.rs#L97)).

⚠️ **`hive-stream` is a promise of its own** ([`models.rs:683`](../crates/remote_control/src/models.rs#L683)):
a build that carries streams, advertised while its store is open. The spec's diagram had
the server check the source for `hive-replica`. A primary need not hold copies of other
devices' sessions to serve its own, though, and P2c-4b already tells a primary it is
replicated whatever its owner said of others'. Asking for `hive-replica` would silently
stop such a primary replicating. A frame is read only from a connection that advertises
it ([`agent_socket.rs:321`](../crates/modules/hive/src/agent_socket.rs#L321)), and pushed
only to one ([`hub.rs:2164`](../crates/modules/fleet/src/hub.rs#L2164)).

⚠️ **Grants are pod-local**, like view grants, and need both devices' sockets on the pod
that mints them. While a roll moves an org's devices, a want is refused `source_offline`,
and the member asks again. With the replicaset's switch off, no stream frame is answered
or sent ([`stream.rs:212`](../crates/modules/hive/src/stream.rs#L212)).

The source is the session's primary in P2d. A fresher member serving while the primary
is away comes with promotion (P2e). No build advertises `hive-stream` before P2d-2, so
until then no grant is pushed and no want arrives.

### P2d-2a: the direct rung, on the devices

P2d-2a puts the devices' half of the handshake under the server's (P2d-1), on the
`direct` rung ([`supervisor/stream.rs`](../agents/roomlerd/src/hive/supervisor/stream.rs)).
The source stands an endpoint up for each grant, the member dials it, and the two say
where the member's copy stands. That opening exchange has no I/O in it
([`hive-node/src/stream.rs`](../crates/hive-node/src/stream.rs)): the messages, their
framing, and the source's answer, read from its chain.

```mermaid
sequenceDiagram
    participant M as member
    participant S as server
    participant P as source
    S->>P: rc:hive.replica.grant, direct
    Note over P: holds the session? under 32 streams?<br/>one UDP socket: STUN on it, then QUIC on it
    P->>S: rc:hive.replica.ready {fingerprint, host and reflexive addresses}
    S->>M: rc:hive.replica.dial
    Note over M: a member of it, by its own record?<br/>an address a source could hold?
    M-->>P: QUIC to each address in turn, the certificate pinned
    M-->>P: the token, on a stream of its own
    M-->>P: hello {v, session, fence, tip, floor, checkpoints}
    P-->>M: hello {v, tip}, or diverged {at}, or refused {reason}
```

| The source answers | When | Where |
|---|---|---|
| `hello {v, tip, live}` | the member's tip is in the source's chain (genesis, for an empty copy): its copy is a prefix, and the stream goes on from there. `live` and `diverged`'s `tip` came with P2d-3b | [`stream.rs:340`](../crates/hive-node/src/stream.rs#L340) |
| `diverged {at, tip, live}` | the member's tip is not in the chain, or is past its end: `at` is the newest of the member's checkpoints the source holds too, else genesis | the same |
| `refused {reason}` | a version it does not speak (`version`), a session it does not hold (`not_held`), a hello for another session or fence than the grant's (`wrong_session`, `wrong_fence`): the token proves the grant, and the hello must be for it; a first frame that is not a hello (`not_a_hello`) | the same, and [`supervisor/stream.rs:958`](../agents/roomlerd/src/hive/supervisor/stream.rs#L958) |

The devices' own checks, before anything is dialed:

| Side | Answered with | When |
|---|---|---|
| source | `ready` refused `other` | a secondary org's connection: a device serves streams for its primary org only |
| source | `ready` refused `path_unavailable` | a rung this build cannot name |
| source | `ready` refused `not_held` | its store holds nothing of the session, or is closed |
| source | `ready` refused `busy` | it serves 32 streams already |
| member | `close`: `secondary_org`, `path_unavailable`, `not_a_member` | a secondary org's dial, another rung, or a session it never joined, by its own store's record ([`supervisor/stream.rs:1203`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1203)) |
| member | `close` `no_address` | a dial naming no address a source could hold: the unspecified address, port 0, multicast, broadcast ([`remote_control/src/hive.rs:753`](../crates/remote_control/src/hive.rs#L753)) |
| source | `close` `auth_failed` | a connection that presents another token |
| source | `close` `bad_frame` | a frame that announces more than 1 MiB, or is not a message |

⚠️ **STUN is asked on the very socket QUIC then serves on**
([`supervisor/stream.rs:248`](../agents/roomlerd/src/hive/supervisor/stream.rs#L248)),
so the reflexive address the member dials is the NAT mapping QUIC uses; a second
socket would hold a different one. No hole is punched: as on the tunnel's direct path,
the member dials, and a NAT that admits nothing it did not ask for falls to the `relay`
rung.

⚠️ **The member dials only an address a source could hold**
([`remote_control/src/hive.rs:753`](../crates/remote_control/src/hive.rs#L753)).
The addresses reach it through the server, from the source, and it dials as SYSTEM or
root: a multicast address relayed to every member of a tenant would have each of them
spray its own network. The check is the member's own, so it holds against a
compromised server and a compromised source alike.

⚠️ **One connection per grant, and the grant's life is the end of it.** Each endpoint
holds a UDP socket. A connection ends when the other side closes it, when the server's
`close` arrives, or at `ttl_secs`, whichever comes first, and the server is told why. A
refusal is held until the member lets the connection go, 10 s at most: the last to
receive is the one to close (quinn's `Connection::close`). Closed under its data, a
connection loses it, and the member would hear no answer where there is one. An
acknowledgement is not enough: a receiver may drop what it acknowledged but had not
yet read once the close arrives.

⚠️ **A frame says its length first, and one past 1 MiB is refused before it is read**
([`stream.rs:301`](../crates/hive-node/src/stream.rs#L301)): the other side is a
device of the org, not a trusted one.

⚠️ **Not advertised yet.** This build answers grants and dials, but does not advertise
`hive-stream`. A source that answers the hello but streams no event would hold a member
waiting for nothing; events flow from P2d-3a, and the verb is advertised with the member's wants (P2d-3b). Until then no
grant reaches this code outside its tests. It is stripped from a secondary org's
connection already, for when it is advertised.

### P2d-2b: the relay rung, and a grant registered first

P2d-2b serves and follows the `relay` rung
([`supervisor/stream.rs`](../agents/roomlerd/src/hive/supervisor/stream.rs)), for the
networks where no direct path carries: a symmetric NAT, a corporate LAN that drops
unsolicited UDP. Each side allocates on the TURN server the grant names, QUIC runs over
both allocations, and the opening exchange is the direct rung's. The relay sees
ciphertext only.

```mermaid
sequenceDiagram
    participant M as member
    participant S as server
    participant T as TURN
    participant P as source
    S->>P: rc:hive.replica.grant, relay, TURN credentials
    Note over P: registered at once
    P->>T: allocate
    P->>S: rc:hive.replica.ready {fingerprint, relayed address}
    S->>M: rc:hive.replica.dial {relayed address, TURN credentials}
    M->>T: allocate, on the source's worker first
    M->>S: rc:hive.replica.candidate {its relayed address}
    S->>P: rc:hive.replica.candidate
    P->>T: one datagram to it: the permission
    M->>T: one datagram to the source: its own permission
    M-->>P: QUIC through both allocations, then the token and the hello
```

| Step | Side | Why |
|---|---|---|
| the grant registered before its endpoint is built ([`supervisor/stream.rs:687`](../agents/roomlerd/src/hive/supervisor/stream.rs#L687)) | source | the server gives up on a source silent for 30 s and ignores a later `ready`, and an allocation was measured at ~20 s on a UDP-hostile network (#1761). Registered only once built, an endpoint the server had already forgotten waited out its grant. Now a `close` aborts the setup wherever it is, and an allocation dropped mid-flight frees its socket (FR-48) |
| the member's allocation on the source's worker ([`supervisor/stream.rs:1592`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1592)) | member | its first url is `turn:<the source's relayed IP>:<the grant's UDP port>`: two allocations on one coturn worker relay to each other, across workers they may not (the tunnel client's `setup_quic_over_relay`). The granted urls follow, should that one not answer |
| the candidate ([`supervisor/stream.rs:896`](../agents/roomlerd/src/hive/supervisor/stream.rs#L896)) | member, then source | the source is the QUIC server and never sends first, so without a permission for the member's relayed address its TURN server drops the member's opening Initials. A candidate that arrives before the allocation lands is held, 16 at most, and permitted once it does, before the `ready` |
| the member's own permission | member | one datagram to each of the source's addresses before its first Initial, as the tunnel's client sends; the TURN client would install the permission on that first send anyway, so this only spares the first Initial a race. 300 ms to settle, then the dial |

| Answered with | When |
|---|---|
| `ready` refused `path_unavailable` | the grant carries no TURN server, or no allocation could be made: the member climbs down a rung |
| `ready` refused `busy` | the source serves 32 streams already, those still being stood up among them |
| `close` `no_turn` · `allocate_failed` | the member's dial carries no TURN server, or it could not allocate |
| `close` `busy` | the member follows 32 streams already: each holds a socket, and on this rung an allocation |

⚠️ **A TURN permission is an IP's, not an address's** (RFC 8656 §9). The source's one
datagram to the member's relayed address permits that whole IP on its allocation.

⚠️ **The address check is the device's, mirrored on the server.** One rule, in the wire
crate ([`remote_control/src/hive.rs:753`](../crates/remote_control/src/hive.rs#L753)), read
at both ends: the server drops a `ready` or a candidate offering an address no device
could be reached at (the unspecified address, port 0, multicast, broadcast), as a
courtesy ([`modules/hive/src/stream.rs:118`](../crates/modules/hive/src/stream.rs#L118)),
and the device checks again before it dials or permits. The device's check is the one
that holds against a compromised server.

⚠️ **A replayed grant or dial is not taken twice.** A second stream for one grant would
replace the first's entry, and the first, its abort handle lost, would hold its socket
and connection to its grant's end.

### P2d-2c: the derp rung, on a route of its own

P2d-2c serves and follows the `derp` rung
([`supervisor/stream.rs`](../agents/roomlerd/src/hive/supervisor/stream.rs)), the floor
for a pair no UDP path joins. QUIC rides each device's own established `/derp`
connection, the one the overlay already holds to the server, addressed by WireGuard
key, with the MTU clamped under the relay's 2,048-byte frame. The relay sees
ciphertext only.

```mermaid
sequenceDiagram
    participant M as member
    participant S as server (hive, and the /derp relay)
    participant P as source
    S->>P: rc:hive.replica.grant, derp, the member's key
    Note over P: registered at once, a route of the stream's<br/>own toward the member's key, by the grant's id
    P->>S: rc:hive.replica.ready {fingerprint, its own key}
    S->>M: rc:hive.replica.dial {the source's key}
    Note over M: a route of its own toward the source's key
    M-->>P: QUIC through /derp: RMHIVE01 · grant id · packet
```

| Piece | Where | Why |
|---|---|---|
| a route per stream, by the grant's id | `DerpMux::hive_conn_for` ([`derp.rs:521`](../crates/tunnel-core/src/transport/derp.rs#L521)) | the tunnel's route is one per peer and the last caller takes it: on it, a stream would take a `quic-derp-v1` tunnel's route between the same two devices, and a second stream between the pair (another session, a renewal) the first's |
| the framing: `RMHIVE01` (8 bytes), the grant's id (12), the packet | `DerpConn::send_to`; split first in `DerpMux::deliver` ([`derp.rs:596`](../crates/tunnel-core/src/transport/derp.rs#L596)) | WireGuard's header never opens on the magic and disco's is another; a QUIC packet that does is a 2^-64 event. A frame for a tag nobody holds is dropped and counted (`hive_drops`), never reported `Unrouted`, which sends the overlay after a WireGuard peer |
| the source's endpoint | `derp_endpoint` ([`supervisor/stream.rs:278`](../agents/roomlerd/src/hive/supervisor/stream.rs#L278)) | `ready` names its own key and no address |
| the member's client | `member_derp` ([`supervisor/stream.rs:1562`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1562)) | it dials the route's synthetic peer address; the `no_address` check does not apply on this rung |

| Answered with | When |
|---|---|
| `ready` refused `path_unavailable` | the device has no live DERP connection (a build without the overlay, or the overlay down), or the grant names no member key |
| `close` `no_derp` · `no_derp_key` | the member has no live DERP connection, or the dial names no source key |

⚠️ **Under an enforcing overlay ACL, only a pair the netmap shows each other gets
through** (the relay's own check, `crates/modules/network/src/derp_acl.rs`). Two
members of one session that are not overlay peers never connect on this rung, and
stay on `relay`, whose TURNS/443 tier is the TCP floor for them.

⚠️ **Hive frames share the relay's tunnel budget.** They are neither WireGuard nor
disco, so the server's DERP shaper counts them with the device's `quic-derp-v1`
tunnels, about 3 Mbit/s sustained per device.

### P2d-3a: the events, the blobs and the acks

P2d-3a carries the copy itself over the stream P2d-2 stands up
([`supervisor/stream.rs`](../agents/roomlerd/src/hive/supervisor/stream.rs)). After the
opening exchange the source sends the events past where the member's copy ends, from its
store and then as they are appended. The member asks for each blob an event names that it
lacks, keeps them, applies the event through its store's chain check, and acks each frame
once all of it is on disk. The messages have no I/O in them
([`hive-node/src/stream.rs`](../crates/hive-node/src/stream.rs)).

```mermaid
sequenceDiagram
    participant M as member
    participant P as source
    M->>P: hello {tip, floor, checkpoints}
    P->>M: hello {tip}, or diverged {at}
    Note over M: on diverged: the older fence's tail<br/>past at set aside
    P->>M: events [{seq, fence, ts_ms, prev_hash, len}], then each event's JSON
    Note over M: a checkpoint: does the store hold its blobs?
    M->>P: need {the blobs it lacks}
    P-->>M: a QUIC stream per blob: blob {hash, len}, then its bytes
    Note over M: each blob kept, then the event applied
    M->>P: ack {seq, hash}, once the frame is on disk
    Note over P: then the live tail, never more<br/>than 4 MiB past the last ack
```

| Message | From | Says | Where |
|---|---|---|---|
| `events {events: [{seq, fence, ts_ms, prev_hash, len}]}`, then each event's JSON | source | at most 500 events and 1 MiB of JSON a frame, one larger event alone, and at most 32 MiB an event | [`stream.rs:157`](../crates/hive-node/src/stream.rs#L157) |
| `need {blobs}` | member | the blobs the next event names that it lacks, at most 64 | [`stream.rs:120`](../crates/hive-node/src/stream.rs#L120) |
| `blob {hash, len}`, then its bytes | source, on a stream per blob | one blob of the `need`, at most 4 MiB | [`stream.rs:210`](../crates/hive-node/src/stream.rs#L210) |
| `ack {seq, hash}` | member | the frame's last event: applied and on disk, with every blob the frame names | [`stream.rs:123`](../crates/hive-node/src/stream.rs#L123) |

| Rule | Why | Where |
|---|---|---|
| The source subscribes to its store's feed first, then reads the store from the member's tip | an event appended in between is in one or the other, and the cursor drops what is in both, as the viewer's `follow` does | [`supervisor/stream.rs:1034`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1034) |
| A member asks for blobs; a source never pushes one | a blob pushed ahead of its event reaches the member before it can know the name, so it could hold the blob to none, and a source could fill its disk; a blob it could not read at once would fill the connection's flow-control window under the data that frees it | [`stream.rs:27`](../crates/hive-node/src/stream.rs#L27) |
| The blobs an event names are kept before the event is applied | a copy never holds a checkpoint without its files: an ack, a tip report and a hello all speak for the blobs too | [`supervisor/stream.rs:1391`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1391) |
| A wave of blobs is asked for only once this device's blob budget holds it, and all of its streams are read at once | a stream left waiting for another fills the connection's flow-control window under it, and the one waited for can then never arrive | [`supervisor/stream.rs:1430`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1430), [`supervisor/stream.rs:566`](../agents/roomlerd/src/hive/supervisor/stream.rs#L566) |
| An event held already, the same `seq` hashed the same, is taken as held | a renewal comes up beside the stream it replaces, and both carry the same events | [`store.rs:450`](../crates/hive-node/src/store.rs#L450) |
| On `diverged`, only an older fence's tail is set aside, and only past a point the member named | the one rewrite the store allows; a source's word alone never moves a copy back | [`supervisor/stream.rs:622`](../agents/roomlerd/src/hive/supervisor/stream.rs#L622) |
| A source sends no more than 4 MiB of events past the last ack, and checks each ack against what it sent | what a member that stopped reading leaves unread stays well under the window its blobs need beside it | [`supervisor/stream.rs:434`](../agents/roomlerd/src/hive/supervisor/stream.rs#L434), [`supervisor/stream.rs:518`](../agents/roomlerd/src/hive/supervisor/stream.rs#L518) |
| The member reports its tip at most every 5 s while its copy moves, and when the stream ends | the server learns each member's freshness from the member itself (spec §3b), without a write per event | [`supervisor/stream.rs:1499`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1499) |
| What a member holds in memory is bounded across every stream it follows: 64 MiB of events and 16 MiB of blobs, each taken before it is read | 32 sources must not each make it hold their largest at once | [`supervisor/stream.rs:117`](../agents/roomlerd/src/hive/supervisor/stream.rs#L117) |

The words a stream ends in, as the server hears them in `rc:hive.replica.close`:

| Close | Side | When |
|---|---|---|
| `blob_missing` | source | a blob the member asked for is not in its store |
| `event_too_large` | source | an event past 32 MiB: no copy follows past it |
| `bad_ack` | source | an ack for a frame it did not send, or by another hash |
| `bad_frame` | either | a frame that is not one, out of turn, or announcing past a limit |
| `bad_blob` | member | a blob it did not ask for, at another length, or whose bytes do not hash to its name |
| `bad_answer` | member | `diverged` at a point it did not name |
| `diverged_at_floor` | member | its tail is of the current fence, and is never set aside |
| `stale_fence` · `gap` · `broken_chain` · `purged` | member | its store refused an event |
| `stalled` | member | 60 s without a byte, in the middle of an event or a blob |
| `source:<word>` · `member:<word>` | either | the other side ended the stream on its word: `source:blob_missing`, `member:diverged_at_floor` |

⚠️ **A copy at the current fence is never set aside, whatever the source says.** One way
to get there: a primary loses its last events in a power cut (the store is `synchronous =
NORMAL`, durable across a daemon crash, not a power cut) after a member applied them. The
two then disagree at the current fence. The member keeps its copy and ends the stream
`diverged_at_floor` until promotion (P2e) can decide which copy is the session's.

⚠️ **The build holds the limits.** The largest event a device records, a checkpoint's
manifest (16 MiB), a stream-json line (8 MiB) and an adopted session's line (4 MiB), must
stay under the stream's 32 MiB, and each memory budget must hold the largest thing it
counts. Both are compile-time assertions
([`supervisor/stream.rs:126`](../agents/roomlerd/src/hive/supervisor/stream.rs#L126)): a producer raised
past the limit fails the build, not a copy in the field.

⚠️ **Not advertised in P2d-3a.** That build did not advertise `hive-stream`, so no grant
reached this code outside its tests. The member's wants, which ask the server for a grant
for each session it holds, came with the advertisement (P2d-3b).

### P2d-3b: the member's wants, and what it follows

P2d-3b gives each copy a follow ([`supervisor/follow.rs`](../agents/roomlerd/src/hive/supervisor/follow.rs)).
For every session the device holds as a member, it asks the server for a stream
(`rc:hive.replica.want`) on one rung of the ladder at a time, follows the dial it gets,
and acts on every other answer. The build advertises `hive-stream` from here, so devices
serve and follow streams wherever the server's `hive.replicaset` switch is on (off by
default).

```mermaid
stateDiagram-v2
    [*] --> Wanting: a join, or the primary connection
    Wanting --> Following: dial
    Wanting --> Wanting: a rung down
    Wanting --> Waiting: later
    Waiting --> Wanting: the wait is over
    Wanting --> Queued: every slot taken
    Queued --> Wanting: a slot frees
    Following --> Wanting: the stream ended
    Following --> Parked: caught up, the session over
    Wanting --> Stopped: no member of it
    Following --> Stopped: let go
    Stopped --> Wanting: the next join
```

| Answer | Next | Where |
|---|---|---|
| refused `path_unavailable` or `no_answer`; a stream that never got past its opening exchange (`unreachable`, `no_turn`, `allocate_failed`, `no_derp`, `auth_failed`, …) | the next rung down, at once; past the bottom, a wait, then the top | [`supervisor/follow.rs:209`](../agents/roomlerd/src/hive/supervisor/follow.rs#L209), [`supervisor/follow.rs:222`](../agents/roomlerd/src/hive/supervisor/follow.rs#L222) |
| refused `source_offline`, `busy`, `rate_limited`, or a word this build cannot read; a stream that ended for its data | a wait, then the same rung | the same |
| refused `source_unsupported` or `not_held`; a stream ended `diverged_at_floor` | a long wait (30 min) | the same |
| a stream that carried and ended (`expired`, the source let go) | at once, from the top | the same |
| a stream ended `caught_up` | parked: asked for no more | the same |
| refused `not_a_member` or `stale_fence`; the server's close `member_left` or `device_removed` | stopped, until the next join | the same |

Waits start at 5 s and double to 5 min, a fifth either way so devices that lost the same
source do not all ask again at once; a stream that carried starts them over.

| Rule | Why | Where |
|---|---|---|
| Every new ask starts from `direct`; only a rung that did not carry is climbed down | never ratchet: a member that fell to `derp` is back on a direct path at the first ask that finds one | [`supervisor/follow.rs:817`](../agents/roomlerd/src/hive/supervisor/follow.rs#L817) |
| A source says whether the session is live there, as it runs there or its hosted record will resume it; a member caught up with one that is over lets the stream go (`caught_up`), and the session is parked | a device holds copies of every session its owner ran, most of them over; the slots go to the sessions that move | [`supervisor/stream.rs:1535`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1535), [`supervisor/stream.rs:1290`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1290) |
| At most 32 asked for or followed at once, the rest queued in order and given the first slot freed | as many as the stream lets a member follow | [`supervisor/follow.rs:893`](../agents/roomlerd/src/hive/supervisor/follow.rs#L893) |
| A dial is followed only while the owner lets the device hold copies (`hive_replica`), or for the copy of a teleport the device was prepared for (P2f-1b, below) | the gate that survives a compromised server: without it, a server could have a device go on taking a session its owner said it should no longer hold. A teleport's copy is the one the device will run, admitted by its own prepare's gates | [`supervisor/stream.rs:1149`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1149) |
| Only the primary organization's connection asks | a device holds copies for its primary organization alone (P2c-3) | [`supervisor.rs:2634`](../agents/roomlerd/src/hive/supervisor.rs#L2634) |
| `hive-stream` is advertised while the store is open, whatever the owner says of holding copies | a primary serves its own sessions' members (P2c-4) | [`agents/roomlerd/src/encode/caps.rs:1657`](../agents/roomlerd/src/encode/caps.rs#L1657) |

⚠️ **Parked lives in memory.** After a restart the device asks for every copy once more.
A copy whose session is over and that it holds whole lets its stream go at the opening
exchange, since the source's tip is the copy's own.

⚠️ **No renewal in P2d-3b.** A stream ended with its grant (10 min) and was asked for
again from the top: a few seconds without one. P2d-4 renews at half-life and keeps the
old connection until the new one carries (below).

### P2d-4: renewals

A grant lives 10 minutes. P2d-4 keeps a copy's stream going past that. At half a
grant's life the member asks for the session again, from the top of the ladder, and
follows the renewal's stream beside the old one. Once the new stream gets past its
opening exchange, it is the one followed and the old one is let go (`renewed`). A
copy is never without a stream at its grant's end (make-before-break).

```mermaid
sequenceDiagram
    participant M as member
    participant S as server
    participant P as source
    Note over M,P: M follows grant g1 (10 min)
    M->>S: rc:hive.replica.want, at 5 min, from direct
    S->>P: rc:hive.replica.grant g2
    P->>S: rc:hive.replica.ready
    S->>M: rc:hive.replica.dial g2
    M->>P: g2: token, hello, beside g1
    P->>M: g2: hello or diverged
    Note over M: g2 answered: followed from here
    M->>S: rc:hive.replica.close g1, renewed
    S->>P: rc:hive.replica.close g1, renewed
```

| Rule | Why | Where |
|---|---|---|
| The renewal is asked for at half the grant's life, on `direct` whatever rung the stream it renews is on, and climbs down a rung on its own answers | never ratchet: a member that fell to `derp` is back on a direct path at the first renewal that finds one | [`supervisor/follow.rs:616`](../agents/roomlerd/src/hive/supervisor/follow.rs#L616), [`supervisor/follow.rs:635`](../agents/roomlerd/src/hive/supervisor/follow.rs#L635) |
| The old stream is let go once the new one answers its opening exchange, before the new one sets anything aside | a rung that answered carries, the ladder's own test (P2d-3b); the old stream stops applying events before a `diverged` answer moves the copy's tail | [`supervisor/follow.rs:583`](../agents/roomlerd/src/hive/supervisor/follow.rs#L583), [`supervisor/stream.rs:1280`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1280) |
| A renewal that finds no rung, or hears "not now", is dropped. The stream followed carries on to its end, and the follow asks again from the top then | the old stream still carries, and the next ask is at most minutes away | [`supervisor/follow.rs:696`](../agents/roomlerd/src/hive/supervisor/follow.rs#L696) |
| The stream followed that ends with its renewal dialed hands over to it, and a rung down from there is a rung down from the renewal's. An end that lets the session be (`caught_up`, `member_left`) lets the renewal's stream go too | no gap when the old stream ends before the new one answered; a session parked or stopped holds no stream | [`supervisor/follow.rs:757`](../agents/roomlerd/src/hive/supervisor/follow.rs#L757) |
| A dial while a stream is followed is that stream's renewal, a late one included. A newer one replaces a renewal dialed already (`replaced`) | a member holds at most two streams of a session, as the server holds at most two grants of it, a third replacing the oldest | [`supervisor/follow.rs:522`](../agents/roomlerd/src/hive/supervisor/follow.rs#L522) |
| A renewal asked for when the connection went is asked for again on the next one, on its rung. Each of its asks times out on its own clock | its answer would have come on the connection that went; a timeout must not move an ask the renewal moved on from | [`supervisor/follow.rs:277`](../agents/roomlerd/src/hive/supervisor/follow.rs#L277), [`supervisor/follow.rs:664`](../agents/roomlerd/src/hive/supervisor/follow.rs#L664) |
| A member follows at most 64 streams: 32 sessions, each with its renewal beside it | the follow's slots are 32; a renewal must not be turned away `busy` by the stream it renews | [`supervisor/stream.rs:102`](../agents/roomlerd/src/hive/supervisor/stream.rs#L102) |

⚠️ **The old stream is let go from outside its task.** The task is aborted, which
drops its QUIC connection, its socket and, on `relay`, its allocation
([`supervisor/stream.rs:1687`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1687)). The server
is told `renewed`, and tells the source. A test holds the source's side of the old
stream to end with the member's word rather than its own clock.

⚠️ **A refusal names no ask.** `rc:hive.replica.refused` carries the session, the fence
and the rung. A renewal's refusal that arrives after the stream followed has ended is
taken for the follow's next ask on that rung. That costs one rung climbed down for
nothing, never a stream, as does an answer that arrives after its ask timed out
(P2d-3b).

⚠️ **In the field** once a release carries it, where the server's `hive.replicaset` is
on (off by default).

### P2d-5: a replica's read-only view

A session's transcript sits on its members as well as on its primary. P2d-5 lets a
viewer read a member's copy while the primary cannot serve, offline above all. Before
it, the server always asked the primary, and with the primary offline the room showed
its stubs and nothing more.

```mermaid
flowchart TD
    O[hive:view.open] --> P{can the primary serve?}
    P -- yes --> G1[a grant to the primary: a driver drives]
    P -- no --> S{hive.replicaset on?}
    S -- no --> R[refused in the primary's word]
    S -- yes --> M{a joined member that serves copies?}
    M -- no --> R
    M -- yes --> G2[a grant to the freshest copy: read only]
```

| Rule | Why | Where |
|---|---|---|
| The primary while it can serve (online here, advertising `hive-view`); else the freshest member's copy; with neither, the primary's own word refuses (`device_offline`, `device_unsupported`) | a session readable only while its device is online was the gap the replicaset closes for reading | [`view.rs:229`](../crates/modules/hive/src/view.rs#L229) |
| The freshest by the tip each member last reported, the fence first and then the seq; an archive replica on a tie, then the lowest device id. A member that reported no tip ranks below any that did | the reader sees as much as the copies hold, and an archive replica is the one kept for reading (design §5.2) | [`replica.rs:94`](../crates/modules/hive/src/replica.rs#L94) |
| Only a member that took its copy (`joined`), on a connection advertising both `hive-view` and `hive-replica` | a pending or leaving member holds no copy to read, and a device whose owner turned `hive_replica` off serves none | [`hub.rs:1847`](../crates/modules/fleet/src/hub.rs#L1847) |
| A grant toward a copy never carries `may_prompt`, nor the driver's address | the harness runs on the primary, the one device a prompt can reach; a reader's address never leaves the server | [`view.rs:279`](../crates/modules/hive/src/view.rs#L279) |
| The device serves a copy on `hive_replica` alone, with agent sessions themselves off, and serves none once its owner has turned `hive_replica` off, `hive_enabled` or not | an archive replica runs no sessions; and this gate survives a compromised server: an owner who no longer holds copies serves none of those still on disk until the purge | [`view.rs:283`](../agents/roomlerd/src/hive/view.rs#L283) |
| A copy is read only whoever the server named, and its hello says `copy` | the viewer is told why it cannot prompt, instead of a refusal worded for a driver | [`view.rs:352`](../agents/roomlerd/src/hive/view.rs#L352), [`view.rs:793`](../agents/roomlerd/src/hive/view.rs#L793) |
| The audit row of a view served from a copy says `copy`, and names the member | every decision is audited, and which device served the reader | [`view.rs:333`](../crates/modules/hive/src/view.rs#L333) |

⚠️ **A member's tip is its own claim.** It only orders the members. The device serves
what it holds, so a member that claims more than it has shows less than it claimed.

⚠️ **One switch.** With `hive.replicaset` off no copy serves a view, whatever its
members hold: the replicaset's switch stops all of it.

⚠️ **The SPA says so.** A view from a copy reads "You are reading a copy another device
holds", with no composer, for a driver too
([`HiveTranscript.vue:189`](../ui/src/components/hive/HiveTranscript.vue#L189)).

### P2d-6: AC2 on two devices in CI

P0f's canary test ([`hive_canary.rs`](../crates/tests/tests/hive_canary.rs)) found every
canary in its one device's store and in none of the places the server keeps. P2d-6 runs
it with the replicaset on and a second device: each session is placed on a member too,
and every canary must reach the member's store as well, carried over the replica stream,
while the server carries the stream's signalling and the member's tips.

```mermaid
sequenceDiagram
    participant T as the test (server + primary)
    participant M as the member (a child process)
    T->>M: the same binary, member_device_process, a config that holds copies
    M->>T: online, advertising hive-replica
    T->>T: sessions start, placed on the member too
    M->>T: the join, the wants
    T-->>M: the stream (direct, on loopback): events, checkpoints, their blobs
    M->>T: tips, until each copy ends where the primary does
    T->>M: stdin closed: the member stops
    T->>T: the member's store read, the server's places searched
```

| What | Why | Where |
|---|---|---|
| The member is this test binary again, its one member test asked to be a device by an environment variable; an ordinary run passes that test at once | a process holds one supervisor; and a `[[bin]]` cannot carry the test launcher, which reaches `roomlerd` only as a Linux dev-dependency, so that no build but a test's ever compiles it | [`hive_canary.rs:260`](../crates/tests/tests/hive_canary.rs#L260) |
| Each copy is waited for until the member's tip, as the server heard it, is the primary's, which the test reads as a viewer does | the test never opens a store a running device writes | [`hive_canary.rs:314`](../crates/tests/tests/hive_canary.rs#L314), [`hive_canary.rs:304`](../crates/tests/tests/hive_canary.rs#L304) |
| Every canary is in the member's copy, and so is a checkpoint, none of them failed | a member applies a checkpoint only once it holds every blob it names, so its checkpoint is the blobs' crossing too | [`hive_canary.rs:337`](../crates/tests/tests/hive_canary.rs#L337) |
| An integration test's device takes its checkpoints in process (`hive-test-launcher`) | its binary is a test's, which knows no `hive-checkpoint`: every checkpoint of a replicated session failed before | [`supervisor.rs:975`](../agents/roomlerd/src/hive/supervisor.rs#L975) |

⚠️ **Not the server's `/derp`.** The stream rides `direct` here, on loopback. The tests
build carries no overlay, so neither device has a DERP connection, and no stream frame
crosses the server's `/derp`: an absence checked there could not fail. That the `derp`
rung carries ciphertext only is held by the device's own tests (P2d-2c); the server's
relay, in the field run (P2k).

⚠️ **AC2 stays open.** The test passes in CI; the criterion is ticked on the field run's
evidence (P2k), like every other.

### P2e-1: the fence on the device

A session's lease is a fence: the server's word on where the session runs, which only
grows. P2e-1 makes the device hold to it before anything moves a session (the promotion,
P2e-2 onward). A start at a newer fence ends the run here first. `rc:hive.demote` tells
an old primary that it no longer holds the session. A run that ends removes only what is
its own. On the server, a device that reports a session held elsewhere is answered with
the demote.

```mermaid
sequenceDiagram
    participant S as server
    participant D as device
    participant R1 as run, fence n
    participant R2 as run, fence n+2
    S->>D: rc:hive.start {fence n+2}
    D->>R1: fenced: its model calls, prompts and approval answers refused
    D->>R1: stop (superseded)
    R1-->>D: let go: its live entry and hosted record, at fence n only
    D->>S: rc:hive.state ended at fence n (not the record's fence: ignored)
    D->>R2: launched, --resume on the history R1 left
    D->>S: rc:hive.state idle at fence n+2, then rc:hive.start_ack
```

```mermaid
sequenceDiagram
    participant A as old primary, fence n
    participant S as server
    Note over S: the record: device B, fence n+1
    A->>S: a report about the session: a state, a turn, an approval, its manifest, a late start answer
    S->>A: rc:hive.demote {session, fence n+1, not_location}
    Note over A: the floor rises to n+1 · the run is fenced and stopped · hosted.json forgets it
    A->>S: rc:hive.state ended at fence n (ignored)
```

| Rule | Why | Where |
|---|---|---|
| A start at an older fence than the run here is refused, and so is one at the fence of a run a newer fence took | a stale frame: the server's word only grows | [`supervisor.rs:1553`](../agents/roomlerd/src/hive/supervisor.rs#L1553) |
| A start at a newer fence fences the run here out and stops it before any other gate, and launches only once that run's task has let go: at most 15 s, then it is refused | two harnesses never run on one history (§3d); whatever the gates say, the server has moved past that run | [`supervisor.rs:2392`](../agents/roomlerd/src/hive/supervisor.rs#L2392), [`supervisor.rs:159`](../agents/roomlerd/src/hive/supervisor.rs#L159), [`supervisor.rs:3500`](../agents/roomlerd/src/hive/supervisor.rs#L3500) |
| A fenced run's model calls, prompts and approval answers are refused from that moment, before its harness has gone | the sidecar's gate is the run's fence ([`sidecar.rs:143`](../agents/roomlerd/src/hive/sidecar.rs#L143)): a run being stopped must not spend | [`supervisor.rs:2366`](../agents/roomlerd/src/hive/supervisor.rs#L2366), [`supervisor.rs:2059`](../agents/roomlerd/src/hive/supervisor.rs#L2059), [`supervisor.rs:2102`](../agents/roomlerd/src/hive/supervisor.rs#L2102), [`supervisor.rs:3068`](../agents/roomlerd/src/hive/supervisor.rs#L3068) |
| A run's end removes its live entry and its hosted record only at its own fence, and a run writes its hosted record only at its own fence | an old run's end would take the new run's entry, and with it its model access (§3d ⚠️) | [`supervisor.rs:3264`](../agents/roomlerd/src/hive/supervisor.rs#L3264), [`supervisor.rs:2851`](../agents/roomlerd/src/hive/supervisor.rs#L2851), [`supervisor.rs:2863`](../agents/roomlerd/src/hive/supervisor.rs#L2863), [`hosted.rs:207`](../agents/roomlerd/src/hive/hosted.rs#L207), [`hosted.rs:235`](../agents/roomlerd/src/hive/hosted.rs#L235) |
| `rc:hive.demote {session, fence}` raises the store's floor to that fence first, then fences out and stops a run at an older fence and forgets its hosted record. A run at that fence or a newer one is left alone | the run's late writes are refused on this device as on every member; no restart resumes it; a session that came back is not the demote's | [`supervisor.rs:2329`](../agents/roomlerd/src/hive/supervisor.rs#L2329), [`supervisor.rs:1215`](../agents/roomlerd/src/hive/supervisor.rs#L1215) |
| A resume reads the hosted record again under the start lock | a demote or a stop that came while it waited for the lock wins | [`supervisor.rs:2735`](../agents/roomlerd/src/hive/supervisor.rs#L2735) |
| A stop for a newer fence than the run here ends that run, and reports the newer fence `ended` | a start at that fence may be waiting for this very run to let go; its launch is answered with a stop when it reports in | [`supervisor.rs:2161`](../agents/roomlerd/src/hive/supervisor.rs#L2161) |
| The server answers a report from a device that is not the session's location with `rc:hive.demote` at the record's fence: a state, a turn, an approval, a manifest, a late answer to its start. Another organization's device that names the session is told nothing | an old primary hears that it no longer holds the session from whichever report it sends first, a reconnect's replay and manifest included | [`agent_socket.rs:859`](../crates/modules/hive/src/agent_socket.rs#L859), [`agent_socket.rs:760`](../crates/modules/hive/src/agent_socket.rs#L760), [`agent_socket.rs:597`](../crates/modules/hive/src/agent_socket.rs#L597), [`agent_socket.rs:624`](../crates/modules/hive/src/agent_socket.rs#L624), [`agent_socket.rs:834`](../crates/modules/hive/src/agent_socket.rs#L834) |

⚠️ **The demote carries the record's fence, not the run's.** The device ends what it
runs below that fence. A run at it or above is the session come back to this device.

⚠️ **Only a promotion moves a session.** A record's location changes only with the
promotion (P2e-3), which needs both ends to advertise `hive-promote`, as every build does
from P2e-5c. A start at a newer fence reaches a device from a promotion, or from a lease
moved back.

⚠️ **The turn count across a fence is P2e-5's.** `rc:hive.move` carries the record's
last turn. Until then a run that superseded another counts its turns from 0, and the
server ignores a stub older than its newest.

⚠️ **A member that follows an old primary** hears that the session is over once the
primary is demoted, since its source then runs nothing. The member joins at the new
fence (P2e-3) take it to the new primary.

⚠️ **An older daemon drops the frame** as an `rc:` frame it cannot read, at warn. A
server sends it only after a move, which needs a device that promotes (P2e-3).

### P2e-2: the lease word

A primary that was demoted while it could not hear would run the session again on its own
word: its resume launches what it hosted before the server has said anything, and a
reconnect clears its offline clock at once, so its model calls go again. P2e-2 holds a
leased session to the server's word on each connection. The device runs it, and lets it
call the model, only once the server has said on that connection that the device still
holds it.

```mermaid
sequenceDiagram
    participant D as device
    participant S as server
    Note over D: a leased session, running, or hosted before a restart
    D->>S: a new connection, then rc:hive.manifest {session, fence}
    Note over D: its model calls wait · a hosted one is held back
    alt the record holds it here at that fence
        S->>D: rc:hive.lease {session, fence}, on this connection
        Note over D: the calls go · the held-back session launches
    else the record holds it elsewhere
        S->>D: rc:hive.demote (P2e-1)
    else the record says it is over
        S->>D: rc:hive.stop
    end
```

| Rule | Why | Where |
|---|---|---|
| A session is leased when its record names a member besides its primary, on a device that advertises `hive-lease`. Its start says `lease`, and the start is the word on its own connection | a member is what a promotion moves a session to: a session alone keeps P1's behaviour, and so does a build before P2e-2 | [`routes.rs:520`](../crates/modules/hive/src/routes.rs#L520), [`agent_socket.rs:1066`](../crates/modules/hive/src/agent_socket.rs#L1066), [`supervisor.rs:1093`](../agents/roomlerd/src/hive/supervisor.rs#L1093) |
| Every manifest is answered from the record, session by session, in one read: the word for one held here at that fence and leased, a demote for one held elsewhere, a stop for one that is over | the device hears the word on every connection; a held-back session would otherwise wait for a word that never comes | [`agent_socket.rs:810`](../crates/modules/hive/src/agent_socket.rs#L810) |
| The word goes on the connection whose manifest it answers and on no other. The report loop holds that connection weakly, and the word is sent on its own sender | a word counts only on its own connection: should a newer connection take the slot meanwhile, the word does not move to it | [`hub.rs:1971`](../crates/modules/fleet/src/hub.rs#L1971), [`agent_socket.rs:394`](../crates/modules/hive/src/agent_socket.rs#L394), [`lease.rs:30`](../crates/modules/hive/src/lease.rs#L30) |
| A session that gains its first member while it runs is leased from then on: the word goes at once, beside its join | its device then holds it to the word from its next connection, not from the one after | [`placement.rs:561`](../crates/modules/hive/src/placement.rs#L561) |
| The device keeps a word only when it came on the latest connection, and only for a session it runs or hosts. A newer fence's word voids an older one's, and the first word for a running session leases it, in `hosted.json` too | a word that comes late on an older connection must never take the latest one's place, and a server cannot fill the device with words | [`supervisor.rs:2484`](../agents/roomlerd/src/hive/supervisor.rs#L2484), [`supervisor.rs:2415`](../agents/roomlerd/src/hive/supervisor.rs#L2415) |
| A leased run's model call waits for the word on the latest connection, at most 30 s, then is refused in the provider's "overloaded" shape, which Claude Code retries | after a reconnect, the server may have moved the session while the device could not hear | [`sidecar.rs:175`](../agents/roomlerd/src/hive/sidecar.rs#L175), [`supervisor.rs:164`](../agents/roomlerd/src/hive/supervisor.rs#L164) |
| After a restart, a leased hosted session is held back until the word, and listed in the manifest meanwhile. The word or a start at its fence launches it; a stop or a demote ends it | the resume launched before the server had said anything | [`supervisor.rs:2750`](../agents/roomlerd/src/hive/supervisor.rs#L2750), [`supervisor.rs:2663`](../agents/roomlerd/src/hive/supervisor.rs#L2663), [`supervisor.rs:2199`](../agents/roomlerd/src/hive/supervisor.rs#L2199) |

⚠️ **Offline, the word still counts.** While the device is offline, the latest connection is
the one that closed, so its word stands and `offline_grace` decides, as for every session
(P0e). Only a new connection voids the word.

⚠️ **Not the replicaset's switch.** The word is sent for a session whose record names a
member, with `hive.replicaset` on or off, so a session placed while the switch was on is not
stranded once it is off. A new start asks for the lease only with the switch on, as it says
`replicated`.

⚠️ **A server rolled back below P2e-2 sends no word.** A leased session then waits on its
device; the owner's stop still ends it.

⚠️ **A build before P2e-2 runs what it hosts on its own word.** The promotion (P2e-3) moves
a session only between two connections that advertise `hive-lease`, and `hive-promote`.

### P2e-3: the server's promotion

The owner moves a session to one of its members. P2e-3 builds the server's half: the route,
its gates, every step of the move and its failures, and the lease CAS. The devices' halves
follow: the drain is P2e-4 (below), and the prepare and the move are P2e-5. Until a build advertises
`hive-promote` the route refuses every promotion (`device_unsupported`), so the server half
ships inert. The replicaset's switch, `hive.replicaset`, takes the route away altogether.

```mermaid
sequenceDiagram
    participant O as owner (browser)
    participant S as server
    participant A as primary, fence n
    participant B as member
    participant M as every other member
    O->>S: POST …/session/{sid}/promote {device_id, folder?}
    Note over S: the gates, then the promotion begins on the record (CAS)
    S->>B: rc:hive.prepare {fence n+1, folder, the owner}
    B-->>S: rc:hive.prepare_ack (ready, or a start's refusal word)
    S-->>O: promoting · refused · pending (no answer in 10 s)
    S->>A: rc:hive.drain {fence n}
    Note over A: no word answers its manifest meanwhile
    A-->>S: rc:hive.drained {seq, hash}
    B->>S: rc:hive.replica.tip, until it is {seq, hash}
    Note over S: the lease CAS: fence n+1, location B, roles, where fence n ended
    S->>A: rc:hive.demote {fence n+1, promoted} · its join as a member
    S->>M: rc:hive.replica.join {fence n+1}
    S->>B: rc:hive.replica.join {primary} · rc:hive.move · rc:hive.start {resume, lease}
    B-->>S: rc:hive.start_ack
    Note over S: done: the room says who moved it where
```

```mermaid
stateDiagram-v2
    [*] --> preparing: the owner promotes
    preparing --> draining: the member is ready
    draining --> syncing: the primary drained, at its tip
    syncing --> moving: the member's own tip is the drained one (the lease CAS)
    moving --> [*]: the member runs it
    moving --> [*]: the member refused the start, so the lease moves back to fence n+2
    preparing --> [*]: refused, or no answer in time
    draining --> [*]: not drained, or not in time, so the primary gets its word back
    syncing --> [*]: diverged, or not in time, so the primary gets its word back
```

| Rule | Why | Where |
|---|---|---|
| The promotion lives on the record (`agent_sessions.promotion`), and every step is a compare-and-set on the state it leaves. A device's frame takes each step, on whichever pod reads it | a frame that arrives twice, late, or after the promotion ended matches nothing | [`dao.rs:428`](../crates/modules/hive/src/dao.rs#L428), [`dao.rs:488`](../crates/modules/hive/src/dao.rs#L488), [`dao.rs:580`](../crates/modules/hive/src/dao.rs#L580) |
| The owner promotes, holding `HIVE_RUN`, a session that is running (launched, not being stopped, not adopted), to a member that took its copy (`joined`). Both ends must be online and advertise `hive`, `hive-lease` and `hive-promote`. The start's per-(user, device) ceiling is spent last | a promotion starts the session on a device as one of its accounts; a build before P2e-2 runs what it hosts on its own word | [`promote.rs:413`](../crates/modules/hive/src/promote.rs#L413), [`hub.rs:2013`](../crates/modules/fleet/src/hub.rs#L2013) |
| The member is asked first, with the start's own fields, and the caller hears its answer, in the start's words, within the start's bound | a refusal leaves the session where it was, and the person hears why (FR-83) | [`promote.rs:619`](../crates/modules/hive/src/promote.rs#L619), [`promote.rs:1133`](../crates/modules/hive/src/promote.rs#L1133) |
| While the primary is drained for the move, its manifest gets no lease word | the word lifts the drain: the lease is in motion | [`promote.rs:1947`](../crates/modules/hive/src/promote.rs#L1947), [`agent_socket.rs:845`](../crates/modules/hive/src/agent_socket.rs#L845) |
| The lease moves only once the member's OWN tip is the drained one: behind it is waited for, past it or another event at its `seq` fails the move | the primary's tip says nothing of the member's copy; a copy that is not the primary's is never promoted | [`promote.rs:1615`](../crates/modules/hive/src/promote.rs#L1615), [`promote.rs:1572`](../crates/modules/hive/src/promote.rs#L1572) |
| The lease CAS moves the fence to n+1 and the location to the member, and sets the roles in the same write: the member is the primary, owed its join; the old primary is a member if placement's rules let it hold a copy, else `leaving`; every other member is owed its join at the new fence. Array filters, never a rewritten member list | members report tips every few seconds, and a list read before the CAS would undo them | [`dao.rs:613`](../crates/modules/hive/src/dao.rs#L613), [`placement.rs:175`](../crates/modules/hive/src/placement.rs#L175) |
| At the move, in this order: the old run's open approvals withdrawn, every view and stream of the session ended, the old primary demoted, every member joined at the new fence, then the member's own join, the record's `rc:hive.move` and the start, as a resume | nothing the old primary does counts from the move on; its harness stops before the next one starts | [`promote.rs:1649`](../crates/modules/hive/src/promote.rs#L1649), [`promote.rs:1786`](../crates/modules/hive/src/promote.rs#L1786) |
| The member's acceptance, or a run state it reports at the new fence, ends the promotion, and the room says who moved it where. A moved start posts no "Started" note | the member's own word that it runs it; one note per move | [`promote.rs:1838`](../crates/modules/hive/src/promote.rs#L1838), [`agent_socket.rs:471`](../crates/modules/hive/src/agent_socket.rs#L471) |
| A moved start the member refuses moves the lease back, to fence n+2, and the session starts again where it ran; it is not refused | a fence handed out once is never handed out again | [`promote.rs:1881`](../crates/modules/hive/src/promote.rs#L1881), [`dao.rs:710`](../crates/modules/hive/src/dao.rs#L710), [`agent_socket.rs:431`](../crates/modules/hive/src/agent_socket.rs#L431) |
| A failure before the move ends the promotion in the state its caller read, gives the primary its lease word back once it was asked to drain, writes the audit and tells the room why. A session stopped meanwhile gets neither the word nor a note | the word lifts the drain; a stopped session's end has its own note | [`promote.rs:1001`](../crates/modules/hive/src/promote.rs#L1001) |
| One timer per promotion fails a step that runs past its bound, and a promotion past its deadline counts as failed anywhere | a promotion whose pod died strands nothing: the next promotion replaces it, and the primary's next manifest ends it and gets the word | [`promote.rs:1095`](../crates/modules/hive/src/promote.rs#L1095), [`settings.rs:87`](../crates/config/src/settings.rs#L87) |
| A start past the first fence that is re-sent on connect, however late (P2e-6b), is a resume, preceded by the member's join as the primary and the record's move | a member that was away at the move hears what it would have heard | [`agent_socket.rs:1041`](../crates/modules/hive/src/agent_socket.rs#L1041), [`promote.rs:1743`](../crates/modules/hive/src/promote.rs#L1743) |
| The record keeps where each fence a promotion superseded ended (`fences`, the drained tip) | a member behind the drained tip at the move, or one that joins later, still needs that fence's events below a floor that rose past it (spec §3b, "Open for P2e"); the move is the one moment the server holds it | [`model.rs:185`](../crates/modules/hive/src/model.rs#L185) |

| The route's gates, in order | Answer |
|---|---|
| the caller is a member of the org, the org is served | 403 `not_a_member` · 404 |
| `hive.replicaset` is on | 404 |
| the session is the caller's own | 404, the answer a bogus id gets |
| the device is live in the org | 404 |
| the org is not archived · `HIVE_RUN` | `org_archived` · `no_permission` |
| the session is running (launched, not stopping, not adopted) · no promotion in flight | `session_not_running` · `promotion_in_progress` |
| the device is not where it runs · it is a joined member | `already_primary` · `not_a_member` |
| the device, then the primary, online here and advertising `hive`, `hive-lease`, `hive-promote` | `device_offline` · `device_unsupported` · `primary_offline` · `primary_unsupported` |
| a primary not online here: not heard from for 230 s, its last hello advertising `hive-lease`, and the device the member that holds the most (P2e-6a, below) | `primary_offline` · `primary_unsupported` · `not_freshest` |
| a teleport's (P2f-1b, below): the device is no joined member, both ends advertise `hive-teleport`, and the primary is online here | `already_a_member` · `device_unsupported` · `primary_unsupported` · `primary_offline` |
| the start's per-(user, device) ceiling | `rate_limited` |

| Bound | Default | Past it |
|---|---|---|
| `hive.promote_prepare_secs` | 30 s | failed `prepare_timeout`, nothing moved |
| `hive.promote_drain_secs` | 75 s: the device's turn cut, 60 s ([`hive.rs:900`](../crates/remote_control/src/hive.rs#L900)), and a margin | failed `drain_timeout`, the word given back |
| `hive.promote_sync_secs` | 120 s | failed `sync_timeout`, the word given back |
| `hive.promote_join_secs` | 600 s, a teleport's only (P2f-1b, below): under the target's 30 min admission of its join | failed `join_timeout`, nothing moved, the target let go |
| a moved start unanswered | none: re-sent on the member's every connection, however late (P2e-6b) | nothing moves back on its own; once the member is unreachable for 230 s, its owner may move the session on |

⚠️ **The old primary is demoted at the move, not after the member's answer.** §3b's
diagram demotes it once the member accepts. Demoted at the CAS, its harness stops before
the member's starts, and nothing it does at the old fence counts from that moment on (AC4).
A move back starts it again at fence n+2.

⚠️ **A member silent after the move holds the session until it answers.** Nothing moves the
lease back on its own: the member may have launched it, and a second executor is what AC4
forbids. The start is re-sent on its every connection, however late. Once the member has
been unreachable for 230 s, its owner can move the session on (P2e-6b, below).

⚠️ **A member behind the drained tip at the move needs P2e-5.** Its join at the new fence
raises its floor, and today's store then refuses the rest of the old fence's events. The
record keeps where that fence ended (`fences`). P2e-5's joins carry it, and its store
accepts an older fence's events up to there. No promotion happens before then: nothing
advertises `hive-promote`.

⚠️ **Pod-local, like a start.** Both devices' sockets must be on the pod that serves the
route (tenant affinity puts them there), and the timer is that pod's. The steps are not
pod-local: a frame read on any pod takes its step.

⚠️ **Not under test:** a timer firing at the very moment a step is taken (`fail` ends the
promotion only in the state its caller read, so the step wins), and the `rate_limited`,
`org_archived`, `no_permission` and `primary_unsupported` gates.

### P2e-4: the primary's drain

Before the lease moves, the session must go quiet on its primary, at a tip the member's
copy can match. P2e-4 builds the device's half of `rc:hive.drain`. The run takes no new
prompt, its running turn ends or is cut, its open approvals are withdrawn and a last
checkpoint is taken. Then `rc:hive.drained {seq, hash}` says where the store's copy ends.
The run stays up, idle, until the server's word at its fence lifts the drain (the lease
stayed) or a demote ends the run (it moved). The server sends a drain only to a connection
that advertises `hive-promote`, as every build does from P2e-5c.

```mermaid
sequenceDiagram
    participant S as server
    participant D as primary, its dispatch
    participant T as the session's task
    participant H as Claude Code
    S->>D: rc:hive.drain {session, fence n}
    Note over D: taken up before the next frame is read · prompts refused from now on
    D->>T: the drain
    Note over T: the open approvals withdrawn · queued prompts held
    opt a turn runs
        Note over T: its result waited for, at most 60 s
        T->>H: the interrupt, a control_request on stdin
        H-->>T: control_response · the interrupted marker · result
        Note over T: the turn reported interrupted · no result 10 s later: not drained
    end
    Note over T: a replicated session's last checkpoint, then the store's tip
    T-->>D: drained at seq, hash
    D->>S: rc:hive.drained {seq, hash}, on the connection the drain came on
    alt the promotion failed before the move
        S->>D: rc:hive.lease {fence n}
        Note over T: the drain lifted · the held prompts begin
    else the lease moved
        S->>D: rc:hive.demote {fence n+1}
        Note over T: the run ends (P2e-1)
    end
```

```mermaid
stateDiagram-v2
    [*] --> waiting: rc:hive.drain, a turn running
    [*] --> checkpoint: rc:hive.drain, idle
    waiting --> cut: 60 s and the turn still runs
    waiting --> checkpoint: the turn ended
    cut --> checkpoint: the cut turn ended
    cut --> [*]: no result 10 s later, so refused and lifted
    checkpoint --> drained: kept, or not replicated
    checkpoint --> [*]: not taken, so refused and lifted
    drained --> [*]: the word lifts it, or the demote ends the run
    waiting --> [*]: the word, right behind the drain
```

| Rule | Why | Where |
|---|---|---|
| The drain is taken up as its frame is read, before the next one: from that moment the run refuses prompts, and the session's task reads the drain before anything sent after it | a lease word right behind the drain must reach the session after it, or it would lift nothing and the drain would hold the session for ever | [`signaling.rs:4340`](../agents/roomlerd/src/signaling.rs#L4340), [`supervisor.rs:1161`](../agents/roomlerd/src/hive/supervisor.rs#L1161), [`supervisor.rs:2227`](../agents/roomlerd/src/hive/supervisor.rs#L2227) |
| Only the primary enrollment's drain counts. One for a run this device does not have at that fence, or for one a newer fence took, is refused `not_running` and moves nothing | like every Hive frame: a secondary org's admin has no say over the owner's sessions, and a stale frame moves nothing | [`supervisor.rs:1161`](../agents/roomlerd/src/hive/supervisor.rs#L1161), [`supervisor.rs:2227`](../agents/roomlerd/src/hive/supervisor.rs#L2227) |
| No prompt begins while the session drains: a new one is refused in words, and one admitted before the drain waits in the queue | the session must go quiet at a tip | [`supervisor.rs:2107`](../agents/roomlerd/src/hive/supervisor.rs#L2107), [`supervisor.rs:4475`](../agents/roomlerd/src/hive/supervisor.rs#L4475), [`supervisor.rs:4484`](../agents/roomlerd/src/hive/supervisor.rs#L4484) |
| The open approvals are withdrawn at once, and one that opens while the session drains is withdrawn as it opens | a turn waiting on a person could not end; the harness hears the approval withdrawn, and goes on | [`supervisor.rs:4279`](../agents/roomlerd/src/hive/supervisor.rs#L4279), [`supervisor.rs:4415`](../agents/roomlerd/src/hive/supervisor.rs#L4415), [`toolbelt.rs:182`](../agents/roomlerd/src/hive/toolbelt.rs#L182) |
| A running turn is waited for, at most 60 s ([`hive.rs:904`](../crates/remote_control/src/hive.rs#L904)), then cut with Claude Code's own interrupt on its stream-json input, the transcript saying why first | the server waits 75 s for the answer; the interrupt ends the turn as the SDK's own does, and the process takes the next prompt | [`supervisor.rs:4673`](../agents/roomlerd/src/hive/supervisor.rs#L4673), [`supervisor.rs:4326`](../agents/roomlerd/src/hive/supervisor.rs#L4326), [`supervisor.rs:4929`](../agents/roomlerd/src/hive/supervisor.rs#L4929) |
| A cut turn is reported `interrupted`, and Claude Code's `[Request interrupted by user]` that follows the cut is not recorded | no person interrupted it, and the note before it says what did. An adopted session's marker is a person's, and is kept | [`supervisor.rs:4588`](../agents/roomlerd/src/hive/supervisor.rs#L4588), [`supervisor.rs:4548`](../agents/roomlerd/src/hive/supervisor.rs#L4548) |
| A cut turn that has not ended 10 s later is not drained: the drain is refused and lifted, and the turn runs on | the lease must not move while the old primary may still be calling the model | [`supervisor.rs:4673`](../agents/roomlerd/src/hive/supervisor.rs#L4673) |
| A replicated session's drain ends at a checkpoint: one of its own when it found the session idle, the last turn's when it waited for that turn, never one more. A checkpoint not taken is not drained | the member's copy must hold the files as the session left them; why it was not taken stays in the transcript, on the device | [`supervisor.rs:5096`](../agents/roomlerd/src/hive/supervisor.rs#L5096), [`supervisor.rs:4872`](../agents/roomlerd/src/hive/supervisor.rs#L4872), [`supervisor.rs:5001`](../agents/roomlerd/src/hive/supervisor.rs#L5001) |
| The answer is the store's tip, read as a member reports its own, and nothing is written to the session's events after it | the lease moves only once the member's own tip is that one (P2e-3), and one more event would fail the move as diverged | [`supervisor.rs:5096`](../agents/roomlerd/src/hive/supervisor.rs#L5096), [`replica.rs:201`](../agents/roomlerd/src/hive/supervisor/replica.rs#L201) |
| A drain asked again waits for the one under way, or is answered with the tip it gave | a frame that arrives twice drains once | [`supervisor.rs:4279`](../agents/roomlerd/src/hive/supervisor.rs#L4279) |
| The server's word at the run's fence lifts the drain: one still under way is refused (`the lease stayed on this device`), and the held prompts begin | the server gives the word back when a promotion fails before the move | [`supervisor.rs:2448`](../agents/roomlerd/src/hive/supervisor.rs#L2448), [`supervisor.rs:4627`](../agents/roomlerd/src/hive/supervisor.rs#L4627), [`supervisor.rs:4306`](../agents/roomlerd/src/hive/supervisor.rs#L4306) |
| A drain not answered within its bound (120 s: the cut, its grace and 50 s for the checkpoint) is refused and lifted | the device never stays drained on a drain it could not finish | [`supervisor.rs:2261`](../agents/roomlerd/src/hive/supervisor.rs#L2261), [`supervisor.rs:171`](../agents/roomlerd/src/hive/supervisor.rs#L171) |

| Bound | Default | Past it |
|---|---|---|
| the running turn | 60 s, `promote_limits::DRAIN_TURN_CUT_SECS` | the turn is interrupted |
| the cut turn's grace | 10 s | not drained; the turn runs on |
| the last checkpoint | 50 s more: 120 s in all | not drained, "the drain did not finish in time" |
| the server's own | 75 s, `hive.promote_drain_secs` ([`settings.rs:93`](../crates/config/src/settings.rs#L93)) | the promotion fails `drain_timeout`, and the primary gets its word back |

⚠️ **A drain that answered is lifted only by the word and ended only by a demote.** No
timer on the device lifts it. Once the device has said where its copy ends, the lease may
move at any moment, and a run that took prompts again would write past the tip the member
was held to. If the connection drops, the run stays drained until the next connection's
manifest is answered. By then the promotion has either moved the lease or failed, at
75 s at the latest.

⚠️ **The interrupt was measured, not assumed.** Measured on Claude Code 2.1.280, headless
with the hive's own flags, against a mock Messages API on loopback that held the turn's model
call:
- The interrupt aborted the call at once.
- It was answered with `control_response {subtype: success}`, then
  `[Request interrupted by user]` and a `result` (`error_during_execution`,
  `terminal_reason: aborted_streaming`).
- The same process answered the next prompt.

Not yet on a fleet device: the field run comes with the move, P2e-5.

⚠️ **Not measured: a turn cut during a tool's run.** The experiment held a model call.
Claude Code marks a turn cut during a tool with `[Request interrupted by user for tool use]`,
which the marker the drain drops covers. Whatever the tool left half done is in the
checkpoint as the files are.

⚠️ **A drained session shows `idle`.** There is no run state for "drained". The server knows
the promotion's state, and a prompt refused meanwhile says why.

### P2e-5a: the member's prepare and the moved start

The member's half of a promotion, built ahead of anything that would ask for it. P2e-5a
answers `rc:hive.prepare` with a start's gates, and keeps `rc:hive.move` for the start right
behind it. A moved session is then taken over from the copy in the device's own store: the
conversation's history and the folder are put in place as the account, the folder is held
to the copy's tree, and Claude Code resumes. P2e-5b, below, carries where the old fence
ended on the joins, and P2e-5c turns promotion on.

```mermaid
sequenceDiagram
    participant S as server
    participant B as the member it moves to
    participant St as B's store
    participant M as hive-materialize, as the account
    participant H as Claude Code
    S->>B: rc:hive.prepare {session, fence n+1, folder, the owner}
    Note over B: a start's gates, as configured now · a checkpoint in the store · nothing stopped or launched
    B-->>S: rc:hive.prepare_ack, ready as an account, or a start's refusal word
    Note over S: the drain, the sync, the lease CAS (P2e-3, P2e-4)
    S->>B: rc:hive.replica.join {primary, n+1} · rc:hive.move {turns, from} · rc:hive.start {n+1}
    Note over B: the move kept for that fence
    alt it ran on another device
        B->>St: the session's checkpoints, oldest first
        B->>M: the last checkpoint's files and packs, from the store's blobs
        M-->>B: what it put in place, the folder at its tree
        Note over B: the tree is the copy's · a session with turns has its history
    else a move back: it ran here
        Note over B: its state is here, so nothing is put in place
    end
    B->>H: --resume with a history, --session-id without one
    B-->>S: rc:hive.start_ack, then idle, counting on from the turns
```

| Rule | Why | Where |
|---|---|---|
| `rc:hive.prepare` runs every gate a start passes, as the device is configured now, and none of a start's side effects: nothing is stopped, nothing launched, no word kept. It answers in the start's own refusal words, on the connection it came on | a member that would refuse the start refuses before anything moves (spec §3b) | [`supervisor.rs:1889`](../agents/roomlerd/src/hive/supervisor.rs#L1889), [`supervisor.rs:583`](../agents/roomlerd/src/hive/supervisor.rs#L583), [`signaling.rs:4354`](../agents/roomlerd/src/signaling.rs#L4354) |
| The prepare also asks for the copy: the session's checkpoints in this device's store. A teleport's prepare does not (P2f-1a): its copy comes after it | a moved start is put in place from them | [`supervisor.rs:1991`](../agents/roomlerd/src/hive/supervisor.rs#L1991) |
| A run of the session here at an older fence does not count against `hive_max_sessions`, and a run at the fence asked, or a newer one, is refused unless it is that very run | the start supersedes the older run (P2e-1); a newer one is the session come back | [`supervisor.rs:1974`](../agents/roomlerd/src/hive/supervisor.rs#L1974), [`supervisor.rs:1942`](../agents/roomlerd/src/hive/supervisor.rs#L1942) |
| `rc:hive.move` is kept by (session, fence) for the start right behind it, 10 min and 64 at once, and dropped only once that start has launched | a start refused before its launch finds it again when re-sent | [`supervisor.rs:1333`](../agents/roomlerd/src/hive/supervisor.rs#L1333), [`supervisor.rs:1353`](../agents/roomlerd/src/hive/supervisor.rs#L1353), [`supervisor.rs:1374`](../agents/roomlerd/src/hive/supervisor.rs#L1374) |
| A moved start counts on from the move's `turns`, in what the device hosts too, and says nothing of a restart | the server ignores a stub older than its newest, and a move is no restart | [`supervisor.rs:3455`](../agents/roomlerd/src/hive/supervisor.rs#L3455), [`supervisor.rs:3480`](../agents/roomlerd/src/hive/supervisor.rs#L3480) |
| From another device, the session's copy is put in place before the harness is launched: every checkpoint in the store, oldest first, `hive-materialize` as the account, and the folder held to the last checkpoint's tree | the daemon never writes a path in the account's tree; a history only grows, so its chunks span every checkpoint since it began | [`supervisor.rs:1741`](../agents/roomlerd/src/hive/supervisor.rs#L1741), [`supervisor.rs:1770`](../agents/roomlerd/src/hive/supervisor.rs#L1770), [`store.rs:804`](../crates/hive-node/src/store.rs#L804) |
| A move back, whose `from` is this device, puts nothing in place | the session comes back where its state is: the drain checkpointed it there, and nothing ran after | [`supervisor.rs:1385`](../agents/roomlerd/src/hive/supervisor.rs#L1385) |
| A session that has had turns must find its history, and the history decides between `--resume` and `--session-id` | without it Claude Code would start the conversation afresh, silently; a session moved before its first prompt has none | [`supervisor.rs:1755`](../agents/roomlerd/src/hive/supervisor.rs#L1755), [`supervisor.rs:1693`](../agents/roomlerd/src/hive/supervisor.rs#L1693) |
| A moved start that cannot take the session over is refused `launch_failed`, in fixed words; why goes to the device's log | what the materializer names can be the account's own files, which never reach the server | [`supervisor.rs:1686`](../agents/roomlerd/src/hive/supervisor.rs#L1686) |
| A folder that holds work of its own is refused: one that is not empty, not at its own `HEAD`, and not what this session last left there | a person's work is never overwritten (P2b-4b) | [`workspace.rs:695`](../agents/roomlerd/src/hive/workspace.rs#L695) |

| Bound | Value | Past it |
|---|---|---|
| the copy's chain | 20 000 checkpoints | the start is refused whole, never put in place cut short |
| the materialize | 300 s (`MATERIALIZE_TIMEOUT`) | the start is refused |
| a kept move | 10 min, 64 at once | the oldest goes; a start without its move runs as any start does |

⚠️ **The materialize runs under the start lock.** A device's other starts wait for it, for up
to its 300 s. That keeps it simple, and the moved session cannot be started twice meanwhile.

⚠️ **A refused moved start moves the lease back** (P2e-3): the session starts again on the
device it came from, at the fence after. A promotion never leaves a session on a device that
could not take it over.

⚠️ **Not under test:** a child that reports a tree other than the copy's. P2b-4b's `switch`
holds the folder to the tree itself, and the daemon checks it again. The move's dispatch over
a real connection is P2e-5c's test.

### P2e-5b: the old fence's end, on every join

A join raises a member's floor to its fence (P2c-3), and below the floor an event is
refused (P2a). That refuses the old primary's late writes, as it should. It also refused
the history itself: after a move, every event the session had is of a fence older than the
floor a join raises. A member behind the drained tip at the move, or one that joined later,
could never take that history, so after one promotion no new copy could be made.

P2e-5b closes it. The record keeps where each superseded fence ended (P2e-3's `fences`), and
every join carries them. The member's store takes such a fence's events up to its end. At
the end it takes only the event that fence ended with, and nothing after it.

```mermaid
sequenceDiagram
    participant S as server
    participant C as a member joined after the move
    participant St as C's store
    participant B as the new primary
    Note over S: the move kept where fence 1 ended, the drained tip {seq 5, hash}
    S->>C: rc:hive.replica.join {fence 2, fences [{fence 1, seq 5, hash}]}
    C->>St: the floor to 2, and fence 1's end, in one transaction
    C-->>S: rc:hive.replica.join_ack
    C->>B: the stream (P2d)
    B-->>C: fence 1's events, seq 1 to 5, then fence 2's
    Note over St: fence 1 is below the floor: taken up to seq 5, and at 5 only its end
    Note over St: a fence 1 event at seq 6 is the old primary's late write: stale_fence
```

| Rule | Why | Where |
|---|---|---|
| Every join carries the record's `fences`, oldest first, whoever it goes to: at a start, at placement, at a move and at its move back, and on every connection owed one. `push_join` takes the record, never its fields | no caller can send a join without them | [`replica.rs:128`](../crates/modules/hive/src/replica.rs#L128), [`:173`](../crates/modules/hive/src/replica.rs#L173) `fence_marks` |
| A mark no promotion could have written, a negative number, is left out, never sent wrapped | that fence's events are then refused below the floor, as every one was before P2e-5b | [`replica.rs:173`](../crates/modules/hive/src/replica.rs#L173) |
| The device reads the ends whole before its store is asked: at most 256, each older than the join's fence, in order, and each hash a hash. One it cannot read refuses the join `other`, and nothing of it is kept | an end is the one way an older fence's events get past the floor; ends taken in part would let some of them through | [`supervisor/replica.rs:165`](../agents/roomlerd/src/hive/supervisor/replica.rs#L165), [`:110`](../agents/roomlerd/src/hive/supervisor/replica.rs#L110); [`hive.rs:505`](../crates/remote_control/src/hive.rs#L505) |
| The ends are kept with the floor, in the join's own transaction. A later join that names none takes none away, and a purge drops them | no event finds the floor raised without its ends. A fence's end never changes once it is superseded, and a server from before P2e-5b names none | [`store.rs:275`](../crates/hive-node/src/store.rs#L275), [`:307`](../crates/hive-node/src/store.rs#L307), [`:1021`](../crates/hive-node/src/store.rs#L1021) |
| Below the floor, an event of a fence that has an end is taken while its `seq` is at most the end's, chained as any event is. At the end's `seq` it must be the event whose hash the end is (`not_fence_end`). Past the end, or of a fence with no end, it is refused `stale_fence` | the history up to the drained tip is the session's; an event after it at that fence is the old primary's late write | [`chain.rs:210`](../crates/hive-node/src/chain.rs#L210), [`store.rs:476`](../crates/hive-node/src/store.rs#L476) |
| An apply reads only the end of its own event's fence, by its key | an apply never reads them all | [`store.rs:353`](../crates/hive-node/src/store.rs#L353) |
| A member ends the stream `not_fence_end` for an event at an end that is not that end | the source holds a chain the record does not: the member waits, then asks again | [`supervisor/stream.rs:673`](../agents/roomlerd/src/hive/supervisor/stream.rs#L673) |
| A primary's join does not read them | it runs the session, and held all of its history before the move | [`supervisor/replica.rs:236`](../agents/roomlerd/src/hive/supervisor/replica.rs#L236) |
| The server refuses a promotion that could leave the session with more ends than a device takes in one join: `moved_too_often`. A move keeps one end, and the move back of a refused start keeps a second | past that, no member could ever join the session again | [`promote.rs:579`](../crates/modules/hive/src/promote.rs#L579), [`:459`](../crates/modules/hive/src/promote.rs#L459) |

| Bound | Value | Past it |
|---|---|---|
| the ends one join names | 256 (`MAX_FENCE_MARKS`) | the device refuses the join whole |
| the ends a session holds when a promotion begins | 254 | the promotion is refused `moved_too_often` |

The ends live in a new table, `fence_ends`, made only when missing, like P2a's
([`store.rs:125`](../crates/hive-node/src/store.rs#L125)). The schema version stays 1, so a
daemon from before P2e-5b still opens the store.

⚠️ **An end pins its fence's events only at its own `seq`.** Before that, an older fence's
event is taken on its chain link alone, as any event is. A copy that took a chain the record
does not hold stops at the end, `not_fence_end`. Every event of that tail is below the
floor, so the next stream's hello sets it aside, as it sets aside a tail a promotion cut
off (P2d-3).

⚠️ **A member from before P2e-5b reads no `fences`.** After a move its copy stands still
below its floor, and its streams end `stale_fence`. A later join that reaches it on a build
that reads them, the next move's, lets it catch up. P2e-5c's promotion asks `hive-promote`
only of the two devices it moves between, not of the session's other members.

A real member joining after a real move, and following one move back, is P2e-5c's test.

### P2e-5c: promotion, on

Every build that runs agent sessions advertises `hive-promote` from here on
([`caps.rs:1680`](../agents/roomlerd/src/encode/caps.rs#L1680)). It is a property of the
build, like `hive-lease`: a device still refuses a drain or a prepare its own gates do not
pass, in its own words, and a secondary organization's connection refuses both. A session's
owner can now move it to a member (P2e-3), and the route's gates are all that stand in the
way.

P2e-5c's test is the first promotion between real devices
([`hive_promote.rs`](../crates/tests/tests/hive_promote.rs)). The route starts it, and every
step after that is a device's own frame. A and B run sessions and hold copies, C only holds
copies, and each one is a process of its own, because a process holds one Hive supervisor.

```mermaid
sequenceDiagram
    participant O as the owner
    participant S as server
    participant A as A, the primary
    participant B as B, its member
    participant C as C, joined later
    O->>S: POST …/promote {device B, a folder}
    S->>B: rc:hive.prepare
    B-->>S: ready
    S->>A: rc:hive.drain
    A-->>S: rc:hive.drained {seq, hash}
    Note over B: its copy reaches the drained tip
    S->>A: rc:hive.demote, then its member join at fence 2
    S->>B: its join as the primary, the floor to 2 · rc:hive.move · rc:hive.start
    Note over B: the copy put in place as the account, then --resume
    B-->>S: accepted: the session runs on B, its conversation whole
    O->>S: PUT …/policy {min 3}
    S->>C: its join at fence 2, naming where fence 1 ended
    Note over C: the history below its floor, then fence 2's
    O->>S: POST …/promote {device A, a folder holding someone's work}
    S->>A: prepare, B drained, the lease to fence 3
    A-->>S: refused, launch_failed: the folder holds work that is not the session's
    S->>B: the lease back at fence 4: its join as the primary · move · start
    Note over B: a move back puts nothing in place
    Note over C: it followed every move: at B's tip, three fences' ends held
```

| What the first real promotion found | Now | Where |
|---|---|---|
| The target heard its join as the primary before its start, for a session it did not run yet, and refused it (`other`, "this device does not run the session"). After every real move its own row read refused until it next connected, and its floor never rose before it wrote | a primary's join for a session the device holds a copy of, ahead of its start, raises the copy's floor to the join's fence and is taken up; one for a session it holds nothing of is refused, as before | [`supervisor/replica.rs:389`](../agents/roomlerd/src/hive/supervisor/replica.rs#L389) |
| From the lease's move until its start launched, the target told every member it streamed to that the session was over (`live: false`), since it neither ran nor hosted it yet. A member that held its copy whole parked it while the session went on there | a device keeping a move for the session says the session is live | [`supervisor/stream.rs:1535`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1535), [`supervisor.rs:1365`](../agents/roomlerd/src/hive/supervisor.rs#L1365) |
| A member that asked for a stream as the session moved was granted one toward the old location. The server ends such a grant at the move, before its dial, and a member never hears of a grant it was not dialed for. Its newer join found it still asking, and only raised the fence, so it waited out the ask's whole timeout, 45 s | a newer join that overtakes an ask out asks again at once. The old ask's refusal names its fence and its timer its generation, so neither moves anything | [`supervisor/follow.rs:371`](../agents/roomlerd/src/hive/supervisor/follow.rs#L371) |
| A stop or a park was taken at its word even when a newer join had overtaken the ask or stream it answered | the session is asked for again from the top, at the new fence: that answer was about the fence the session left | [`supervisor/follow.rs:830`](../agents/roomlerd/src/hive/supervisor/follow.rs#L830) |
| The move ended every stream of the session, and the move back none. A stream the refusing member had been asked to serve told its member that the session ran nowhere | the move back ends every view and stream of the session, as the move does | [`promote.rs:1928`](../crates/modules/hive/src/promote.rs#L1928) |

⚠️ **CI green proved none of these.** P2e-3's integration tests drive fake devices that
answer whatever they are sent, and each device test answers one step on its own. Only two
real devices in one promotion put a device's real answers against the server's real order
of frames. The real test first failed on the target's own row, then on a member's parked
copy. Then it passed in 71 s with a 45 s wait in it; with every fix it passes in 26 s.

⚠️ **Not under test:**
- the real Claude Code. That is the field run (AC9).
- devices on separate machines: the test's streams ride `direct` on loopback.
- the `derp` rung under a promotion.

### P2e-6a: away from an unreachable primary

A primary can go dark in the middle of a session: a laptop closed, a network gone, a crash.
Until P2e-6a its session stayed there, because a promotion needs the primary's drain. Now
the owner can move it away once the primary is unreachable, with nothing asked of it.

The risk is two executors: the old primary still calling the model while the new one runs
the session. The server cannot ask the device, so it waits until the device's own clocks
have stopped its model calls:

| Clock | Where | Time |
|---|---|---|
| the device notices its connection is gone: no frame received, and it closes it | `WS_RX_DEADLINE` ([`signaling.rs:60`](../agents/roomlerd/src/signaling.rs#L60)) | 80 s |
| from that close, its sidecar keeps serving model calls ([`supervisor.rs:3148`](../agents/roomlerd/src/hive/supervisor.rs#L3148), `lost_connection`) | `OFFLINE_GRACE` ([`sidecar.rs:64`](../agents/roomlerd/src/hive/sidecar.rs#L64)) | 120 s |
| the server may have sent to it after it last recorded hearing from it: a heartbeat comes every 30 s, and a pod can die with no teardown | a margin | 30 s |
| all of it: `promote_limits::UNREACHABLE_SECS` ([`hive.rs:914`](../crates/remote_control/src/hive.rs#L914)) | | **230 s** |

"Unreachable" means two things at once: the device has no connection on this pod, and its
row's `last_seen_at` is 230 s old. Every hello stamps that field before the connection is
registered ([`socket.rs:99`](../crates/modules/fleet/src/socket.rs#L99)), every
heartbeat on any pod refreshes it ([`agent.rs:468`](../crates/services/src/dao/agent.rs#L468)),
and every teardown stamps it as it marks the device offline
([`agent.rs:437`](../crates/services/src/dao/agent.rs#L437)). A device with a live
connection anywhere is never unreachable.

```mermaid
sequenceDiagram
    participant O as the owner
    participant S as server
    participant A as A, the primary, dark
    participant B as B, the member that holds the most
    participant C as C, another member
    Note over A: no hello, no heartbeat · last_seen_at stamped at its teardown
    O->>S: POST …/promote {B}, under 230 s later
    S-->>O: refused primary_offline, the wait named
    O->>S: POST …/promote {C}, 230 s later
    S-->>O: refused not_freshest: B holds more
    O->>S: POST …/promote {B}
    S->>B: rc:hive.prepare {fence n+1}
    B-->>S: ready, tip {fence, seq, hash}: where its copy ends, read at the answer
    Note over S: the tip within the ends · the freshest still · syncing, drained = that tip · A asked about once more · the lease CAS
    S->>C: rc:hive.replica.join {n+1, fences: fence n ends at B's tip}
    S->>B: its join as the primary · rc:hive.move · rc:hive.start
    S-->>O: promoting, the session already moving
    Note over S: the room: A cannot be reached, so the session goes on from B's copy
    A->>S: back: hello, rc:hive.manifest {fence n}
    S->>A: rc:hive.demote {n+1}, never the word · its join as a member
    Note over A: what it wrote past B's tip is set aside when it follows B (P2d-3)
```

```mermaid
stateDiagram-v2
    [*] --> preparing: the owner promotes, the primary unreachable
    preparing --> syncing: the member is ready, its own tip named
    syncing --> moving: the primary still unreachable (the lease CAS)
    preparing --> draining: the primary heard from again first
    preparing --> [*]: no tip, or one past an end, or a member holds more
    syncing --> [*]: the primary came back meanwhile, so it gets its word back
    moving --> [*]: the member runs it
```

| Rule | Why | Where |
|---|---|---|
| A primary that is not online here may be moved away from only once its row's `last_seen_at` is `UNREACHABLE_SECS` old, and only if its last hello advertised `hive-lease`. Sooner it is `primary_offline`, the message naming the wait; without `hive-lease` it is `primary_unsupported` | by then it calls the model no more, by its own clocks. A build before P2e-2 would run the session again the moment it reconnects, before its demote lands | [`promote.rs:322`](../crates/modules/hive/src/promote.rs#L322) `unreachable_reach`, [`:507`](../crates/modules/hive/src/promote.rs#L507) |
| The member promoted to must hold at least as much of the session as every other joined member, as the server last heard. Else `not_freshest` | what only the dead primary held is lost anyway; a move to any other member loses more | [`promote.rs:336`](../crates/modules/hive/src/promote.rs#L336) `fresher_than` |
| How much a copy holds is a `seq` on the session's chain: a fence that ended counts up to its end and no further, and an empty copy, `seq` 0 at the fence it joined at, counts least | past an end is a tail a no-drain move cut off. `(fence, seq)` compared whole would rank an empty copy at a newer fence above a full one | [`promote.rs:356`](../crates/modules/hive/src/promote.rs#L356) `on_chain` |
| The member's ready names where its copy ends, `tip {fence, seq, hash}`, read from its store as it answers. A ready without one, from a build before P2e-6a, fails the move `no_copy_tip` | the lease moves to the copy the member holds, never to a report seconds old | [`supervisor.rs:1870`](../agents/roomlerd/src/hive/supervisor.rs#L1870), [`supervisor/replica.rs:483`](../agents/roomlerd/src/hive/supervisor/replica.rs#L483), [`signaling.rs:989`](../crates/remote_control/src/signaling.rs#L989) |
| That tip must lie on the record's chain: at or before where its fence ended, and at the end, the same event. Else `cut_off` | the next fence would begin on a tail the record cut off, and no member could follow it | [`promote.rs:367`](../crates/modules/hive/src/promote.rs#L367) `within_ends` |
| The promotion goes to `syncing` first, with `drained` set to the member's tip, and only then is the primary asked about again. Heard from meanwhile, the move fails `primary_back` and the primary gets its word back. Else the lease CAS moves the lease to that tip, and every join at the new fence says the old one ended there | a primary that reconnects after `syncing` gets no word on its manifest, and one that reconnected before it is seen by the second look. One of the two always catches it | [`promote.rs:1432`](../crates/modules/hive/src/promote.rs#L1432), [`:1400`](../crates/modules/hive/src/promote.rs#L1400), [`:1947`](../crates/modules/hive/src/promote.rs#L1947) |
| A primary heard from again before the member answers is drained, as any online primary is | it can answer now, and its drain loses nothing | [`promote.rs:1174`](../crates/modules/hive/src/promote.rs#L1174) |
| The caller hears `promoting` once the lease has moved, and the room says the session goes on from the member's copy, and that what the primary did after it is not in it | the owner learns what the move cost (design §12: the unconfirmed call is flagged) | [`room.rs:130`](../crates/modules/hive/src/room.rs#L130) |

| Bound | Value | Past it |
|---|---|---|
| a primary not heard from | 230 s, `UNREACHABLE_SECS` | it may be moved away from with no drain |

⚠️ **The 230 s is the device's own budget, held by a test.** A roomlerd test asserts
`UNREACHABLE_SECS == WS_RX_DEADLINE + OFFLINE_GRACE + 30 s`
([`sidecar/tests.rs:746`](../agents/roomlerd/src/hive/sidecar/tests.rs#L746)).
Lengthen either clock on the device and the test fails before the server moves a session
too soon.

⚠️ **No socket is closed to start a device's clock.** The design (spec §3b) closes the socket
of a primary that ignores a drain, to start that clock. As built, nothing is closed, because
it would buy nothing and cost a healthy device:
- a link that carries one way only is closed already. The device closes it when it hears no
  pong for 80 s (its receive deadline), and the server tears it down when it hears no frame
  for 90 s (its reaper). Either close starts the device's clock. While frames flow both
  ways, the device is reachable, and a promotion drains it;
- a drain may legitimately take the device 120 s while the server waits 75 s. Closing a
  slow, healthy primary's socket would tear its rc, tunnel and overlay planes for seconds,
  and could leave it relay-locked behind a strict corporate VPN, for nothing.

⚠️ **What only the dead primary held is lost.** That can be the turn it was in, and its last
checkpoint if no member had it yet. The member that holds the most is chosen so the loss is
smallest, and the room says so. When the old primary comes back it is demoted, joined as a
member, and its first stream sets aside what it wrote past the new fence's start (P2d-3).

⚠️ **A copy that holds a cut-off tail is never moved to, and it can hold the session up.** If
the new primary goes dark too, before the old one has followed it, the old one cannot be
moved to (`cut_off`), and a member behind the end yields to it (`not_freshest`). The session
then waits for a primary to come back rather than lose history. A target that set its own
tail aside at its moved start would lift this. Not built.

⚠️ **"Online here" is this pod's.** A device whose connection a pod roll moved to another pod
is not unreachable: its heartbeats there keep `last_seen_at` fresh, so the route refuses
`primary_offline`.

The real-device test ([`hive_promote.rs`](../crates/tests/tests/hive_promote.rs)) ends on this
move. B, the primary at fence 4, is stopped and waited out. A's own ready names where its copy
ends, the lease moves there with no drain, and A resumes the conversation with all four
prompts. C follows A, event for event.

⚠️ **Not under test:**
- the `primary_back` window itself, a reconnect between `syncing` and the second look: it is
  milliseconds wide, and the order is the guard. The second look's refusal did run, under a
  negative control that broke the first look: the move failed `primary_back`;
- a pod that died with no teardown: the margin covers it.

### P2e-6b: a target that never answered

A move hands the session to its member with a start (P2e-3). The member can go dark before
it answers: a network gone in the middle of the take-over, a crash. Until P2e-6b such a
session went wrong one of two ways:
- **Back late, the session was lost.** On a reconnect, a start older than 10 minutes was
  not sent again: the session was marked `lost`. That window is for a session's first
  start, whose caller has gone. It counted from the record's creation, and a moved
  session's record was made long before its move, so a member back from a short drop lost a
  session that had run for hours.
- **Gone, it held the session for good.** The route moves only a running session, and one
  waiting on its start is not running. Nothing could move it on.

Now a moved start is sent again however late its member comes back. And once that member
has been unreachable for 230 s, the owner can move the session on, as from any unreachable
primary (P2e-6a): the move that sent the start is replaced.

```mermaid
sequenceDiagram
    participant O as the owner
    participant S as server
    participant A as A, the old primary
    participant B as B, the member it moved to
    S->>B: its join as the primary · rc:hive.move {from A} · rc:hive.start {fence 2}
    Note over B: dark before it answers
    B->>S: back, however late
    S->>B: the same three again, from A
    Note over B: dark again
    O->>S: POST …/promote {A}, under 230 s later
    S-->>O: refused primary_offline
    O->>S: POST …/promote {A}, 230 s later
    S->>A: rc:hive.prepare {fence 3}
    A-->>S: ready, tip: where its copy ends
    Note over S: P2e-6a's move, the move to B replaced: no drain, fence 2 ends where it began
    S->>A: its join as the primary · rc:hive.move {from B} · rc:hive.start {fence 3}
    B->>S: back again
    S->>B: rc:hive.demote {fence 3}, and nothing to run
```

| Rule | Why | Where |
|---|---|---|
| A start past the first fence is sent again on its device's every connection, however late. The redelivery window, after which a start is `lost`, bounds a session's first start only | that start's caller has gone. A moved session's conversation lives on, and its record was made long before the move | [`agent_socket.rs:1014`](../crates/modules/hive/src/agent_socket.rs#L1014) |
| A session waiting on a moved start its device never answered counts as running for a promotion: past the first fence, starting, never accepted, and no run state reported. It is moved on only once that device is unreachable for 230 s; sooner, `primary_offline`. With the device online, it is `promotion_in_progress` while the move is under way, and `session_not_running` after | it runs nowhere yet. By 230 s that device makes no model call, and back, it is demoted before it runs anything | [`promote.rs:557`](../crates/modules/hive/src/promote.rs#L557) `unanswered_move`, [`:510`](../crates/modules/hive/src/promote.rs#L510) |
| That promotion replaces the move that sent the start, in flight or not, by a compare-and-set on the session still waiting on it | that move is over: its member never answered | [`dao.rs:428`](../crates/modules/hive/src/dao.rs#L428) `begin_promotion` |
| The record keeps where the start its device holds came from (`moved`), set at every move of the lease, and every start sent again says so | a device takes a session that comes from elsewhere over from its own copy, and one from itself back as it left it. The move that sent a start can be gone before the start is answered | [`model.rs:194`](../crates/modules/hive/src/model.rs#L194), [`dao.rs:627`](../crates/modules/hive/src/dao.rs#L627), [`agent_socket.rs:1036`](../crates/modules/hive/src/agent_socket.rs#L1036) |
| A move on from a device that never answered keeps where that device's start came from (`prior`), and a move back to it, should the new member refuse the start, restores it | a move back to a device that never ran the session must not tell it the session is its own: it has none of it in place, and it takes it over from its copy | [`model.rs:306`](../crates/modules/hive/src/model.rs#L306), [`dao.rs:718`](../crates/modules/hive/src/dao.rs#L718), [`promote.rs:1938`](../crates/modules/hive/src/promote.rs#L1938) |

⚠️ **Found while building it: the lost session.** P2e-3's reconnect counted the window from
the record's creation. It never showed: the tests made each session seconds before its
move. It is not live either, since no released agent advertises `hive-promote` yet.

⚠️ **A move back to the silent device is still a move back.** If the member the session
moved on to refuses its start, the lease returns to the device that never answered, which
may still be dark. The owner can move it on again, as before.

⚠️ **Nothing moves a session off a member that is online and silent.** It holds its
connection, so its clocks never stop it; the route waits for it.

⚠️ **Not under test:** real devices. The move itself is P2e-6a's, which the real-device test
covers.

### P2e-7: the resume note

A session moved to another device resumes there with Claude Code's own `--resume`. The model
gets every earlier turn, and Claude Code adds the new platform and folder (spec §3b's
probe). What the model did not know is that the session moved: who moved it and when, that
paths in its earlier turns now live elsewhere, and that its last tool call may never have
finished. P2e-7 tells it, once, as it resumes (design §6.6).

```mermaid
sequenceDiagram
    participant S as server
    participant D as the member's daemon
    participant St as its store
    participant H as Claude Code, as the account
    S->>D: rc:hive.move {by, at, from device, from folder} · rc:hive.start {fence n+1}
    D->>St: the last tool call, and whether a result follows it
    Note over D: the note composed · left in the runtime directory, the account's, 0600 · recorded as a note event
    D->>H: --resume, with --settings holding a SessionStart hook (matcher resume)
    H->>H: the hook, as the account: roomlerd hive-resume-note <file>
    Note over H: the note reaches the model as context · the history keeps it
```

A note, as a session moved from one machine to another receives it:

```text
This session was moved to this device by Alice at 2026-10-12 14:02 UTC.
It ran on mars, in /srv/work. It runs here now, in /home/alice/work.
Paths in earlier turns under /srv/work now live under /home/alice/work.
The last tool call before the move, `cargo test -p roomlerd --lib`, has no result: it may not have run, or not to its end.
```

| Rule | Why | Where |
|---|---|---|
| The note is left only for a start that brings the session from another device. Every other launch removes one an earlier launch left: a first start, a restart, a move back | the history keeps each note once, so a restart's resume adds nothing. A move back comes back to its own state and conversation | [`supervisor.rs:1702`](../agents/roomlerd/src/hive/supervisor.rs#L1702), [`:4011`](../agents/roomlerd/src/hive/supervisor.rs#L4011) `leave_note` |
| The note is the account's, `0600`, in the daemon-owned runtime directory, written as core memory is; on Windows the directory's DACL hands it to the user | it names folders and a command, and no other local account reads it | [`supervisor.rs:4011`](../agents/roomlerd/src/hive/supervisor.rs#L4011) |
| Every session's settings carry a `SessionStart` hook with the matcher `resume`. It runs this binary as the session's account (`roomlerd hive-resume-note <file>`), which prints the note when there is one and nothing otherwise | a hook that prints nothing adds nothing (the probe), so one hook serves every launch. The account runs it, as it runs the toolbelt's relay | [`launch.rs:376`](../crates/hive-node/src/launch.rs#L376), [`resume_note.rs:107`](../agents/roomlerd/src/hive/resume_note.rs#L107), [`:143`](../agents/roomlerd/src/hive/resume_note.rs#L143), [`main.rs:1119`](../agents/roomlerd/src/main.rs#L1119) |
| The note's facts come from the move (who moved the session, when, and from which device and folder) and from the device's own store: the last tool call that no result follows | the server holds the first and never the second; the store holds the transcript | [`resume_note.rs:44`](../agents/roomlerd/src/hive/resume_note.rs#L44), [`supervisor.rs:4028`](../agents/roomlerd/src/hive/supervisor.rs#L4028) `last_open_call` |
| Every quoted name, folder and command is one line, with no control character and no backtick, at most 200 characters | names and folders come from the server and a command from the model; none may break the note's lines or its quoting | [`resume_note.rs:92`](../agents/roomlerd/src/hive/resume_note.rs#L92) |
| The note goes into the session's transcript as a `note` event, on the device | a viewer sees what the harness was told; like every transcript event, it never reaches the server | [`supervisor.rs:3390`](../agents/roomlerd/src/hive/supervisor.rs#L3390) |
| The kept move keeps where the session came from and who moved it | the device used to drop them | [`supervisor.rs:274`](../agents/roomlerd/src/hive/supervisor.rs#L274) |

⚠️ **The hook is a command line, and the shell that reads it differs by platform.** Both
paths are quoted: single quotes on Linux and macOS, and double quotes on Windows, where the
shell Claude Code runs hooks with is not verified yet. A path the quoting cannot carry
leaves no hook, and the session resumes without its note.

⚠️ **Not built:**
- the path map across several moves (P2f, with teleport): the note names the last move's
  folder only;
- toolchain drift, and what the move did not carry (background processes, caches);
- the credentials re-evaluated on the new device (P3).

⚠️ **Not under test:** the real Claude Code reading the note; that is the field run (AC9).
The real-device test checks the note, the hook and the transcript on each device. Its devices
are the test binary, which does not dispatch `hive-resume-note`, so the printer itself runs in
unit tests only.

### P2f-1a: a teleport, the devices' half

A teleport moves a session to a device that holds no copy of it (spec §3b, "Teleport, the
path map and fork"; design §6.1, §6.2). The target joins the session's replicaset for this
one session and takes a full copy over the stream while the session still runs; then the
promotion runs as it does to any member: the drain, the sync of what the drain added, the
move. P2f-1a is the devices' half. The route and its joining step are the server's half,
P2f-1b, below.

```mermaid
sequenceDiagram
    participant S as server (P2f-1b)
    participant T as the target's daemon
    participant P as the primary's daemon
    S->>T: rc:hive.prepare {fence n+1, teleport}
    Note over T: every start gate but the copy: hive_enabled, the account, the folder, capacity, sudo, the sidecar
    T-->>S: rc:hive.prepare_ack · the session's join admitted for 30 min
    S->>T: rc:hive.replica.join {fence n, role teleport}
    Note over T: admitted whatever hive_replica says · the membership kept · the stream followed
    S->>P: rc:hive.replica.join {fence n, role primary}
    Note over P: no turn running: its first checkpoint now · a turn running: that turn's end takes it
    P-->>S: rc:hive.replica.tip, where its copy ends
    Note over S,P: the target's copy follows the stream to that tip, then the drain, the sync and the move, as for a member
```

| Rule | Why | Where |
|---|---|---|
| A teleport's prepare (`rc:hive.prepare` with `teleport`) asks every gate a promotion's prepare asks but the copy: the primary enrollment's connection, `hive_enabled`, the harness, the account, the folder, capacity, the store, `sudo` on a Mac, the sidecar | the copy comes after the prepare; whether the device may RUN the session is the same question either way. A prepare that does not say `teleport` is a promotion's, and still needs the copy | [`supervisor.rs:1900`](../agents/roomlerd/src/hive/supervisor.rs#L1900) `prepare_teleport`, [`:1914`](../agents/roomlerd/src/hive/supervisor.rs#L1914) `prepare_as`, [`:1988`](../agents/roomlerd/src/hive/supervisor.rs#L1988) |
| One that passes admits that session's join in a new role, `teleport`, at the record's fence (one before the start's the prepare named), for 30 minutes, whatever the owner's `hive_replica` | a device that will run a session needs `hive_enabled`, not `hive_replica` (spec §3b): the copy it takes is the one it will run. `hive_replica` is its owner's offer to hold copies of sessions run elsewhere, which a teleport is not | [`supervisor.rs:1393`](../agents/roomlerd/src/hive/supervisor.rs#L1393) `admit_teleport`, [`:1412`](../agents/roomlerd/src/hive/supervisor.rs#L1412) `teleport_admitted`, [`hive.rs:921`](../crates/remote_control/src/hive.rs#L921) |
| A join in the `teleport` role that no prepare admitted is refused `not_prepared`, `hive_replica` on or off; a prepare admits no other role; a secondary org's connection takes none. The store's own gates still hold: the quota, a purge, the floor | the prepare's gates are what make this copy safe to hold, and a server could otherwise put any session on any device with `hive_replica` off | [`replica.rs:77`](../agents/roomlerd/src/hive/supervisor/replica.rs#L77), [`hive.rs:610`](../crates/remote_control/src/hive.rs#L610) |
| The admission ends when the session launches here at the prepare's fence, or when it runs out; a later prepare of the same session replaces it | a join re-sent at a connection is admitted until then | [`supervisor.rs:1435`](../agents/roomlerd/src/hive/supervisor.rs#L1435) `forget_teleport` |
| A device prepared for a teleport sends the manifest of the copies it holds at each connection | the server learns where the target's copy ends after a reconnect, as it does for a member | [`replica.rs:433`](../agents/roomlerd/src/hive/supervisor/replica.rs#L433) `reports_copies` |
| A primary's join takes the session's first checkpoint at once when no turn runs, no checkpoint is due and no drain is under way; with a turn running, that turn's end takes it. Where the copy ends is told either way, and a join again says so too | before, a member that joined a session that ran alone held none of its files until its next turn ended. A teleport waits for its target's copy to reach the primary's tip before it drains, so the full copy moves while the session still runs, and the drain moves only what it added | [`replica.rs:286`](../agents/roomlerd/src/hive/supervisor/replica.rs#L286) `take_up_replication`, [`:363`](../agents/roomlerd/src/hive/supervisor/replica.rs#L363) `tell_replicated`, [`supervisor.rs:5063`](../agents/roomlerd/src/hive/supervisor.rs#L5063) `checkpoint_now` |
| `hive-teleport` (`RpcCap::HiveTeleport`), a property of the build like `hive-promote`, and kept on a secondary org's connection like it | a build before it refuses a teleport's prepare for want of a copy, and its primary says nothing at a join: the server must teleport only between two devices that advertise it. No placement chooses a teleport's target, and a secondary org's prepare and join are refused there either way | [`caps.rs:1690`](../agents/roomlerd/src/encode/caps.rs#L1690), [`models.rs:726`](../crates/remote_control/src/models.rs#L726) |

⚠️ **The admission lives in the daemon's memory.** A daemon that restarts between a
teleport's prepare and its join refuses the join `not_prepared`, and the teleport fails in
that word. Asked again, it prepares again.

⚠️ **A teleport that fails before its move leaves the target holding the copy it took**,
listed `leaving` by the server's half (P2f-1b), until the purge (P2g). Its owner did
not offer to hold copies; the session's owner moved the session there.

### P2f-1b: a teleport, the server's half

`POST /api/tenant/{tid}/hive/session/{sid}/teleport {device_id, folder?}` moves a session to
a device that holds no copy of it: the session's owner, holding `HIVE_RUN`, the same gates
as a promotion except the target's membership. The promotion record carries it
(`teleport: true`) and gains one step, `joining`, between the target's ready and the drain:
the target takes a full copy while the session still runs, so the drain, the sync and the
move are a promotion's, and the drain moves only what it added
([`lib.rs:192`](../crates/modules/hive/src/lib.rs#L192),
[`promote.rs:634`](../crates/modules/hive/src/promote.rs#L634) `teleport`).

```mermaid
stateDiagram-v2
    [*] --> preparing: POST …/teleport
    preparing --> joining: the target is ready (rc:hive.prepare_ack)
    joining --> draining: the target joined, the primary joined and said where its copy ends, the target's copy holds as much
    draining --> syncing: rc:hive.drained
    syncing --> moving: the target's tip is the drained one (the lease CAS, fence n+1)
    moving --> [*]: the target's start accepted
    preparing --> [*]: refused or past its bound, nothing moved
    joining --> [*]: a join refused or past its bound: the target is let go
    draining --> [*]: as a promotion's, and the target is let go
    syncing --> [*]: as a promotion's, and the target is let go
```

| Rule | Why | Where |
|---|---|---|
| A teleport is refused to a device that holds a copy already (`already_a_member`: promote the session to it), and between two connections that do not both advertise `hive-teleport` (`device_unsupported`, `primary_unsupported`) | a device with a copy is promoted to; a build before P2f-1a refuses a teleport's prepare for want of a copy, and its primary says nothing at a join | [`promote.rs:466`](../crates/modules/hive/src/promote.rs#L466) |
| The primary must be online here (`primary_offline`); there is no move away from an unreachable primary for a teleport, nor one that replaces a move whose start was never answered | the stream's only source is the primary (P2d): an unreachable one leaves nothing to copy from | [`promote.rs:486`](../crates/modules/hive/src/promote.rs#L486) |
| The prepare says `teleport` and goes to a connection advertising `hive-teleport`; a session with no replicaset yet is placed first | the target's own gates decide whether it may run the session (P2f-1a); a teleport adds a member, so the record needs a replicaset | [`promote.rs:645`](../crates/modules/hive/src/promote.rs#L645) `begin`, [`hub.rs:2103`](../crates/modules/fleet/src/hub.rs#L2103) `push_hive_teleport` |
| At the target's ready, one compare-and-set on the promotion still `preparing` adds the target in the role `teleport`, `pending` (a row it had that held no copy is taken over), and enters `joining` with `hive.promote_join_secs` (default 600) | a ready that came twice, or after the promotion moved on, adds nothing | [`dao.rs:514`](../crates/modules/hive/src/dao.rs#L514) `begin_joining`, [`promote.rs:1246`](../crates/modules/hive/src/promote.rs#L1246), [`settings.rs:107`](../crates/config/src/settings.rs#L107) |
| The target is sent its join in the `teleport` role, through `hive-teleport`, never `hive-replica`; the primary its join as the primary, every time. A session that ran alone is owed that join and leased from then on, as when placement gives it a first member | the target's owner need not let it hold copies; the primary answers its join by saying where its copy ends (P2f-1a), which is what the joining step waits for | [`replica.rs:128`](../crates/modules/hive/src/replica.rs#L128) `push_join` |
| `joining` goes on to the drain once the target answered `joined`, the primary answered `joined` and reported a tip heard since the teleport began, and the target's copy holds at least that tip, counted as `on_chain` counts | an older tip of the primary's may predate what the session did since; a tail past an ended fence counts for nothing | [`promote.rs:1309`](../crates/modules/hive/src/promote.rs#L1309) `joined_of`, [`:1355`](../crates/modules/hive/src/promote.rs#L1355) `check_joined` |
| Each join's answer and each tip, the target's or the primary's, asks whether the step can go on | a frame drives every step, on whichever pod reads it, as every promotion step | [`promote.rs:1375`](../crates/modules/hive/src/promote.rs#L1375) `on_join_answer`, [`:1572`](../crates/modules/hive/src/promote.rs#L1572) `on_tip` |
| A refused join fails the teleport in the device's word (`join_refused`, `primary_join_refused`), and the step past its bound in `join_timeout`; a teleport that fails before the move, or whose moved start is refused after it, lets its target go (`leaving`) and ends its streams | its owner never offered to hold copies: it took one only to run the session; what it holds goes with the purge (P2g) | [`promote.rs:1084`](../crates/modules/hive/src/promote.rs#L1084) `let_go`, [`dao.rs:710`](../crates/modules/hive/src/dao.rs#L710) `move_lease_back` |
| A target back before it answered its join is sent it again on a connection advertising `hive-teleport`, whatever it says of holding copies | the join is re-sent at every connection until it is answered, as a member's is | [`replica.rs:208`](../crates/modules/hive/src/replica.rs#L208) `reconcile_joins` |
| Placement decided again leaves the target of a teleport in flight alone | no placement chose it, and it must not be let go for that | [`placement.rs:478`](../crates/modules/hive/src/placement.rs#L478) |
| Every audit row of a teleport says `teleport`; the room says the target takes a copy first, then the drain's and the move's notes | a teleport reads as one in the audit and in the room | [`room.rs:117`](../crates/modules/hive/src/room.rs#L117) |
| On the target, a device whose owner holds no copies follows the stream of a teleport it was prepared for, and at a connection follows those copies only | the member's dial kept `hive_replica`'s gate, so a target with it off refused its own copy; a leftover of a teleport that is over waits for the purge | [`stream.rs:1149`](../agents/roomlerd/src/hive/supervisor/stream.rs#L1149), [`follow.rs:334`](../agents/roomlerd/src/hive/supervisor/follow.rs#L334) `follow_teleports` |

⚠️ **Found by the real-device test, the first time a teleport ran end to end:** P2f-1a's
target admitted its join and then refused its own stream. The member's dial kept the
`hive_replica` gate (P2d-3b), and a target with `hive_replica` off closed every dial
`replica_disabled`, so its copy never moved and the teleport waited out its joining step.
The fake-device tests could not see it: their devices answer frames, and follow nothing.

⚠️ **`hive.promote_join_secs` must stay under the target's admission of its join (30 min,
`promote_limits::TELEPORT_ADMISSION_SECS`).** A join re-sent after that is refused
`not_prepared`. A unit test holds the default to it.

⚠️ **Measured here only on loopback.** The real-device test teleports between two
processes on one host. AC10 (under 30 s on a LAN pair, under 120 s with the source on the
DERP floor) is the field run's (P2k).

Next, P2f-2: the workspace's have and want. The target's `hive-materialize` names the commits
its folder's repository already holds, and the source packs against them, so a target with a
clone of the same repository receives only the session's own changes (spec §3b). Then fork
(P2f-3), an ended adopted session made managed (P2f-4), and the path map across several
moves (P2f-5).

The rest of P2 is next (spec §3b and §4): the rest of P2f, purges and retention (P2g), the
archive replica image (P2h), full-text search on archive replicas (P2i), the UI (P2j) and the
field run (P2k). Its server switch is `hive.replicaset` and its device switch `hive_replica`,
both off by default.

---

## Code map

| Piece | Where |
|---|---|
| the wire: frames, refusal words, limits | [`crates/remote_control/src/hive.rs`](../crates/remote_control/src/hive.rs); the `rc:hive.*` variants in [`signaling.rs`](../crates/remote_control/src/signaling.rs) (`ClientMsg` `:661`–`:1070`, `ServerMsg` `:3069`–`:3515`); `RpcCap::Hive`, `HiveView`, `HiveMemory`, `HiveAdopt`, `HiveReplica`, `HiveArchive`, `HiveStream`, `HiveLease`, `HivePromote` and `HiveTeleport` in [`models.rs`](../crates/remote_control/src/models.rs) `:618`–`:726`, matched by equality, since `hive` is a prefix of the other nine |
| the server module | [`crates/modules/hive/src/`](../crates/modules/hive/src/): `lib.rs` (the module, routes, indexes), `routes.rs` (start, list, get, stop), `agent_socket.rs` (reports, reconcile, the manifest, the demote), `view.rs` (grants and their signalling, and who serves them: the primary, or a member's copy), `participants.rs`, `access.rs`, `room.rs`, `dao.rs`, `hooks.rs` (removals), `scope.rs` (`hive.tenants`), `adopt.rs`, `brain.rs`, `policy.rs` (the replica policy), `placement.rs` (who holds a session's copies), `replica.rs` (joins, answers and tips), `stream.rs` (the replica stream's grants), `lease.rs` (the lease word), `promote.rs` (the promotion: the route and its gates, its steps, the lease move and the move back, the move away from an unreachable primary or from a target that never answered, and its timer; a teleport's route and its joining step) |
| the device core, with no daemon in it | [`crates/hive-node/src/`](../crates/hive-node/src/): `event.rs`, `chain.rs`, `stream_json.rs`, `store.rs`, `launch.rs`, `roots.rs`, `checkpoint.rs`, `stream.rs` (the replica stream's messages, their framing and limits, and its opening exchange) |
| the device | [`agents/roomlerd/src/hive/`](../agents/roomlerd/src/hive/): `gates.rs`, `supervisor.rs` (starts, the fence, the lease word, the drain, the prepare and the moved start, a teleport's prepare and the join it admits, the session task and its first checkpoint at a primary's join, the resume), `supervisor/adopt.rs`, `supervisor/replica.rs` (the join's gates, the teleport's among them, and the fences' ends it keeps, the primary's join, tips), `supervisor/stream.rs` (the replica stream: its endpoints and dials on the direct, relay and derp rungs, a teleport's dial on a device that holds no copies, then its events, blobs and acks), `supervisor/follow.rs` (the member's wants: the ladder, the waits, the sessions parked, the renewals, a join that overtakes them, a teleport's copy followed at a connection), `toolbelt.rs`, `resume_note.rs` (the resume note and the hook's printer), `sidecar.rs`, `view.rs`, `framing.rs`, `store.rs` (the writer), `hosted.rs`, `procs.rs`; Windows in [`hive_win.rs`](../agents/roomlerd/src/hive_win.rs) |
| what builds it | the `hive` feature, and `cfg(hive_host)` on Linux, macOS and Windows ([`build.rs:52`](../agents/roomlerd/build.rs#L52)); the capabilities in [`caps.rs:1619`](../agents/roomlerd/src/encode/caps.rs#L1619) |
| the update wait | [`updater.rs:278`](../agents/roomlerd/src/updater.rs#L278); the macOS hold, `HiveUpdateHold`, in [`crates/localapi/src/lib.rs`](../crates/localapi/src/lib.rs) |
| `adopt` | the CLI in [`hive_hooks.rs`](../agents/roomler-cli/src/hive_hooks.rs); its protocol in [`crates/localapi/src/hive_adopt.rs`](../crates/localapi/src/hive_adopt.rs) |
| the SPA | [`stores/hive.ts`](../ui/src/stores/hive.ts), [`useHiveViewer.ts`](../ui/src/composables/useHiveViewer.ts), [`components/hive/`](../ui/src/components/hive/), [`views/hive/`](../ui/src/views/hive/) |
| the integration tests | `crates/tests/src/hive_tests.rs`, and in `crates/tests/tests/hive_*.rs` the canary, drivers and memory, a device's frames of a promotion (`hive_drain.rs`), a promotion between real devices (`hive_promote.rs`), and a teleport between real devices (`hive_teleport.rs`) |

## Not built

- The rest of the replicaset (P2f onward). A session is readable from a member's copy
  while its device is offline (P2d-5), a device holds to the session's fence (P2e-1) and
  to the server's word on each connection (P2e-2), the server moves a session to a member
  (P2e-3), a primary drains for the move (P2e-4), and a member answers the prepare and
  takes a moved session over from its copy (P2e-5a). The joins carry the old fence's end
  (P2e-5b), and promotion is on, proven between real devices (P2e-5c). An unreachable
  primary is waited out and moved away from (P2e-6a), and so is a target that never answered
  its moved start, whose start is re-sent however late it comes back (P2e-6b). A session
  moved to a device is told, once, that it moved (P2e-7). A device prepares for a teleport
  and takes the copy it will run, and a primary's join checkpoints at once (P2f-1a); the
  server moves a session to a device that held no copy of it (P2f-1b). The workspace's
  have and want is next (P2f-2).
- The vault and the rest of the toolbelt (P3), knowhow (P4) and the learning loop
  (P5). `docs/vault.md` and `docs/knowhow.md` come with them.
- `adopt` on Windows, and a Windows session for anyone but the console user.
- A device-owned permission mode (decision 7). A session runs in `default`, where a
  person decides everything but reads.
- WSL (decision 4): a daemon inside the distro, or a launcher from the Windows daemon.
