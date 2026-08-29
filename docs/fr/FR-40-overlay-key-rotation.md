# FR-40: Rotate a device's overlay key from the dashboard — the server orders a re-mint it never sees

Status: **P0 shipped; P1 implemented (PR #963), awaiting release 0.4.24 + field verification on CORPLAP-3** (2026-08-30). Tracking issue: `FR-40` (#962).
Sibling of the remote-configuration work (`docs/remote-config.md`) — same push / report-back /
reconcile-on-connect shape — and of `rc:agent.update`, which is the operator's mental model for
it ("update now", but for the key).

## Goal

An admin can retire a device's WireGuard overlay identity from the device grid, the way they
push an update: the device mints a fresh key **locally**, persists it, re-joins the mesh under
it, every peer reinstalls it within seconds, the old key is useless everywhere, and the
dashboard shows — honestly — whether that happened. A device that is offline rotates on its
next connect. The server never holds, transports or chooses a private key at any step.

## What happened (field evidence, 2026-08-29)

A diagnostic over the Fleet RPC read CORPLAP-3's non-secret overlay/relay/encoder settings with
`Select-String -Path config.toml -Pattern '^(overlay|relay|…)'`. `overlay_wg_secret_key`
starts with `overlay`. One value — the primary org's WG secret of one single-org device — landed
in a session transcript (a local file plus the model API). The agent token and the SSH host key
were not printed (re-checked with a masked scan of the transcript, 2026-08-30).

There is **no remedy for that today** short of re-enrolling the device: no rotation on the CLI,
none over LocalAPI (`overlay_wg_secret_key` is deliberately unwritable there,
`crates/agent-core/src/config_surface.rs:2107-2118`), none on the web. Re-enrolling needs a fresh
enrollment token, local or `exec` access as SYSTEM/root, a daemon restart, and it is not what an
operator reaches for when a key leaks — they reach for "rotate".

What the key is worth to whoever holds it: it is the device's whole **data-plane identity**. A
holder can complete WireGuard handshakes as that node with every peer that has its public key
installed — everything the overlay ACL grants the node, and its *inbound* (SSH grants dial the
overlay address, FR-19 relay sessions, tunnel routes). The control plane is NOT reachable with it
(joins and DERP registration need the agent JWT, `crates/api/src/ws/derp.rs:230-254`), and DERP
additionally refuses a registration whose pubkey is not the row's (`derp.rs:339-362`).

## What is in force today (verified on master `f6bc3ffd`)

| piece | where | note |
|---|---|---|
| primary key mint | `agents/roomlerd/src/main.rs:2113-2121` | lazy, at daemon start, `WgKeypair::generate()` → `config::save`; the ONLY primary mint |
| secondary-org mint | `crates/agent-core/src/enrollment.rs:328-334` | at enroll-append; never copied from the primary (cross-org correlation) |
| storage | `crates/agent-core/src/config.rs:1008` (primary), `:1132` (`OrgEntry`), `for_org` at `:1222-1245` | the key is **per org** — `for_org` scopes it, unlike `exec_enabled`/`ssh_*` which are host-global |
| atomic save | `config.rs:2130` | tmp + `sync_all` + 0600/ACL + `.prev` + rename, under the daemon-wide write lock (`org_join.rs:48`) |
| how a session gets its key | `agents/roomlerd/src/signaling.rs:442-447`, `:881-883`; `overlay.rs:210-217` | from the in-memory `AgentConfig` snapshot taken at start — **never re-read from disk per session** |
| runtime identity | `agents/roomlerd/src/overlay.rs:79-86` (`RuntimeFingerprint::same_shape` includes `wg_public_key`), `:236-243`, `:417-425` | a changed secret already fails re-attach and rebuilds the runtime |
| existing per-org cycle | `main.rs:2809-2846` (`spawn_org`: stop the previous loop, re-load config from disk, respawn) | the primary loop has NO stop handle |
| server join | `crates/api/src/ws/overlay.rs:232`, `:279-297` (`wg_key_taken_by_other`, machine-scoped ⇒ a same-machine rotation is allowed), `:299-390` (`rehydrate` stores `wg_public_key` + `key_epoch`) | `key_epoch` is stored and **read by nothing** (`models.rs:2651`; the agent always sends 0, `runtime.rs:1921`) |
| peer fan-out | `overlay.rs:531-561` → `OverlayNetmapDelta { upserts, removes }` (`signaling.rs:1824`) | an upsert IS the update; the re-join already carries the new key to every peer |
| peer reinstall | `crates/tunnel-core/src/overlay/runtime/establish.rs:1147-1180` | "peer's WG public key changed — reinstalling its carrier" (`PeerRoute::Keep`) |
| DERP | `crates/api/src/ws/derp.rs:339-362` (pubkey must equal the row's), `derp_acl.rs:99-140` rebuilt on every join (`overlay.rs:481-490`) | the row must carry the new key BEFORE the node re-registers — the join does exactly that |
| the push precedent | `crates/api/src/routes/remote_control.rs:831-872` (`trigger_agent_update`, `MANAGE_AGENTS`, `Hub::send_to_agent`), agent arm `signaling.rs:2740-2762` (primary-only) | `UpdateNow` is sent BLIND — no cap gate |
| the report-back precedent | `ClientMsg::ConfigStatus` (`signaling.rs:461`), `record_config_report` (`ws/remote_control.rs:940-985`), `config_audit` (`models.rs:2220-2241`) | the shape to copy: revision-bumped request, device reports the revision, server resolves ONE state |
| capability verbs | `crates/remote_control/src/models.rs:271-373` | `RpcCap` — no rotation verb exists |
| UI | `ui/src/components/admin/AgentsSection.vue:1853-1875`, `ui/src/stores/agents.ts:678-686` | "Update now"; **no view shows the overlay public key anywhere** |
| retired keys | — | none: `wg_key_taken_by_other` is live-scoped, a tombstone's key neither blocks nor denies |

## Design

### Invariants

1. **The server never sees a private key.** The push is an *order to mint*, never a delivery.
   `DesiredConfig` stays structurally unable to carry a key; the only key-shaped field on the
   wire is a PUBLIC key in the device's report, and a test asserts the serialised order has no
   such field.
2. **Per org, honoured on any org's WS.** The key is per org by design (`for_org`), so org B's
   admin rotating org B's key on a shared host touches nothing of org A's. This is the opposite
   of `rc:agent.update` / config push, which are host-global and therefore primary-only
   (`docs/remote-config.md` §4). The handler must not copy that guard.
3. **Persist before anything else.** Mint → save under the write lock → *then* report and
   reconnect. If the save fails the identity stays and the device reports `failed`, the same rule
   the SSH host key follows (an unpersisted key would be lost at the next restart and the device
   would come back as the key it just retired).
4. **One data-plane path.** The rotation reuses a reconnect: the WS loop that received the order
   replaces the key in its own snapshot and re-enters `connect_once`; `maybe_start` sees the
   fingerprint mismatch and rebuilds the runtime; the join carries the new pubkey; the server's
   existing upsert fan-out and the peers' existing reinstall arm do the rest. Nothing new is
   added to the packet path.
5. **Immediate and disruptive, by design.** The rotation ends every session the device carries
   on that org (RC, SSH over the overlay, tunnel flows) — a key that leaked must not wait for
   the holder's session to end. The confirm dialog says so. ⚠️ A corp-VPN host may come back
   relay-locked after the cycle (the renumber runbook's caveat, `docs/multi-org.md`).
6. **The device reports back, and the join is the proof.** A report is a claim by the device
   (`ssh_activity` sense); the join under the new key is what the server can verify. The
   dashboard state is resolved ONCE server-side from `{request, report, node row}` — never in the
   client.

### The order — `rc:agent.key_rotate`

`ServerMsg::KeyRotate { request_id }`. Minted by `POST /api/tenant/{tid}/agent/{id}/overlay-key/rotate`
(`MANAGE_AGENTS` — this grants nothing, it retires something; no `EXEC_DEVICE`/`SSH_DEVICE`
bit is involved). The route:

1. resolves the device in the tenant (404 otherwise, no existence leak);
2. records the request on the agent row: `key_rotation = { request_id, requested_at, requested_by }`
   (the *desired state*, so the offline case has somewhere to live);
3. if the device is online **and** advertises `key-rotate`: pushes → `delivered: true`;
   online without the verb → `refused: agent_unsupported` (409 — the old-agent frame would
   evaporate silently, the failure `RpcCap::Config`'s doc was written about); offline → `queued`;
4. writes ONE audit row for either arm (`key_rotation_audit`, 90 d TTL; `decide()` returns
   `Result<Pushed|Queued, DenyReason>` and a single call site records both, the `agent_ssh::dispatch`
   shape, so a new refusal cannot forget to audit itself);
5. per-device ceiling: one request per 60 s (`rate_limit.rs`, after the identity gates so the
   refusal is attributable).

Capability: `RpcCap::KeyRotate` (wire `key-rotate`; equality match, `ALL` entry, wire string
locked by test).

### On the device

Handler in the WS loop that received the order (`signaling.rs`, next to `ConfigPush`):

1. kill switch `overlay_key_rotation` (tribool, default on; config-surface key + env
   `ROOMLERD_OVERLAY_KEY_ROTATION`) — off ⇒ report `refused: disabled`;
2. rate limit: an order < 60 s after the last rotation on this org ⇒ `refused: rate_limited`;
3. `WgKeypair::generate()` (same mint as enrollment); take the daemon write lock,
   `config::load(path)`, set `overlay_wg_secret_key` on the primary scalar or on the `[[orgs]]`
   entry whose `tenant_id` is this loop's, bump `overlay_wg_key_epoch` (new, persisted next to
   the key; `0` when absent), `config::save`;
4. report `rc:agent.key_rotated { request_id, outcome, old_public_key, new_public_key, key_epoch, detail }`
   on the CURRENT session (it is about to end);
5. replace the key + epoch in the loop's own `cfg` snapshot and return
   `ConnectError::KeyRotated` — an immediate reconnect (no stagger: this is one device, not a
   fleet event). `maybe_start` rebuilds the runtime (fingerprint mismatch), the join sends the
   new pubkey and the bumped `key_epoch`.

Builds without an overlay surface refuse `unsupported` and never advertise the verb.
`overlay_wg_secret_key` stays unwritable over LocalAPI — rotation is an ACTION, not a config
write, and the local `roomler overlay rotate-key` (P3) goes through the same action.

### Re-join and the mesh

Unchanged code, now exercised on purpose: `wg_key_taken_by_other` admits the same machine;
`rehydrate` stores the new key + epoch; the per-recipient upsert fan-out carries it; peers hit
the reinstall arm; DERP ACL is rebuilt. ⚠️ Known bounded transient: `derp_acl::rebuild` is
spawned at join, so the node's first DERP frames under the new key may be denied for
milliseconds until the table lands — WireGuard's handshake retry covers it. Measure it in the
field rather than adding a barrier.

### Report-back and the state the dashboard shows

Server ingest of `rc:agent.key_rotated` stores `key_rotation.report` on the agent row (the
claim). `KeyRotationState` is resolved server-side from request + report + the live node row:

| state | meaning | operator's move |
|---|---|---|
| `none` | never requested | — |
| `queued` | requested, device offline, no report | wait; it rotates on connect |
| `delivered` | pushed, no report yet | wait a few seconds |
| `rotated` | report says rotated **and** the node row's `wg_public_key == report.new_public_key` | done |
| `reported_not_joined` | report says rotated, but the row still shows the old key | the re-join failed — read the device log |
| `refused: <reason>` | disabled / rate_limited / unsupported / failed | fix the stated thing |
| `unsupported` | device online without the verb | update the device |

⚠️ Compare `request_id`s, not outcomes — a report about an earlier request says nothing about
this one (the remote-config lesson). The device row also gains `overlay_public_key` (short form,
copyable) and `key_epoch`, so the operator can SEE the key change instead of trusting a chip.

### Offline devices — reconcile on connect

At agent register (`ws/remote_control.rs:176-195`, where `ConfigPush` reconciles), a pending
`key_rotation` with no report is pushed if the hello advertises `key-rotate` — the same path as
the online case, so it runs on every connect rather than only when nobody is watching.

### Retired keys (P2)

On a pubkey change at join, the old key is appended to the node row's `retired_keys[]`
(`{ public_key, key_epoch, retired_at, reason: rotate|rejoin }`, multikey index). A join or a
DERP registration presenting a retired key is refused with its own reason (`key_retired`) —
defense in depth for the `.prev`-rollback and the "attacker holds both the WG key and the agent
token" cases — and, when the device advertises `key-rotate`, the refusal is answered with a
`KeyRotate` order instead of a dead join: a device that rolled back to a retired key heals itself.
Also adds the missing index on `(network_id, wg_public_key)` (`wg_key_taken_by_other` is an
unindexed `find_one` per join today).

### Multi-org

| | `rc:agent.update` / config push | `rc:agent.key_rotate` |
|---|---|---|
| scope of the thing changed | host-global | this org's key only |
| honoured on | primary WS only | the WS the order arrived on |
| disk write | scalar keys | primary scalar OR the matching `[[orgs]]` entry |

### Kill switches

`overlay_key_rotation` (device, default on). No server switch: the route is admin-initiated
and permission-gated; a defective push is stopped by the device switch.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | spec + issue + ledger claim | — | this |
| P1 | verb + order + device mint/persist/report/reconnect + route + audit + ceiling + reconcile-on-connect + UI action/state/pubkey column; release | `overlay_key_rotation` | implemented — PR #963 (server + agent + UI, unit + integration tests); release + field verification pending |
| P2 | retired keys: refuse at join + DERP, self-heal order, pubkey index | none needed (refusal is fail-closed) | open |
| P3 | `roomler overlay rotate-key [--org]` over LocalAPI (break-glass when the control plane is the compromised thing); tunnel-only clients (`roomler` standalone) | — | open |

## Acceptance criteria (field, the real device: CORPLAP-3, single org, `0.4.24+`)

- [ ] the route on a device advertising the verb returns `delivered`; the device log shows
      mint → save → report → reconnect; the node row's `wg_public_key` and `key_epoch` change
      within 10 s and the dashboard reads `rotated`
- [ ] neo16's `roomler peers --json` shows CORPLAP-3 under the NEW public key with a working
      carrier, and traffic flows (RC session or overlay ping) within 30 s of the click
- [ ] the old public key is on no peer's WG device afterwards (`roomler peers` on ≥ 2 peers)
- [ ] a device on `0.4.23` (no verb) gets `unsupported` on the route, not a spinner
- [ ] a device that is offline is `queued`, and rotates on its next connect with no operator action
- [ ] a second click within 60 s is `refused: rate_limited` and audited
- [ ] `overlay_key_rotation = false` on the device ⇒ `refused: disabled`, key unchanged
- [ ] the request and the report carry public keys only — a test asserts the serialised
      `KeyRotate` frame has no key-shaped field, and the audit row stores none
- [ ] (P2) a join presenting a retired key is refused `key_retired`, and a device with the verb
      is ordered to rotate instead of staying off-mesh

## Open decisions

- **Self-heal on a retired key (P2)** — auto-order vs. surface-only. Leaning auto-order: the
  alternative is a device that is on the control plane and off the mesh with nothing to press.
- **Whether the ledger of retired keys should live per network or per node.** Per node keeps the
  forensic record with the holder (the tombstone convention); the index makes the check cheap
  either way.
- **Bulk rotation** — deliberately not offered. Rotating a fleet at once is a storm (every peer
  reinstalls every carrier); if it is ever needed it is a paced job, not a button.

## Out of scope / what this does NOT rotate

- The **agent token** (JWT, 1 y) — a different identity with a different fix (`token_epoch`,
  Known Issues). A leaked token is a control-plane compromise; this FR is about the data-plane
  key.
- The **SSH host key** (`ssh_host_key`) — clients pin it; rotating it is a client-side TOFU
  event and deserves its own runbook.
- Scrubbing `config.toml.prev` — the retired key remains on disk there until the next save; an
  on-host reader of `.prev` can read `config.toml` too, so it is not a boundary this could
  defend. P2's retired list makes the copy worthless off-host.
- Tunnel-only clients (P3) and the standalone `roomler` CLI's own key.

## Field-verification log

| date | build | note |
|---|---|---|
| 2026-08-30 | — | P0: exposure bounded to ONE device / ONE key by a masked transcript scan; no token, no SSH host key. |
