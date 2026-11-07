# Overlay key rotation — the server orders a re-mint it never sees

A device's WireGuard key is its whole **data-plane identity** on the overlay: whoever
holds the private half can complete handshakes as that node with every peer that has
its public key installed — everything the overlay ACL grants it, every inbound SSH
grant and tunnel route addressed to it. Before FR-40 the only remedy for a leaked key
was a re-enrollment: a fresh token, SYSTEM/root on the host, a daemon restart. An
operator whose key just leaked reaches for *rotate*, not *re-enroll*.

Rotation is an **order, never a delivery**. An admin clicks *Rotate overlay key…* in
the device grid; the server writes the order on the device row and pushes
`rc:agent.key_rotate { request_id }` — a frame that carries no key material and is
test-locked never to grow any. The device mints a fresh keypair **locally**, persists
it, reports back with public keys only, and reconnects; its join under the new key is
what the server verifies, every peer reinstalls the carrier within milliseconds, and
the old key is useless everywhere. A device that is offline is ordered on its next
connect. **The server never holds, transports or chooses a private key at any step.**

> Design record, the three field-run defects and their fixes, and the full field log:
> [`fr/FR-40-overlay-key-rotation.md`](fr/FR-40-overlay-key-rotation.md) (#962). This
> page describes what shipped (P1–P1d, `agent-v0.4.25`/`0.4.26` + web
> `v20260830-6a166a1ec9f7`) and the retired-key refusal (P2,
> [§ Retired keys](#retired-keys)), which an org switches on. The CLI (P3) is under
> [Not built](#not-built).

```mermaid
sequenceDiagram
    autonumber
    participant A as Admin (device grid)
    participant S as roomler server
    participant D as roomlerd (the device)
    participant P as every peer

    A->>S: POST …/agent/{id}/overlay-key/rotate
    S->>S: gates: MANAGE_AGENTS · 1 order/min/device · the device advertises key-rotate
    S->>S: agents.key_rotation = {request_id, public_key_before} · key_rotation_audit row
    S-->>D: rc:agent.key_rotate {request_id}  (only to a live socket, else queued)
    D->>D: kill switch? own 60 s ceiling? → mint WgKeypair → config::save (lock, fsync, .prev)
    D-->>S: rc:agent.key_rotated {request_id, rotated, old_pub, new_pub, key_epoch}
    D->>D: close every session on this org · reconnect under the new key
    D->>S: rc:overlay.join (new pubkey, key_epoch)
    S->>S: agents.overlay_identity = {public_key, key_epoch, joined_at}  ← the proof
    S-->>P: rc:overlay.netmap_delta {upserts: [the device]}
    P->>P: "peer's WG public key changed — reinstalling its carrier"
    D-->>S: rc:agent.key_rotated re-sent on the NEW session (the first copy rode a dying socket)
```

## What is rotated, and what deliberately is not

| identity | rotated by this? | why |
|---|---|---|
| the org's **overlay WireGuard key** (`overlay_wg_secret_key`, per org via `AgentConfig::for_org`) | **yes** — the whole point | it is the data-plane identity; a holder can be the node on the mesh |
| the **agent token** (JWT, the control-plane credential) | no | a different identity with a different fix (`token_epoch`, Known Issues); a leaked token is a control-plane compromise |
| the **SSH host key** (`ssh_host_key`) | no | clients pin it; rotating it is a client-side TOFU event with its own runbook |
| `config.toml.prev` | not scrubbed | the retired secret stays in the `.prev` copy until the next save; an on-host reader of `.prev` can read `config.toml` too, so it is not a boundary — P2's retired-key ledger is what makes the copy worthless off-host, once the org enforces it ([§ Retired keys](#retired-keys)) |
| the standalone `roomler` tunnel client's key | no (P3) | tunnel-only hosts have no agent WS to receive the order |

The control plane is **not** reachable with a leaked overlay key: joins and DERP
registration need the agent JWT, and DERP additionally refuses a registration whose
public key is not the row's. The exposure is the mesh — which is exactly what the
rotation closes.

## The order — `POST /api/tenant/{tid}/agent/{aid}/overlay-key/rotate`

Owned by the **network** module (`crates/modules/network/src/lib.rs:445-448`, handler
`crates/modules/network/src/routes/overlay_key.rs:118-250`). It touches no key. In
order:

1. **`MANAGE_AGENTS`** — retiring a key *grants* nothing, so neither `EXEC_DEVICE` nor
   `SSH_DEVICE` is involved (`overlay_key.rs:131-138`).
2. The device is resolved **live, in the tenant** — a foreign id is a 404, not a
   cross-tenant order, and a removed device is a 404 too: an order written onto a
   tombstone would be desired state nothing can ever reconcile (`overlay_key.rs:140`,
   #1821).
3. **Online?** — the pod-local Hub, `rc_hub.is_agent_online` (`overlay_key.rs:143`).
   **Understands it?** — the hello's `rpc` verbs, persisted on the row at every
   connect, must contain `key-rotate` (`overlay_key.rs:146`). **Ceiling** — one order
   per device per minute, keyed on the device in both slots so it is per *device*, not
   per admin; checked **after** the identity gates so a refusal is attributable
   (`overlay_key.rs:39`, `:167-169`).
4. `decide(rate_ok, online, supports_rotation)` — a pure function whose four cells are
   unit-tested (`overlay_key.rs:91-104`):

   | rate ok | online | advertises `key-rotate` | result |
   |---|---|---|---|
   | no | — | — | **409** `rate_limited` |
   | yes | yes | no | **409** `agent_unsupported` — "needs 0.4.25 or later; push an update first, then rotate" |
   | yes | yes | yes | **200** `{dispatch: "pushed", delivered: true}` |
   | yes | no | — | **200** `{dispatch: "queued", delivered: false}` — ordered on its next connect |

5. **Desired state first, then the push** (`overlay_key.rs:175-214`): the request is
   written onto the agent row — `request_id`, `requested_by`, `requested_at`, and
   **`public_key_before`**, the key the device holds *now* (its last verified join) —
   before `rc:agent.key_rotate` is sent, so a report can only ever refer to an order
   that already exists. A push that races a disconnect leaves the order standing as
   `queued`.
6. **One audit row for either arm** (`overlay_key.rs:225-242`): `decide()` returns
   `Result<Pushed|Queued, DenyReason>` and a single call site records both into
   `key_rotation_audit`, so a new refusal cannot forget to audit itself (the
   `config_audit` / `ssh_audit` shape). Best-effort — an audit insert is never what
   stops a legitimate rotation.

⚠️ **One order path, two callers (P2).** Steps 3–6 are one function, `place_order`
(`overlay_key.rs:156`). The route calls it for an operator; the overlay join calls it for
a device that presented a retired key ([§ Retired keys](#retired-keys)), as the server:
the zero `user_id` and `trigger: key_retired` on the order and on its audit row. An order
the server places cannot skip a gate, or an audit, that an operator's goes through, and it
counts against the same per-device ceiling.

⚠️ **The push is capability-gated, and `rc:agent.update` is not — on purpose, in
both directions.** `UpdateNow` is sent blind because an unknown frame to an old agent
costs nothing; a rotation order dropped silently by a pre-feature agent would leave
a *security action* showing "in flight" on a dashboard for a device that never heard
it. An **online** device without the verb is therefore refused up front, never
spun on. An **offline** device is queued regardless — it may well update before it
reconnects, and the connect-time reconcile gates again (`overlay_key.rs:64-67`).

⚠️ **Which pod answers matters.** `is_agent_online` reads the pod-local Hub; the
tenant-affinity LB (`deployment.md`) is what lands the admin's call on the pod that
holds the device's socket. An online device that comes back `queued` is an affinity
suspect first.

## On the device

The handler sits in the WS loop that received the order, next to `ConfigPush`
(`agents/roomlerd/src/signaling.rs:5991-6133`). Every branch **reports**, refusals
included — the operator is looking at a security action and each refusal has a
different fix:

| step | what | reports | anchor |
|---|---|---|---|
| 1 | kill switch `overlay_key_rotation == false` | `refused: disabled` | `signaling.rs:5994-6009` |
| 2 | the device's **own** 60 s ceiling, per org — "a bound that exists only on the ordering side is not a bound" | `refused: rate_limited` | `signaling.rs:3413-3434`, `:6012-6027` |
| 3 | mint `WgKeypair::generate()` — the enrollment mint; `None` in a build with no overlay surface | `refused: unsupported` | `agents/roomlerd/src/key_rotation.rs:19-30` |
| 4 | **persist first**: under the daemon-wide write lock, `config::load` → set the secret on the primary scalar or on the `[[orgs]]` entry whose `tenant_id` is this loop's → bump `overlay_wg_key_epoch` → atomic `config::save` (tmp + fsync + 0600/ACL + `.prev` + rename) | on error: `failed` + detail, **identity unchanged** | `agents/roomlerd/src/remote_config.rs:656-688` |
| 5 | queue the same report for the *next* session, then send `rc:agent.key_rotated { rotated, old_pub, new_pub, key_epoch }` on this one and give the pump 300 ms | `rotated` | `signaling.rs:6089-6115` |
| 6 | close every peer, tunnel peer and QUIC peer this org loop carries (the `Goodbye` teardown), return `ConnectError::KeyRotated { secret, epoch }` | — | `signaling.rs:6119-6131`, `:1606` |
| 7 | the loop adopts the key + epoch into its one config snapshot and re-enters `connect_once` **immediately** — one device re-joining, not a fleet event, so no stagger | — | `signaling.rs:1318-1342` |
| 8 | `overlay::maybe_start` sees the fingerprint change (it includes the public key), rebuilds the runtime; the join carries the new key and epoch | — | `agents/roomlerd/src/overlay.rs:399` |

⚠️ **Persist before anything else.** A key that is not written down would be lost at
the next restart and the device would come back as the identity it just retired —
the same rule the SSH host key follows. A failed save reports `failed` and changes
nothing.

⚠️ **Per org, honoured on whichever org's WS the order arrives on.** The key is per
enrollment (`AgentConfig::for_org` scopes it), so org B rotating its key on a shared
host touches nothing of org A's. This is the **opposite** of `rc:agent.update` and
config push, which drive machine-wide state and are therefore primary-only
(`remote-config.md` §4). Do not copy that guard here.

| | `rc:agent.update` / config push | `rc:agent.key_rotate` |
|---|---|---|
| scope of the thing changed | host-global | this org's key only |
| honoured on | the primary WS only | the WS the order arrived on |
| disk write | scalar keys | the primary scalar **or** the matching `[[orgs]]` entry |

⚠️ **Immediate and disruptive, by design.** A key that leaked must not wait for the
holder's session to end, so the rotation ends every remote-control, overlay-SSH and
tunnel session the device carries on that org and reconnects. The confirm dialog says
so (`ui/src/components/admin/AgentsSection.vue:1313-1314`). A corp-VPN host may come
back relay-locked after the cycle (the renumber caveat in `multi-org.md`).

`overlay_wg_secret_key` stays **unwritable over LocalAPI** (`config_surface.rs:13-14`,
locked by test): rotation is an *action*, not a config write, and the P3 CLI goes
through the same action.

## The join is the proof — three records, three trust levels

The device's report is a **claim** (the `ssh_activity` sense). What the server can
*verify* is the join: `rc:overlay.join` under the new public key passes
`wg_key_taken_by_other` (machine-scoped, so a device rotating its own key is admitted
— `security-baseline.md` §4), and the server stamps what the device presented onto
the agent row as `overlay_identity` (`crates/modules/network/src/overlay.rs:633-656`
→ `dao/agent.rs:780`). It is stamped on **every** join, not only after a rotation, so
the grid can always show the device's current key.

| record | lives in | written by | what it is |
|---|---|---|---|
| the **order** | `agents.key_rotation` (`KeyRotationRequest`, `models.rs:1672`) | the route, or the server itself for a retired key (P2: `trigger: key_retired`, the zero `requested_by`) | what was asked, when, by whom — and `public_key_before`, the key held at that moment |
| the **decision** | `key_rotation_audit` (`KeyRotationAuditEvent`, `models.rs:5541`; DAO `crates/services/src/dao/key_rotation_audit.rs`) | `place_order`, one call site, both arms, whoever placed the order (`trigger` names the server's) | `pushed` / `queued` / `denied: <reason>`. Records what the **server** decided — never what the device did. 90 d TTL, `(tenant_id, at)`, `(agent_id, at)` (`network/src/lib.rs:825-833`) |
| the **claim** | `agents.key_rotation_report` (`KeyRotationReport`, `models.rs:1723`) | `rc:agent.key_rotated` ingest (`agent_socket.rs:175` → `agent_arms.rs:81-125`) | the device's account; `reported_at` is server-stamped, `detail` re-clamped on receipt |
| the **proof** | `agents.overlay_identity` (`OverlayIdentity`, `models.rs:1754`) | the overlay join | the public key and epoch the device actually presented, and when |

⚠️ **Never fold them.** A later **refusal** for the same `request_id` is withheld
rather than written over a `rotated` claim (`dao/agent.rs:754-771`) — a rotation that
happened, happened; the only way a refusal follows a success for one order is the
duplicate-delivery race below. Any report about a *different* order, and any
`rotated`, still replaces.

## The state the dashboard shows

Resolved **once, server-side**, from the order, the claim and the proof
(`crates/modules/fleet/src/agent.rs:612-696`); the grid only names the states
(`AgentsSection.vue:2091-2192`). The device row also carries `overlay_public_key` and
`overlay_key_epoch` straight from the proof (`agent.rs:1767-1775`), so an operator
can *see* the key change instead of trusting a chip.

```mermaid
flowchart TD
    R["order on the row?"] -->|no| N["(no chip)"]
    R -->|yes| M{"verified identity change?<br/>join under key ≠ public_key_before,<br/>joined_at ≥ requested_at"}
    M -->|yes| ROT["rotated"]
    M -->|no| REP{"report for THIS request_id?"}
    REP -->|rotated| J{"joined under report.new_public_key?"}
    J -->|yes| ROT
    J -->|"no, ≤ 60 s old"| ING["rotating"]
    J -->|"no, > 60 s old"| RNJ["reported_not_joined"]
    REP -->|disabled · rate_limited · unsupported| REF["refused"]
    REP -->|failed| F["failed"]
    REP -->|none| C{"device advertises key-rotate?"}
    C -->|no| U["unsupported"]
    C -->|yes| DL{"delivered_at set?"}
    DL -->|yes| D["delivered"]
    DL -->|no| Q["queued"]
```

| state | resolved when | chip | operator's move |
|---|---|---|---|
| `queued` | ordered, never pushed, no answer | *key rotation queued* | wait — it rotates on the device's next connect |
| `delivered` | pushed to a live socket, no answer for **this** order yet | *rotating key…* | wait a few seconds |
| `rotating` | the device claims `rotated` (≤ 60 s ago) but its last join still shows the previous key | *rotating key…* | wait — it is re-joining |
| `rotated` | the join proves it (key ≠ `public_key_before` after the order), **or** the claim's `new_public_key` is what the device joined with | *key rotated · e⟨epoch⟩* | done |
| `reported_not_joined` | claimed `rotated` > 60 s ago (`KEY_ROTATION_REJOIN_GRACE_SECS`, `agent.rs:594`), still joined under another key | *key not re-joined* | the re-join failed — read the device log |
| `refused` | the device answered `disabled`, `rate_limited` or `unsupported` | *key rotation refused (⟨outcome⟩)*, tooltip = `report.detail` | fix the stated thing |
| `failed` | mint or persist failed; identity unchanged | *key rotation failed* | `report.detail` says why |
| `unsupported` | an order stands, no answer, and the device's hello lacks `key-rotate` | *agent too old for key rotation* | update the device; the order stays queued and runs on the first connect that advertises the verb |

⚠️ **Compare `request_id`s, never outcomes alone.** A report about an earlier order
says nothing about this one — the remote-configuration lesson, applied here
(`agent.rs:600`).

⚠️ **`unsupported` is a state an *offline* order reaches.** An online pre-feature
device never gets an order recorded — the route refuses it 409 up front. The state
appears when a device was ordered while offline and then connected without the verb
(or whose build lost its overlay surface); the socket logs *"overlay-key rotation not
ordered — this device's agent predates rc:agent.key_rotate"* and leaves the order
standing (`fleet/src/socket.rs:380-384`).

## Reconcile on connect — and the three races it must not re-run

At agent register, where `ConfigPush` reconciles, a standing order is pushed again
if the hello advertises `key-rotate` (`crates/modules/fleet/src/socket.rs:281-297`,
`:362-370`) — the offline case, through the same path as the online one, so it runs
on every connect rather than only when nobody is watching. Three field cycles on
2026-08-30 each found a way for that to go wrong; each left a rule:

| cycle | what the run showed | the rule it left |
|---|---|---|
| 1 → **P1b** | 30 ms after the re-join, the reconnect's own register re-pushed the **same** order (the `rotated` report, sent on the dying session and written by a spawned task, had not landed); the device refused the duplicate under its own ceiling, and that refusal **overwrote** the success — the grid read `refused (rate_limited)` for a rotation that worked | an order delivered < **120 s** ago is in progress, not lost: `should_redeliver` (`models.rs:6328`, `:6353`); and a refusal never overwrites a `rotated` report for the same order (`dao/agent.rs:754-771`) |
| 2 → **P1c** | the dying-session report was **lost** this time, so the state stuck at `delivered` although the server had verified the new key at the join | the order snapshots `public_key_before`; a join under another key resolves `rotated` with or without a report; the agent **re-sends** its report on the next session, keyed per org (`signaling.rs:2741-2763`) |
| 3 → **P1d** | the cycle-2 order (report lost, no snapshot) was re-delivered on every later connect — a pod roll, then the 0.4.26 restart: **three rotations for one click** | `order_is_satisfied` (`models.rs:6338`): a verified identity change after the order satisfies it, and a satisfied order is never pushed again |

⚠️ The dying-session copy of the report was lost in **3 of the 4** ordered runs. The
re-send on the new session is the reliable path, and the identity rule is what keeps
the grid honest for a device that has not yet updated to it — a 0.4.25 device shows
`rotated` by the join alone.

## Retired keys

A rotation makes the old key useless to the **mesh**: every peer drops it. P2 (FR-40,
2026-11-03) makes it useless to the **server** too. Without it, a join presenting a key
the node moved away from — a `.prev` rollback, a restored backup, a copied config, a
holder of both the old key and the agent token — is admitted, the row takes the old key
back, and every peer reinstalls it. With the org's switch on `enforce`, that join is
refused `key_retired`, and a device that can rotate is ordered to, instead of being left
on the control plane and off the mesh.

```mermaid
sequenceDiagram
    autonumber
    participant D as roomlerd (presents a retired key)
    participant S as roomler server
    participant P as every peer

    D->>S: rc:overlay.join (a pubkey some row of this network retired)
    S->>S: wg_key_taken_by_other → retired_key_in_network (live rows and tombstones)
    S-->>D: rc:overlay.join_refused {reason: key_retired, detail}
    S->>S: an order already being executed? → refuse, order nothing more
    S->>S: place_order(server, key_retired): ceiling · decide · agents.key_rotation {public_key_before = the PRESENTED key} · audit row (zero user, trigger)
    S-->>D: rc:agent.key_rotate {request_id}
    D->>D: P1 unchanged: kill switch · own ceiling · mint · persist · report · reconnect
    D->>S: rc:overlay.join (a NEW pubkey)
    S->>S: admitted · ledger += the key it replaces (rotate) · overlay_identity → the order is satisfied
    S-->>P: rc:overlay.netmap_delta {upserts: [the device]}
    P->>P: "peer's WG public key changed — reinstalling its carrier"
```

**The ledger** is per node: `overlay_nodes.retired_keys[]`, each entry
`{ public_key, key_epoch, retired_at, reason }`, public halves only
(`crates/remote_control/src/models.rs:3203`, `:6818`). It is written at one moment —
an **accepted join under a different key**, the verified identity change P1c made the
proof — in the same write that installs the new key: a compare-and-set on
`{_id, wg_public_key: <old>, retired_keys.public_key ≠ <old>}` that `$push`es the old key
and keeps the newest 64 (`crates/services/src/dao/overlay_node.rs:346-368`,
`crates/modules/network/src/overlay.rs:441-471`). Never on a device's report, which is a
claim. `reason` is `rotate` when the join is the one that satisfies the device's standing
order and `rejoin` otherwise; it is forensic, and nothing acts on it. Tombstones keep
their entries: removal is not a pardon. The ledger records in **every** mode, `off`
included, so it is full before anything enforces it.

**The check** is network-wide: a join presenting a key found in ANY row's ledger of the
org's network, live or tombstoned, is presenting a retired key
(`dao/overlay_node.rs:145`, one read on the multikey
`(network_id, retired_keys.public_key)` index). A config copied to another machine of the
org cannot make a key live again. It is still per org, because a network is: another org's
node for the same machine holds its own key (`AgentConfig::for_org`). The check runs after
`wg_key_taken_by_other` and before any address is leased (`overlay.rs:378-420`), so a
refused join holds no lease.

| `retired_key_mode` | the ledger | a join presenting a retired key | a DERP registration with one | the device |
|---|---|---|---|---|
| `off` (default) | recorded | **admitted**, the pre-P2 join — no ledger read | the pre-P2 registration | nothing ordered |
| `warn` | recorded | admitted; the pod logs `would refuse — key_retired` | admitted; `would refuse registration` | nothing ordered |
| `enforce` | recorded | **refused** `key_retired` | refused | with `key-rotate`: ordered to rotate. Without it: audited `agent_unsupported`, ordered nothing, stays off the mesh |

**The refusal** is `rc:overlay.join_refused { reason: key_retired, detail }`
(`crates/remote_control/src/signaling.rs:463`). It is not retryable: the same key is
refused again, and the way out is a new key. The detail says when and why the key retired
and names no key (`overlay.rs:790`). A ledger read that fails refuses `store_unavailable`
under `enforce`, failing closed like the uniqueness check above it.

⚠️ **Old agents survive the new reason.** Agents from 0.4.41 run a decoder whose unknown
reason is `Unknown` — the fallback shipped in the same commit as `supports_join_refusal`
(#1081) — so they log the refusal at ERROR with its detail, record it for
`roomler status`, and neither retry nor tear down
(`agents/roomlerd/src/overlay.rs:1019`). An agent before 0.4.41 never advertised
`supports_join_refusal`, is sent no frame, and waits on a netmap that does not come; the
server's log and audit carry the reason.

**The self-heal** is the route's own order path. `place_order`
(`crates/modules/network/src/routes/overlay_key.rs:156`) is what the route calls and what
the join calls for a refused agent (`overlay.rs:860`): the shared one-a-minute ceiling,
`decide()`'s cells, the order on the row as desired state, the push, and ONE audit row.
The server places it as nobody — `OrderedBy::Server`, the zero `user_id`, and
`trigger: key_retired` on both the order and its audit row (`overlay_key.rs:114`) — and
snapshots the key the device **presented** as `public_key_before`, so the device's join
under any other key satisfies the order (P1c and P1d hold unchanged). The frame is P1's
`KeyRotate { request_id }`, sent after the refusal, so the device logs why before it
rotates.

⚠️ **The loop guard is the order already on the row** (`order_in_flight`,
`models.rs:10343`). An order that is unsatisfied, unanswered, and delivered less than
`REDELIVER_AFTER_SECS` (120 s) ago is being executed: a second retired-key join inside
that window is refused and ordered nothing, with no second audit row. Past the window, the
60 s ceiling bounds it. A device that refuses the order — its own switch off, say — is
ordered again only at a later join, which only a reconnect sends: the agent never
re-joins on a refusal.

**DERP** still requires a registration's key to equal the row's. A mismatch that is a
retired key is logged as `key_retired` rather than "stale row or rotation race"; a row
whose **own** key is retired — admitted under `off` or `warn`, not re-joined since — is
refused under `enforce` and logged under `warn` (`crates/modules/network/src/derp.rs:512-548`).
The mode read fails **open** to `off` (`derp.rs:440`): the join enforces on the key the row
holds, and DERP is the floor that connects a node when nothing else does.

⚠️ Known limits, by design:
- Under `off` or `warn` a rollback join is admitted and retires the **newer** key it
  replaces. The rolled-back key it presents stays in the ledger, so `enforce` later
  refuses and heals it.
- A join that loses a race with another join for the same row refreshes the row without
  retiring the key the winner wrote: the compare-and-set falls back to the plain refresh.
  Only two installs fighting over one node race like that, and under `enforce` such a pair
  also rotates each other's key at every start. `warn` shows them first.
- A device without the verb that re-joins inside the minute is audited `rate_limited`, not
  `agent_unsupported`: `decide()` checks the ceiling first, for every caller.

## Kill switch and configuration

| key | where | default | effect |
|---|---|---|---|
| `overlay_key_rotation` | device config (tribool), config surface, env `ROOMLERD_OVERLAY_KEY_ROTATION` (`crates/agent-core/src/config.rs:1075-1080`, `config_surface.rs:1258-1265`) | **on** | `false` ⇒ every order is answered `refused: disabled`, key unchanged. Read from the start-time snapshot — **restart required** |
| `overlay_wg_key_epoch` | persisted next to the key, primary scalar and per `[[orgs]]` entry (`config.rs:1974`, `:2099-2101`) | `0` | bumped per rotation; presented on every join, shown in the grid |
| server switch for the route | — | — | **none, deliberately**: the route is admin-initiated and permission-gated; a defective push is stopped by the device switch |
| `retired_key_mode` (P2) | the org's `overlay_networks` row, `GET/PUT /api/tenant/{tid}/overlay-retired-keys` (`MANAGE_AGENTS`; `crates/modules/network/src/routes/overlay_key.rs:353`, `:380`) | **`off`** | `warn` logs what it would refuse; `enforce` refuses a retired key at join and DERP and orders a device with the verb to rotate. Read per join and DERP registration — **no restart**, no live session disturbed. A stored value a build does not know reads `off`; a typo in the `PUT` is a 400 |

⚠️ **A kill switch, not a gate.** Unlike `exec_enabled` and `ssh_enabled`, this one
defaults to *on*: the order grants nothing and leaks nothing — the worst a hostile
server can do with it is make a device re-key, which costs that device its sessions
for a second. There is no gate-4 argument for default-off, and a default-off switch
would have left the one feature a key leak needs disabled fleet-wide.

The server cannot see the device switch, so a switched-off device is still ordered
(`pushed`); the refusal is the device's and reaches the grid as the `refused` state.

## Operating it

1. Device grid → row menu → **Rotate overlay key…** → confirm (`AgentsSection.vue:177`).
   Online: *"ordered — it mints a new key, re-joins the mesh and reports back within
   seconds."* Offline: *"queued and runs on its next connect."*
2. Watch the chip and the **overlay public key** column: the key and `e⟨epoch⟩` change
   within ~2 s of the order on a healthy device (field: order → join in 1.1–1.3 s).
3. Peers reinstall the carrier on the netmap upsert — 18 ms after the join when
   measured (`establish.rs`, *"peer's WG public key changed — reinstalling its
   carrier"*). Traffic resumes after WireGuard's next handshake; a relay-locked pair
   re-climbs the ladder from the floor. ⚠️ `roomler peers --json` carries **no key
   field** — the peer's log line is the evidence that the old key is gone.
4. Both ceilings are 60 s. A second click inside the minute is **409 `rate_limited`**
   from the server; if an order still reaches the device inside *its* minute (the
   duplicate-delivery race), the device refuses and the grid shows `refused
   (rate_limited)` only when no `rotated` report exists for that order.
5. `reported_not_joined` means the device claims it rotated and has not come back
   under the new key within a minute: `roomler exec <device> -- roomler logs --grep
   key_rotate` (never a path guess, `fleet-rpc.md`). On a corp-VPN host, expect the
   re-join to land on relay first.
6. **DERP transient, bounded**: `derp_acl::rebuild` is spawned at the join, so the
   first DERP frames under the new key may be denied for milliseconds until the table
   lands; WireGuard's handshake retry covers it. Measured, not barriered.
7. Nothing in the audit or on the row ever holds a private key — a test asserts the
   order frame has no key-shaped field, and the audit row stores `request_id`,
   `dispatch`/`denied`, who and when.
8. **Retired keys (P2)**: start the org at `warn` and read the pods for
   `would refuse — key_retired` — a cloned VM, a second install sharing one node, or a
   restored backup shows up there before anything is refused — then
   `PUT …/overlay-retired-keys {"mode": "enforce"}`. A refused device with the verb is
   ordered within the same join: its chip reads the rotation, `key_rotation.trigger` says
   `key_retired`, and `requested_by` is the zero id. One without the verb stays off the mesh,
   and the pod's `overlay.join: refused` line plus the audit row (`agent_unsupported`,
   `trigger: key_retired`) carry the reason; an agent before 0.4.41 cannot be told.

## Measured in the field

| date | build | read |
|---|---|---|
| 2026-08-30 00:19 UTC | CORPLAP-3 `0.4.25` | first ordered rotation: mint+persist → `key_epoch=1` → join **1.1 s** after the order; identity stamped within the first API sample; the peer reinstalled and overlay ping ran 4/4 at 84–97 ms within 60 s. Found the duplicate-delivery race (P1b) |
| 2026-08-30 00:55 UTC | CORPLAP-3 `0.4.25`, web P1b | clean; no duplicate refusal; the immediate second order **409 `rate_limited`**. Found the lost dying-session report (P1c) |
| 2026-08-30 01:30–01:35 UTC | web P1c, CORPLAP-3 → `0.4.26` | the re-sent report ingested 26 ms after the new session; the lost-report order re-delivered twice (e3, e4) — P1d |
| 2026-08-30 02:09 UTC | web P1d, CORPLAP-3 `0.4.26` | order → persisted :09.085 → `key_epoch=5` → re-sent report :10.210 → join :10.223 → ingest :10.229 → **two peers reinstalled 18 ms after the join**; `rotated` in the grid within 5 s; the P1d pod roll re-delivered nothing. ⚠️ One reader's clock was 12 s ahead of the server — the earlier "+12 s reinstall latency" was that offset |
| 2026-10-07 02:28–02:36 UTC | two throwaway Ubuntu VMs enrolled permanently against prod — `0.4.116`, and `0.4.24` pinned with `auto_update = false` | the three reads the fleet could never give (every real device auto-updates past the verb; no corp laptop restarts for a kill switch), each a refusal arm beside a success arm on the same deploy. **Kill switch** off → the server still `pushed`, the device refused 5 ms later, `refused (disabled)`, key byte-identical through the next restart; on → persisted 9 ms after the order, e1, `rotated` at the first sample 4 s later, the peer reinstalled 12 ms after the re-join. **Offline** → `queued`; started and left alone → `delivered on connect` at the hello, persisted 7 ms later, e2, `rotated` within 8 s, `delivered_at` 19 s after `requested_at`; 135 s on, still e2 and exactly one delivery. **`0.4.24`** online → **409 `agent_unsupported`** in 2 ms, nothing recorded, nothing pushed; ordered while offline → `unsupported` before it even reconnected, *not ordered — predates* on the pod at the connect, key and epoch unchanged |

## Code map

| piece | where |
|---|---|
| capability verb `key-rotate` (equality match, `ALL` entry, wire string locked) | `crates/remote_control/src/models.rs:926`, `:1108`, `:1136`, test `:8097`; advertised by any build with an overlay surface, `agents/roomlerd/src/encode/caps.rs:4805-4809` |
| wire: `rc:agent.key_rotate` (order), `rc:agent.key_rotated` (report) | `crates/remote_control/src/signaling.rs:2718`, `:1010`; owner `network` (`:1960`, `modular-monolith.md`) |
| models: request · outcome · report · identity · audit event · `order_is_satisfied` · `should_redeliver` | `models.rs:3090`, `:3124`, `:3149`, `:3180`, `:5541`, `:10308`, `:10323` |
| route, `decide()`, the 409 bodies, the audit call site | `crates/modules/network/src/routes/overlay_key.rs` |
| reconcile-on-connect | `crates/modules/fleet/src/socket.rs:281-297`, `:362-383` |
| report ingest | `crates/modules/network/src/agent_socket.rs:175` → `agent_arms.rs:81-125` |
| identity stamp at the join | `crates/modules/network/src/overlay.rs:649-672`; `crates/services/src/dao/agent.rs:1081` |
| the conditional report write | `crates/services/src/dao/agent.rs:1046-1076` |
| state resolver + grid view | `crates/modules/fleet/src/agent.rs:770-931`, `:1767-1775` |
| audit DAO + index plan | `crates/services/src/dao/key_rotation_audit.rs`; `crates/modules/network/src/lib.rs:825-833` |
| device handler · re-send · ceiling · adoption on reconnect | `agents/roomlerd/src/signaling.rs:5991-6133`, `:3369-3411`, `:3413-3434`, `:1318-1342` |
| mint · persist | `agents/roomlerd/src/key_rotation.rs`; `agents/roomlerd/src/remote_config.rs:656-688` |
| kill switch · epoch | `crates/agent-core/src/config.rs:1075-1080`, `:1812-1817`, `:1942-1944`; `config_surface.rs:1258-1265` |
| UI: action, dialog, chip, store | `ui/src/components/admin/AgentsSection.vue:177`, `:1287-1314`, `:2089-2190`; `ui/src/stores/agents.ts:1162-1176` |
| P2 models: ledger entry · reason · the switch · `trigger` · `order_in_flight` · their tests | `crates/remote_control/src/models.rs:3203`, `:3221`, `:3943`, `:3191`, `:10343`, `:10494`; the node and network fields `:6818`, `:7060` |
| P2 wire: `key_retired` | `crates/remote_control/src/signaling.rs:463`, `:478`, test `:7312` |
| P2 join: the check · the refusal · the ledger write · detail · reason · the self-heal | `crates/modules/network/src/overlay.rs:378-420`, `:441-471`, `:790`, `:811`, `:860` |
| P2 order path: `OrderedBy` · `place_order` · the switch route | `crates/modules/network/src/routes/overlay_key.rs:114`, `:156`, `:353`, `:380`; nest `crates/modules/network/src/lib.rs:545`, `:603` |
| P2 DERP | `crates/modules/network/src/derp.rs:440`, `:512-548` |
| P2 DAO: the check · the CAS write · the switch | `crates/services/src/dao/overlay_node.rs:145`, `:346-368`; `crates/services/src/dao/overlay_network.rs:326` |
| P2 indexes | `crates/modules/network/src/lib.rs:788-792` |
| P2 integration tests | `crates/tests/src/retired_key_tests.rs` |

## Not built

- **`roomler overlay rotate-key [--org]` (P3)** over LocalAPI — the break-glass path
  for when the control plane is the compromised thing — and rotation for tunnel-only
  clients.
- **Bulk rotation** — deliberately not offered. Rotating a fleet at once makes every
  peer reinstall every carrier; if it is ever needed it is a paced job, not a button.
