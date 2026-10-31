# FR-47: One address space per org — carve by default, grow without renumbering

**Issue:** [#1071](https://github.com/gjovanov/roomler-ai/issues/1071) ·
**Status:** shipped — P0–P6 complete; **every criterion an agent can reach is field-verified** (AC1, AC2, AC6, AC7 on prod 2026-08-31; AC3 on a throwaway stack 2026-10-30; the docs). Open, and the operator's: AC4–AC5, the growth half (P5a–e) that ships **dark** behind `overlay.multi_block_enabled` (FR-68 P7 decides the flag), and AC8's page, which nothing wires yet (`[~]`) · **Owner:** overlay/networking

## Goal

Every organization gets an overlay address range that is **its own**, from the moment the
org exists, and can **grow past its first block without renumbering a single device**.

Two properties, stated as the acceptance bar rather than as slogans:

- **Isolation is the default, not an opt-in.** Creating an org is enough to get a disjoint
  range. Nothing an operator forgets to switch on can put two orgs on the same addresses.
- **Running out is visible and survivable.** A device that cannot be given an address is
  told so; an org that fills its block is given another one instead of a refusal.

## 1. Field evidence — measured on production, 2026-08-31

`overlay_networks` + `overlay_blocks`, 66 tenants:

| network | cursor (`next_host`) | live nodes | tombstoned | free pool |
|---|---|---|---|---|
| `100.65.4.0/22` (carved) | 36 | 17 | 12 | 1 |
| `100.65.0.0/22` (carved) | 7 | 6 | 0 | 0 |
| `100.64.0.0/10` (legacy) | 3 | 2 | 0 | 0 |
| `100.64.0.0/10` (legacy) | 6 | 5 | 0 | 0 |

### 1a. Carving is OFF in production, so isolation is opt-in and nobody opts in

There is no `ROOMLER__OVERLAY__*` key in the prod configmap
(`k8s/base/configmap-roomler2-config.yaml` in the deploy repo), so the shipped default
`blocks_enabled: false` applies (`crates/config/src/settings.rs:159`, `:913`). Every
organization created today lands on the shared `100.64.0.0/10` with its cursor seeded at 1.

The two legacy rows above are therefore **not merely un-migrated — they overlap right now**.
Both orgs own `100.64.0.1` and `100.64.0.2`; one of them also owns `.3` through `.5`. The
two carved networks exist only because someone renumbered them by hand.

This is the collision class `overlay_blocks` was built to make unrepresentable, still live,
because the mechanism that prevents it was left switched off.

### 1b. Address exhaustion is invisible

`OverlayNetworkDao::allocate_host` refuses correctly and loudly at the DAO boundary. The
join path then discards that (`crates/api/src/ws/overlay.rs:357-366`): on `Err` it logs
`overlay.join: IPAM allocate failed` at WARN and returns, telling the agent nothing at all.

There is no refusal frame in `ServerMsg`, so the agent waits for a netmap that will never
arrive. **A device that cannot get an address is indistinguishable from one that is simply
offline** — including to the operator looking at the dashboard.

### 1c. Roughly half of the oldest org's issued ordinals belong to no document

`100.65.4.0/22` has issued 35 ordinals. 17 are held by live nodes and 1 sits in `free_hosts`.
The remaining ~17 are orphans: released before the recycle pool existed, with nothing to
return them to. No eviction path can reach them, because they belong to no row.

On a `/22` (1 022 usable) that is survivable. The *rate* is the point — it is the org that
has been running longest, and half its issued space has leaked.

### 1d. The two legacy networks are not the same problem

Read before starting P4, and it changed P4:

| network | tenant | nodes | versions | status | last write |
|---|---|---|---|---|---|
| A | **row does not exist** (`9bc1…4472`) | `mbb-mars` `.1`, `mbb-zeus` `.2` | `0.3.0-rc.209` | both offline | 2026-07-23 |
| B | `demo` | 5 nodes, `.1`–`.5` | 0.4.37–0.4.40 | all online | live |

The overlap is concrete: `mbb-mars` and `macbook-pro` both hold `100.64.0.1`.

**A is not migrated, it is cleaned up.** Its tenant row is gone, so there is no
org to migrate and no admin to authorise it, and its agents sit *below* the
`0.3.0-rc.301` floor — a renumber would refuse without `--force` even if one
made sense. ⚠️ It also points at a cascade gap: §13 of `docs/multi-org.md`
records that there is **no tenant-delete route at all**, yet this tenant is gone
and left its network, nodes and addresses behind. Whatever removed it did not go
through `release_overlay_node`.

**B migrates cleanly** — every device is online and far above the floor, so
`below_floor` is empty and no `--force` is needed.

### 1e. What is NOT broken — a correction worth recording

An earlier assessment of this area claimed the allocator had no per-block ceiling and would
bleed into a neighbouring block. **That is false, and was false when it was written.**
`allocate_host` takes `max_host`, rides the bound on the atomic filter
(`next_host` must be `$lte max_host`), returns a loud `DaoError::Validation` on exhaustion,
and discards a pooled ordinal that sits above a since-shrunk ceiling rather than issuing it
(`crates/services/src/dao/overlay_network.rs:250-310`). The test
`allocate_host_stops_at_the_block_ceiling` locks it. Treat "1 022, then it refuses" as
verified behaviour.

## 2. What already exists — the parts this FR must not rebuild

Anchors verified against master at `201e3f5a`:

| Mechanism | Where |
|---|---|
| Lock-free global slot registry — monotonic, buddy-aligned, unique `slot`, `DuplicateKey` retry | `crates/services/src/dao/overlay_block.rs:212-379` (`allocate`, and since FR-68 its two halves `next_free_slot` / `claim_slot`) |
| Variable width `/16` to `/22`; 4 032 slots above the legacy `/16` reserve | `crates/remote_control/src/models.rs` (`block_slots_for_prefix`, `OVERLAY_BLOCK_FIRST_SLOT`) |
| `Quarantined` to `Reclaimed`; `take_reclaimed` as the **exhaustion fallback only** | `dao/overlay_block.rs:232-250` (the `Exhausted` arm), `:407` (`take_reclaimed`) |
| `POST /api/admin/overlay-block/reclaim` — dry-run, age gate, live-occupancy gate | `crates/api/src/routes/overlay_block.rs:866+` |
| `headroom { total, used, burned, reclaimed }`, with two readers | `dao/overlay_block.rs:525` |
| Renumber planner — ordinal preservation, capacity refusal, version floor, WS cycle | `routes/overlay_block.rs:183-255`, `:374+` |
| Per-address v6 derivation, so blocks follow for free | `overlay::router::derive_overlay_v6(Ipv4Addr)` |

## 3. Key design

### 3a. Carving becomes the default (P1)

Flip `blocks_enabled` and set it **explicitly** in the prod configmap. Existing networks are
untouched by construction: `ensure_block`'s virginity guard (default CIDR **and**
`next_host <= 1` **and** an empty pool, `dao/overlay_network.rs:120-131`) refuses to re-base
a network that has already leased anything. Migrating a populated network stays the
renumber endpoint's job.

⚠️ `ensure_block` **falls back to the shared `/10` when a carve fails**, at
`tracing::error!` (`dao/overlay_network.rs:139-148`). That is the right availability trade —
never fail a join over address bookkeeping — but it degrades isolation silently to anyone
not reading logs. This FR wires that line to an alert and adds a `headroom` threshold, so
the fallback stays a monitored condition instead of a quiet regression.

### 3b. Refusal becomes a frame, in one release (P2)

Add `ServerMsg::OverlayJoinRefused { reason, detail }`.

**This is a single-release change, and the reasoning is load-bearing.** An unknown
*top-level* `ServerMsg` variant fails `serde_json::from_str` on a fielded agent and is
absorbed by the existing "ignoring non-rc:* frame" `Err` arm — no panic, no fatal exit. That
is not an assumption: `pre_rc53_server_msg_rejects_goodbye_so_agent_err_arm_fires` in
`crates/remote_control/src/signaling.rs` asserts exactly it, and fires if anyone ever adds
`#[serde(other)]` and changes the behaviour.

The trap that *does* need a forward-compat pre-roll is a new variant of a **nested** enum
inside a frame the old agent already parses — the `RelayStrategyWire` case, where an unknown
tag fails the *whole enclosing `ServerMsg`* and the agent silently drops entire netmaps
(`signaling.rs:5693-5709`, `relay_strategy_lenient`). `OverlayJoinRefused` is not that shape.

### 3c. Multi-block per org (P5) — the growth mechanism

An org's address space becomes an **ordered, append-only list of blocks** rather than one
CIDR. Ordinals are assigned over the *concatenated* space (`1..1022` to block 0, `1023..` to
block 1), which is what keeps every existing `free_hosts` entry valid: blocks are
quarantined, never removed from the middle, so the list only ever grows at the tail.

Four consequences, each a sub-phase:

1. **`BlockList`** in `remote_control::models` generalizing `overlay_ip` / `overlay_host`
   into `ip_for_ordinal` / `ordinal_for_ip`.
2. **The registry's one-block invariant goes.** The partial unique index on `network_id`
   scoped to assigned rows (`crates/db/src/indexes.rs:328-331`) *is* that invariant, so it
   is dropped. It must be dropped before any second block can be allocated, and is
   effectively one-way afterwards.
3. **Allocation extends instead of refusing.** `allocate_host`'s ceiling becomes the sum
   across assigned blocks; exhaustion carves another block. P2's refusal frame then fires
   only for the genuinely terminal case — a full registry.
4. **The wire stays backward-compatible by making `cidr` per-recipient.**
   `OverlayNetworkInfo` gains `cidrs: Vec<String>` (`#[serde(default)]`), and the existing
   `cidr` field carries **the block containing that recipient's own address**. The netmap is
   already shaped per recipient, so this costs nothing structurally.

   Why it works: a fielded agent uses the network CIDR in exactly three places — the TUN
   netmask (`crates/tunnel-core/src/overlay/runtime.rs:2099`), the subnet-router NAT source
   scope (`:2087`), and change detection (`:3662`). All three are correct for an agent that
   is told *its own* block, and peers in other blocks are reached through the per-peer
   `/32`s the agent already installs. ⚠️ This is the compatibility claim the whole phase
   rests on, and it is to be **tested against a pinned old-agent decoder**, not argued.

### 3d. Orphan reclamation (P3)

An admin reconcile that enumerates `1..next_host`, subtracts ordinals held by **live** nodes
and those already pooled, and routes every gap back through
`OverlayNetworkDao::release_host` — never by writing `free_hosts` directly, so the
`$addToSet` de-dupe, the `next_host > host` guard and the `MAX_FREE_HOSTS` cap all still
apply. Dry-run by default, mirroring the block-reclaim route.

Running it twice, a month apart, answers a question nobody can currently answer: whether the
leak is purely historical or still active.

## 4. Phases

| # | Phase | State | Kill switch |
|---|---|---|---|
| **P0** | Spec + ledger row + issue | ✅ shipped | — |
| **P1** | Carving on by default; configmap key; carve-failure alert + `headroom` threshold | ✅ shipped + **field-verified** | `overlay.blocks_enabled` |
| **P2** | `OverlayJoinRefused` frame + `roomler status` surface | ✅ shipped | additive; frame is ignorable |
| **P3** | `reconcile-hosts` admin route | ✅ shipped + **run against prod** (17 ordinals reclaimed) | dry-run default |
| **P4** | Migrate `demo`; clean up the orphaned network | ✅ done — the legacy `/10` holds no live node | existing renumber runbook |
| **P5a** | `BlockList` — the ordinal↔address model | ✅ shipped | — |
| **P5b** | Block list ordered by an explicit `seq` | ✅ shipped | — |
| **P5c** | A full network grows instead of refusing | ✅ shipped, **dark** | `overlay.multi_block_enabled`, default off |
| **P5d** | The wire: `cidrs` + per-recipient `cidr` | ✅ shipped, **dark** | same flag |
| **P5e** | Per-block NAT scope (Linux + Windows) | ✅ shipped, **dark** | same flag |
| **P6** | Docs | ✅ this file + `multi-org.md` §4.2; the 2026-10-30 close-out added the P2 refusal (`multi-org.md` §4.2) and the P3 reconcile (`overlay-communication.md`, `api.md`), which no doc covered | — |

### What is NOT field-verified

⚠️ **A real pre-P5d agent BINARY meshing against a multi-block org.**
`overlay_growth_tests` proves the *server* shapes the netmap correctly — the
1023rd device grows the org over a real WebSocket, is told its own block, and
costs the first 1 022 nothing. It cannot prove an *old daemon* sizes its TUN
netmask and NAT scope correctly from that string. That needs an old binary
against a grown org, and until it is run, multi-block should stay off.

⚠️ **No org has ever actually grown.** The largest holds 17 devices against a
1 022 ceiling, so P5a–P5e is capability built ahead of need. That is why every
phase of it is behind one flag that is off.

## 5. Acceptance criteria

Ticked by the 2026-10-30 close-out, each against the evidence it names. The prod reads of
2026-08-31 were first ticked only in issue comments. AC4 and AC5 are the dark growth half
and the operator's call: FR-68 P7 decides the flag.

- [x] **AC1** A newly created organization receives a disjoint block with no operator action.
      Prod 2026-08-31: the registry's `end_slot` pinned the next org to `100.65.8.0/22`
      before the deploy, and it landed there
      ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5486110349)).
      CI pins the default: `the_shipped_default_carves_a_block_for_a_new_org`
      (`crates/tests/src/overlay_tests.rs:1860`).
- [x] **AC2** Two organizations can never be issued overlapping ranges — asserted by test, and by
      the absence of any `100.64.0.0/10` network in production.
      Tests: `blocks_are_disjoint_across_tenants` (`crates/tests/src/overlay_tests.rs:1622`)
      and `aligned_starts_are_never_partially_overlapping`
      (`crates/remote_control/src/models.rs:9589`). Prod 2026-08-31: no network left on the
      `/10` ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5486110349)),
      and the P3 dry run that listed every network found all four on `100.65.x`
      ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5486189495)).
- [x] **AC3** A device that cannot be allocated an address receives a stated reason, and
      `roomler status` shows it.
      2026-10-30, on a throwaway stack running the server image prod ran that day and the
      released agent 0.4.187: with the org's block exhausted, a joining node's
      `roomler status` printed `join REFUSED 17s ago — address_space_exhausted (retrying
      will not help; an operator has to act)` and the server's detail, and the server
      logged the matching `alert="overlay_join_refused"`. The control, a node of the same
      org that joined before the exhaustion, prints no `join` line. Reads in §8.
- [ ] **AC4** An org that fills its block is given a second one; no device changes address.
      Operator: shipped dark behind `overlay.multi_block_enabled`; the flip is FR-68 P7.
- [ ] **AC5** A pre-P5 agent meshes correctly against a multi-block org — verified with a real old
      binary, not a decoder test alone.
      Operator: the same test as FR-68 AC2, deferred as the operator's call.
- [x] **AC6** The ~17 orphaned ordinals in `100.65.4.0/22` are returned to its pool.
      Prod 2026-08-31: `reconcile-hosts` released exactly 17 (`[1,3,5,8–13,16–23]`) and
      refused 0. The pool then held 18, no pooled ordinal was held by a live node, and a
      second dry run read `orphaned: []`
      ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5486189495)).
- [x] **AC7** Both legacy networks are migrated; `100.64.0.0/10` holds no live node.
      As it happened, one was migrated and one was deleted. `demo` was renumbered onto
      `100.65.12.0/22`, every node keeping its ordinal
      ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5486110349)).
      The other belonged to a tenant row that no longer existed, so there was no org to
      migrate. It was deleted by operator decision after a backup
      ([#1071](https://github.com/gjovanov/roomler-ai/issues/1071#issuecomment-5485248072)).
      Prod 2026-08-31: 0 live nodes on `100.64.x`.
- [~] **AC8** Registry `headroom` is alerted on, and the shared-`/10` carve-failure fallback pages.
      Met: both conditions emit a structured alert line. The carve-failure fallback logs
      at ERROR with `alert = "overlay_block_carve_failed"`
      (`crates/services/src/dao/overlay_network.rs:187-193`). Registry pressure logs at
      WARN with `alert = "overlay_block_registry_pressure"` once a carve takes the slot
      cursor to 80 % or more (`crates/services/src/dao/overlay_block.rs:62`, `:71-84`), a
      threshold pinned by `registry_pressure_crosses_at_the_documented_threshold`
      (`:572`). Missing: nothing routes either line to a page. No alerting rule for them
      exists in this repository, the deploy repository or the cluster repository
      (searched 2026-10-30). The 2026-08-31 comment ticked this box on the log lines
      alone.
- [x] **AC9 — Docs** updated, with diagrams, linked from `docs/README.md`:
      [`multi-org.md`](../multi-org.md) §4.2 (the registry, carve-on-create, both alert
      lines, growth and its races, with a sequence diagram, and the refused join with its
      `roomler status` line and lifecycle diagram) and the §4.3 renumber runbook;
      [`overlay-communication.md`](../overlay-communication.md) (the leaked ordinals and
      `reconcile-hosts`); [`api.md`](../api.md) (the route); the P6 row above. The
      close-out found P2 and P3 documented nowhere outside this spec, and added both.

## 6. Open decisions

- Default carve width stays `/22` (1 022 devices, 4 032 orgs). With multi-block landing in
  P5, growth no longer argues for buying width up front, and the largest org in production
  holds 17 devices.
- Whether `reconcile-hosts` should also run on a schedule, or stay operator-invoked. Decide
  after the second manual run tells us whether the leak is ongoing.

## 7. Out of scope

- Reclaiming the legacy `/10`'s pre-release gaps. Those belong to no document *and* to a
  range that will hold no live node after P4; the `/16` reserve stays reserved permanently.
- Changing the quarantine-by-default policy for freed blocks.
- IPv6 as an independently allocated space — it stays derived per address.

## 8. Field-verification log

| date | version | what was verified | result |
|---|---|---|---|
| 2026-08-31 | prod | Carving off; 2 of 4 networks on the shared `/10` with overlapping addresses; ~17 orphaned ordinals in the oldest carved org | recorded in §1 |
| 2026-10-30 | server image `hosted-20261030-b101fc4` (prod's that day) + released agent 0.4.187, throwaway stack | AC3: a node refused an address says why in `roomler status`. Control first, then the exhausted block (reads below) | **AC3 met** |
| 2026-10-30 | — | Close-out: AC1, AC2, AC6 and AC7 ticked against the 2026-08-31 prod reads; AC8 → `[~]` (the alert lines exist, nothing pages); the docs criterion added, with the P2 and P3 docs it found missing | spec + docs |

### AC3 — `roomler status` names a refused join (2026-10-30)

The stack ran on a private container network with no route out: MongoDB, Redis, the
server from the image production ran that day (`ghcr.io/gjovanov/roomler-ai:hosted-20261030-b101fc4`,
`/health` → `0.4.187`, with the shipped defaults: carving on, `multi_block_enabled` off),
and two containers running the released `roomlerd` 0.4.187 linux build with
`ROOMLERD_AUTO_UPDATE=0`. Each agent reached the server through a loopback relay inside
its own container. The stack has no TLS, and the agent refuses to send an enrollment
token in plaintext to a host that is not loopback (FR-101 P1b).

1. **Control.** Agent A joins a fresh org. The server logs `overlay blocks: carved a
   tenant block … cidr=100.65.0.0/22 slot=64` and leases it `.1`. Its `roomler status`
   carries no `join` line:

   ```
   ● fr47w3-agent-a
     node id     6ae47f9af860…
     version     0.4.187
     mode        service (root)
     tenant      6ae47f31f8603e6bdfbba5fc
     overlay ip  100.65.0.1
     overlay ip6 fd72:6f6f:6d6c::6441:1
     server      connected
     credential  expires in 364 days
     lan         clear (own prefixes route on-link)
     srflx       NONE — cannot hole-punch (no public candidate)
     route guard evicted=0 spared=0 waves=1 [tick=0 pulled=0 event=1] revalidations=0
                (cumulative — DIFF two readings, never judge the absolute)
     direct sock 172.25.0.5:43824  rx=0 last_rx=never
     direct sock 0.0.0.0:44080 (public-dial)  rx=0 last_rx=never
   ```

2. **Exhaust.** In the throwaway database the org's cursor moves past its block's
   ceiling: `next_host` 2 → 1023, `free_hosts` empty. Only the scale is faked, as in
   `overlay_growth_tests`; the allocator's refusal is the real one.
3. **Refused.** Agent B, a second machine in the same org, joins. The server logs:

   ```
   ERROR roomler_ai_mod_network::overlay: overlay.join: refused node_ref=Agent { agent_id: ObjectId("6ae47fd6f8603e6bdfbba60d") } reason=AddressSpaceExhausted detail=Validation: overlay network 6ae47fa7f8603e6bdfbba607 exhausted: every host ordinal up to 1022 is leased (grow the tenant's block or release unused devices) alert="overlay_join_refused"
   ```

   Agent B's daemon logs `ERROR overlay: the server REFUSED our join … retryable=false`,
   and its `roomler status` reads:

   ```
   ● fr47w3-agent-b
     node id     6ae47fd6f860…
     version     0.4.187
     mode        service (root)
     tenant      6ae47f31f8603e6bdfbba5fc
     overlay ip  —
     overlay ip6 —
     server      connected
     credential  expires in 364 days
     lan         clear (own prefixes route on-link)
     join        REFUSED 17s ago — address_space_exhausted (retrying will not help; an operator has to act)
                 Validation: overlay network 6ae47fa7f8603e6bdfbba607 exhausted: every host ordinal up to 1022 is leased (grow the tenant's block or release unused devices)
   ```

   `roomler status --json` carries the same as `join_refusal { reason:
   "address_space_exhausted", detail, at_unix, retryable: false }`. Agent A, read again
   afterwards, still has no `join` line.

`server connected` with no overlay address is the shape §1b found indistinguishable from
an offline node. It now states its reason.

### The state this FR opened on, and the state now

| | before | after |
|---|---|---|
| networks on the shared `100.64.0.0/10` | **2, holding overlapping addresses** | **0** |
| live nodes on `100.64.x` | 7 | **0** |
| orgs with a disjoint block | 2 (both hand-migrated) | **all of them** |
| carving for a new org | off; nobody opted in | **on by default, field-proven** |
| leaked host ordinals in the oldest org | ~17, unreachable | **0** |

`mbb-mars` and `macbook-pro` both held `100.64.0.1`. That is gone, and so is the
mechanism that allowed it.

Two bugs the work surfaced that were not in the original plan, both fixed:

- **A tenant row can be deleted outside the owner-only `archive` path**, leaving
  its overlay network, nodes and agents behind — that orphan is what produced
  one of the two overlapping networks. The instance is cleaned up; the CAUSE is
  not, and deserves its own FR.
- **`ensure_indexes` tolerated only `IndexNotFound` (27)** when dropping the
  one-block guard, not `NamespaceNotFound` (26). 26 is every fresh database, so
  multi-block would have worked on every environment it was developed against
  and broken the first clean production install.
