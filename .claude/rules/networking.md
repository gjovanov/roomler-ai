---
paths:
  - "agents/roomlerd/src/**"
  - "agents/roomler-cli/**"
  - "agents/roomler-cli-shim/**"
  - "agents/roomler-desktop/src/**"
  - "crates/tunnel-core/**"
  - "crates/derp-relay/**"
  - "crates/tcp-turn-conn/**"
  - "crates/localapi/**"
  - "crates/agent-core/**"
  - "crates/modules/network/**"
  - "crates/modules/fleet/**"
  - "crates/remote_control/src/models.rs"
---

# Networking — pillar 2 invariants

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps a one-line pointer per topic plus the cross-pillar invariants. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

Read this before designing or changing anything in the overlay, tunnels, DERP, relays, roomler SSH, fleet RPC, multi-org or remote configuration. Mechanics and history: `docs/overlay-communication.md` and the docs it links.

### Pillar 2 design goal — read before touching networking code

> **A resilient, secure private network that "just works" — at maximum performance
> and lowest achievable latency — across varied and complex real-world network
> infrastructures.**

Six commitments follow. All are load-bearing in the code today; don't regress them
(mechanics: `docs/overlay-nat-traversal.md`, `docs/overlay-communication.md`):

1. **Best carrier that works, always measured, never assumed.** LAN →
   direct-public → srflx hole-punch → single-relay (TURN) → org relay (FR-19) →
   DERP over WSS :443, chosen by a **server verdict over measured `CapVector`s**.
   Heuristics may *detect*; they never *decide*. **Never ratchet** — a node that
   fell to relay keeps re-attempting direct (make-before-break, then relentless
   re-upgrade).
2. **A floor that always connects.** With every UDP path blocked, DERP over TLS
   :443 still carries the mesh (`derp_floor`). Connectivity is never
   all-or-nothing.
3. **Never self-wedge.** Route/exit/firewall changes pin carrier + control-plane
   exemptions FIRST and **withhold** the change if they can't. A mesh feature must
   never cost the operator their own remote access to the box; route guards
   re-assert and boot reconcilers heal stale state after a hard exit.
4. **No OS privileges required as a fallback.** Where a TUN or routing table is
   unavailable or owned by someone else, `overlay-netstack` gives the same mesh
   through a userspace stack + loopback SOCKS5 front, with zero routing changes.
5. **Default-deny, tenant-scoped, end-to-end encrypted** — overlay ACLs, tunnel
   ACLs, and an agent-local gate that survives a compromised server. Every
   decision audited.
6. **Field-validated, not CI-validated.** Proven on the real fleet across the
   topologies above via `roomler exec` after every roll. **CI green ≠ done.**

### Networking

Start at `docs/overlay-communication.md`. Sub-topics: `overlay-nat-traversal.md`,
`overlay-exit-nodes.md`, `overlay-wfp.md`, `multi-org.md`, `magicdns.md`,
`tunnels.md`, `tunnel-install.md`, `roomler-ssh.md`, `fleet-rpc.md`,
`remote-config.md`, `ephemeral-nodes.md`, `device-naming.md`,
`peer-relays.md`.

**Overlay address leases** (`docs/overlay-communication.md` §1) — the release order
is load-bearing: read peers while live → **CAS-tombstone** (winning the CAS is the
release *token*, so two concurrent removals can't pool one host twice) → pool the
host → fan `netmap_delta{removes}`.
⚠️ **Pooling before the tombstone** would hand the address out while the old row
still held it, and the unique index would lock that joiner out **permanently**.
This order only ever *leaks* a host.
⚠️ Rows are **tombstoned, not deleted**, and lookups are live-scoped ⇒ **removal is
final**: a re-enrolled machine gets a fresh lease, never the revived tombstone.
Evict = "force a new lease", **not** a ban.
⚠️ Client-side teardown is keyed by **pubkey**, never by IP.

**Overlay exit nodes** (`docs/overlay-exit-nodes.md`, default-OFF) — an admin
approval writes `is_exit_node` **and** adds `/0` to `approved_routes`; the route
list is the data-plane signal, not the flag alone. Core invariant = **never
self-wedge**: pin `/32`/`/128` carrier + control exemptions first, then install the
split-default, else WITHHOLD.
⚠️ An exit reroutes the host's own *inbound*-reply traffic, so it breaks
un-exempted SSH — and never run the exit field-test on a prod cluster node.

**Multi-org** (`docs/multi-org.md`) — ONE multi-tenant daemon, never N side-by-side
installs. The config's scalar identity stays the PRIMARY enrollment (rollback-safe);
secondaries live in `[[orgs]]` with their **own freshly-minted WG key** (never a
copy — cross-org pubkey correlation), and `rc:agent.update` is honoured only from
the primary.
⚠️ Every legacy tenant shares `100.64.0.0/10`, so tenant A's and tenant B's
`100.64.0.7` are the **same address**. Carved blocks are disjoint by construction
(uniquely-indexed `slot`, buddy-aligned starts — the index arbitrates concurrent
claims, no lock). Freed blocks are **quarantined, never re-issued**: a device that
missed the migration still believes it holds an address there.
⚠️ Joining a **secondary** org does not start its WS loop — the daemon must be
restarted. Signature: `last_seen_at == created_at` forever.

**Peer relays** (FR-19) — a tenant-owned `roomlerd` forwards **ciphertext** between
two nodes of the same tenant over UDP **3478** (the one port the symmetric-NAT corp
population was measured to reach; 41641 and the coturn band are dead for it). Four
default-deny gates, each owned by a different party, every decision audited in
`peer_relay_audit`.
⚠️ It is a third `RelayKind` behind the existing `RelayConn` seam, **not** a new
tier — `is_direct()` reads any new tier variant as DIRECT, silently.
⚠️ Serving **and** use are **primary-org only** — a UDP listener is host-global and
a secondary org's admin must not mint onto the device owner's listener.
⚠️ `static_endpoints` are public `ip:port` literals only, checked at the route
**and** at mint time — a server-pushed probe target is a port scanner run by every
device in the tenant as SYSTEM/root.
⚠️ Org relay engages on a **ladder climb, not the mode flip**: a pair on a healthy
DERP floor won't re-request until it churns.

**Roomler SSH** (`docs/roomler-ssh.md`) — SSH into any node by overlay address with
**no `sshd`, no bound port, no firewall rule**; packets are intercepted below the OS
by `SplitTun`. That is not an elegance choice: binding `overlay:22` fails
EADDRINUSE wherever sshd covers `0.0.0.0:22`, and a corp-managed laptop cannot have
sshd at all. Interception also leaves nothing for an EDR agent to terminate. Four
default-deny gates; `ssh_enabled` alone grants nothing (empty `ssh_authorized_keys`
= nobody).
⚠️ `-R` is **deliberately not implemented** — it would make the device bind a
listening socket, the one thing this design exists to avoid.
⚠️ `direct-tcpip` reads `forward_acl` **default-DENY, the opposite sense to the
tunnel path that shares the struct**: a tunnel flow was already authorized by the
server, but an SSH channel has no server in the path at all, so empty must mean
*nowhere* or every session is a silent open pivot.
⚠️ A key-list session with an unset `ssh_account_mode` authenticates and then runs
**nothing**, rather than quietly taking SYSTEM/root. An unparseable mode is a
refusal, never a fallback.
⚠️ **Session CONTENT is never recorded** — recording a terminal means shipping
whatever the operator typed (passwords into `sudo`, `mysql -p`) off the host, which
is the exact property this pillar exists to provide.
⚠️ `ssh_audit` is the server's own **decision** (authoritative); `ssh_activity` is a
**claim by a host that may be compromised**. Two collections on purpose; join on
`grant_id`. **An empty activity result is not evidence of inactivity** — the
device-owned `ssh_activity_log` defaults to off.

**Fleet RPC** (`docs/fleet-rpc.md`) — `roomler exec` over the agent's **existing
control WS**, deliberately not the overlay: the diagnostics this exists for are most
needed when the mesh is broken. Four independent default-deny gates, the last of
which (`exec_enabled` on the device) is the only refusal that survives a compromised
server.
⚠️ Who runs a command: **root under systemd**. On Windows, **SYSTEM in session 0
through the device's service once an administrator anchored it** (FR-94:
`roomlerd anchor create`, elevated); otherwise the process that holds the agent
socket, the console-session **worker**, which is the signed-in user on an attended
install and SYSTEM in SystemContext mode. ⚠️⚠️ On the SYSTEM path the service takes
**nothing but a request id** from the worker (the user's process): the command is
claimed from the server with the anchor's own credential, and the result goes back
to the server, never to a local pipe. Nothing the signed-in user can write may
decide what SYSTEM runs; `docs/fleet-rpc.md` § Privilege.
⚠️ A caller **awaits** this frame, so unlike `Goodbye`/`UpdateNow` it must gate on
`AgentCaps.rpc` containing `exec` (412 otherwise) — pushing to a pre-feature agent
would hang the caller until its deadline.
⚠️ `roomler exec` **re-splits argv**: Windows targets need ONE quoted argument, and
`;` never `&&`. On a host that restarts its own service it answers "no answer within
45 s" — the command ran.

**Remote configuration** (`docs/remote-config.md`) — the design constraint is **make
the device remotely configurable without making it server-configurable**.
Resolution: a device-owned `remote_config_enabled`, default OFF, **structurally
absent from `DesiredConfig`** (a test asserts it never appears in a serialised
request). Delivery is **reconcile-on-connect only**, so an offline device converges
by the same code path as an online one.
⚠️ **The device REPORTS BACK** — without it, *applied / applied-pending-restart /
refused-not-opted-in / refused-secondary-org / never-arrived* are ONE state on
screen, and each has a different fix. **Compare revisions, not just outcomes.**
⚠️ **Local edits are live too** (`adopt_local`) — making a server push live while
the owner's own edit waited for a restart inverts the very property gate 4 exists
for.
⚠️ **Remote configuration never restarts a daemon** — exiting an orphan `roomlerd
run` host takes it permanently offline. The one restart path is LOCAL (LocalAPI
`RestartDaemon`: "Apply now", `roomler restart`, FR-84 D3), and it refuses unless it
can PROVE a supervisor (`agents/roomlerd/src/supervision.rs`).

**Linux root daemons resolve `/etc/roomler/config.toml`** (rc.435,
`docs/installation.md`). ⚠️ `systemctl is-active` reads **inactive while such a host
is perfectly healthy** (the live daemon is an unmanaged orphan) — check
`pgrep -x roomlerd` and `roomler peers`, and never "restart to fix" on that basis.

**Declared tunnel routes** — `roomlerd` supervises forwards/SOCKS5 listeners
declared as `[[tunnel_routes]]` in its config, reconciled into hub flows on every
start (`docs/tunnel-install.md` §6).

⚠️ **A WebRTC peer MUST be `close()`d — dropping it frees NOTHING.** Its UDP sockets
are owned by tasks the ICE agent spawned, not by the struct, so an `Arc` drop leaves
every one of them live. This once consumed the entire ephemeral port range and
**took host DNS down** while `ping 1.1.1.1` stayed at 3 ms. That signature — names
unresolvable, IPs fine — is socket exhaustion, **not** a DNS-server problem. Full
note, with the diagnostic one-liner: `docs/tunnels.md`.

**Overlay ACL** (the open Known Issue in CLAUDE.md, with its invariant):

- **[LOW]** [2026-08-03] Overlay ACL is feature-complete but **not field-proven
  under `enforce`**. ⚠️ `ingress_rules` is `Option`: `None` = no ACL compiled (fall
  back to the coarse scope), `Some([])` = **deny** — never collapse them. Rules ship
  ONLY under `enforce`, so `warn` can never cause a node to drop. Flip a tenant to
  `warn` first and read `rx_denied` before cutting over.

**Tunnel CLI `self-update`** — requires the artifact's `.asc` against the pinned key on every OS (`pgp_verify`, in `crates/tunnel-core`; FR-97 P6b, shipped in `tunnel-v0.4.149`).
⚠️ It shares the PGP check, NOT `code_signature.rs`: WinTrust stays in `roomlerd`
(tunnel-core is on `windows-sys` **0.61**, the agent on **0.59**), and no OS needed
it, because Windows takes the same `.asc` check.
