# FR-20 — Per-tenant relay cost metering

**Issue:** [#807](https://github.com/gjovanov/roomler-ai/issues/807) · **Status:** design · **Owner:** @gjovanov

## Goal

Attribute **relay cost to the tenant that caused it**, and surface it in the two
places that already exist: `/observability` for the platform owner and
`/tenant/{tid}/analytics` for org admins.

This is the precondition for every pricing decision. Pricing before measuring is
how you set numbers you cannot defend, so this FR ships **no quota and no
enforcement** — those are a separate FR, opened only after a month of real data.

## Root cause / field evidence

Measured against `origin/master` @ `fa364b12`.

### What already exists — more than a licensing/pricing audit assumed

The observability subsystem is substantial and shipped:

| Collection | Keyed by | Retention |
|---|---|---|
| `stats_relay` | **`{region, ts}`** | 7 d + `_1h` 90 d + `_1d` 730 d |
| `stats_machine` | `{tenant_id, agent_id, ts}` | 7 d + rollups |
| `stats_call` / `stats_call_user` | `{tenant_id, room_id/user_id, ts}` | 7 d + rollups |
| `call_sessions` | `{tenant_id, room_id, started_at}` | 730 d |
| `stats_events` | `{tenant_id, agent_id, ts}` presence ledger | 730 d |
| `stats_mesh`, `ws_sessions`, `page_views` | topology, sessions, pageviews | 7–90 d |
| `tunnel_audit` | per-flow bytes + `RelayMode` | 90 d |

Served by `routes/stats.rs` (1 448 lines), `routes/usage.rs` (1 096 lines,
per-user minutes+bytes across remote-desktop / calls / tunnels), `stats_rollup.rs`,
`relay_load.rs`, `media_stats.rs`. Both UIs exist and are populated:
`ObservabilityView.vue` (relay fleet, orgs, users & sessions, platform-wide
participant-minutes, usage by person) and `AnalyticsView.vue` (machines online,
transports direct/relay/derp, peer latency, peak participants, call minutes,
tunnel traffic, flows direct vs relayed).

### The gap

**`stats_relay` is keyed `{region, ts}`** (`crates/db/src/indexes.rs:523`). It
records CPU, memory, rx/tx rate, DERP registrations and coturn allocations *per
PoP* — fleet health, not consumption. There is no per-tenant relay byte anywhere
in the system, so the question *"what did org X cost me last month?"* has no
answer, and neither does *"what is the gross margin on the Pro tier?"*

The two genuinely variable costs are relay traffic (coturn TURN + DERP) and the
mediasoup SFU. Neither is attributed. Everything else — device count, signalling,
mesh coordination, chat — is effectively free per unit.

### Attribution is already reachable at all three relay tiers

Verified in code, not assumed:

| Tier | Tenant known? | Byte count available? |
|---|---|---|
| API-pod DERP (`crates/api/src/ws/derp.rs`) | **yes** — `verify_agent_token` + authoritative DB lookup (`derp.rs:236`); a forged tenant claim cannot widen scope | **yes** — `frame.len()` already computed on that path for `tunnel_budget_permits` (`derp.rs:508`) |
| PoP DERP (`crates/derp-relay`) | via the ticket's `network` → `overlay_networks` → tenant | in-process; the binary is deliberately **DB-free** |
| coturn TURN | username is `{expiry}:{user_id}` (`crates/remote_control/src/turn_creds.rs:63`) — user, **not** tenant | `coturn_prometheus()` already scrapes; reads only `turn_total_allocations` / `turn_total_sessions` today |

## Key design

### The rule: bill only on what we measured ourselves

`tunnel_audit`'s byte columns are reported by the **client endpoint** on flow
close — the server cannot measure them, because the payload is P2P over the data
channel (`routes/usage.rs:14-19`). That is a *claim by a host we do not control*.

Same distinction CLAUDE.md already draws between `ssh_audit` (the server's own
decision, authoritative) and `ssh_activity` (the device's account of itself).
⚠️ **They are never folded together.** Client-reported bytes stay in analytics,
labelled as such; only server-measured bytes enter the cost ledger.

### One ledger

`stats_usage`, following the shipped pattern exactly — deterministic string `_id`
= `{tenant_id}:{meter}:{bucket}` so every writer is an **idempotent upsert**.
That, not a lease, is what makes the 2-pod deployment race-free; it is the same
property `stats_relay` and `stats_machine` already rely on. Rollups `_1h` (90 d)
and `_1d` (730 d) come from the existing `stats_rollup` task via `$merge` on `_id`.

Meters — **cost drivers only**:

```
derp_bytes                relay bytes forwarded on this tenant's behalf
turn_bytes                coturn-relayed bytes
sfu_participant_seconds   the SFU's real marginal cost
storage_bytes             gauge
```

⚠️ **Never metered:** direct P2P sessions, device count, signalling, mesh
coordination, chat messages. A direct session costs the control plane kilobytes
of signalling; metering it would invert the growth model and punish exactly the
outcome the NAT-traversal work exists to produce.

### Three collection points

**A · API-pod DERP.** One lock-free atomic add into a per-network map inside
`forward_frame`, flushed to `stats_usage` on a 60 s timer. ⚠️ **Never a Mongo
write per frame** — and nothing else on that path: this is where relay latency
lives, and FR-18 is actively fighting queueing there.

**B · PoP DERP.** `derp-relay` stays DB-free (a design invariant of that binary —
it holds only the ticket's public key and no Mongo at all). It counts bytes per
`network` in an in-process map and exposes a new `per_network` object in the
`/stats` payload that `relay_load.rs` already polls every 30 s; the poller
resolves network → tenant and writes the buckets.
⚠️ These are **cumulative counters**, so the poller must diff successive samples —
the same shape `machine_series_pipeline` already uses for agent counters
(`stats.rs:190-201`). A PoP restart resets mid-bucket, so that bucket
**under-reports rather than going negative** — the identical trade the host-total
`net_*_bytes` columns already make. Do not invent a different one.

**C · coturn TURN.** Extend `coturn_prometheus()` to read `turn_traffic_*`.
⚠️ The username is `{expiry}:{user_id}` and a user may belong to several orgs, so
the username alone cannot attribute. ⚠️ **Do not change the username format** — it
is HMAC input (`turn_creds.rs:55-70`) and both ends would need coordinating.
Instead write a TTL'd `turn_grant` map (username → tenant) at *issuance*, where
the API already knows the tenant. A grant that has expired out of the map yields
an **unattributed** bucket, never a wrongly-attributed one.

### Cost model

`config/relay-costs.toml` maps meter → unit cost, edited in one place, so
`/observability` can render currency and margin without any price constant
appearing in code.

### Surfaces

**`/observability`** (platform allowlist) — a *Cost & usage* section under the
existing relay-fleet block:

- fleet totals per meter over the range + estimated cost
- per-org table: org · relay GB · SFU hours · storage · est. cost · plan · MRR ·
  **margin**, sorted by cost
- **relayed fraction** — the fraction of traffic that could not go direct. The
  most valuable number on the page: simultaneously the cost driver *and* a
  NAT-traversal regression alarm that fires before any user complains.

**`/tenant/{tid}/analytics`** (membership, fail-closed) — a *Usage* section
extending the existing "Transports (direct / relay / derp)" card: relay bytes,
SFU participant-hours, storage, and the org's own relayed fraction — framed as
**actionable**, because a high relay fraction means that org's network is
blocking direct paths, which they can act on. A quota bar is left as a slot,
wired dark until quotas exist.

⚠️ Both surfaces reuse the existing gates unchanged, and **failures stay 404,
never 403** — the web client wipes tokens on 403, so a member removed mid-poll
must not be logged out of everything (`stats.rs:45-47`).

## Phases

| P | Scope | Kill switch |
|---|---|---|
| **P1** | `stats_usage` + rollups + **A** (API-pod DERP). Smallest end-to-end slice; proves the shape on real traffic. | meter writes are additive; flag off ⇒ no rows |
| **P2** | **B** — PoP `per_network` counters + poller attribution | PoP omitting `per_network` ⇒ "not monitored", never `0` |
| **P3** | **C** — coturn `turn_traffic_*` + `turn_grant` map | absent grant ⇒ unattributed bucket, not a wrong one |
| **P4** | `sfu_participant_seconds` folded in — mostly a rollup over existing `stats_call_user` | rollup-only |
| **P5** | `relay-costs.toml` + `/observability` Cost & usage section | UI-only |
| **P6** | `/tenant/{tid}/analytics` Usage section | UI-only |

## Acceptance criteria

- [ ] A forced-relay transfer of *N* MB between two fleet hosts appears as *N* MB ±5% in that tenant's `stats_usage` within one rollup interval (driven via `roomler exec`, `derp_floor` forcing the path)
- [ ] **The same transfer over a direct carrier writes zero relay bytes** — the load-bearing test; if direct traffic ever meters, the growth model is broken
- [ ] Killing a PoP mid-bucket under-reports; **no bucket is ever negative**
- [ ] Two pods writing the same bucket yield exactly one row (asserted in `crates/tests`)
- [ ] Ledger fleet total agrees with the PoPs' own `net_tx_bytes` within a stated tolerance — an independent cross-check that the meter is honest
- [ ] A tenant with no relay traffic renders `0`; an unmonitored PoP renders "not monitored" — never the same cell
- [ ] `/observability` shows per-org cost + margin and the fleet relayed fraction
- [ ] `/tenant/{tid}/analytics` shows the org's own usage and relayed fraction
- [ ] Measured DERP forward-path overhead is within noise of the pre-change baseline

## Open decisions

1. Whether P4 (SFU participant-seconds) belongs here or in its own FR — it is
   nearly a pure rollup over data that already exists, which argues for here.
2. Currency and unit costs in `relay-costs.toml` — owner-supplied; the FR only
   builds the mechanism.
3. Whether `/tenant` analytics should show a currency figure at all before
   quotas exist, or only raw units.

## Out of scope

- **Quotas and enforcement.** Deliberately deferred: measure for a month first.
- Any change to a price, tier or plan id.
- The 11 declared-but-unenforced `PlanLimits` fields — own FR.
- Licensing (`AGPL`/`MPL` split) — separate program, undecided.

## Related

- Child of the pricing program; **blocks** any tier reshape
- A rising relayed fraction is a NAT-traversal regression signal for
  [#767](https://github.com/gjovanov/roomler-ai/issues/767) (FR-1),
  [#801](https://github.com/gjovanov/roomler-ai/issues/801) (FR-18)
- [#805](https://github.com/gjovanov/roomler-ai/issues/805) (FR-19, peer relays)
  would add a **fourth** relay tier that must also be metered

## Field-verification log

| date | what | result |
|---|---|---|
| 2026-08-28 | Audit of the shipped stats subsystem | 10 collections + 2 populated UIs already exist; the gap is attribution, not measurement |
| 2026-08-28 | `stats_relay` key inspection (`indexes.rs:523`) | `{region, ts}` — fleet health, **no tenant dimension** |
| 2026-08-28 | Attribution reachability at all three relay tiers | API-pod DERP: tenant + length both already in hand; PoP: ticket carries `network`; coturn: needs the grant map |
| 2026-08-28 | `tunnel_audit` byte provenance (`usage.rs:14-19`) | client-reported on flow close ⇒ **not billable**, analytics only |
