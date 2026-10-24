# Knowhow: the organization's map

Knowhow is the organization's map. It records what exists (machines,
repositories and their checkouts, databases, clusters, services), where each
lives, and how a session reaches each. Its facts are meant to be verified by
probes rather than remembered, and every fact says where it came from. People
read it to find their way around their own infrastructure. From P4-4 agent
sessions read it too, through their toolbelt.

> Spec, steps and the field log: [`fr/FR-90-hive-agent-sessions.md`](fr/FR-90-hive-agent-sessions.md)
> §3e "P4, knowhow, as designed" (#1827). The design is
> [`roomler-hive-design.md`](roomler-hive-design.md) §9. **This page describes
> what is built: P4-1, the module, its store and the people's routes; P4-2,
> the fleet's devices mirrored as machines, with what the fleet and the
> network say of each (§10); P4-3a, a scan asked of a device, delivered,
> answered and written as the device's own part of the map (§11); and
> P4-3b-1 to P4-3b-3, the device's own half on Linux and macOS: its
> checkouts with the services their manifests name, a listed kubeconfig's
> contexts, each root read as the account that owns it, and, when its owner
> says so, the ports it listens on (§11.2); and P4-4-1, a session's
> questions: the word that the map is there, a session's search and card,
> believed from its record and held to its starter's `KNOWHOW_VIEW` (§12),
> asked through the toolbelt's `knowhow_search` and `knowhow_get` (§12.1).**
> People, the sync and scans fill the map. Windows (P4-3b-4), a session's
> proposals (P4-4-2), probes (P4-5) and `roomler_connect` (P4-6) come with the
> steps that build them (§13).

## 1. A map, not a gate

Knowhow grants nothing, and nothing that decides consults it. The vault
decides every credential and every egress, the network every route, and the
device its own gates, as they do without knowhow. A wrong fact misleads a
session toward a host. It never opens anything.

```mermaid
flowchart LR
    P["a person (the map UI, P4-7)"] -->|"reads, writes"| K["knowhow<br/>entities · edges · history"]
    S["a session (its toolbelt, P4-4)"] -->|"reads, proposes"| K
    K -.->|"where things are, never a way in"| S
    S -->|"every reach, asked"| V["vault: credentials, egress"]
    S -->|"every route, asked"| N["network: overlay ACLs, tunnels"]
    S -->|"every command"| D["the device's own gates"]
    style K fill:#f3e8fd
```

> [!WARNING]
> **What the map holds is information, so it is read with a permission of its
> own** (`KNOWHOW_VIEW`, §5): where the production database lives is worth
> keeping from a guest. But no answer of the vault, the network or a device
> ever depends on it. A planted fact can send a session toward a host; the
> reach is still decided as any other is, and P4-6's `roomler_connect` asks a
> person first.

## 2. The model

An **entity** is one thing the organization has
([`model.rs:111`](../crates/modules/knowhow/src/model.rs#L111)):

| Field | Meaning |
|---|---|
| `kind` | what it is: one of `KINDS` (below). Never changes |
| `key` | its name among the live of its kind in the organization: lowercase segments of `a-z 0-9 . _ : @ -` joined by `/` (`github.com/acme/api`, `prod/payments`, `db-1:5432`). Never changes: a chip and a session name an entity by its kind and key, so another name is another entity |
| `name` | what a person reads, up to 200 characters |
| `env` | an environment, one lowercase word (`prod`, `staging`), or none |
| `owner_id` | the member who answers for it, or none. A member of the organization, checked on every write |
| `attrs` | a JSON object, up to 16 KiB stored: what describes it |
| `facts` | what is known of it, each with its provenance (below), up to 64. A machine keeps 15 of them for the sync and its scans, a scan's own entity 9 for its scans (§10, §11.1) |
| `status` | `active`; `archived` once tombstoned. `proposed` and `stale` come with P4-4 and P4-5 |
| `source` | where it came from: `person`, `sync` (§10) or `scan` (§11.1); `session` later |
| `device` | for a scan's own entity, the device whose scans made it, keep it and retire it (§11.1); absent otherwise |
| `rev` | bumped by every write. A write names the rev it read (§3) |

Kinds: `machine`, `repo`, `checkout`, `database`, `cluster`, `namespace`,
`service`, `endpoint`, `bucket`, `cloud_account`, `domain`, `environment`,
`team`, `doc`. An **edge** relates two entities, `from` `kind` `to`
([`model.rs:181`](../crates/modules/knowhow/src/model.rs#L181)), its
kind one of `runs_on`, `checked_out_at`, `deployed_to`, `depends_on`, `reads`,
`writes`, `part_of`, `owned_by`, `documented_in`, `authenticates_with`,
`reachable_via`, with attributes of its own (up to 4 KiB). No rule pairs an
edge kind with the kinds at its ends yet.

```mermaid
flowchart LR
  repo["repo github.com/acme/api"] -->|deployed_to| ns["namespace prod/api"]
  ns -->|part_of| k8s["cluster prod"]
  svc["service api"] -->|part_of| ns
  svc -->|reads, writes| db["database prod-mongo"]
  svc -->|owned_by| team["team payments"]
```

A **fact** is a path, a value and where the value came from
([`model.rs:159`](../crates/modules/knowhow/src/model.rs#L159)):

| Field | Meaning |
|---|---|
| `path` | `[a-z0-9]` then `[a-z0-9._-]`: `port`, `tls.fingerprint`. Stored beside the value, never as a key in the document, so a `.` is safe |
| `value` | any JSON but null, up to 2 KiB |
| `source`, `by`, `device` | `person` and who; `sync`, with nobody; or `scan` and its device (§11.1) |
| `observed_at`, `verified_at` | when it was said; when a probe last confirmed it (P4-5) |
| `status` | `active` for a person's. A session's will be `proposed` until a probe or a person confirms it (decision 39) |

> [!IMPORTANT]
> **A person's change to one fact never restamps another**
> ([`merge_facts`](../crates/modules/knowhow/src/model.rs#L536)). An
> update names facts by path: a value sets that fact, as the caller's; null
> removes it; a fact it does not name keeps who said it, from where and when,
> and so does one set to the value it already holds while active. Restamping
> every fact on every edit would make a person the source of what a device or
> a probe said.

> [!NOTE]
> **One spelling per name, and nothing a document reads as a path.** A key is
> lowercase by rule, not folded, as the vault's paths are: two keys differing
> only in case would be one thing to a person and two to the unique index. An
> attribute's name is a key in the stored document, so it is a word of
> `A-Z a-z 0-9 _ -`, never a `.` path or a `$` operator, at every depth.
> A kind, an edge kind, a status and a source are strings in the row, never
> enums: a newer server writes values this build has never heard of, and a
> reader shows an unknown one. Only a write is held to the lists.

⚠️ **A `machine` is never a person's.** A machine mirrors a device, and the sync
records it (§10), keyed by its node id with its overlay address beside its name
(§10.2): labels move and are reused, and MagicDNS can be dead on a GPO-locked
desktop and is absent on macOS. A create naming `machine` is refused **400**
([`SYNCED_KINDS`](../crates/modules/knowhow/src/model.rs#L46)). A host
that is no device is an `endpoint`.

## 3. The store

Four collections, per organization
([`lib.rs:233`](../crates/modules/knowhow/src/lib.rs#L233)):

- **`knowhow_entities`**, unique on `(tenant_id, kind, key)` among the live (a
  partial index on `deleted_at: {$type: "null"}`, so a live row stores an
  explicit null).
- **`knowhow_edges`**, unique on `(tenant_id, from, kind, to)` among the live,
  so a kind of link between two entities exists once.
- **`knowhow_history`**, every write appended.
- **`knowhow_scans`**, the scans asked of devices (§11).

An organization's map holds at most 10,000 live entities and 50,000 live links
([`dao.rs:28`](../crates/modules/knowhow/src/dao.rs#L28)); a write past
either is **409**.

**A write is a compare-and-set on `rev`**
([`dao.rs:207`](../crates/modules/knowhow/src/dao.rs#L207)): its filter
names the rev the writer read, its update `$inc`s it, and the row after is
returned. A writer that lost the race matches nothing and is told so, rather
than overwriting what it never saw.

```mermaid
sequenceDiagram
    participant A as a writer
    participant B as another writer
    participant M as knowhow_entities
    A->>M: read the entity, rev 4
    B->>M: read the entity, rev 4
    A->>M: update where rev is 4, then rev becomes 5
    M-->>A: the row after, rev 5
    B->>M: update where rev is 4
    M-->>B: nothing matched
    Note over B: 409, read it again
```

⚠️ **The history is appended after the write it records, never before**
([`write.rs:113`](../crates/modules/knowhow/src/write.rs#L113)).
Without a transaction, a row written first could name a write that then lost
its compare-and-set. A row appended after can only go missing: the write
stands, the failure is logged, and the entity still names who changed it last
(`updated_by`, `updated_at`).

**Archiving an entity** tombstones it at the rev it was read, which frees its
key, and archives every live link to it, each in the history with the cause
`entity_archived`
([`archive_links`](../crates/modules/knowhow/src/write.rs#L149)).
Nothing links to an archived entity: a link's ends are resolved live, within
the organization, before it is made. A link the archive raced is invisible,
because a card and a neighbourhood show only the links whose other end is live.

## 4. The switch and the org gate

| Switch | Effect |
|---|---|
| `[modules] knowhow` (`ROOMLER__MODULES__KNOWHOW`) | **default OFF**, like `hive` and `vault`: no route is mounted |
| `knowhow.tenants` (`ROOMLER__KNOWHOW__TENANTS=<id>,<id>`) | the organizations whose map it keeps ([`KnowhowSettings`](../crates/config/src/settings.rs#L163)). A list alone mounts the module ([`knowhow_on`](../crates/config/src/settings.rs#L1624)), the form a hosted server uses, because no older image reads it. Clearing it is the kill switch |

`knowhow.tenants` reads like `vault.tenants`: empty or `*` is every
organization, and an entry that is not an organization id matches nothing
([`scope.rs`](../crates/modules/knowhow/src/scope.rs)). Every route checks, in
this order ([`routes.rs:75`](../crates/modules/knowhow/src/routes.rs#L75)):

1. **membership**: someone outside the organization is told `403 not_a_member`,
   and so learns nothing about whether its map is kept;
2. **the scope**: a member of an organization knowhow does not serve is told
   `404 knowhow is not available to this organization`;
3. **the permission** (§5);
4. **the id**, resolved within the organization: another organization's
   entity answers **404**, as a bogus id does, on every route that names one.

## 5. Who reads and who changes it

| Bit | Grants | In the managed roles |
|---|---|---|
| `KNOWHOW_VIEW` (bit 33, [`role.rs:121`](../crates/db/src/models/role.rs#L121)) | every read: the status, a search, a card, a neighbourhood, the history | in `DEFAULT_MEMBER`, so `member`, `moderator` and `admin` hold it, and `owner` by `ALL`. Never `guest` |
| `KNOWHOW_EDIT` (bit 34, [`role.rs:125`](../crates/db/src/models/role.rs#L125)) | every write, and every read | in `DEFAULT_ADMIN` |

These are decisions 37 and 36 as recommended, built pending the operator's word
(the FR-90 spec, §6). Every alternative stays open: a bit no route reads grants
nothing, so "every member, as a wiki" or `MANAGE_AGENTS` would move the gate
without taking a grant back.

> [!NOTE]
> **The startup reconcile hands both bits to every existing organization.**
> The managed roles are reconciled additively at boot
> ([permissions.md](permissions.md) §3), so the first boot of a build with
> P4-1 grants `KNOWHOW_VIEW` to each org's `member`, `moderator` and `admin`
> rows and `KNOWHOW_EDIT` to its `admin` row, and records each stratum in
> `role_reconcile_audit`. With the module switched off the bits open nothing.

The UI mirror lists both under *Knowhow* (`ui/src/utils/permissions.ts`), as
`2 ** 33` and `2 ** 34`: a shift cannot express either in JS.

## 6. Routes

All under `/api/tenant/{tenant_id}/knowhow`
([`routes.rs`](../crates/modules/knowhow/src/routes.rs)):

| Method | Path | Does | Needs |
|---|---|---|---|
| GET | `` | `{entities, edges}`: how much the map holds, live | `KNOWHOW_VIEW` |
| GET | `/search` | the live entities whose key or name holds `q` (as written, case aside, never a pattern), of a `kind`, in an `env`, by kind and key; `?limit=` 1–200 (default 50) and `truncated`. Each as a node: no attributes, no facts. A kind this build does not know is searched for, not refused | `KNOWHOW_VIEW` |
| GET | `/entity?kind=…&key=…` | one live entity's card, by the name a chip and a session use | `KNOWHOW_VIEW` |
| GET | `/entity/{id}` | its card: `{entity, links[{direction: out · in, edge, other}], truncated}`, up to 200 links to live entities | `KNOWHOW_VIEW` |
| GET | `/map/{id}` | its neighbourhood (§7): `?depth=` 1 (default) or 2, `?kind=`, `?env=` → `{center, entities, edges, truncated}` | `KNOWHOW_VIEW` |
| GET | `/history` | the writes, newest first; `?entity=` one entity's, its links' included, archived or not; `?limit=` 1–200 | `KNOWHOW_VIEW` |
| POST | `/entity` | `{kind, key, name, env?, owner_id?, attrs?, facts?: {path: value}}` → **201**. **409** when a live entity holds the kind and key; **400** for `machine`, a kind or a spelling outside the rules, a null fact, an owner who is no member; **422** for an unknown field | `KNOWHOW_EDIT` |
| PATCH | `/entity/{id}` | `{rev, name?, env?, owner_id?, attrs?, facts?: {path: value or null}}`: `""` clears `env` and `owner_id`; `attrs` is replaced whole; `facts` is merged by path (§2). **409** at a rev already gone; a change that changes nothing writes nothing and answers the entity as it is. `kind` and `key` are unknown fields (**422**). A machine's name, owner and synced facts are the fleet's (**400**, §10), a scan entity's name, owner and scanned facts and any fact a scan said are its device's (**400**, §11.1), and a person's own facts stop at 49 on a machine and 55 on a scan entity (**400**) | `KNOWHOW_EDIT` |
| DELETE | `/entity/{id}?rev=…` | archive it and its links: `{archived, links_archived}`. **409** at a rev already gone, **404** once archived; **400** for a machine, which leaves when its device does (§10), and for a scan entity, which leaves when its device's scans no longer report it (§11.1) | `KNOWHOW_EDIT` |
| POST | `/edge` | `{from, to, kind, attrs?}`: two live entities of the organization → **201**. **409** when they are already linked by that kind; **400** for a link to itself | `KNOWHOW_EDIT` |
| PATCH | `/edge/{id}` | `{rev, attrs}`: its attributes, replaced. Its ends and kind never change: a different link is archived and made again | `KNOWHOW_EDIT` |
| DELETE | `/edge/{id}?rev=…` | archive the link | `KNOWHOW_EDIT` |
| POST | `/scan` | `{device}`: ask a device to scan (§11) → **202** with the scan, or the device's pending one. **409** for a device that cannot scan or is a secondary enrollment; **404** for one that is not the organization's | `KNOWHOW_EDIT` |
| GET | `/scan?device=…` | a device's scans, newest first: `{scans[{id, device_id, requested_by, requested_at, state, delivered_at?, answered_at?, counts, truncated?, detail?}]}`; `?limit=` 1–200 | `KNOWHOW_VIEW` |

## 7. A neighbourhood

The map route walks the links both ways, one step at a time
([`routes.rs:462`](../crates/modules/knowhow/src/routes.rs#L462)). Each step
is one indexed read of the live links from or to the step's entities, at most
500 ([`touching`](../crates/modules/knowhow/src/dao.rs#L265)), each way on
its own index (`(tenant_id, from, deleted_at)`, `(tenant_id, to, deleted_at)`).
The entities they lead to are read live, and filtered by `kind` and `env`; the
centre is always shown. A filtered entity is not walked through. A link is
shown only when both its ends are.

```mermaid
flowchart LR
    C(("centre")) --- A["one step out"]
    C --- B["one step out"]
    A --- A2["two steps out (depth=2)"]
    B -.-|"filtered out: not shown, not walked"| X["another kind"]
```

⚠️ **Bounded at the source.** The design named `$graphLookup`, whose traversal
has no limit of its own: a hub two steps out would load most of the map before
any cap applied. A step that reaches the bound sets `truncated`, and the walk
stops there.

## 8. History

One row per write ([`model.rs:214`](../crates/modules/knowhow/src/model.rs#L214)):

| Field | Meaning |
|---|---|
| `target`, `target_id` | `entity` or `edge`, and which |
| `entity_ids` | the entity, or the link's two ends: an entity's history is read by it, so a link made or archived shows in the history of both its ends |
| `rev`, `op` | the rev the write made; `create`, `update` or `archive` |
| `source`, `by`, `at` | `person` and who, in P4-1; when |
| `set` | what the write set: a create's whole body, an update's changed fields (facts as `{path, value}` rows, a null value for one removed), an archive's status and, for a link an entity's archive took, `cause: entity_archived` |

The history is kept as long as the map: a fact's provenance may be asked for
years after it was written, so it has no TTL.

## 9. Collections

| Collection | One row per | Indexes | Retention |
|---|---|---|---|
| `knowhow_entities` | entity: kind, key, name, env, owner, attributes, facts, status, source, device, rev | unique `(tenant_id, kind, key)` over live rows (partial on `deleted_at: {$type: "null"}`); `(tenant_id, deleted_at, kind, key)`; `(tenant_id, device, deleted_at)`, a device's scan part | tombstoned, kept |
| `knowhow_edges` | link: from, to, kind, attributes, status, source, device, rev | unique `(tenant_id, from, kind, to)` over live rows; `(tenant_id, from, deleted_at)`, `(tenant_id, to, deleted_at)`; `(tenant_id, device, deleted_at)` | tombstoned, kept |
| `knowhow_history` | write | `(tenant_id, at desc, _id desc)`, `(tenant_id, entity_ids, at desc, _id desc)` | kept, no TTL |
| `knowhow_scans` | scan asked of a device: who asked and when, its state, when delivered and answered, its counts and detail (§11) | unique `(tenant_id, device_id)` over pending rows (partial on `pending: true`); `(tenant_id, device_id, requested_at desc)`; `(device_id, state)` | 90 days (TTL on `requested_at`) |

## 10. Machines, from the fleet and the network (P4-2)

Every live device of an organization knowhow serves is a **machine** on the map
([`sync.rs`](../crates/modules/knowhow/src/sync.rs)). The sync is the first of the
design's sources and the most trusted: what the fleet and the network say of a device is
the server's own record, not anybody's claim.

| A machine's | Is |
|---|---|
| `key` | the device's id: the node id, which a re-enrolment of a live machine keeps and a removal retires. It never moves when a label does |
| `name` | the label the device list shows: its display name, else its own name. A label the map cannot keep falls back to the machine's name, then to its id, never to nothing |
| `owner_id` | the device's owner |
| `source` | `sync`; its history rows too, with no person behind them |
| facts, the sync's | what the fleet says (§10.1) and what the network says (§10.2), fourteen paths in all ([`SYNCED_FACTS`](../crates/modules/knowhow/src/sync.rs#L65)), each sourced `sync`, with no `by` |

```mermaid
sequenceDiagram
    participant D as a device
    participant F as fleet (its socket, its routes)
    participant C as core (agent socket, hooks)
    participant K as knowhow
    D->>F: hello (enrolment, every reconnect)
    F->>C: the agent socket's lifecycle
    C->>K: hello, then the machine mirrored off the socket's path
    F->>C: a rename, new tags or a new owner (hooks, in HOOK_ORDER)
    C->>K: the machine brought up to date
    F->>C: a removal (hooks, before the row goes)
    C->>K: the machine archived, and its links
    Note over K: at boot, and every knowhow.sync_secs, a reconcile
```

It hears of a device from core, never from a call by fleet or the network, so the module
still has no edge in the graph. It reads devices through the shared `AgentDao`, as the
vault does, and the overlay through the shared overlay DAOs: a stateless dependency is no
edge (FR-69's rule 5).

- **A hello** mirrors its device: an enrolment's first, and every reconnect. It runs off the
  socket's path, so a slow database never delays a device's session.
- **A rename, new tags, a new owner** reach the machine through fleet's hooks
  (`agent_renamed`, `agent_updated`). The rename hook answers `NoLiveNode`, which never
  displaces the overlay's answer.
- **A removal** archives the machine and every link to it, each in the history.
- **The reconcile** brings every served organization into line. It runs at boot, on the pod
  that holds the startup lease, and every `knowhow.sync_secs` (default 600) on every pod. It
  catches what no hook carries, such as a display name and everything the network says
  (§10.2). It also retires a machine whose device went while knowhow was not looking:
  switched off, or a removal whose hook failed. A second run changes nothing.

> [!WARNING]
> **A sync that fails is logged, never the caller's failure.** No rename, no update and above
> all no removal fails because the map could not be written. The next reconcile tries again.

> [!IMPORTANT]
> **What the fleet says of a machine is the fleet's.** A person's change to a machine's name,
> its owner or a synced fact is refused **400**, because the next sync would undo it. So is
> archiving a machine, which leaves the map when its device is removed. A person may give a
> machine an environment, attributes and facts of their own, and every sync leaves those as
> they are ([`the_fleets`](../crates/modules/knowhow/src/routes.rs#L184)).

> [!NOTE]
> **A machine keeps room for the sync's facts.** An entity holds at most 64 facts, and the
> sync owns 14 of a machine's paths and its scans one more (§11.1), so a person's own facts on
> a machine stop at 49
> ([`persons_room`](../crates/modules/knowhow/src/routes.rs#L247)), and no sync
> takes a machine past 64. An edit that adds none of a person's still passes, so a machine
> that a newly synced path left past its room can still be tidied.

| Setting | Effect |
|---|---|
| `knowhow.sync_secs` (`ROOMLER__KNOWHOW__SYNC_SECS`) | how often each pod reconciles, in seconds; 0 or absent: 600 |

### 10.1 What the fleet says (P4-2a)

`machine_name`, `os`, `arch` (where the device said it), `version`, `ephemeral` and `tags`:
the device row as the fleet keeps it.

### 10.2 What the network says (P4-2b)

| Fact | Is | Present |
|---|---|---|
| `overlay.ipv4` | the address the device leases on the organization's overlay | while it holds a live overlay node |
| `overlay.name` | its name there, the MagicDNS label, as the device list shows it | while the node has a label (every node since names) |
| `overlay.magicdns` | `<label>.<domain>`, the name the device list shows ([`mirror`](../crates/modules/knowhow/src/sync.rs#L222)) | while the organization has a MagicDNS domain |
| `overlay.routes` | the subnets it carries for its peers: approved by an admin **and** still offered by the device ([`effective_routes`](../crates/remote_control/src/models.rs#L6576)), the default route apart; at most 64 | while it holds a node (`[]` for none) |
| `overlay.exit` | whether it carries the default route `0.0.0.0/0` for the peers that choose it, approved and still offered | while it holds a node |
| `tunnel.routes` | the subnets it is a gateway for in the tunnel mesh, as an admin set them | always (`[]` for none); at most 64 |
| `ssh.server` | whether its build serves roomler SSH: the `ssh` verb it advertises, matched whole | always |
| `ssh.policy` | gate 3: `on` while the organization lets it accept sessions, else `off` | always |

```mermaid
flowchart LR
    subgraph network["the network's rows"]
        N["overlay_nodes: the device's live node<br/>its address, label and carried routes"]
        O["overlay_networks: the organization's one network"]
        T["tenants: the MagicDNS domain"]
    end
    A["agents: tunnel routes, the ssh verb, gate 3"]
    H["a hello"] --> S["the sync"]
    R["the reconcile, every knowhow.sync_secs"] --> S
    N --> S
    O --> S
    T --> S
    A --> S
    S -->|"facts sourced sync"| M["the machine"]
    style S fill:#f3e8fd
```

A hello reads the device's own node ([`device`](../crates/modules/knowhow/src/sync.rs#L353));
the reconcile reads the organization's live nodes once, by device
([`device_nodes`](../crates/modules/knowhow/src/sync.rs#L315)). An overlay fact
exists while the device holds a live node and leaves the machine with it: an evicted node's
facts go at the next sync, and the rest of the machine stays, because an eviction forces a new
lease and is no removal.

> [!WARNING]
> **The network tells nobody when it changes, so its word reaches the map late.** A first
> join (which follows the hello), an admin's approval of a route or an exit, gate 3, the
> organization's MagicDNS domain and a device's tunnel routes (fleet's `agent_updated` fires
> for tags and an owner only) reach the machine at the device's next hello or the next
> reconcile, within `knowhow.sync_secs`. Nothing calls knowhow, and a hook on the overlay's
> join path would buy a fresher map with the mesh's own latency, the wrong way round for a
> map.

> [!IMPORTANT]
> **What the network says is no promise of reach.** `ssh.server` says the device's build
> carries roomler SSH's server, not that the device switched it on (its own gate 4, which the
> server cannot see), nor that anyone may open a session: gates 1 and 2 still decide, and
> `ssh.policy` is gate 3 alone ([roomler-ssh.md](roomler-ssh.md)). `overlay.routes` and
> `overlay.exit` are the data plane's (an approval the device no longer offers is no route),
> and the overlay ACL still decides what passes. `tunnel.routes` are still gated by the
> organization's tunnel policies.

⚠️ **A MagicDNS read that fails fails the sync.** Read as "no domain", it would take every
machine's MagicDNS name off the map until a later sync put it back
([`magic_domain`](../crates/modules/knowhow/src/sync.rs#L305)).

**Declared tunnel routes** (`[[tunnel_routes]]`) are not among the facts: the server never
holds them. They live in the device's own config, outside its remote configuration surface,
and the tunnel hub knows only the flows live on its pod. They come with the device's own scan
(§11.1), which sends them from P4-3b-1.

## 11. Scans (P4-3a)

A scan is the server's request and the device's answer (decision 38): a person asks a device
to scan, and the device reads only the roots its owner lists, each as the account that owns it
([`scan.rs`](../crates/modules/knowhow/src/scan.rs)). P4-3a-1 builds the request, its
delivery and the answer's record; P4-3a-2 writes what a scan found onto the map (§11.1,
decision 42). The device's half (the roots, the scanners) is P4-3b's, designed with
decisions 45–47 in the FR-90 spec's §3e "P4-3b, a device's scan, as designed", and built on
Linux and macOS by P4-3b-1 and P4-3b-2 (§11.2). The server's half, with decisions 41–44, is its §3e
"P4-3, scans, as designed".

```mermaid
sequenceDiagram
    participant P as a person (KNOWHOW_EDIT)
    participant K as knowhow, any pod
    participant M as knowhow_scans
    participant H as knowhow, the pod holding the device
    participant D as the device
    P->>K: POST knowhow/scan {device}
    K->>M: a scan, requested (409 if the device cannot scan or is secondary)
    K-->>P: 202 {scan}
    H->>M: requested to delivered, a compare-and-set (at once, a sweep, a hello)
    H->>D: rc:knowhow.scan {scan_id}
    D->>H: rc:knowhow.scanned {scan_id, outcome, findings}
    H->>H: each finding read and checked, counted by kind
    H->>M: answered, once, by the device it was sent to
```

**Who asks, and which devices** ([`request`](../crates/modules/knowhow/src/scan.rs#L483)).
A person with `KNOWHOW_EDIT` (decision 41) asks for a live device of the organization: **202**
with the scan, or with the device's pending scan when it has one, since a scan is never doubled
(a partial unique index on `pending`). A device is refused **409** up front
([`refusal`](../crates/modules/knowhow/src/scan.rs#L145)) when its last hello did not
advertise `knowhow-scan`, since an agent that cannot scan would never answer, and when it is a
secondary enrollment (its caps' `update_mode` is `primary_org`), since its roots are one
device-owned list, scanned for its primary organization alone (decision 43). That is how fleet
refuses an update push a device would ignore (FR-67).

**The delivery: the row is the bus** ([`deliver`](../crates/modules/knowhow/src/scan.rs#L163)).
The pod holding the device's connection tells it, the vault's way, so knowhow needs no edge to
fleet's Hub, which is pod-local:

| When | Who sends |
|---|---|
| the request lands on the pod holding the device | that pod, at once |
| it landed on another pod | the holding pod's next sweep, every `knowhow.scan_sweep_secs` (5 s) |
| the device was offline | the pod its next hello reaches ([`hello`](../crates/modules/knowhow/src/scan.rs#L204)) |
| it was sent to a connection that died unanswered | the next hello's pod, again |

Each first send is a compare-and-set from `requested` to `delivered`, so one connection is told
once; a send that cannot go out within 2 s is put back to `requested` for the next sweep. A
connection is linked at its hello when the device can scan, and unlinked when it closes.

> [!NOTE]
> **Sent again, run once.** A scan delivered to a connection that dies before its answer is sent
> again at the device's next hello. The device runs a scan id once, and the server keeps the
> first answer, so a repeat costs a frame, never a second scan.

**The record.** `knowhow_scans`, one row per request, kept 90 days:

| State | Means | Its fix |
|---|---|---|
| `requested` | asked, not yet sent | the device comes online, or this pod's sweep runs |
| `delivered` | sent, not yet answered | the device answers, or is sent it again at its next hello |
| `done` | answered with what it found, counted by kind | — |
| `no_roots` | its owner lists no root (decision 38) | the owner lists roots on the device |
| `failed` | the device could not scan; why is `detail` | the detail's |
| `expired` | 24 h without an answer | ask again |

**The answer** ([`read_answer`](../crates/modules/knowhow/src/scan.rs#L280)).
Recorded once, and only from the device the scan was sent to: an answer for another device's scan,
one already answered, or a made-up id is dropped and logged. Only a `done` answer's findings are
read, each on its own by the wire's rules
([`Finding::read`](../crates/remote_control/src/knowhow.rs#L224)), and counted:
`checkouts`, `contexts`, `listeners`, `tunnel_routes`, `unknown` (a kind this build does not
know: skipped, so a newer device's answer is never lost whole) and `refused`. An answer past
2,000 findings or 1 MiB is recorded `failed`. A `detail` is clamped to 200 characters.

> [!WARNING]
> **A finding is closed, and never holds a credential.** A known kind with a field this build
> does not know is refused, since an open shape is where a token would ride along unseen. Every
> string is checked for a credential's shape
> ([`holds_credential`](../crates/remote_control/src/knowhow.rs#L334)): a URL's
> password, a token by a prefix it knows, an AWS access key, a PEM block, a JWT. A finding that
> holds one is refused whole, and a `detail` that holds one is withheld. Only the findings
> read are written (§11.1); what was refused is counted, never stored.

| Setting | Effect |
|---|---|
| `knowhow.scan_sweep_secs` (`ROOMLER__KNOWHOW__SCAN_SWEEP_SECS`) | how often each pod looks for scans asked of the devices connected to it, in seconds; 0 or absent: 5 |

### 11.1 What a scan writes (P4-3a-2)

Decision 42: what lives on its device, and nothing else
([`intake.rs`](../crates/modules/knowhow/src/intake.rs)). Its findings are written once its
answer has taken, so a scan is written once, and what it wrote is stamped on its record
(`written`: checkouts, endpoints, repos and clusters made, links made, what it retired, and
whether the map was full).

```mermaid
flowchart LR
    subgraph own["the device's own: source scan, its device named"]
        C["checkout (node id)/(16 hex of the path)"]
        E["endpoint (node id):port/protocol"]
        F["its machine's tunnel.declared"]
    end
    subgraph shared["shared: made bare, never changed, no device's"]
        R["repo github.com/acme/api"]
        K["cluster k8s.example.com:6443"]
    end
    M["machine (the sync's)"]
    C -->|checked_out_at| M
    C -->|"part_of (the remote's name)"| R
    E -->|runs_on| M
    K -->|"reachable_via (its contexts)"| M
    style own fill:#f3e8fd
```

| A finding | Writes | Keyed |
|---|---|---|
| a checkout | a `checkout` named `<last segment> on <machine>`, owned by the device's owner: `path`, `remotes`, `root_commit`, `branch`, `services`; `checked_out_at` its machine; `part_of` the repository of each remote, the remote's name as the link's attribute | `<node id>/<16 hex of the path's SHA-256>` ([`checkout_key`](../crates/modules/knowhow/src/intake.rs#L81)), since a path is not a key (case, spaces, separators) |
| a listener | an `endpoint`: `port`, `protocol`, `address`, `process`; `runs_on` its machine | `<node id>:<port>/<protocol>` |
| a kube context | the `cluster` its server names, made bare; `reachable_via` its machine, the device's contexts for it (name, namespace, CA) as the link's attributes | its server's `host:port` ([`cluster_key`](../crates/modules/knowhow/src/intake.rs#L125)), the scheme's port when it names none |
| a declared tunnel route | its machine's `tunnel.declared`, a list | — |
| a remote | the `repo` it names, made bare | its host and path, lowercase, with no scheme, user, port or `.git` ([`repo_key`](../crates/modules/knowhow/src/intake.rs#L95)). A local path names none |

Every fact and link a scan writes says so: source `scan`, and the device (`device`), with no
person (`by`), as do its history rows. A shared repository or cluster is a scan's (source
`scan`, its history naming the device that made it), but no device's: `device` is empty.

> [!IMPORTANT]
> **A scan is its device's claim about itself.** It writes only entities keyed by its own node
> id and its own machine's scan fact. It makes a repository or a cluster only bare and never
> changes one another source made, including another device's scan. A key a person already
> holds is left as it is. So the worst a lying device does is describe itself falsely, and name
> a repository or a cluster that does not exist.

**A later scan replaces its device's part**
([`retire`](../crates/modules/knowhow/src/intake.rs#L650)):

| The answer | Archives |
|---|---|
| `done`, complete | what the device's earlier scans wrote and this one no longer reports: its own entities with their links, then its own links. No routes declared clears `tunnel.declared` |
| `done`, truncated | nothing: what it left out is unknown, not gone |
| `no_roots` | all of the device's part |
| `failed` | nothing |

Never a shared entity: no device owns one. A device removed from the fleet takes its own
entities with its machine.

> [!NOTE]
> **What a scan says is the device's.** A person's change to a scan entity's name, owner or one
> of its scan's facts, to a machine's `tunnel.declared`, or to any fact a scan said, is refused
> **400** (`its device's scan says this of a checkout: path — change the device instead`), and
> so is archiving a scan entity: it leaves when its device's scans no longer report it. A person
> may give it an environment, attributes and facts of their own, and every scan leaves those as
> they are. A shared repository or cluster is a person's to describe; no scan changes it again.
> A machine now keeps room for 15 facts (the sync's 14 and its scans' `tunnel.declared`), so a
> person's own stop at 49; a scan entity keeps room for its scans' 9, so a person's own stop at
> 55.

### 11.2 A device's scan (P4-3b-1 to P4-3b-3)

The device's half of decision 38: the daemon reads the roots its owner lists, each through a
child running as the account that owns it, and answers with what it found, checked as the
server will check it ([`hive/knowhow`](../agents/roomlerd/src/hive/knowhow/mod.rs)). Linux and
macOS, on every build carrying `hive`. Since P4-3b-2 a checkout's manifests are read, and a file
listed as a root is read as a kubeconfig (decision 46). Since P4-3b-3 the TCP ports the machine
listens on are reported too, when its owner turns that on (decision 47). Windows is P4-3b-4's.

```mermaid
flowchart TD
    F["rc:knowhow.scan, on the primary's connection"] --> O{"this scan id run before?"}
    O -- "yes" --> AGAIN["answered from what it found"]
    O -- "no" --> C["knowhow_scan_roots, read from the config file now"]
    C -- "nothing listed: no root, no listeners" --> NR["no_roots"]
    C -- "knowhow_scan_listeners on" --> L["the machine's TCP listeners, read by the daemon"]
    C --> R["each root resolved, and its owner found"]
    R -- "owned by root, or reached through another account's link" --> U["unread, and why"]
    R --> K["roomlerd knowhow-scan, as that account"]
    K -- "cannot list it, or no longer its own" --> U
    K -- "a folder" --> W["the walk: its checkouts, each read as text, with its manifests' services"]
    K -- "a file" --> KC["a kubeconfig: its contexts, its users never read"]
    W --> A["the answer: checked, bounded"]
    KC --> A
    L --> A
    U --> A
    A --> S["rc:knowhow.scanned"]
```

**The roots.**

| | |
|---|---|
| the key | `knowhow_scan_roots`: comma-separated absolute paths, set in the device's config or over the LocalAPI, `roomler config set knowhow_scan_roots …` ([`config_surface`](../crates/agent-core/src/config_surface.rs#L544)). A folder is walked for its checkouts; a file is read as a kubeconfig (decision 46) |
| empty | nothing is read: the answer is `no_roots` |
| live | read from the config file again at each scan, under the LocalAPI's write lock ([`read_config`](../agents/roomlerd/src/hive/knowhow/mod.rs#L523)): an edit takes the next scan, with no restart |
| whose | the device's: absent from `DesiredConfig`, and named in the test that keeps every device-owned key out of it ([`the_device_owned_refusals_are_not_pushable`](../crates/remote_control/src/models.rs#L9219)) |
| the listeners | P4-3b-3: `knowhow_scan_listeners`, off by default, live and the device's like the roots ([`config_surface`](../crates/agent-core/src/config_surface.rs#L554)). Nothing listed at all, neither a root nor the listeners, is `no_roots` |

**Whose eyes** ([`whose`](../agents/roomlerd/src/hive/knowhow/owner.rs#L78)).

| The root | Read as |
|---|---|
| owned by an account, the daemon root | that account: `roomlerd knowhow-scan` started as it, through the drop an agent session takes ([`run_child`](../agents/roomlerd/src/hive/knowhow/mod.rs#L560)) |
| owned by the account the daemon runs as (a per-user install) | the daemon itself |
| owned by another account, the daemon not root | unread: it reads only its own |
| owned by root | unread: the owner lists the folders below it. A file root owns, a cluster's `admin.conf` among them, is never read |
| reached through a symbolic link that belongs to neither root nor the root's owner | unread ([`resolve`](../agents/roomlerd/src/hive/knowhow/owner.rs#L132)) |
| a file | read as a kubeconfig, by the account that owns it, as a folder would be |
| neither a folder nor a file | unread |

The child is started as hive's own children are: fed its request on stdin, its environment
emptied but for its account's, in a process group of its own, at low priority. It refuses to run
as root at all, and resolves its root again: a root it cannot list, or one no longer its
account's, is refused ([`produce`](../agents/roomlerd/src/hive/knowhow/mod.rs#L195)).

**The walk** ([`walk`](../agents/roomlerd/src/hive/knowhow/walk.rs#L71)).

| Bound | Value | When it is reached |
|---|---|---|
| depth below the root | 8 levels | deeper is never seen, the same at every scan: not a cut |
| entries read | 100,000 | cut |
| time | 60 s a root, which the child keeps; a child 15 s past it is ended whole; 5 min a scan | cut, or the root unread |
| roots | the first 32 | the rest unread |
| filesystem | the root's own | a mount under it is passed by |
| links | never followed | — |
| dot-folders, `node_modules`, `target`, `vendor`, `venv`, `__pycache__`, `bower_components`, `dist`, `build` | skipped | — |

A folder holding `.git` (a folder, or a file naming one: a worktree) is a checkout, and the walk
goes no deeper into it. A folder under the root the account cannot list is passed by: the same at
every scan, so not a cut.

**A checkout** ([`read`](../agents/roomlerd/src/hive/knowhow/checkout.rs#L48)).

| Fact | Read from |
|---|---|
| `path` | where it is, resolved |
| `remotes` | each remote's first `url` in its config (the shared one, for a worktree), its user and password, query and fragment taken out ([`clean_url`](../agents/roomlerd/src/hive/knowhow/checkout.rs#L189)), and one still holding a credential's shape dropped |
| `branch` | `HEAD`; none when it is detached |
| `root_commit` | `git rev-list --max-parents=0 HEAD`, as the account: hooks and fsmonitor off, no prompt, no fetch, neither the system's git config nor the account's, 5 s; only a real git ([`find_git`](../agents/roomlerd/src/hive/knowhow/checkout.rs#L33)), and never in a shallow clone, whose oldest commit is where it was cut |
| `services` | P4-3b-2: its manifests' services ([`services`](../agents/roomlerd/src/hive/knowhow/manifests.rs#L91)). A compose file's services, and a Kubernetes workload's containers (a Deployment, StatefulSet, DaemonSet, ReplicaSet, Job, CronJob or Pod), each a name and an image: the workload's name for one container, `<workload>/<container>` for several. From YAML files three levels down at most: 200 a checkout, 256 KiB each, 64 services. A Helm template is not YAML until it is rendered, and is skipped |

**A kubeconfig** ([`contexts`](../agents/roomlerd/src/hive/knowhow/kubeconfig.rs#L28)), P4-3b-2,
decision 46: a file listed as a root, read by the account that owns it.

| Fact | Read from |
|---|---|
| `name` | the context's |
| `server` | its cluster's `server`, its user and password taken out; a context naming no cluster is passed by |
| `namespace` | the context's, when it names one |
| `ca_sha256` | the SHA-256 of its cluster's CA certificate (the DER of the first one), from `certificate-authority-data` or from the file `certificate-authority` names: the fingerprint `openssl x509 -noout -fingerprint -sha256` prints, so a person can check it by hand. None when the cluster skips verification |

> [!WARNING]
> **A kubeconfig's `users` is never read.** The reader passes the key by at the top of the file
> without keeping a byte of its value
> ([`documents`](../agents/roomlerd/src/hive/knowhow/yaml.rs#L64)'s `skip`): the credentials a
> context uses stay where they are, and nothing a scan sends could carry one. A file that holds
> no `contexts` or no `clusters` is refused as no kubeconfig, never read as an empty one.

> [!NOTE]
> **One YAML reader, bounded.** Manifests and kubeconfigs are read through `yaml-rust2`'s event
> parser, never its loader, which copies an anchor's whole subtree at every alias, so a file of a
> few lines can grow without bound. Here an alias counts as its anchor's size against
> 100,000 nodes a document ([`MAX_NODES`](../agents/roomlerd/src/hive/knowhow/yaml.rs#L18)):
> a document past it is refused, and its file passed by.

**The listeners** ([`listeners`](../agents/roomlerd/src/hive/knowhow/listeners.rs#L32)), P4-3b-3,
decision 47: the machine's TCP listeners, read by the daemon from the system's own table when
`knowhow_scan_listeners` is on. No account's file is opened, so no account's eyes are needed.

| OS | Table | Its process |
|---|---|---|
| Linux | `/proc/net/tcp` and `tcp6`, the rows in state `0A` | the socket's inode found under `/proc/<pid>/fd`, then `/proc/<pid>/comm` |
| macOS | `lsof -nP -iTCP -sTCP:LISTEN -F pctn`, within 10 s | its own lines; `*` is the family's any-address |

Each is a `listener` finding: its port, `tcp`, the address it is bound to, and its process when
the table can say. Each bound address and port is reported once.

> [!NOTE]
> **The daemon's own ports are left out**: its loopback forwards are its declared routes, which
> the scan reports as such. A daemon that is not root sees only its own account's processes, so
> another account's listener comes without its process. A table that cannot be read is one more
> source unread: the answer is `truncated`, and nothing is retired.

> [!WARNING]
> **macOS's `/usr/bin/git` is never run.** Without the developer tools it is a stub that offers
> the person at the console an install dialog. The scanner runs Homebrew's git or the tools' own,
> and names no first commit without one.

**The answer** ([`assemble`](../agents/roomlerd/src/hive/knowhow/mod.rs#L695)).

| Outcome | When | The server then (§11.1) |
|---|---|---|
| `no_roots` | nothing listed: no root, and the listeners off | archives the device's whole scan part |
| `done` | every root, and the listener table when on, read whole | writes what it found, archives what it no longer reports |
| `done`, `truncated` | a root or the listener table unread, a root cut or ended, or the answer past 2,000 findings or 1 MiB | writes what it found, archives nothing |
| `failed` | nothing it lists read | writes nothing, archives nothing |

The primary's declared tunnel routes go first
([`declared_routes`](../agents/roomlerd/src/hive/knowhow/mod.rs#L670)): a secondary's names
another organization's node. Then each root's checkouts, each checked as the server will check it
and dropped when it would be refused, and a checkout two roots share sent once. `detail` names
each root not read and why, and counts what was dropped.

> [!WARNING]
> **Only a scan that read everything it lists retires anything.** A root that is missing,
> refused, unlistable or cut makes the answer `truncated`, so a folder on an unplugged disk, a
> typo, or a folder macOS's privacy protections hide (`~/Desktop`, `~/Documents`, `~/Downloads`
> without Full Disk Access for `roomlerd`) never archives what earlier scans found. Its `detail`
> names the root and why, until the owner fixes the list.

> [!NOTE]
> **Once per scan id, on the primary's connection, off the loop**
> ([`handle_scan`](../agents/roomlerd/src/hive/knowhow/mod.rs#L617)). A frame on a secondary
> enrollment's connection is answered `failed` (decision 43), though the server never sends one.
> A scan runs off the signaling loop, one at a time, and the last 16 answers are kept, so a scan
> delivered again after a lost connection is answered from what it found, never run twice. The
> verb is advertised by every Linux and macOS build carrying `hive`
> ([`rpc_caps`](../agents/roomlerd/src/encode/caps.rs#L1991)), whatever its roots say: an empty
> list is an answer.

## 12. A session's questions (P4-4-1)

A session asks the map through its toolbelt, and its device carries each
question on its primary connection, as it carries a session's lease requests
([`session.rs`](../crates/modules/knowhow/src/session.rs)). P4-4-1a builds the
server's half: the word that the map is there, the two reads, and who is
believed. The device's half (its client, the toolbelt's tools, the session's
settings) is P4-4-1b's. The design, with decisions 48–52, is the FR-90 spec's
§3e "P4-4, a session's reads and proposals, as designed".

```mermaid
sequenceDiagram
    participant D as the device (the primary's connection)
    participant K as knowhow, the pod holding it
    participant H as agent_sessions (hive's record)
    K-->>D: rc:knowhow.served {tools}, at its hello (decision 49)
    D->>K: rc:knowhow.get {req_id, session_id, fence, kind, key}
    K->>H: this organization's? on this device? at this fence? live?
    K->>K: the starter's KNOWHOW_VIEW, read now (decision 48), then the rate
    K-->>D: rc:knowhow.card {req_id, card}, or rc:knowhow.refused {req_id, reason}
```

**The word that the map is there** ([`served`](../crates/modules/knowhow/src/session.rs#L181)). At each hello the
pod's knowhow sends `rc:knowhow.served {tools}` to a device whose hello
advertised `knowhow-tools`, on its primary enrollment's connection, when
knowhow serves its organization. `tools` names what this server answers
(`knowhow_search`, `knowhow_get`), so a newer agent offers a session no tool an
older server would leave unanswered. No other device is sent it: an older
agent would log a frame it cannot read. A device offers its sessions the
tools only on this word, so where knowhow is switched off no session is shown
a tool that no module would answer.

**Who is asking** ([`identify`](../crates/modules/knowhow/src/session.rs#L303)). A question names a session and
its fence; the organization and the device come from the socket. Every
question is checked, in this order:

| Check | Refused |
|---|---|
| knowhow serves the organization (`knowhow.tenants`) | `not_served` |
| the session's record, read through hive's own DAO, is this organization's and runs on this device | `not_located` |
| at the frame's fence | `stale_fence` |
| live: starting, idle, running or awaiting approval | `ended` |
| its starter holds `KNOWHOW_VIEW` or `KNOWHOW_EDIT` in the organization, read now ([`may_read`](../crates/modules/knowhow/src/session.rs#L350), decision 48) | `denied` |
| 120 questions a minute for the session on its device | `busy` |

Then the question itself: `bad_request` for one past its bounds, `not_found`
for a name no live entity holds, `unavailable` when a read failed. Each
connection's questions wait in one queue of 16, answered in order by one task,
so a slow read never holds the socket and a flood is answered `busy` instead
of queued ([`Questions`](../crates/modules/knowhow/src/session.rs#L88)).

> [!IMPORTANT]
> **A session reads what its starter may, now.** The vault holds a session's
> requests to its role, chosen at its start; knowhow reads the starter's roles
> again at every question, as a person's route reads them. A starter whose
> role loses `KNOWHOW_VIEW` stops their sessions' reads at the next question,
> and one who leaves the organization ends their sessions.

**The reads.** What a person's routes answer, bounded for a model:

| Question | Answer | Bounds |
|---|---|---|
| `rc:knowhow.search {q?, kind?, env?, limit?}` | `rc:knowhow.found {entities, truncated}`: the people's search (§6), each entity as a listing shows one, no attributes and no facts | at most 50, 20 by default; `q` at most 200 characters |
| `rc:knowhow.get {kind, key}` | `rc:knowhow.card {card}`: the people's card (§6), by the name a chip and a session use | at most 50 links, cut from the end until the card fits 64 KiB ([`bounded`](../crates/modules/knowhow/src/session.rs#L456)); the entity itself is never cut, its own bounds hold it |

Every entity, fact and link comes with its status and source, so a session
sees what a person said, what the sync and scans said, and (with P4-4-2) what
a session proposed, each for what it is.

| Refusal | Means | Asking again |
|---|---|---|
| `not_served` | knowhow does not keep this organization's map | will not help |
| `bad_request` | the question is malformed or past a bound | not as it is |
| `not_located` | no session of this organization runs on this device by that id | will not help |
| `stale_fence` | the session runs at a newer fence: this run no longer holds it | will not help |
| `ended` | the session is over | will not help |
| `denied` | its starter may not read the map | once their role holds `KNOWHOW_VIEW` |
| `not_found` | no live entity by that kind and key | once one is on the map |
| `busy` | too many questions are waiting, or this minute's are spent | later |
| `unavailable` | a read failed | later |

> [!NOTE]
> **`knowhow → hive`, a stateless DAO.** A session's record is read through
> hive's own DAO, as the vault reads it for a lease (the module graph's edge,
> [`graph.rs`](../crates/core/src/graph.rs)). There is no module state to wait
> on, so a knowhow mounted without hive finds no session and refuses every
> question `not_located`; the `knowhow` build feature implies `hive`.

### 12.1 The toolbelt's tools (P4-4-1b)

The device's half ([`knowhow_tools.rs`](../agents/roomlerd/src/hive/knowhow_tools.rs)):
two tools in a session's toolbelt, `mcp__roomler__knowhow_search` and
`mcp__roomler__knowhow_get`, compiled wherever agent sessions are (Linux, macOS,
Windows), apart from the scanner.

| Piece | Design |
|---|---|
| the verb | every build that runs sessions advertises `knowhow-tools`, so its server tells it whether the map is there |
| the word | the device forgets `rc:knowhow.served` at every primary connection and keeps the tools it names from that frame on, each only when this build knows it |
| the offer | fixed at a session's launch, as the vault's tools are: a run is offered the tools its server named at that moment, and its settings allow exactly those (`permissions.allow`), since a read asks no person (decision 39) |
| the questions | on the primary's connection, a `req_id` each, the session and its fence attached by the daemon, never by the model; each answer matched to its question, 15 s each |
| the tools | `knowhow_search {q?, kind?, env?, limit?}` and `knowhow_get {kind, key}`, each argument checked before anything is asked; the answer is the map's JSON as text, a refusal an error in words the model can act on |

> [!WARNING]
> **A tool the server did not name is an unknown tool.** A newer agent may know
> a tool (P4-4-2's `knowhow_propose`) that an older server never answers: shown
> anyway, every call would wait out its 15 s. So a session is offered only what
> its server named, and a call of anything else is refused before it is asked.

## 13. What comes next

| Step | Builds |
|---|---|
| P4-3b-4 | Windows, every root read as the console user (decision 45) |
| P4-4-2 | `knowhow_propose`: an entity, a fact or a link written `proposed`, naming its session, within a ceiling (decisions 39, 50, 51) |
| P4-4-3 | a person's word on a proposal: confirmed with who and when, or rejected as the routes already archive (decision 52) |
| P4-5 | verification: `rc:knowhow.probe`, `knowhow_verify`, a proposal a probe confirms, staleness as decision 40 says |
| P4-6 | `roomler_connect(entity)`: the best verified path from the session's device, its listener and its credential, each asked of the vault, a person approving |
| P4-7, P4-8 | the map in the web app; `#` chips |
| P4k | the field run in the test organization (AC15) |
