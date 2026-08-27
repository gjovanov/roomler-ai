# FR-19: Peer relays — tenant-owned UDP relay nodes between direct and DERP

Status: **proposed** (2026-08-28). Tracking issue: [`FR-19` (#805)](https://github.com/gjovanov/roomler-ai/issues/805).
Reference design: [Tailscale peer relays](https://tailscale.com/docs/features/peer-relay).
Sibling of FR-18 (#801) and FR-17 (#799) — both are about the *cost* of the relay path;
this FR is about **replacing that path with a better one** rather than tuning it.

---

## Goal

Let an org nominate one of its own enrolled nodes as a **peer relay**: a `roomlerd` that
forwards *ciphertext* between two other nodes of the same tenant over UDP, on a port the
operator chose, without decrypting anything and without the roomler control plane in the
data path at all.

The carrier cascade grows one rung:

```
LAN → direct-public → srflx hole-punch → [ ORG RELAY ] → single-relay (TURN) → DERP/WSS:443
```

Three things this buys, in priority order:

1. **The API pod stops carrying video.** Today a relayed carrier is `relay:derp/tcp` —
   frames cross the `roomler2` pod, the same process serving HTTP, WS and mediasoup. That
   is in direct tension with the standing invariant in `CLAUDE.md`: *"The server
   coordinates but never carries plaintext… any design that would make the control plane a
   data path is wrong on those grounds alone."* DERP **is** that design, accepted as the
   **floor**. An org relay is the same escape hatch without the control plane in it.
2. **HQ-owned relaying** — an org with a well-connected headquarters box gets its branch
   and remote devices relayed *through its own hardware, inside its own network and
   jurisdiction*, instead of through Hetzner. This is the deployment this FR is written
   for; see [§12](#12-hq-deployment--the-primary-use-case).
3. **Latency and throughput.** Tailscale's published field result for the equivalent
   change: **2.24 → 27.5 Mbit/s (12.5×)** and **452 → 298 ms** on an India→Minnesota pair.

**Non-goal, stated up front:** this does not replace DERP and must not be able to. DERP
over TLS:443 stays the floor (design commitment #2). See
[§7](#7-never-self-wedge-never-remove-the-floor).

---

## 2. Why now — field evidence, measured before any code

Taken **2026-08-28 from mars over Fleet RPC** against the live fleet at 0.4.10, before this
spec was written. It is the reason the design departs from the reference in two places.

### 2a. Who is on the relay today

`sudo roomler peers` on mars, primary org, online peers:

| peer | carrier | RTT |
|---|---|---|
| `clk00017265` | **`relay:derp/tcp`** | 45 ms |
| `pc55331` | **`relay:derp/tcp`** | 56 ms |
| `goran-xmg-neo16-wsl-2` | **`relay:derp/tcp`** | 42 ms |
| neo16, jupiter, zeus, pc50045, rozalina-2, scw-m2-asahi, macbook-pro | `direct` | 0–137 ms |

Three of twelve online peers are relayed and every one is on **TCP** — a whole
head-of-line-blocking layer *below* the SCTP one FR-17 is about, on top of the 512-frame
`OUTBOUND_QUEUE` (`crates/tunnel-core/src/transport/derp.rs`) FR-18 measured at ≈1.8 s.
Small population, but exactly the population whose remote-desktop sessions are bad.

### 2b. The finding that shapes the design: **the relay band is blocked on the hosts that need a relay**

`roomler netcheck` across the fleet, same session:

| host | `stun/udp` | `relay band/udp` | NAT | role here |
|---|---|---|---|---|
| **mars** | ok | **reachable** | cone | ✅ relay server — utility tier, public IP |
| **jupiter** | ok | **BLOCKED** | cone | relay server *only after host-firewall provisioning* |
| **zeus** | ok | **BLOCKED** | cone | relay server *only after host-firewall provisioning* |
| **clk00017265** | ok | **BLOCKED** | **symmetric** | ✅ **client** — the target population |
| **pc55331** | **NO MAPPING** | BLOCKED | untyped | ❌ UDP is dead; stays on the DERP floor |
| pc50045 | ok | reachable | cone | already direct |
| scw-m2-asahi | ok | reachable | cone | already direct |

Three separate conclusions:

1. **A relay on a high UDP port would be unreachable by its own target audience.** The
   reference documents `tailscale set --relay-server-port=40000`. On `clk00017265` — corp
   laptop, symmetric NAT, permanently on DERP — UDP/3478 passes and 49152–65535 does not.
   This is not news to this codebase; it is written down at
   `crates/remote_control/src/signaling.rs:2063`: *"a corp egress that whitelists STUN:3478
   still drops the ~10-13k relay band"*. **Port choice is a first-order design decision,
   not a deployment detail** — see [§5](#5-port-selection--3478-first-not-40000).
2. **`clk00017265` is the proof the feature is worth building.** Its NAT is *symmetric* —
   which is why direct fails and why no amount of hole-punching will fix it — and its
   STUN/UDP works. A relay on a port it can reach is precisely and only what it needs.
3. **`pc55331` is the proof DERP must stay.** `stun/udp: NO MAPPING` = no UDP path at all.
   No relay on any port helps it. If this FR ever makes the DERP floor optional, that host
   goes dark.

### 2c. jupiter and zeus will repeat the FR-4 failure if we let them

Both cluster hosts report `relay band/udp: BLOCKED` — their own host firewall. Standing up
a relay there means opening a UDP port on a Hetzner box whose firewall is generated by
Ansible in `~/k8s-cluster-multi`. FR-4 (#776) is the cautionary tale, verbatim from
`CLAUDE.md`: the playbook *"flushes and rebuilds the COTURN chains — never hand-fix a host
without also fixing the vars, or the next playbook run reverts it."* Conference media was
dead on zeus for **weeks**, silently, for a hash-determined subset of tenants, for exactly
this reason.

**Provisioning is therefore a phase of this FR (P5), not a prerequisite someone does by
hand**, and the drift audit is an acceptance criterion.

---

## 3. The reference design (Tailscale), for orientation

Verified against `tailscale.com/net/udprelay` and `tailscale.com/disco`.

| element | Tailscale |
|---|---|
| Session identity | 32-bit **VNI**, allocated per ordered pair of disco public keys |
| Framing | **Geneve** (RFC 8926); `Control` bit separates handshake from data |
| Allocation | `AllocateEndpoint(discoA, discoB)` → `ServerEndpoint{ServerDisco, ClientDisco, LamportID, VNI, BindLifetime, SteadyStateLifetime, AddrPorts}`; idempotent per pair |
| Ordering | **`LamportID`**, a server-wide logical clock — a newer allocation supersedes an older one for the same pair |
| Bind handshake | 3-way, disco-sealed (NaCl box): `BindUDPRelayEndpoint` (0x04) → `…Challenge` (0x05) → `…Answer` (0x06) |
| Challenge | **BLAKE2s MAC over `vni ‖ generation ‖ invited-party key ‖ addr:port`**, checked against the current *and previous* rotation window — the server stores **no per-attempt state** |
| Anti-tamper | The VNI is repeated *inside* the sealed payload, so a rewritten Geneve header is detected rather than mis-routed |
| Notification | `CallMeMaybeVia` (0x07) over DERP carries candidate relay endpoints to the peer |
| Lifetimes | `defaultBindLifetime = 30 s` (3 legs × 10 s), `defaultSteadyStateLifetime = 5 min` idle |
| Forwarding | Pure `addr:port` swap keyed by VNI once both `boundAddrPorts` are set; per-side `packetsRx`/`bytesRx` |
| Authorization | ACL grant `tailscale.com/cap/relay` — *"a device without that grant can't allocate relay bindings"* |
| Requirements | v1.86+ both ends; relay role unavailable on iOS / tvOS / Android |
| Surfacing | connection type reads `peer-relay` |

Two properties are copied wholesale: the **stateless MAC challenge** (a relay that
allocates nothing for an unauthenticated packet cannot be memory-exhausted) and
**ciphertext-only forwarding** (the relay is not a trust boundary).

One property is deliberately **not** copied — Tailscale's *client* decides to allocate.
Ours cannot; see [§1](#1-the-relay-is-a-role-of-roomlerd-offered-by-the-server-never-assumed-by-a-node).

---

## 4. Naming — why the tier is `orgrelay`, not `peer`

⚠️ **"Peer relay" already means something else in this codebase, and it means almost the
opposite.** `TierWhy.blocked_by` (`crates/localapi/src/lib.rs:500`) has the value
**`peer-relays-instead`**, produced by `PathMonitor::on_peer_relayed_instead`
(`crates/tunnel-core/src/overlay/path.rs:590`), with the backing state
`relayed_instead_until` / `_strikes` / `_at` (`path.rs:374-382`) and the surfaced fields
`relayed_instead_s` / `relayed_instead_strikes` (`lib.rs:478`, `:484`). It means *"the
remote peer chose the relay tier, so this tier is suppressed."*

A new carrier tier called `peer` would render as:

```
TIER      ELIGIBLE   ...   WHY NOT
peer      no               peer-relays-instead
```

— two unrelated meanings of "peer relay" in one row of the operator's primary explanation
surface. So:

| concept | token |
|---|---|
| Product / FR name (industry term, what the user asked for) | **peer relay** |
| Carrier tier in `roomler why` and `TierWhy.tier` | **`orgrelay`** |
| `PeerInfo.relay_kind` (joins existing `"turn"`, `"derp"`) | **`"org"`** |
| `roomler peers` CONN column | **`relay:org/udp`** |
| Agent capability verb (`RpcCap`) | **`relay-server`** |

⚠️ `TierWhy.tier`'s doc comment (`lib.rs:496`) enumerates `lan | public | srflx | relay`
and must be extended in the same commit; a wire-lock test pins the spellings
([§10a](#10a-unit-no-mongodb)).

---

## Key design

### 1. The relay is a role of `roomlerd`, offered by the server, never assumed by a node

`CLAUDE.md` commitment #1 is load-bearing: *"Best carrier that works, always measured,
never assumed… chosen by a **server verdict over measured `CapVector`s**. Heuristics may
detect; they never decide."*

So the flow is **not** "node A asks relay R for a binding":

1. A node opts in locally (`relay_server_enabled`, `relay_server_port`) and advertises the
   capability verb `relay-server` through `models::RpcCap` — variant → `wire()` arm (the
   match is exhaustive, so the compiler forces it) → `ALL` entry (a test forces it).
   ⚠️ Equality matching only: `relay` is a prefix of `relay-server`, the exact
   `ssh` / `ssh-consent` trap `CLAUDE.md` documents.
2. An admin approves the device as a relay — the offer is not the grant. Shape mirrors exit
   nodes (`PUT …/overlay-node/{id}/exit-node`), including its lesson that the *data-plane*
   signal must be explicit, not inferred from a boolean.
3. The **server** decides which pairs may use which relay, mints the session, and pushes
   the endpoint to *both* nodes over their control WS.
4. The nodes bind, measure, and report. The measurement enters the `CapVector`; the verdict
   ranks the resulting carrier like any other.

Node-side autonomy is limited to *measuring and reporting*. That keeps a compromised or
buggy agent from steering another tenant's traffic through a relay of its choosing.

The verdict machinery to extend already exists and is pure:

| what | anchor (verified against master) |
|---|---|
| Server verdict entry point | `crates/api/src/ws/overlay.rs:1851` `fn server_relay_verdict(state, recipient, peer)` |
| …delegating to the pure decision | `overlay.rs:1944` `fn relay_verdict_core(pinned, both_derp, both_single, my_udp_ok, peer_udp_ok, …)` |
| Measured caps supersede presence | `overlay.rs:1893-1902` (inside `verdict_from_nodes`, `:1872`) |
| Wire verdict enum | `crates/remote_control/src/signaling.rs:1987` `RelayStrategyWire { SingleRelayAnchor, SingleRelayDialer, Derp, BothAllocate }` |
| Client-side cascade that consumes it | `crates/tunnel-core/src/overlay/relay_link.rs:1016` `fn relay_strategy(&self, node_id, peer)` |

`RelayStrategyWire` gains an `OrgRelay { session }` arm, and `relay_verdict_core` gains one
branch — pure, so it is unit-testable without a fleet.

### 2. Session allocation — server-minted, tenant-scoped, default-deny

```
PeerRelaySession {
  vni:            u32,              // unique within the relay node, per tenant
  relay_node:     ObjectId,
  endpoints:      Vec<SocketAddr>,  // measured + static
  members:        [WgPublicKey; 2],
  lamport:        u64,              // per-relay monotonic; newer supersedes older
  bind_deadline:  Timestamp,        // +30 s
  idle_deadline:  Timestamp,        // +5 min, refreshed on traffic
  tenant_id:      ObjectId,
}
```

Four independent gates, each owned by a different party — the shape Fleet RPC and Roomler
SSH already use, for the same reason (a server compromise must not be sufficient):

| # | gate | owner | default |
|---|---|---|---|
| 1 | `OverlayNetwork.peer_relay_mode` (`off` \| `warn` \| `on`) | the org | **`off`** |
| 2 | overlay ACL: explicit `relay` capability, *src* → *relay node* | the policy author | **deny** |
| 3 | `Agent.peer_relay_policy` — may this device serve, and for whom | the fleet admin | **deny** |
| 4 | agent-local `relay_server_enabled` + `relay_server_port` | the device owner | **off** |

Gate 2 is the direct analogue of `tailscale.com/cap/relay` and reuses `overlay_policies`,
inheriting `acl_mode`'s `off`/`warn`/`enforce` semantics and the `ingress_rules` compile.
⚠️ The documented `Option` discipline applies unchanged: `None` = no ACL compiled,
`Some([])` = **deny**; never collapse them (`crates/tunnel-core/src/overlay/ingress.rs`,
locked by `an_empty_rule_set_denies`, `ingress.rs:334`).

The precedent to copy is the **TURN relay-grant gate**,
`crates/api/src/ws/overlay.rs:693` (`handle_overlay_relay_request`), whose ACL check at
`:718-741` carries its own justification verbatim: *"a TURN grant is a carrier for the very
pair the netmap may have just denied … which would make the whole ACL decorative."* An org
relay is a carrier for exactly the same reason, so it gets exactly the same gate, in the
same place — cross-tenant check first (`:703-716`), then `load_acl` → `overlay_source_of` →
`evaluate_overlay`.

⚠️ **But it must fail CLOSED, and the two nearest precedents both fail OPEN.**
`load_acl` (`overlay.rs:1768`) documents itself as failing open on a DB error, and
`DerpAclCache` (`crates/api/src/ws/derp_acl.rs:81`) treats an *absent* per-tenant table as
permissive (`permits`, `:59`: `!self.enforcing || …`). That is defensible for those two —
they guard an **established** data path, where a cache miss must not black-hole a working
fleet. It is **not** defensible here: an org relay is a brand-new capability with no
established path to protect, so an unavailable ACL means "do not mint", and the pair keeps
the carrier it already had. Failing open on a *new* privilege converts a transient database
error into a silent grant. This asymmetry is deliberate and must survive review.

Gate 4 survives a compromised server, which is why the relay port is a *device* key, never
a server-pushed one: `relay_server_enabled` is **structurally absent from
`DesiredConfig`** — same rule and same test as `remote_config_enabled`
(`docs/remote-config.md`).

### 3. Wire — Geneve-framed VNI on the node's existing UDP socket

Geneve (RFC 8926), `Protocol = Disco` for control and the WireGuard payload for data. The
reason to adopt Geneve rather than invent a header is not interop — there is none — it is
that the format is specified, has a control bit and VNI at fixed offsets, and every packet
capture tool already dissects it during a field debug.

Receive-path demux is unambiguous without a magic number: a WireGuard message's first byte
is its type, 1–4; a Geneve header's first byte is version `00` plus option length. The
existing receiver-index demux gains one arm —
`CarrierPlane::route_by_index` (`crates/tunnel-core/src/overlay/carrier_plane.rs:738`),
returning `Routed` (`:966`). The arm is table-tested, because a mis-demuxed packet fails
*silently*.

**Bind authorization has a precedent worth copying exactly.** DERP registration already
refuses to let a node claim a key that is not its own:
`crates/api/src/ws/derp.rs:355` asserts the presented 32 bytes equal the node's stored
`wg_public_key`. The relay's bind must make the same assertion against the minted session's
`members`, or a node could bind into a session it was never party to.

**The relay holds no key that decrypts anything.** It forwards WireGuard ciphertext between
two bound `addr:port`s selected by VNI — the same property DERP has today, for the same
reason.

### 4. The bind handshake — 3-way, and stateless until bound

```
client ──BindOrgRelayEndpoint{vni, generation}───────────────▶ relay
client ◀─BindChallenge{ MAC(vni‖generation‖peer_pubkey‖addr:port) }── relay
client ──BindAnswer{ challenge echoed }──────────────────────▶ relay   ⇒ bound
```

BLAKE2s MAC under a per-session secret derived from the mint, checked against the current
and previous rotation window. **The relay allocates no per-attempt state**: an
unauthenticated packet costs one MAC computation and nothing else.

A relay that kept a table entry per inbound bind attempt would be a trivially
memory-exhaustible service on a public UDP port, on a node the operator also uses for
something else. **This is the single most important security property to preserve through
review**, and it has an explicit test ([§10a](#10a-unit-no-mongodb)).

The VNI is carried inside the sealed payload as well as in the Geneve header, so header
rewriting is detected rather than silently mis-routed.

Lifetimes as per the reference — **30 s** to complete the bind (it spans three legs:
client↔server, client↔client over the control plane, client↔relay) and **5 min** idle once
bound. `lamport` resolves re-allocation races.

### 5. Port selection — **3478 first, not 40000**

The conscious departure from the reference, on the strength of [§2b](#2b-the-finding-that-shapes-the-design-the-relay-band-is-blocked-on-the-hosts-that-need-a-relay).

- **Default `relay_server_port = 3478`** — what the measured corp egresses actually permit,
  and what this codebase already documents (`signaling.rs:2063`).
- A relay MAY additionally bind **UDP/443**, which passes some egresses that drop 3478.
- A relay MAY bind a high port; it will simply be useless to the CORPLAP-class hosts, and
  `roomler why` must **say so** rather than leave the operator guessing.
- ⚠️ **On a host already running coturn — mars is exactly this host — 3478 is taken.** The
  relay must then bind 443 or an alternate, and the daemon must **refuse to start the relay
  and log why** rather than silently binding nothing. Compare `ssh_port`'s default of 2222,
  chosen for exactly this "don't collide with the incumbent" reason.
- Eligibility is **measured, not assumed**: the existing `relay_band_udp` probe
  (`CapVectorWire`, `signaling.rs:1804`; consumed at `overlay/netmap.rs:184`) generalises to
  *"can this node reach relay R on port P"*, and the answer becomes a `CapVector` field. A
  relay endpoint a node cannot reach is not offered to it twice.

### 6. Carrier tier and scoring

`roomler why 100.65.4.30` on mars, 2026-08-28 — the real ladder:

```
  TIER      ELIGIBLE  SCORE   = BASE  + Q      - PENALTY   WHY NOT
  lan       yes        400.0 =   400    +0.0        0.0
  public    yes        410.0 =   330   +80.0        0.0
  srflx     no         177.8 =   260    +0.0       82.2   penalty
  relay     yes        297.6 =   200   +97.6        0.0
```

Those bases are literal constants, not display values —
`crates/tunnel-core/src/overlay/path.rs:174-177`:

```rust
pub(crate) const B_LAN: f64 = 400.0;
pub(crate) const B_PUBLIC: f64 = 330.0;
pub(crate) const B_SRFLX: f64 = 260.0;
pub(crate) const B_RELAY: f64 = 200.0;
```

Add one row: **`orgrelay`, BASE 230** — above `relay` (200), below `srflx` (260).

Three consequences in the selector that are **not** cosmetic, all verified in
`PathMonitor` (`path.rs`):

**(a) The eligibility floor is `B_RELAY`, and it will swallow the new tier if left alone.**
`eligible` (`path.rs:850`) ends:

```rust
base(tier) - penalty >= B_RELAY - ELIGIBILITY_EPS
```

So `orgrelay` at 230 has **30 points** of penalty headroom before it goes ineligible, where
`srflx` has 60 and `lan` has 200. A tier that drops out of contention after one or two
failures looks, in the field, exactly like *"the feature doesn't work"*. Either the floor
becomes relative to the lowest *offered* tier, or `orgrelay` gets its own penalty budget.
**A P4 blocker, and it belongs in the first implementation PR's tests** — not in a field
debug three weeks later.

**(b) The relay tier is currently "always eligible" by construction.** `eligible` opens
`let Some(idx) = tier_idx(tier) else { return true };` — `tier_idx` returns `None` for
`Relay`, *because it is the floor*. `orgrelay` is **not** a floor and must therefore get a
real `tier_idx`, which means widening the fixed-size `p.tiers[idx]` array and every
construction of it. Skipping that and returning `true` would give the new tier no penalty
memory at all: it would be retried forever at full score against a relay that is down.

**(c) `relayed_instead` may mis-fire.** `eligible` gates outright on
`relayed_instead_until` (`path.rs:374`), set by `on_peer_relayed_instead` (`:590`), which is
driven from `MuxEvent::Unrouted` on the **DERP mux** (`runtime.rs:2564`). A peer that moves
to an org relay is no longer on the DERP mux, so the signal's meaning — *"the peer is
relaying instead of using ANY direct tier"* — needs re-deriving, or a peer pair on
`orgrelay` will look to each other like a peer with no relay at all. Decide this in P4 and
test both directions.

Touch points, all pure and unit-testable: `DirectTier`
(`crates/tunnel-core/src/overlay/lifecycle.rs:74`), `DIRECT_TIERS` (`path.rs:398`), `decide`
(`path.rs:981`), `eligible` (`:850`), `score` (`:875`).

Any working *direct* path should beat any relay, so `orgrelay` sits under `srflx`; a
tenant-owned UDP relay should beat TURN/DERP, so it sits over `relay`. The measured quality
term `Q` does the rest — note above that a penalised `srflx` (177.8) already loses to
`relay` (297.6), so "a good org relay outscores a bad srflx path" falls out of the existing
arithmetic and needs no special case.

**Rejected alternative:** folding it into the existing `relay` tier as a third
`relay_kind`. Smaller diff, but the tier ladder is the operator's primary explanation
surface, and collapsing a 230-vs-200 preference into one row makes `roomler why` unable to
answer *"why am I on DERP when there is a relay right there"* — the exact question this
feature creates.

### 7. Never self-wedge, never remove the floor

Commitments #1, #2 and #3, each as a test rather than a sentence:

- **The floor is unconditional.** DERP registration is never torn down because an org relay
  came up. `pc55331` must be measurably unaffected by this entire feature.
- **Never ratchet.** A pair on `orgrelay` keeps re-attempting `srflx`/direct on the existing
  cadence; a pair that fell from `orgrelay` to `relay` keeps re-attempting `orgrelay`.
- **The relay node protects itself.** Serving is capped (`relay_max_sessions`,
  `relay_max_bitrate_per_session`), and the relay's own carriers are exempt from its
  forwarding budget. An org must not be able to cost its HQ box its own remote access —
  commitment #3 applied to bandwidth instead of routes.
- **Failure is a downgrade, never a black hole.** A relay that stops forwarding is convicted
  by the existing carrier-health sweep and demoted within the same deadline any other
  carrier gets. ⚠️ The FR-9 lesson applies exactly: the dangerous bug was never "the carrier
  failed", it was *"both ends held a hold-down and went mutually deaf"* (#746). Assert on
  **both** ends' logs, never one.

### 8. Observability — shipped in P1, with its reader

`feedback_ship_diagnostics_with_fix`, and FR-1's experience that the age pill *"immediately
localized the relay queue below the agent"*.

⚠️ **The precedent that makes this non-negotiable:** FR-18's own field log records that
`dropped_stale` *"could NOT be evaluated — the counter was added without a reader
(`stale_drops()` has no consumers)"*. Still true on master: `stale_drops()`
(`transport/derp.rs:407`) has zero consumers in `localapi`, `roomler-tunnel` or the agent's
`NodeStatus` builder. **A counter without a reader silently invalidates the acceptance
criterion that depends on it.** Every counter below ships with its reader in the same PR.

| surface | addition | anchor |
|---|---|---|
| `roomler peers` | `CONN` reads `relay:org/udp` | `localclient.rs:1095` (`fmt_peer_row`) |
| `roomler peers --json` | `relay_kind: "org"`, new `relay_via` (relay node name), `relay_endpoint` | `PeerInfo`, `crates/localapi/src/lib.rs:346` |
| `roomler why <peer>` | the `orgrelay` row, `blocked_by` ∈ {`no-relay-offered`, `relay-unreachable`, `acl`, `relay-at-capacity`} | `TierWhy`, `lib.rs:495`; printer `localclient.rs:252` |
| `roomler netcheck` | relay node: `relay server: listening :3478, N sessions`; client: reachability per offered relay | `localclient.rs:141` |
| relay node | per-session `packets_rx`/`bytes_rx` per side + `binds_rejected`, surfaced in `NodeStatus` | `NodeStatus`, `lib.rs:87` |
| process-global | `ORG_RELAY_BINDS_REJECTED`, `ORG_RELAY_SESSIONS_ACTIVE` | `crates/tunnel-core/src/evidence.rs:18+` |
| server | `peer_relay_audit` (90 d TTL) — every mint **and every refusal**, reason enumerated, written in **one** place via `decide() -> Result<Granted, DenyReason>` so "a new refusal that forgets to audit itself" is unrepresentable | the `agent_ssh::dispatch` shape |

⚠️ `evidence.rs` counters are **cumulative since daemon start** — consumers DIFF two
readings and never judge absolutes (`evidence.rs:1-9`).

⚠️ An empty `relay_via` is **not** evidence of "no relay available" — it may mean the node
never measured. Distinguish, exactly as `CapVector` already does: *absence of measurement is
never evidence of absence of capability* (`signaling.rs:1795`).

### 9. Where the code goes — implementation map

Every anchor below verified against `origin/master` on 2026-08-28.

| change | file:line |
|---|---|
| New tier in the tier enum | `crates/tunnel-core/src/overlay/lifecycle.rs:74` `DirectTier` |
| Tier priors + eligibility + score | `crates/tunnel-core/src/overlay/path.rs:174-177`, `:398`, `:850`, `:875`, `:981` |
| Carrier transport arm | `crates/tunnel-core/src/overlay/wg.rs:138` `enum Carrier` (`Direct` / `Relay` / `QuicRelay`) |
| **The send path that must learn "via peer X"** | `wg.rs:907` `send_ip_packet` (maps one dst IP → one peer → one carrier). Closest existing primitive: `wg.rs:932` `send_to_peer` |
| Receive demux arm | `crates/tunnel-core/src/overlay/carrier_plane.rs:738` `route_by_index` → `Routed` (`:966`) |
| Relay sub-kind | `crates/tunnel-core/src/overlay/relay_link.rs:152` `RelayKind { Turn, Derp }` |
| Client-side cascade | `relay_link.rs:1016` `relay_strategy` |
| Server verdict | `crates/api/src/ws/overlay.rs:1851` / `:1944` |
| ACL gate on the grant | `overlay.rs:693` `handle_overlay_relay_request` (check at `:718-741`) |
| Netmap shaping (what a recipient may see) | `overlay.rs:1982` `shape_peer` |
| Peer wire model | `crates/remote_control/src/signaling.rs:2007` `NetmapPeer` |
| Measured caps (agent) / wire | `crates/tunnel-core/src/overlay/netcheck.rs:60` `CapVector` / `signaling.rs:1800` `CapVectorWire` |
| Carrier health + demotion | `crates/tunnel-core/src/overlay/runtime/establish.rs:271` `sweep_carrier_health` |
| Bind-authz precedent | `crates/api/src/ws/derp.rs:355` (presented key must equal the node's own) |
| CLI surfaces | `agents/roomler-tunnel/src/localclient.rs:1095` (`fmt_peer_row`), `:252` (`print_why`), `:141` (`netcheck`) |

**Config keys.** Four wiring points each, per the contract comment at
`crates/agent-core/src/config_surface.rs:34-38`:

1. field on `AgentConfig` (`crates/agent-core/src/config.rs`),
2. entry in `const KEYS` (`config_surface.rs:40`) with kind + a description ending in
   *"Built-in default: off."*,
3. getter + setter dispatch arms,
4. `env_bridge_bools` (`config.rs:1794`) — ⚠️ its return type is
   `[(&'static str, Option<bool>); 54]`, **a fixed-size array whose length must be bumped**,
   and the `env_bridge_pairs_have_surface_parity` test enforces the pairing.

Model the tri-state `peer_relay_mode` on `overlay_rpf` (a validated string:
`config_surface.rs:241` entry, `:907-915` validation); model the booleans on
`overlay_netcheck` / `overlay_derp_floor`.

⚠️ `relay_server_enabled` is default-**off** and opt-**in**, so it takes the opt-in reader
idiom (`overlay/direct.rs:679`, `public_direct_enabled` — only `1|true|yes|on` enables), not
the default-on `crate::env::flag("…", true)` idiom used by `overlay_netcheck`
(`direct.rs:883`). Getting this backwards ships a relay that is on by default.

---

## Phases

| P | scope | kill switch | status |
|---|---|---|---|
| **P1** | Observability + measurement only: `orgrelay` row rendered permanently ineligible, the `relay_band_udp` probe generalised to arbitrary `relay:port`, `peer_relay_audit` collection, every counter **with its reader**. **No forwarding.** | inert by construction | proposed |
| **P2** | Relay server in `roomlerd` behind `relay_server_enabled` (default off): Geneve framing, VNI table, stateless MAC bind, forwarding, caps, counters | `relay_server_enabled=false` | proposed |
| **P3** | Server-side mint + the four gates + `peer_relay_mode` (default `off`) + push to both nodes | `peer_relay_mode=off` ⇒ zero mints | proposed |
| **P4** | Carrier integration: `orgrelay` live in the verdict, promote/demote, re-upgrade cadence | `overlay_org_relay=false` (agent) | proposed |
| **P5** | Provisioning for jupiter/zeus in `~/k8s-cluster-multi` host_vars + `peer-relay-port-audit.sh` weekly drift cron, modelled on `mediasoup-rtc-forwarding.sh` | revert host_vars | proposed |
| **P6** | Admin UI: relay approval per device, org switch, `peer_relay_audit` section | UI-only | proposed |

Ordering rationale: P1 is inert and answers *"would this work, and for whom"* with fleet
data before any forwarding code exists. P2/P3 are independently killable. P4 is the only
phase that can change an existing carrier decision, and it lands last.

---

## Acceptance criteria

Falsifiable, each naming its instrument.

**Correctness**

- [ ] With `peer_relay_mode=off` (the default), **zero** behavioural delta: no rows in
      `peer_relay_audit`, no eligible `orgrelay` row, byte-identical carrier selection over
      a 24 h fleet soak.
- [ ] A relay forwards only between the two `addr:port`s bound for a VNI; a third party
      sending on a known VNI from an unbound address is dropped **and counted**.
- [ ] An unauthenticated bind attempt allocates **no** relay-side state — 10 000 bad-MAC
      attempts leave the session table length unchanged and RSS flat.
- [ ] A tampered Geneve VNI is rejected by the sealed-payload check, not mis-routed.
- [ ] `lamport` ordering: a re-mint for a bound pair supersedes deterministically and both
      nodes converge on the newer session.
- [ ] Cross-tenant: a session minted for tenant A cannot be bound by a node of tenant B,
      even with a correct VNI.
- [ ] ACL: with `acl_mode=enforce` and no `relay` grant the mint is **refused**, and the
      refusal appears in `peer_relay_audit` with an enumerated reason.

**The floor**

- [ ] `pc55331` (`stun/udp: NO MAPPING`) stays on `relay:derp/tcp` throughout, with no added
      reconnects, across the full rollout.
- [ ] Killing every relay mid-session demotes affected pairs to `relay`/DERP within the
      standard carrier-health deadline, with **no mutual-deafness window** — asserted on
      both ends' logs (FR-9 #746).

**The win**

- [ ] `clk00017265` — symmetric NAT, `relay band/udp: BLOCKED`, currently `relay:derp/tcp` —
      converges to `relay:org/udp` via mars on UDP/3478, and `roomler why` shows the
      `orgrelay` row winning.
- [ ] On that pair, measured **before and after, same day, same hosts**: RTT, overlay
      throughput, and RC `send_wait_max_ms`. Target **throughput ≥3×** and
      **`send_wait_max_ms` ≥50 % lower**. *(The reference reports 12.5× and −34 % RTT; we
      are not claiming those, we are claiming we will publish ours.)*
- [ ] `derp_registrations` gauge and DERP bytes through the `roomler2` pod fall measurably
      once the eligible population migrates (`GET /api/cluster/status`).

**Operations**

- [ ] `peer-relay-port-audit.sh check` fails on a host whose UDP port is not open, and the
      weekly cron files a GitHub issue on drift — proven by **removing the rule on zeus and
      watching it fire** (FR-4's lesson: a rule that exists only because someone typed it is
      a rule that will be reverted).
- [ ] A relay at `relay_max_sessions` refuses a mint with a distinct reason, and the pair
      falls back rather than hanging.
- [ ] Relay-node socket census flat over 24 h (see F6).

---

## 10. Integration and E2E tests

### 10a. Unit (no MongoDB)

`cargo test -p roomler-ai-remote-control --lib`, `cargo test -p roomler-ai-tunnel-core --lib`.
⚠️ `cargo test -p roomler-agent --lib` **skips** the overlay tests — the lane needs
`--features overlay-l3`.

| test | asserts | model it on |
|---|---|---|
| `geneve_header_roundtrip` | encode/decode, control bit, 24-bit VNI truncation | — |
| `wg_and_geneve_demux_is_unambiguous` | every WG type byte 1–4 × every legal Geneve first byte classifies correctly — table-driven; this is the class of bug that mis-routes **silently** | `overlay/ingress.rs:288` (`truncated_and_garbage_packets_never_panic`) |
| `bind_mac_is_stateless` | 10 000 bad-MAC attempts leave the session table empty | — |
| `bind_mac_accepts_previous_window` | rotation boundary does not drop an in-flight bind | — |
| `sealed_vni_mismatch_rejected` | header rewrite detected | — |
| `lamport_newer_supersedes` | incl. equal and wrapped cases | — |
| `org_relay_wire_strings_are_locked` | `relay-server`, `relay_kind:"org"`, `tier:"orgrelay"` — a rename does not fail loudly, it makes every deployed device look like it lacks the feature | `rpc_cap_wire_strings_are_locked` |
| `relay_prefix_not_matched_by_equality` | `relay` must not match `relay-server` | `ssh_does_not_imply_ssh_consent` |
| `tier_ladder_includes_orgrelay_between_srflx_and_relay` | base 230, ordering locked | `overlay/path.rs:1899` (`cold_start_ordering_matches_legacy_precedence`) |
| `orgrelay_tier_is_not_confused_with_peer_relays_instead` | `tier:"orgrelay"` and `blocked_by:"peer-relays-instead"` coexist unambiguously (§4) | `localapi/src/lib.rs:2415` |
| `relay_server_absent_from_desired_config` | serialise a `DesiredConfig`, assert the key never appears | the `remote_config_enabled` test |
| `an_empty_relay_grant_set_denies` | `Some([])` = deny, `None` = no ACL | `overlay/ingress.rs:334` |

Existing carrier-selection tests to extend rather than duplicate:
`crates/tunnel-core/src/overlay/path.rs:1421` (`mod tests`) — in particular
`explain_agrees_with_eligible_and_names_the_gate_that_actually_refused` (`:1555`), whose
invariant the new tier must not break, and
`relayed_instead_suppresses_the_tier_without_slamming_its_quality` (`:1628`).

### 10b. Integration (`crates/tests`, real MongoDB + Redis)

New module `crates/tests/src/peer_relay_tests.rs`.

⚠️ **There are currently no carrier tests in `crates/tests` at all** (`grep -l carrier
crates/tests/src/` is empty) — carrier logic is covered only by in-crate unit tests. These
would be the first, so budget for fixture work rather than assuming a pattern exists.

Model on `crates/tests/src/relay_region_tests.rs:46` (`flag_off_credentials_are_byte_identical_legacy`)
— it is 40 lines, uses `TestApp::spawn_with_settings` (`fixtures/test_app.rs:179`), and makes
**both** a REST assertion and an in-process `app.state` assertion. For a two-pod topology
copy `cluster_tests.rs:798` (`derp_split_rehomes_toward_newest_registration`), the only
end-to-end `/derp` test, via `TestApp::spawn_pair` (`test_app.rs:152`).

| test | shape |
|---|---|
| `mint_refused_when_mode_off` | default org settings ⇒ 0 mints, audit row with reason |
| `mint_refused_without_acl_grant` | `acl_mode=enforce`, no grant ⇒ refusal |
| `mint_refused_cross_tenant` | relay in tenant A, requester in tenant B ⇒ 404-shaped: leaks neither content nor existence (the object-level tenant-scoping rule) |
| `mint_requires_relay_server_cap` | agent without the verb ⇒ **412, never a hanging caller** — a caller that *awaits* must gate on the cap (the Fleet RPC lesson) |
| `mint_is_idempotent_per_pair` | second call returns the same VNI + lamport |
| `three_node_relay_roundtrip` | three in-process agents via `agent_presence_tests::enroll` (`:20`) + `connect_agent` (`:57`); A and B forced off direct, C relays; assert bytes arrive and C never holds a decrypting key |
| `relay_removal_releases_sessions` | delete the relay device ⇒ sessions torn down, both peers notified via `netmap_delta`, matching the CAS-tombstone ordering used for address release |
| `audit_records_both_arms` | one grant + one refusal, both present, reason enumerated |

⚠️ The lane asserts a **minimum** test count (`.github/workflows/integration-tests.yml`) —
raise the floor with the new tests. `cargo test` reports success for a filter matching no
test, which is how this crate rotted before #646.
⚠️ Set `RUST_LOG` — the harness installs a subscriber only when it is set
(`fixtures/test_app.rs:62`), and a refusal that logs its reason is invisible otherwise. Use
`--nocapture`.

### 10c. E2E on the real fleet — mars, jupiter, zeus

Roles are assigned by **risk**, not convenience.

> ⚠️⚠️ **jupiter runs production.** `k8s-worker-3` on jupiter holds the node-local PVCs for
> `mongodb-0`, `minio-0` and `roomler2`. Taking jupiter down is a **full roomler.ai
> outage** — this exact wrong assumption caused a multi-hour outage on 2026-07-19. Nothing
> in this plan restarts, reboots, reroutes or firewall-flushes jupiter. Its relay is
> additive, bandwidth-capped, and killable from mars.

| host | overlay | role | risk |
|---|---|---|---|
| **mars** | 100.65.4.14 | **primary relay** — utility tier, public IP, `relay band/udp: reachable` | low; no prod pods |
| **zeus** | 100.65.4.24 | **second relay** — selection, failover, drift-audit target | medium; carries a `roomler2` replica |
| **jupiter** | 100.65.4.15 | **third relay, read-mostly** — multi-relay ranking only | **high; storage-pinned prod** |

**E2E-1 — relay selection.** All three relays offered to `clk00017265`; assert it picks the
lowest measured RTT and that `roomler why` explains the ranking. Then take mars's relay down
with `roomler config set relay_server_enabled false` — **a config change, not a service
restart** — and assert re-selection to zeus without a DERP round trip.

**E2E-2 — the floor holds.** With all three relays live, assert `pc55331` is still
`relay:derp/tcp` and its reconnect count is unchanged. This is the regression test for the
whole feature.

**E2E-3 — port reachability matrix.** {3478, 443, 40000} × {mars, zeus, jupiter} ×
{clk00017265, pc55331, pc50045}, recorded from `netcheck`. This table confirms or refutes
[§5](#5-port-selection--3478-first-not-40000) and **must be run before P2 lands**.

**E2E-4 — kill switch.** `peer_relay_mode=off` at the org ⇒ every pair returns to its
pre-feature carrier within one probe cycle, fleet-wide, with no session left bound.

**E2E-5 — provisioning drift.** Remove the UDP rule on zeus by hand; assert
`peer-relay-port-audit.sh` catches it and the cron files an issue. Then re-run the Ansible
playbook and assert the rule returns **from `host_vars`**, not from the hand fix.

**E2E-6 — relay echo (the silent-blackhole class).** Extend
`scripts/relay-pop/healthcheck.py`, which already does exactly this for coturn PoPs — a real
`relay-echo` sending data *both ways through the relay*, because a bound session that
forwards nothing looks identical to a healthy one from the control plane. Non-zero exit on
failure, same as the existing script.

Driving is over Fleet RPC from mars — the idiom already used for cluster agent deploys and
documented at `docs/fleet-rpc.md:19`:

```bash
sudo roomler exec <host> --timeout 45000 -- "roomler netcheck"
sudo roomler exec <host> --timeout 45000 -- "roomler why <peer> --json"
sudo roomler exec <host> --timeout 45000 -- "roomler peers --json"
sudo roomler diag pair mars clk00017265
```

⚠️ Pass the whole chain as **one quoted arg** — the relay joins argv into a shell line, so
`bash -c "..."` gets word-split and breaks.
⚠️ `roomler exec` needs `exec_enabled` on the target; neo16 currently refuses
(`remote execution is disabled on this device`), so neo16-side steps run locally.
⚠️ `roomler peers`/`why` need the **privileged** daemon on hosts running both halves —
`sudo roomler …`, or the per-user daemon answers and the overlay looks empty.

**Soak.** Model on `scripts/vpn-lab/run-lab.sh`, which already deploys a probe to a corp
laptop over `roomler exec`, runs the dev-box half locally, and collects both sides.
⚠️ It **measures clock skew per run** (`run-lab.sh:44-64`) because pc50045 sat 21.4 s behind
the dev box and manufactured an impossible reading — a relay FR comparing timestamps across
three hosts needs that guard more, not less.

### 10d. What CI can and cannot do here

- `.github/workflows/integration-tests.yml` runs 10b. That is real coverage.
- CI **cannot** cover 10c: no symmetric-NAT corp laptop, no blocked relay band, no
  three-host topology. Per `CLAUDE.md`: *"Field-validated, not CI-validated… CI green ≠
  done."*
- ⚠️ `--workspace` clippy compiles only `pub mod fixtures` from `crates/tests`, so
  `Checking roomler-ai-tests` in a build log is **not** evidence the new tests build.
- `scripts/ci-local.sh` runs the feature lanes locally, but explicitly does **not** run
  `cargo test -p roomler-ai-tests`.

---

## 11. Field tests

Each must be shown to **fail (or be absent) on the current deploy first** — `CLAUDE.md`:
*"CI green is not a result. A field test must be shown to FAIL on the current deploy first,
or its pass proves nothing — record both runs."*

| # | test | before (recorded 2026-08-28, 0.4.10) | after |
|---|---|---|---|
| F1 | `clk00017265` carrier | `relay:derp/tcp`, 45 ms | `relay:org/udp` via mars |
| F2 | Throughput on that pair over the overlay | *to record at P1* | ≥3× |
| F3 | RC `send_wait_max_ms` on that pair | FR-18 measured 10 263 ms on a corp host | ≥50 % lower |
| F4 | `pc55331` carrier | `relay:derp/tcp`, 56 ms | **unchanged** |
| F5 | `derp_registrations` + DERP bytes through the `roomler2` pod | *to record at P1* | measurably lower |
| F6 | Relay-node CPU, RSS, **UDP socket count** on mars while relaying | *to record at P1* | within budget; **no socket growth** |

⚠️ **F6 is not padding.** The 2026-08-22 incident is the precedent: `roomlerd` held **15 446
UDP sockets after 12 h** (10 367 on `:5353`), exhausted the ephemeral range, and the host
lost DNS entirely while `ping 1.1.1.1` stayed at 3 ms. A new component that owns UDP sockets
on a long-lived daemon gets a socket census on a schedule, or it repeats that:

```bash
ss -uap | wc -l                                                   # Linux
netstat -ano -p UDP | awk '{print $NF}' | sort | uniq -c | sort -rn | head   # Windows
```

Recording discipline: results go in a `## Result — field-verified on <version>` comment on
the issue, with before/after tables, the operator's own words quoted verbatim where they
exist, the *unchanged* control number included, and **dead hypotheses recorded too**.

---

## 12. HQ deployment — the primary use case

The shape this is built for: an org runs one well-connected box at headquarters. Its remote
and branch devices — laptops on hotel Wi-Fi, machines behind CGNAT, symmetric-NAT corporate
desktops — cannot reach each other directly, and today they relay through roomler's DERP in
Germany.

With this FR the HQ box is approved as an org relay and:

- **Relayed traffic stays on the org's own hardware, inside its own jurisdiction.** For a
  customer with data-residency obligations this converts *"we relay through the vendor"*
  into *"we relay through ourselves"* — and the relay is provably incapable of reading the
  traffic, because it forwards ciphertext keyed by VNI (§3).
- **Capacity is the org's to size.** No shared fate with other tenants on the DERP path.
- **The path is usually shorter.** Two devices in one country relay via a box in that
  country instead of round-tripping to Hetzner.

Operationally the HQ node needs: a reachable UDP port (**3478 by default**, §5), a static
endpoint if it is behind NAT (`relay_static_endpoints`, the reference design's escape hatch
for exactly this), admin approval, and an ACL grant naming which sources may use it. All
four are visible in the admin UI at P6.

⚠️ **This makes the HQ box part of the org's network infrastructure.** The failure mode
belongs in the customer-facing docs: if it dies, affected pairs fall back to DERP —
*degraded, not disconnected*. That is a property to test (E2E-2, E2E-4), not to assert.

---

## Edge cases

- **Relay behind NAT.** Its mapping must be kept alive or it is unreachable. Static
  endpoints cover the port-forwarded case; a relay that can be neither reached nor
  port-forwarded must be **refused at approval time with a clear reason**, not offered and
  silently useless.
- **A relay that is itself relayed.** A node whose only carrier is DERP must never be offered
  as a relay. Guard on the relay's own measured `CapVector` — and **re-check**, because a
  node can fall to DERP *after* approval.
- **Multi-org.** A device in N orgs may serve N tenants, but sessions must never be shared
  across them and the VNI space is per-tenant-scoped, to avoid the addressing collision
  `overlay_blocks` exists to prevent. Exit roles are primary-only today; relay serving should
  follow the same rule until there is evidence it should not.
- **Corp VPN transitions.** A relay session is a UDP flow, and a Check Point-class client
  kills *fresh* UDP while grandfathering existing flows (`docs/overlay-warm-relay.md`). A
  session established before VPN-up may survive; one attempted after will not. This interacts
  with the C4 warm-relay design — **do not solve both here**.
- **The relay restarts.** Sessions are lost; both peers must re-mint rather than wedge. The
  `lamport` clock must be persisted or monotonically re-seeded, or a restarted relay can
  issue a *lower* id than one a peer still caches.
- **MTU.** Geneve adds 8+ bytes. The overlay MTU must account for it or large frames
  fragment — a classic silent-degradation bug. `roomler diagnose` already probes path-MTU.

---

## Out of scope

- Sharing UDP/3478 with a co-resident coturn (mars). Pick another port; revisit only if
  E2E-3 shows 3478 is the sole port that works there.
- Relaying for *other* tenants (a public or paid relay marketplace).
- Relay chaining (A → R1 → R2 → B).
- TCP or TLS org relays. If UDP is dead, the DERP floor is the answer — that is what it is
  for, and `pc55331` is why.
- Wire compatibility with Tailscale.
- Making the DERP floor optional. Not in this FR, and not in a later one without a different
  kind of evidence than this one has.

---

## Open decisions

1. **Where does the mint live** — the API (consistent with every other policy decision and
   with "the server verdict decides") or the relay node itself holding a server-signed token
   (fewer round trips, keeps working through a control-plane blip)? Leaning API for P3, with
   the token form as a later optimisation *if measurement justifies it*.
2. **One `orgrelay` tier, or one row per offered relay?** One tier keeps `roomler why`
   readable; per-relay rows make the choice auditable. Leaning one tier, winner named in
   `relay_via`.
3. **Default `relay_server_port`** — 3478 has the field evidence; 443 may pass strictly more
   egresses; both collide with something on some hosts. **E2E-3 decides this, before P2.**
4. **Does a relay advertise capacity** so the server can load-balance, or simply refuse at
   the cap? Refusal is simpler and honest; advertisement is better for an HQ node serving
   many devices.
5. **Should `orgrelay` outrank `srflx`** when srflx is penalised but not dead? The current
   arithmetic already lets a good relay win on score; making it structural would be a second
   mechanism doing the same job.

---

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-28 | 0.4.10 | **Pre-implementation evidence sweep.** Fleet-wide `netcheck` + `peers` + `why` from mars over Fleet RPC. 3/12 online peers on `relay:derp/tcp`, **all TCP**. `relay band/udp` **BLOCKED** on `clk00017265` (symmetric NAT), jupiter and zeus; `stun/udp: NO MAPPING` on `pc55331`. Live ladder captured for `100.65.4.30` (bases: lan 400 / public 330 / srflx 260 / relay 200). ⇒ two design changes *before* any code: default port moved to **3478** (not the reference's 40000), and cluster-host firewall provisioning promoted to its own phase (P5). ⚠️ Also found: the term "peer relay" already exists in the tree with the opposite meaning (`blocked_by: "peer-relays-instead"`, `path.rs:590`) ⇒ tier token is `orgrelay`, not `peer` (§4). |
