# Business model

How Roomler makes money, what it costs to run, and which of those numbers are
measured rather than assumed. This is the document the pricing decisions are made
*from* — if a tier changes, the argument for it belongs here.

Companion documents: [`LICENSING.md`](../LICENSING.md) (what the split permits),
[`COMMERCIAL.md`](../COMMERCIAL.md) (the exception we sell), and the FRs behind
each mechanism — [FR-24](fr/FR-24-licensing-split.md) licensing,
[FR-32](fr/FR-32-plan-limit-enforcement.md) enforcement,
[FR-20](fr/FR-20-per-tenant-relay-cost-metering.md) metering.

---

## 1. Three revenue mechanisms, one product

There is exactly one codebase. Money arrives three ways.

| | Who pays | For what | Status |
|---|---|---|---|
| **Cloud subscription** | orgs on `roomler.ai` | seats + capacity above the free tier | live (Stripe) |
| **AGPL exception** | a company embedding or re-hosting the control plane in a proprietary product | permission the AGPL withholds | mechanism live, unpriced |
| **Self-hosting** | nobody | — | free, unlimited, forever |

Self-hosting being free is not charity and not a funnel trick: it is the
distribution channel. The product asks people to run a privileged daemon that can
see their screen, and for an unknown vendor **auditable source is the only trust
substitute available**. Anything that weakens it — telemetry from self-hosted
installs, a crippled community build, a licence key check — costs more than it
could earn. See [`LICENSING.md`](../LICENSING.md).

### Why the licence is a revenue mechanism and not a moat

FR-24 measured this rather than assuming it: **AGPL covers ~37% of the codebase
and 0% of the differentiated code.** `crates/api` links `tunnel-core`
unconditionally, so the carrier cascade is shared and must take the permissive
licence; the encoder cascade is agent-side and permissive too.

So the AGPL does not protect the crown jewels, and we do not claim it does. What
it does is make **hosting the control plane commercially unattractive without a
licence** — and the buyer of that licence is a procurement department reacting to
the letters "AGPL", not an engineer reading the dependency graph. That is the
mechanism. It works at 37% coverage.

---

## 2. What actually costs money

Almost nothing scales with usage. Two things do.

| Cost driver | Scales with | Metered? |
|---|---|---|
| **Relay traffic** (DERP + TURN) | bytes that could not go peer-to-peer | ✅ `derp_bytes` · ⚠️ `turn_bytes` blocked |
| **SFU conferencing** | participant-seconds | ✅ `sfu_participant_seconds` |
| Object storage | GB stored | not yet metered |
| Everything else | ~flat | n/a |

**Direct peer-to-peer sessions cost the control plane kilobytes of signalling.**
A remote-desktop session that finds a direct carrier moves its pixels between the
two endpoints and never touches us. This is why the free tier can be generous
about devices and sessions, and it is why:

> ⚠️ **Direct traffic is never metered and never capped.** FR-20 carries this as
> its load-bearing acceptance test — *a direct-carrier transfer must write zero
> relay bytes*. Metering it would bill users for the exact outcome the whole
> NAT-traversal programme exists to produce, and would make our incentives point
> the wrong way.

### The relayed fraction is the number to watch

The share of traffic that falls back to a relay is simultaneously:

- **the cost driver** — it is the only traffic we pay for; and
- **a product-quality alarm** — it rises when NAT traversal regresses.

It is on `/observability`. A jump there is a bug report before it is a bill.

---

## 3. What the meter has measured so far

First real data, 2026-08-30 (metering went live the same week — this is a
datapoint, **not** a basis for pricing):

| | |
|---|---|
| `derp_bytes`, 24 h | 1 765 944 520 B (~1.77 GB) across **2 tenants** |
| `sfu_participant_seconds`, 24 h | 3 930 s across 1 tenant |
| Regional PoP fleet | **~idle** — all five regions at `tx_mbps` ≈ 0.001 |

Two things follow immediately:

1. **Relay volume today is trivial**, and one tenant is essentially all of it.
   Nothing in the current tier ladder is being tested by real cost.
2. **The regional PoPs are carrying almost nothing** — the fleet's DERP rides the
   API pods. That is a fixed cost being paid for capacity nobody uses yet, and it
   is a question for the infrastructure budget rather than the price list.

⚠️ **Do not price from this.** The stated sequence is measure first, then reshape
(§5). One day of data from two tenants, one of them ours, decides nothing.

---

## 4. The ladder today

Prices are USD/user/month, live in Stripe since 2026-07-27. Plan ids
(`free`/`pro`/`business`) are stored in tenant documents and matched in the
webhook, so **renaming one is a migration**, not a copy change.

| | Free | Pro $8 | Business $16 |
|---|---|---|---|
| Devices | 3 | 30 | 300 |
| Tunnel clients | 3 | 30 | 300 |
| Concurrent RC sessions | 1 | 5 | ∞ |
| Members | 10 | ∞ | ∞ |
| Storage | 100 MB | 10 GB | 100 GB |
| Video participants | 4 | 10 | 100 |
| Exit nodes / MagicDNS | — | ✅ | ✅ |
| Recordings / AI | — | — | ✅ |
| Overlay mesh | ✅ | ✅ | ✅ |

Notes that matter more than the numbers:

- **The mesh is on every tier, including Free.** It is the product's spine and
  costs nothing when it goes direct.
- `Enterprise` exists in the `Plan` enum with Business's limits and price and is
  never returned by `get_plans()`. It is a **placeholder, not a tier.**
- Free's `video_max_participants` was `0` until FR-32 and enforced nowhere, so
  Free tenants have always held calls. It is now **4** — enforcing `0` would have
  removed a capability people already use, which is a different act from holding
  a line.

### Enforcement posture

Fourteen limits, one decision point (`services::quota`), three modes per tenant:
`Off` → `Warn` (record the denial, allow the request) → `Enforce`.

⚠️ **Grandfather before enforcing.** `/plan-compliance` answers *"who would break
if I turned this on?"* as a **snapshot**, not from denial logs — a tenant sitting
at 40 members on a 10-member plan that nobody adds to this month emits no denials
at all and would be flipped straight into a wall.

---

## 5. The sequence, and why this order

The originating strategy brief proposed a new EUR ladder up front. That was
rejected: it set prices before anything measured what a tenant costs, while
admitting the numbers were not researched against our cost base.

| | | status |
|---|---|---|
| **P1** | Make the advertised limits true — enforce all fourteen, log every denial | ✅ FR-32 |
| **P2** | Meter the real cost drivers per tenant; expose, don't bill | ✅ FR-20 |
| **P3** | **Reshape tiers on measured cost** — and only then decide currency, the free-device cap, and whether an MSP tier exists | ⬜ needs ~a month of P2 data |
| **P4** | Offline Ed25519 licence keys for self-hosted Enterprise | ⬜ when someone asks to buy one |

Denials from P1 are the other half of the input: they are the **best
upgrade-intent signal available**, and they come free with enforcement.

### What P3 has to answer

- **Is per-user the right metric?** TeamViewer/ScreenConnect/AnyDesk price per
  *concurrent technician*, because three humans support five hundred machines.
  Our device caps (30/300) become the binding constraint for exactly the customer
  who would pay most. `max_concurrent_sessions` already exists and is enforced —
  the metric is half-built.
- **Is Free too tight at 3 devices?** Self-hosting is unlimited and free, so the
  cloud free tier's only job is converting people who would rather not self-host.
  Three devices may be too few to fall in love with a mesh product.
- **Does conferencing stay?** It is the one component with genuinely high
  marginal cost and the least differentiation.
- **USD or EUR**, and whether `Enterprise` becomes real or is deleted.

---

## 6. Rules that do not change with the price list

1. **Self-hosted never phones home.** No licence check, no telemetry, no
   activation. Currently true by construction; it must stay a design property.
2. **Never meter or cap direct peer-to-peer.**
3. **Bill only on what the server or its own relays measured.** `tunnel_audit`'s
   byte columns are reported by the client endpoint — a claim by a host we do not
   control. They inform analytics and never the ledger.
4. **Never kill a live session over billing.** Quota exhausted ⇒ relayed
   connections refused with a message naming the quota; direct connections keep
   working. Someone debugging a production box at 2 a.m. must be told *why*.
5. **Failed payment degrades, never deletes.** Grace period → Free entitlements.
   Devices and org data survive.
6. **Nothing is removed from the community edition** to make a paid tier look
   better. That inverts the distribution strategy.

---

## 7. Open decisions

| # | Decision | Blocked on |
|---|---|---|
| 1 | Tier reshape — metric, prices, currency | ~a month of FR-20 data |
| 2 | Price the AGPL exception | first real enquiry |
| 3 | `Enterprise`: make real or delete | #1 |
| 4 | HEVC patent pool: licence, or ship HEVC off by default in commercial builds | operator |
| 5 | Retire or right-size the idle regional PoP fleet | infra budget review |
| 6 | Meter object storage | whether it ever becomes material |

---

*Numbers in §3 are measurements with a date attached. Numbers in §4 are what the
code does today. Everything in §5 is a plan, and plans in this document are
expected to be wrong until §3 has enough data to correct them.*
