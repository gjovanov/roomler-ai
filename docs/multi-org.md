# Multi-org devices, multi-user remote control, cross-org notifications

One machine enrolled in N organisations. N people controlling one host at the
same time. Events from every org reaching a user regardless of which org their
UI is parked on.

This document is the map of that program: what shipped, how the pieces fit,
what an operator has to do, how it fails, and what is still open. It assumes
familiarity with [`remote-control.md`](remote-control.md) (the RC subsystem)
and [`overlay-communication.md`](overlay-communication.md) (the mesh).

---

## 1. The problem

Before this work a roomler device was enrolled in exactly **one** org: one
config identity, one agent JWT, one WS, one overlay identity. Three
consequences, each of which real deployments hit:

1. **A contractor's laptop can't be in two fleets.** Re-enrolling REBOUND the
   config to the new org and silently evicted the old one.
2. **Two people can't work on one host.** The agent pinned
   `max_simultaneous_sessions` to 1, and five latent single-session
   assumptions made anything else unsafe.
3. **A user in three orgs sees events from one.** The UI fetches rooms,
   unread counts and device presence for the ACTIVE tenant only, so an alert
   from another org was invisible until you switched to it.

And underneath all three: **every tenant's overlay shares
`100.64.0.0/10`**, with each network's host cursor seeded at 1. Tenant A's
`100.64.0.7` and tenant B's `100.64.0.7` are literally the same address. That
is harmless while a daemon carries one org and fatal the moment it carries
two — one interface, one routing table, two claimants for the same `/32`.

## 2. Topology: ONE multi-tenant daemon

The alternative — N side-by-side installs — is structurally blocked, and it is
worth recording why so nobody re-proposes it:

- the TUN adapter has a fixed name and GUID, and the rc.279 adapter sweep
  pnputil-removes any non-Up `^roomler` adapter — sibling daemons delete each
  other's interfaces;
- two on-link `/10`s on one host are ambiguous, and each daemon's route guard
  purges what the other installs (a purge war);
- the LocalAPI pipe (`\\.\pipe\roomler`) is a singleton, as are the SCM
  service name and the scheduled-task name;
- exit-node split-defaults, NRPT DNS steering and WFP rules are all
  host-global;
- the updater is per-machine.

So: **one daemon, many orgs**. Everything below follows from that.

---

## 3. P1 — the foundation: `[[orgs]]` (shipped, #313)

The config's scalar identity stays the **primary** enrollment — a pre-multi-org
binary keeps serving it and never reads the new table, which is what makes an
MSI rollback safe. Additional enrollments live in an `[[orgs]]` array:

```toml
# primary (scalar, unchanged)
server_url  = "https://roomler.ai"
agent_token = "…"
agent_id    = "…"
tenant_id   = "…"

[[orgs]]
label        = "acme"
server_url   = "https://roomler.ai"
agent_token  = "…"
agent_id     = "…"
tenant_id    = "…"
enabled      = true
overlay_mode = "off"          # off | netstack | tun — P1 forces secondaries off
```

**Enrollment dispatch** (`apply_enrollment`):

| Case | Behaviour |
|---|---|
| same `(server, tenant)` | refreshes in place, keeping operator state |
| a NEW `(server, tenant)` | **appends** a secondary org with a freshly **minted** WG key |
| `--replace` | the legacy rebind, for when you really do mean "move this box" |

The fresh key mint is a security property, not an optimisation: copying the
primary's WG public key into a second org would let two orgs correlate the
same device by pubkey. `machine_id` IS reused across orgs — the server dedupes
per `{tenant_id, machine_id}`, so every org sees the same fingerprint and a
re-enroll finds the existing row.

CLI: `roomler org ls | rm | enable | disable | set-primary`, `enroll --label`,
`re-enroll --org <label>` (with a same-tenant guard).

**Per-org supervisors.** `run_cmd` starts one supervised loop per enabled org.
`OrgCtx` threads the per-enrollment identity through `run` / `connect_once` /
`handle_server_msg`, which buys:

- per-org watchdog pumps and per-org `DOWN_SINCE` outage stamps (this was a
  process-global static — org B reconnecting would erase org A's outage);
- a secondary's goodbye or duplicate-duel terminates **only its own loop**;
- invalid entries are skipped with a surfaced reason, never fatal.

**Primary-only, deliberately:** `rc:agent.update` (a secondary org's admin
must not be able to force-update a shared binary — the ignore is surfaced via
log + a LocalAPI counter), attention sentinels, exit-route purge, and
`process::exit` escalations. The updater polls the primary's server.

LocalAPI grew `NodeStatus.orgs` (wire-locked; empty ⇒ omitted, so single-org
daemons stay byte-identical) and `RouteDescriptor.org`.

---

## 4. P2 — overlay tenant blocks

### 4.1 P2a: forward-compat (shipped rc.301, #314)

Four changes every agent had to carry **before** any tenant could be carved,
because the first renumber would otherwise brick mixed fleets:

- the boot reconciler's keep-set is prefix-aware: it keeps ANY on-link v4
  block inside the CGNAT `/10`. The old literal `100.64.0.0/` match would have
  purged a renumbered tenant's own connected route at every daemon start —
  host-wide mesh blackhole;
- IPAM is bounded by the network's own CIDR (`cidr_max_host`), so a busy
  tenant can never walk into its neighbour's block. Exhaustion is a loud
  error, never an out-of-block lease;
- netmap deltas process **removes before upserts**, and `install_peers`
  reinstalls on a changed `overlay_ip` under a stable key — pre-P2a a
  renumber's `remove(id) + upsert(id, new_ip)` pair netted to "peer gone";
- `dns::configure_os` is gated on `dns_bound` (steering the OS at a resolver
  that never bound `:53` blackholes the magic domain host-wide — NRPT is
  registry-global on Windows).

### 4.2 P2b: the registry + the migration (this phase)

**`overlay_blocks`** is a GLOBAL registry — deliberately not tenant-scoped,
because its whole job is guaranteeing two tenants never hold overlapping
slices of the `/10`.

- The grid is aligned runs of `/22` **slots** (1024 addresses). A block is
  `slot` + `slots` (a power of two, aligned).
- Allocation is **monotonic upward from slot 64 = `100.65.0.0`**. Everything
  below — the whole `100.64.0.0/16` — is reserved for legacy tenants, which
  all start at `100.64.0.1` and grow upward. A carved tenant therefore cannot
  collide with an unmigrated one no matter how many devices the latter leases
  (it would need 65 534 of them to reach slot 64).
- Non-overlap is **structural, not locked**: `slot` is uniquely indexed and
  starts are buddy-aligned, so two concurrent allocations either collide on
  the same slot (the index arbitrates; the loser retries above the winner) or
  claim disjoint ranges. The property is pinned by
  `aligned_starts_are_never_partially_overlapping`.
- Freed blocks are **quarantined, never re-issued**. A device that missed a
  migration (offline, or a stale binary) still believes it holds an address in
  the old range; handing that range to another tenant would give it a live
  neighbour's address. The row stays as the forensic record of who held it.

**Carve-on-create** is behind `overlay.blocks_enabled` (default **off** — with
the flag off the DAO makes zero registry reads and behaves exactly as it did
pre-P2b). Only a *virgin* network is carved: re-basing a populated one under
live nodes would leave every leased address outside its own CIDR. Migrating a
populated tenant is the renumber endpoint's job, because only that path
rewrites the node rows and cycles the sockets.

**Config:**

| Key | Env | Default |
|---|---|---|
| `overlay.blocks_enabled` | `ROOMLER__OVERLAY__BLOCKS_ENABLED` | `false` |
| `overlay.block_prefix` | `ROOMLER__OVERLAY__BLOCK_PREFIX` | `22` (1022 devices; 4032 tenants fit) |
| `overlay.block_version_floor` | `ROOMLER__OVERLAY__BLOCK_VERSION_FLOOR` | `0.3.0-rc.301` |

### 4.3 The renumber runbook

```bash
TID=<tenant hex>;  TOKEN=<admin JWT>

# 1. Pre-flight. `below_floor` MUST be empty; `capacity` must exceed `nodes`.
curl -s -H "Authorization: Bearer $TOKEN" \
  https://roomler.ai/api/tenant/$TID/overlay-block | jq

# 2. Plan. This is the default — it writes NOTHING and consumes no block.
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{}' https://roomler.ai/api/tenant/$TID/overlay-block/renumber | jq '.new_cidr, .moves'

# 3. Apply, during a maintenance window (see the warning below).
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"dry_run":false}' https://roomler.ai/api/tenant/$TID/overlay-block/renumber | jq
```

What the apply does, in order: quarantine the old block → allocate the new one
→ rewrite every live node's `overlay_ip` → reset the network's cursor and clear
its free pool → cycle every agent's WS.

- **Ordinals are preserved** where they fit (`100.64.0.7` → `100.65.0.7`), so
  notes, `known_hosts` entries and dashboards stay readable. Ordinals past the
  new block — or addresses that don't parse under the old CIDR — are
  **compacted** onto the lowest free ordinal. The planner runs preservation as
  a first pass over the whole set, so a compacted node can never steal an
  ordinal a later node would have kept.
- **The version floor** refuses the apply and names the offending devices
  (a dry run still plans, so you can see the damage first). `force: true`
  overrides. Versions are compared numerically — `rc.99` sorts ABOVE `rc.301`
  as a string, which would have waved through exactly the fleet the gate
  exists to stop. An unknown/empty version fails **closed**.
- **The cycle is why this needs a window.** A node's `self_ip` binds once,
  when its overlay session establishes, so nothing short of re-establishing
  the socket makes a live fleet re-bind. The cycle rides the rc-ctrl lane
  (`overlay_cycle`), so it reaches whichever pod owns each socket. Unlike the
  rehome nudge it does **not** refuse a busy agent — a mesh in use has no idle
  moment to wait for.

> ⚠️ **Corp-VPN hosts.** A cycle tears the agent's rc/tunnel/overlay planes for
> seconds. A host behind a strict corporate VPN loses its established direct
> path and cannot re-punch while the VPN client is armed — it comes back
> **relay-locked** until a VPN-off window. Renumber those hosts deliberately.
> See the pc50045 notes in [`overlay-nat-traversal.md`](overlay-nat-traversal.md).

> ⚠️ **Tunnel-client nodes** are not hub-registered, so the server has no cycle
> primitive for them. They are listed in the response under
> `reconnect_required` and pick the new address up on their next reconnect.

### 4.4 P2c: the shared TUN (agent half)

One `roomler` adapter, N per-org runtimes. Each org's `OverlayRuntime` gets a
`MuxPort` facade (`tunnel_core::overlay::tun_mux`) that looks exactly like its
own TUN; a single reader pump demuxes the real device's packets by
**destination, longest prefix first**. The demux table needs no netmap
plumbing — it is built from the very calls that install OS routes
(`add_peer_route` `/32`s, `add_cidr_route` subnet routes + exit
split-defaults, plus each org's own block from registration), so the OS table
and the demux table cannot drift. Derived-ULA v6 unmaps to its embedded v4;
non-ULA v6 (an exit client's egress) matches the installed v6 routes.

Enable it (default **off**):

```toml
overlay_multi_org = true          # or: roomler config set overlay_multi_org true

[[orgs]]
label        = "acme"
overlay_mode = "tun"              # + the org's own WG key, minted at enroll-append
```

The gate requires all of: the flag, `overlay_mode = "tun"`, the **same
`server_url` as the primary** (one control plane — the demux is only decidable
against blocks one registry carved), and the org's own WG key. The device
comes up with the first org's address; each later org's self-IP is added to
the adapter (`SystemTun::add_address_sync`), whose assignment carries the
block's connected route.

**Why P2b is the prerequisite:** a legacy `/10` org and any number of
carved-block orgs coexist (longest prefix wins, and carved blocks start above
the legacy reserve) — but **two un-migrated `/10` tenants are undecidable**,
and the mux refuses the second at registration (`AddrInUse`, warn-logged).
That org's overlay withholds until one tenant renumbers (§4.3); the other
orgs' meshes stay up.

Still primary-only, deliberately: **exit-node roles** (split-defaults and the
NRPT "." steer are host-global) and **netstack mode** (its SOCKS front and
handle channel are process-global singletons — with `overlay_multi_org` on,
the OS TUN is used regardless of `ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS`).

---

## 5. P3 — remote-control concurrency (shipped, #320 + #323)

Security first (#320): `Hub::dispatch` gained **session-party checks**.
`Terminate` / `forward_ice` / `forward_offer` previously matched on session_id
with a wildcard identity — any authenticated user who learned a session id
could kill it or inject SDP/ICE. The cross-pod terminate is authz'd and no
longer a silent pod-local no-op that returned `{"terminated": true}`.

Then capacity (#323): `rc_max_sessions` (1–8, default 2;
`ROOMLER_AGENT_RC_MAX_SESSIONS`) plus the five latent single-session
assumptions that made >1 unsafe — clipboard's single watcher slot, display-match
restore firing on any session close, per-tx controller unregistration, the
viewer's one-handler-per-type registry, and ungated INPUT/FILES data channels.
Until P6 the server kept **one INPUT holder** per agent; a second session was
created view-effective.

## 6. P5 — shared-floor encoder (shipped rc.303, #325)

Same-profile DC viewers share ONE capture + encoder. Rates merge **pre-encode**
into a floor (keyframe union, MAX frame-skip divisor, min dials): tee-side
dropping of delta frames would break reference chains, because the 13-byte DC
header carries no sequence number. A joiner buffers until the next IDR; a
viewer that sustainedly needs far less than the shared rate spills to its own
encoder, bounded at 2 pipelines total. The viewer badge shows `· shared ×N`.

## 7. P6 — InputArbiter (shipped rc.305/306, #329 + #334)

One process-global injection worker with per-session held state.

- **`free`** (default): all sessions inject; modifiers are **fenced** per
  session — the other sessions' held modifiers (HID `0xe0..=0xe7`) are
  neutralised around each burst, so one user holding Shift can't capitalise
  another user's typing. Cursor tug-of-war is accepted (TeamViewer does the
  same).
- **`exclusive`**: a single INPUT holder with request/grant over
  `rc:control.state`, and a 2 s idle takeover.
- Release-all on teardown, per-event actor audit, and `cursor:peer` ghost
  cursors so each session sees where the others are pointing.

`AgentCaps.input` gates the server's P3 single-INPUT-holder strip: the strip is
lifted only for arbiter-capable agents, so a mixed-version fleet can never end
up in chord chaos.

Two field bugs found and fixed in #334: the arbiter and the shared pipeline
broadcast their state during peer setup, **before** any control channel
existed (followers saw neither the participants chip nor the badge, and could
not request the floor in exclusive mode) — both now fire on control-DC open;
and deregistration moved to `Drop for AgentPeer`, the one point every teardown
path funnels through, which also fixed a display mode that could be left
un-restored.

## 8. P4 — cross-org realtime notifications (shipped, #324)

- **`device:presence`** WS events (online / stale / offline) carrying
  `tenant_id`. Emitted at hub register, at tx-gated unregister, and by a
  **cluster-singleton staleness sweeper** — stale and crash transitions have no
  socket-teardown moment to hook, so without the sweep a killed agent's badge
  would freeze forever. Exactly-once cross-pod emission comes from a Mongo CAS
  on a broadcast ledger; fan-out is coalesced per tenant.
- `tenant_id` added to `notification:new` and `message:create` too — without
  it client-side per-org routing is impossible.
- Consent requests now create an in-app notification row (the enum variant
  existed and was never used), and `notification:unread_count` is emitted on
  read mutations (the client handler existed with no server emitter).
- `GET /api/user/unread-summary` — per-org mentions / unread rooms / device
  alerts, refetched on `ws:reconnected` (replay-free recovery, matching the
  established doctrine).
- The UI routes non-active-org events into a per-org badge store instead of
  dropping them, and the org switcher shows per-org badges.

**Same-tab caveat:** switching orgs redials the shared WS and kills that tab's
RC session. Cross-org remote control means separate tabs.

---

## 8b. Consent names the asking org (#356)

A device in N orgs runs N signalling loops into one host, so "**Alice** is
requesting to control this device" is only half the decision: the same person
can be a colleague in one org and an outside contractor in another.

`ServerMsg::Request` carries `tenant_name`, resolved in
`resolve_session_authz` from the agent row it already loads (one extra tenant
read, only on a real session request) and threaded through `SessionAuthz` →
`DispatchCtx` → `create_session` — the path `consent_mode` and `input_mode`
already take. The cross-pod `rc.cmd` relay forwards it too, so a
foreign-homed device isn't left with an unlabelled prompt.

The agent falls back to its own org **label** when the server sends nothing
(older server), and to nothing at all on the primary enrollment — so a
single-org device looks exactly as it always did. `localapi::ConsentRequest`
carries `org`, and the tray modal renders an "On behalf of …" row, hidden
when the field is empty (an empty value is omitted from the JSON entirely, so
no blank row can render).

---

## 9. Failure modes

| Failure | Behaviour | Where |
|---|---|---|
| One org's server is down / its token is revoked | Only that supervisor backs off and retries; other orgs are untouched, each with its own `DOWN_SINCE` | P1 |
| Pod roll | Every org's WS reconnects independently and re-lands on the current LB hash; rc/tunnel/media/derp rehome per the S6 cluster layer | P1 + S6 |
| Half-open WS (middlebox ACKs pings after the upstream leg dies) | Per-connection receive-liveness deadline (rc.293): no inbound frames for 80–90 s ⇒ reconnect | pre-existing, per-org since P1 |
| Redis down | Presence sweeper aborts rather than mass-emitting from a partial view; per-pod fan-out degrades to pod-local; the UI's slow poll is the belt | P4 |
| Agent crash mid-renumber | Node rows are written before the network row, so the worst case is nodes already on new addresses with the network still describing the old range — re-running the renumber is idempotent from there | P2b |
| Renumber loses the block-allocation race | The old block is already quarantined and the network keeps its old CIDR — safe, and the retry allocates above | P2b |
| A device misses a renumber (offline) | It rejoins with a stale `self_ip`; peers already hold its new `/32`, so it is unreachable until it reconnects. Its old range is quarantined, so nothing else can claim it | P2b |
| An agent below the version floor is force-migrated | Its boot reconciler purges its own on-link route ⇒ that host's mesh blackholes until it updates | P2b |
| Two viewers, one host, mixed versions | An agent without `AgentCaps.input` keeps the P3 single-INPUT strip; only arbiter-capable agents go free-for-all | P6 |
| Encoder spill oscillation | Hysteresis on the deviation gate, hard cap of 2 pipelines | P5 |

## 10. Scale notes

- **Presence storms.** A pod roll reconnects a whole fleet within seconds.
  Fan-out is coalesced per tenant (`rc.presence_batch_ms`, default 2 s) and
  recipients are resolved once per tenant from a cached role→member set, so a
  200-device fleet produces one WS event per tenant, not 200.
- **Block space.** `/22` blocks give 4032 tenants × 1022 devices. A tenant that
  outgrows its block renumbers into a wider one (`prefix: 20` → 4094, `16` →
  65 534); the old block is quarantined, so growth costs address space rather
  than risking a collision.
- **Sessions per host.** `rc_max_sessions` defaults to 2. P5 makes additional
  same-profile viewers nearly free (one capture + one encoder); different
  profiles still cost a pipeline each, bounded at 2.

## 11. Test map

| Area | Tests |
|---|---|
| Config v2 + enrollment dispatch | `agents/roomler-agent/src/config.rs`, `enrollment.rs` units; the two-tenant integration test driving ONE agent lib into TWO TestApps |
| Block grid + allocator | `crates/remote_control/src/models.rs` units (grid rendering, prefix→width, buddy-alignment property, in-block leases) |
| Renumber planner + version floor | `crates/api/src/routes/overlay_block.rs` units (preservation, compaction ordering, determinism, capacity refusal, numeric rc compare, fail-closed unknowns) |
| Renumber end-to-end | `crates/tests/src/overlay_tests.rs` — inert dry run, apply + invertibility + next lease, double renumber ⇒ quarantine trail, cross-tenant disjointness over mixed widths, floor refusal + force, permission gate, carve-new-networks-only |
| IPAM ceiling (P2a) | `allocate_host_stops_at_the_block_ceiling` |
| Presence | sweeper claim/dedupe + payload `tenant_id` contract locks |
| InputArbiter | pure state-machine units incl. `close_leaves_no_residue_for_the_next_session` |
| Shared encoder | floor-merge, joiner-on-IDR, spill hysteresis |

## 12. Open items

- **P2c field validation** — the shared-TUN mux (§4.4) is field-proven for
  the two-org case (five hosts, full cross-org reachability, 2026-08-05) and
  for the clean-join path. What is NOT field-tested is the REFUSAL path
  leaving no address behind: it needs a third org still on the legacy `/10`,
  and there is no tenant-delete endpoint, so creating one would leave a
  permanent tenant behind. Unit-locked only (`tun_mux` tests). macOS is
  excluded (`add_address_sync` refuses — utun aliasing is future work).
- **Netstack per-org statics** (`NS_HANDLE`'s single watch channel,
  `SOCKS_BOUND` once-ever) keep netstack single-org; `overlay_multi_org`
  overrides netstack mode to the OS TUN, and a foreign-server org still has
  no overlay path at all.
- **A removed org's adapter addresses linger** until the daemon restarts. A
  REFUSED registration now rolls its address back (`TunMux::deregister` +
  `SystemTun::del_address_sync`), but an org that is disabled or removed at
  runtime still leaves its address up. Harmless — the address answers nothing
  once its runtime is gone — but untidy on a long-lived host.
- **Block reclaim** — quarantined blocks are never automatically re-issued.
  With 4032 slots this is deliberate; a reclaim path can be added if a
  deployment ever churns tenants hard enough to matter.
