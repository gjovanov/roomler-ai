<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# FR-32 — Eleven of fourteen plan limits are advertised and enforced nowhere

**Issue:** [#898](https://github.com/gjovanov/roomler-ai/issues/898) ·
**Status:** design ·
**Parent arc:** pricing (P1 of the sequence agreed under [FR-24](FR-24-licensing-split.md))

## Goal

Make the plan matrix true. `GET /api/stripe/plans` publishes fourteen limits per tier and
takes money against them; three are enforced. A customer on Free is told they get 10
members, 5 channels and 100 MB, and the server will happily give them ten thousand of each.

This is **P1 of the pricing arc**, whose ordering was settled deliberately:

> **P1 enforce → P2 meter ([FR-20](FR-20-relay-cost-metering.md), #807) → P3 reshape tiers
> on MEASURED cost → P4 offline keys. Never invert.**

The brief this replaced priced *before* measuring, while admitting its numbers were not
researched. Nothing here re-prices anything: re-pricing is P3 and is gated on FR-20's
per-tenant cost data.

## Root-cause evidence (read on `057aa6e9`, 2026-08-29)

`Plan::limits()` (`crates/db/src/models/tenant.rs:180`) returns a 14-field `PlanLimits`.
Every field is serialised into the public plan matrix by `crates/services/src/stripe.rs`
(lines 313 / 327 / 341). Only these three read it back:

| Call site | Limit | Enforced |
|---|---|---|
| `crates/api/src/routes/agent_org.rs:210` | `max_devices` | yes |
| `crates/api/src/routes/remote_control.rs:157` | `max_devices` | yes |
| `crates/api/src/routes/tunnel.rs:132` | `max_tunnel_clients` | yes |

**Everything else is dead.** `grep -rn '\.limits()' crates/` returns exactly the three rows
above plus the three that *construct* the advertised matrix. The eleven unenforced fields:
`max_members`, `max_channels`, `max_message_history`, `storage_bytes`,
`video_max_participants`, `max_concurrent_sessions`, `exit_nodes`, `magic_dns`,
`recordings`, `ai_recognition`, `cloud_integrations`.

`overlay_mesh` is the fourteenth and is `true` on every tier — vacuously satisfied, not
enforced. It is a marketing row, not a gate, and this FR leaves it alone.

Two findings that were **not** in the prior evaluation:

1. **There is a second, competing source of truth.** `TenantSettings` carries its own
   `max_members: u32` and `file_upload_limit: u64`, shadowing `PlanLimits.max_members` and
   `PlanLimits.storage_bytes`. Both have **zero readers in the entire workspace** — `grep`
   finds `max_members` only in `crates/tests/src/billing_tests.rs:48`, asserting the
   advertised JSON, and `file_upload_limit` / `storage_bytes` nowhere at all. So this is
   not "two enforcers disagreeing": it is two dead fields shadowing two dead fields, and
   whichever one gets wired becomes the answer by default. Decide it, don't inherit it.
2. **The counting primitives mostly do not exist.** `count_active_for_tenant` exists on the
   `agent` and `tunnel_client` DAOs — precisely the two limits that are enforced. Every new
   gate needs its count built first, and *that*, not the `if`, is the work.

`Plan::Enterprise` is a phantom: `tenant.rs:226` gives it Business's limits, `:250`
Business's price (1600), and `get_plans()` never returns it. It is reachable only by writing
the enum value straight into a tenant document.

## Key design

### 1. Observe before enforcing — the house pattern, for the house reason

Switching eleven gates on against a live fleet **locks out tenants who are already over the
line**, and it was us who put them there. The precedent is already in this codebase:
`OverlayNetwork.acl_mode` is `off | warn | enforce`, and `overlay_rpf` defaults to `warn`,
for exactly this reason.

So: `TenantSettings.plan_enforcement: Off | Warn | Enforce`, **default `Warn`**.

- `Off` — no check runs.
- `Warn` — the check runs, the denial is **recorded and logged**, the request **succeeds**.
- `Enforce` — the denial is returned.

`Warn` is the default rather than `Off` because a mode that does nothing produces no data,
and the entire purpose of P1 is to learn who *would* be denied before anyone is.

### 2. One decision point, not eleven

The three existing sites are already copy-paste: count, compare, hand-format a `Forbidden`
string. Eleven more means eleven chances to forget the log line, the mode check, or the
audit row.

One helper instead — `services::quota::check(tenant, Limit::MaxChannels, used)` returning
`Result<(), QuotaDenial>` — so mode handling, structured logging and the audit write happen
in **one** place. Same argument the SSH work settled on: *the refusals are the load-bearing
rows, so recording them cannot be per-call-site* (`decide` returns
`Result<Granted, SshDenyReason>` and `dispatch` records both arms).

`Limit` is an enum and its `match` inside the helper is exhaustive, so adding a fifteenth
field to `PlanLimits` fails to compile until someone decides whether it is a gate — the same
structural trick as `RpcCap::wire()`.

### 3. The mode must not be able to weaken what was already enforced

Found while building P0. Re-pointing the three live sites through the helper, with the
mode defaulting to `Warn`, **would have stopped enforcing the device cap fleet-wide the
moment P0 shipped** — a billing regression arriving inside a change that reads as a tidy-up,
and one no reviewer would likely catch, because the diff looks like pure extraction.

So `Limit::is_established()` marks `MaxDevices` and `MaxTunnelClients`, and an established
limit **ignores the mode entirely**. `PlanEnforcement` stages *new* enforcement; it is not a
switch that turns billing off. A unit test asserts both established limits refuse under all
three modes, so the property cannot be quietly deleted.

### 4. `u32::MAX` is the "unlimited" sentinel, not a number

`PlanLimits` spells unlimited as `u32::MAX` (`Pro.max_members`, `Pro.max_channels`,
`Business.max_concurrent_sessions`) and `-1` for `max_message_history` — not as `Option`.
The first draft of the helper compared against it as a real cap. That is wrong twice: it
would refuse a tenant the published matrix calls unlimited, and it would file denial records
with a `max` of 4 294 967 295 — **poisoning the exact dataset P2 and P3 exist to read**.

Caught by a unit test written before the bug was suspected; the test now pins the sentinel.

### 5. Denials are data

Every check in `Warn` or `Enforce` emits a structured record: tenant, limit, `used`, `max`,
plan, mode, outcome. This is what P2 and P3 read to reshape tiers — "how many Free tenants
are over 10 members, and by how much" is a **pricing input**, not a log line.

Deliberately **not** a new collection on day one: `quota_denials` rows are only worth keeping
once the gates are wired. P0 logs; P1 adds the collection if the volume justifies it.

## Phases

| # | Scope | Kill switch |
|---|---|---|
| **P0** | `plan_enforcement` mode + `services::quota` helper + `Limit` enum + structured denial logging. **No new gate wired.** Re-point the 3 existing sites through the helper, behaviour byte-for-byte identical. | mode `Off` |
| **P1** | Build the missing `count_*` DAOs and wire all 11 limits, **shipped in `Warn`**. Nothing is refused. | mode `Off` |
| **P2** | Read the warn data. Grandfather over-limit tenants (per-tenant override), then flip to `Enforce` tenant by tenant. | per-tenant mode |
| **P3** | Resolve the shapes: delete or repurpose `TenantSettings.{max_members,file_upload_limit}`; give `Enterprise` real limits and a price, or remove the variant. | n/a — schema only |

Gates by enforcement point (P1):

| Limit | Point |
|---|---|
| `max_members` | `invite::accept_invite:157`, `invite::add_member:479` |
| `max_channels` | `room::create:126` |
| `storage_bytes` | `file::upload:249`, `file::upload_room:443` |
| `recordings` | `recording::create:55` |
| `exit_nodes` | `overlay_route::set_exit_node:312` |
| `magic_dns` | `overlay_route::set_magic_dns:471` |
| `ai_recognition` | `integration::recognize_file:16` |
| `cloud_integrations` | **no enforcement point — the feature has no call path** (see below) |
| `video_max_participants` | call join (`crates/services/src/media/`) |
| `max_concurrent_sessions` | remote-session start |
| `max_message_history` | message read path — **a retention bound, not a gate** (see Open decisions) |

### Found in P1a: `cloud_integrations` is sold but not implemented

`crates/services/src/cloud_storage/` (dropbox, google_drive, onedrive) is declared in
`services/src/lib.rs` and **referenced by nothing else in the workspace** — no route, no
`AppState` wiring, no UI. The `StorageProvider::{GoogleDrive,OneDrive,Dropbox}` enum variants
and the `TenantSettings` OAuth credential fields exist, but no code path ever constructs a
client.

So this is not an unenforced limit — it is an **advertised feature with no implementation to
gate**. Deliberately left unwired rather than given a gate at an invented call site: a check
in front of nothing would read like coverage and assert nothing. The gate goes in when the
feature does, in the same change.

### Found in P1a: only *enabling* may be gated

`exit_nodes` and `magic_dns` are toggles, and the gate is on the **enable** path only.
Gating the disable path too would mean a plan downgrade could leave a tenant holding an exit
node or a DNS zone they are not allowed to turn off — strictly worse than the feature having
been free. Same rule for any future toggle.

## Acceptance criteria

- [ ] `grep -rn '\.limits()' crates/` shows **no** hand-rolled comparison outside `services::quota`.
- [ ] Adding a field to `PlanLimits` fails to compile until it is classified in `Limit`.
- [ ] A Free tenant at 10 members: `add_member` **succeeds** under `Warn` and emits one denial record naming `max_members`, `used=10`, `max=10`; **refuses 403** under `Enforce`; is unaffected under `Off`.
- [ ] The same assertion once per wired limit — 11 tests, each proving the gate fires *and* that `Warn` does not refuse.
- [ ] P0 changes no observable behaviour: the 3 existing device/tunnel refusals return the same status and the same message before and after the helper lands.
- [ ] Denial records are queryable per tenant, so "who would break" can be answered before any flip.
- [ ] No tenant is moved to `Enforce` without its warn data being read first — recorded per tenant in the field log.

## Open decisions

1. **`max_message_history` is not a gate.** Free advertises 5 000 messages; enforcement could
   truncate reads, refuse writes, or delete. Truncating reads silently hides a customer's own
   data; deleting destroys it. Leaning: bound the *read* window and say so in the API
   response, never delete — but this needs the operator's call, and it is the one limit that
   can lose data.
2. **Which `max_members` wins** — `PlanLimits` (plan-derived) or `TenantSettings`
   (per-tenant)? Leaning `PlanLimits` as the ceiling with `TenantSettings` as an optional
   *lower* override, which also hands P2 its grandfathering mechanism for free.
3. **`Enterprise`** — give it real limits and a price, or delete the variant? It currently
   charges Business money for Business limits under a different name.
4. Do denials get their own collection with a TTL (like `exec_audit`), or is structured
   logging enough until P2 needs to query them?
5. **`TenantSettings.max_members` defaults to 100 while Free's `PlanLimits.max_members` is
   10** — the two dead sources of truth disagree by 10×, so whichever is wired changes who
   is over the line. Found during P0; feeds decision 2.

## Out of scope

- **Any change to prices or tier shape** — that is P3, gated on FR-20's measured cost.
- Metering relay bytes (FR-20, #807).
- Offline licence keys (P4 of the arc).
- `overlay_mesh`, which is `true` on every tier.
- Anything agent-side; these are all server-side control-plane gates.

## Field-verification log

_(empty — nothing shipped yet)_

---

**Renumbered from FR-31.** [#897](https://github.com/gjovanov/roomler-ai/issues/897)
(opening-keyframe budget) claimed `FR-31` with the lower issue id, and the repair rule is
that the lower issue number keeps `FR-N` while the higher renumbers. `FR-32` was checked to
be *free* rather than *vacated* before being taken — `git log -S "FR-32"` over the ledger
returns nothing and no issue was ever titled FR-32 — because a vacated number must never be
reused.
