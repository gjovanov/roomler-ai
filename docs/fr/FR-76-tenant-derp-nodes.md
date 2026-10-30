# FR-76 — Tenant-owned DERP nodes: a floor the UDP-blocked population can reach *near them*

**Issue:** [#1454](https://github.com/gjovanov/roomler-ai/issues/1454) · **Status:** P3 built ([#3158](https://github.com/gjovanov/roomler-ai/pull/3158), parked for review); P1, P2, P4 and P5 not started · **Opened:** 2026-09-06

## Goal

A tenant can run its own DERP relay and its nodes will **use it when it is
measurably better**, falling back to the control-plane DERP whenever it is not.
Today every agent's DERP endpoint is derived from its own control-plane URL, so
the floor is always wherever the API pods are — however far that is from the
hosts that can only reach the floor.

## Why — the population that has nothing else

Measured 2026-09-06 on two corporate laptops behind a Check Point full tunnel,
both on the same LAN, both on 0.4.76:

| blocker | evidence |
|---|---|
| **LAN route-captured** | `lan CAPTURED — 192.168.8.0/24 leaves via "Ethernet"` (FR-33 / #905) |
| **no hole-punch** | `srflx NONE` — STUN, 3 vantages + public-dial fallback |
| **UDP egress dropped to the internet** | UDP → corp DNS `:53` **replies (383 B)**; UDP → `1.1.1.1:53`, STUN `:19302`, `:3478` all time out; **TCP 443 passes** |

⇒ No LAN tier, no srflx, no direct-public. **DERP over TLS 443 is not a fallback
for these hosts — it is the only carrier that can exist.** The cascade is already
choosing correctly; there is nothing left to choose.

### The same three blockers, as a picture

The overlay normally tries four ways to reach a peer, cheapest and fastest
first. On these laptops the first three are each blocked by a *different* thing,
which is why no single fix helps:

```mermaid
flowchart TD
    L["💻 Corporate laptop<br/>LAN 192.168.8.108<br/>Check Point full tunnel"]

    L --> T1{"1 · LAN<br/>talk straight to the machine<br/>in the same room"}
    T1 -->|"❌ route CAPTURED"| X1["The VPN claims our own LAN.<br/>It installs 192.168.8.0/<b>25</b>, which beats<br/>the real on-link /<b>24</b> by longest prefix —<br/>so even the laptop next to us is sent<br/>into the tunnel and never comes back.<br/><i>FR-33 / #905</i>"]

    L --> T2{"2 · Hole-punch<br/>learn my public address<br/>from a STUN server"}
    T2 -->|"❌ srflx NONE"| X2["No public candidate at all —<br/>3 vantage points plus the<br/>public-dial fallback, nothing.<br/>Without one there is no<br/>address for a peer to punch to."]

    L --> T3{"3 · Direct to a peer<br/>at its public address<br/>over UDP"}
    T3 -->|"❌ UDP dropped"| X3["The corp gateway forwards<br/><b>no UDP to the internet</b>.<br/>The packets leave the laptop<br/>and die at the egress."]

    L --> T4{"4 · DERP relay<br/>over TLS :443"}
    T4 -->|"✅ the only one that works"| OK["<b>Not a fallback — the floor.</b><br/>And today that floor is always<br/>wherever the API pods are,<br/>however far away that is."]

    style X1 fill:#fde8e8,stroke:#d33
    style X2 fill:#fde8e8,stroke:#d33
    style X3 fill:#fde8e8,stroke:#d33
    style OK fill:#e6f4ea,stroke:#2a7
```

🔑 **Why blocker 3 is the corp network and not the laptop** — the same host, the
same socket, four destinations:

```mermaid
flowchart LR
    H["the laptop's own<br/>UDP + TCP stack"]
    H --> A["UDP → corp DNS :53"] --> AR["✅ replies, 383 B"]
    H --> B["UDP → 1.1.1.1:53"] --> BR["❌ timeout"]
    H --> C["UDP → STUN :19302 / :3478"] --> CR["❌ timeout"]
    H --> D["TCP → :443"] --> DR["✅ connects"]

    style AR fill:#e6f4ea,stroke:#2a7
    style DR fill:#e6f4ea,stroke:#2a7
    style BR fill:#fde8e8,stroke:#d33
    style CR fill:#fde8e8,stroke:#d33
```

UDP works perfectly *into* the corporate network and dies only on the way *out*
to the internet, while TCP 443 passes. So the host is not blocking anything —
**the policy is at the corporate egress**, and nothing we ship on the laptop can
change it.

🔑 **This is exactly the population FR-19 cannot serve.** Org relays run on UDP
**3478**, chosen because the symmetric-NAT corp population was *measured* to
reach it — but that population is NAT'd, not UDP-blocked. Here 3478 is as dead
as everything else. ⚠️ **Probe a host's UDP egress before proposing an org relay
for it.**

So the only lever left for these hosts is *where their floor lives*, and today
that is not a lever at all.

## The gap, precisely

`agents/roomlerd/src/derp.rs:89` derives the DERP URL from the control WS URL:

```rust
/// Derive the `/derp` WSS URL from the control `ws_url` (`wss://host/ws`).
fn derp_url_from_ws(ws_url: &str) -> String { … }
```

There is no per-tenant, per-region or per-node DERP selection anywhere. ⚠️ Note
`GET /api/relay/regions` is the **TURN/remote-control PoP topology**, not DERP —
easy to mistake for this and it is not.

## Key design

1. **The server names the DERP endpoints; the agent still measures.** A tenant
   may register DERP nodes (URL + label). The netmap carries the candidate set;
   the node **measures** each and uses the best. Heuristics may detect; they
   never decide — the carrier-cascade rule, applied one layer down.
2. **Auth needs no new primitive.** `remote_control::derp_ticket` already mints
   an Ed25519 ticket scoped to `(network_hex, wg_pubkey)` and verifies it
   **offline against a pinned public key** — so a tenant DERP authenticates
   agents without ever calling the control plane, and once it is told its
   networks (`DERP_NETWORKS`, P3) it cannot serve another tenant's network.
   The registry key is already `(network_id, pubkey)`.
3. **The control-plane DERP stays the floor, always.** A tenant node is an
   *addition*. If it is unreachable, slow, or lying, the node falls back — and
   `derp_floor` must be provably intact with every tenant node down.
4. **Ciphertext only, unchanged.** DERP already carries only ciphertext; a
   tenant node learns the same metadata the control-plane DERP already does
   (who talks to whom, when, how much). ⚠️ That is *not* nothing, and the docs
   must say so plainly rather than implying a tenant relay is zero-trust.
5. **Reuse `crates/derp-relay`** — the standalone binary already exists. The
   work is registration, distribution of the candidate set, ticket verification
   at the node, and selection at the agent. Not a new relay.

## Phases

| # | Phase | Kill switch | State |
|---|---|---|---|
| P1 | Server: tenant DERP registration + the candidate set in the netmap (advertised only; agents ignore it) | the registration is opt-in; empty set = today's behaviour byte for byte | not started (the open decisions below) |
| P2 | Agent: measure the candidates, pick the best, **fall back to the control-plane DERP** | `overlay_tenant_derp` (default OFF) | not started |
| P3 | `derp-relay`: verify the ticket offline against the pinned key; refuse any other network | `DERP_NETWORKS` unset = every network, byte for byte as before; set but empty or malformed refuses to start, so a node that cannot read its scope serves nobody | **built** in [#3158](https://github.com/gjovanov/roomler-ai/pull/3158), parked for review. The offline verification already existed; P3 adds the refusal ([field log](#2026-10-30--p3-a-relay-serving-network-b-refuses-network-as-ticket)) |
| P4 | Field: a DERP node near the corp egress; measure RTT on both Check Point laptops before/after | the switch | not started |
| P5 | Docs: extend `docs/overlay-communication.md` + a row in `docs/README.md` | — | not started |

## Acceptance criteria

- [ ] With **zero** tenant nodes registered, behaviour is byte-identical to today
      (the empty-set case is the one that must never regress).
- [ ] A node prefers a tenant DERP **only when measured better**, and re-measures
      — never a static preference, never a ratchet.
- [ ] ⚠️ **`derp_floor` holds with every tenant node down**, proven by taking
      them all down on a host that has no other carrier. *This is the criterion
      that matters*: this feature exists for hosts whose only path is DERP, so a
      bug here does not degrade them — it disconnects them.
- [x] A ticket for network A is **refused** by a node serving network B.
      *P3, [#3158](https://github.com/gjovanov/roomler-ai/pull/3158): `DERP_NETWORKS`.
      Locked by `a_relay_serving_one_network_refuses_another_before_registering`
      (403, nothing registered), `the_scope_matches_a_network_id_exactly` and
      `an_empty_or_malformed_derp_networks_refuses_to_start`. The real binaries,
      RED on master `4f2bccc53` and GREEN on the branch, 2026-10-30
      ([field log](#2026-10-30--p3-a-relay-serving-network-b-refuses-network-as-ticket)).*
- [ ] Field: measured RTT improvement on both Check Point laptops, with the
      before/after recorded. **Must fail first** — record the current 56–97 ms.
- [ ] Docs updated with a diagram, linked from `docs/README.md`.

## Open decisions

- **Who may register a node** — presumably `MANAGE_AGENTS` + a tenant-scoped
  route, but a DERP node is closer to infrastructure than to a device.
- **Whether a tenant node may serve *other* tenants** (a "community relay").
  Default no; it multiplies the metadata question by every participant.
- **Selection metric** — RTT alone, or RTT + loss? The relay tier already has
  measurement machinery; reuse rather than invent.
- **Taken by P3 (2026-10-30), for the operator to confirm: a list, not one
  network.** `DERP_NETWORKS` only ever narrows what a relay serves (unset
  already serves every network, as the shared PoPs do), so it widens nothing,
  and it leaves the community-relay question above to P1: which tenants'
  agents the server sends to a tenant node. A tenant has one overlay network
  (`overlay_networks` is unique on `tenant_id`), so a list longer than one is a
  node its operator opened to more than one tenant.
- **A ticket audience for tenant nodes** (found by P3, for P1). A ticket
  carries no relay binding (`aud: "derp"`, `net`, `pk`, `exp`), and a tenant
  node sees its agents' tickets in the upgrade URL. If tenant nodes verify with
  the same key as the regional PoPs, a hostile or compromised tenant node can
  replay a ticket at a regional PoP within its 24 h and register that agent's
  key there (last writer wins), black-holing the agent's DERP at that PoP. It
  still sees only ciphertext. Minting tenant-node tickets under their own
  audience, or with a relay claim, would keep each kind out of the other's
  relays. P3's scope is the node operator's own setting: it stops a misrouted
  ticket at an honest node, and nothing against a hostile one.

## Out of scope

- Bypassing a VPN's LAN capture — policy evasion, out of scope by standing rule
  and, separately, **measured not to work**: 500 packets to a LAN peer,
  default-bound *and* source-bound to the WLAN address, **zero arrived**;
  `Find-NetRoute` returns the VPN interface with the VPN source, and binding a
  source address does not change the egress interface on Windows.
- Making UDP work where the corp egress drops it. Not ours.
- FR-19 org relays (UDP 3478) — a different population, and useless here.

## Related

- **FR-33 / #905** — the LAN-capture surfacing that named this class in one line
  per host; read `roomler status`'s `lan` line before any archaeology.
- **FR-19 / #805** — org relays; the sibling this cannot reuse, and why.
- **FR-28** — DERP drain on pod roll.

## Field log

### 2026-10-30 — P3: a relay serving network B refuses network A's ticket

The real binaries on loopback, with one throwaway Ed25519 ticket key. A
throwaway probe mints tickets with `derp_ticket`'s own signer, tries the
`/derp` upgrade, sends the registration frame on an admitted socket, holds it
open and reads `derp_registrations` from `/stats`. Every run names the process
that answered it (`netstat` → image) before a probe. Network A is
`6a0000000000000000000a0a`, network B `6a0000000000000000000b0b`.

| run | binary | `DERP_NETWORKS` | ticket for A | ticket for B | `derp_registrations` |
|---|---|---|---|---|---|
| RED | master `4f2bccc53` | B (ignored) | **admitted**, 101 | admitted, 101 | 1 with A alone, 2 with both |
| GREEN | the branch | B | **refused, 403**, nothing registered | admitted, 101 | 0 with A alone, 1 with both |
| GREEN | the branch | unset | admitted, 101 | admitted, 101 | 2, as on master |
| GREEN | the branch | B,A | admitted, 101 | admitted, 101 | 2 |

At startup the branch exits 101 before the bind for `""`, `" "`,
`not-a-network`, B in capitals and `B,`, each with an error naming
`DERP_NETWORKS`, and nothing listens. Master, as the control, ignores an empty
value and listens. Two refusals of A five seconds apart produced one line:

```text
WARN derp_relay: derp-relay: refused a valid ticket for a network this relay does not serve (DERP_NETWORKS) network=6a0000000000000000000a0a serves=6a0000000000000000000b0b derp_scope_refused_total=1
```

Each new guard was mutation-checked against its test. With no check at the
upgrade, `a_relay_serving_one_network_refuses_another_before_registering`
fails. With an empty value read as unset,
`an_empty_or_malformed_derp_networks_refuses_to_start` fails. With a prefix
match, `the_scope_matches_a_network_id_exactly` fails.

⚠️ A dead end, kept: the first GREEN attempt was void. On Git Bash, `kill` of a
background native `.exe` had left the RED run's relay alive on the port, so the
branch's relay failed to bind (`os error 10048`) and the stale master answered
every probe. The re-run gives each relay a fresh port, checks that the port is
free, and names the listener before any probe.
