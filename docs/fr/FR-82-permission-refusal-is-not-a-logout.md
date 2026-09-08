# FR-82: A permission refusal is not a logout, and a managed role is not frozen at its birthday

**Issue:** [#1540](https://github.com/gjovanov/roomler-ai/issues/1540) · **Status:** proposed 2026-09-08 ·
**Field report:** a GROX member opened `/tenant/69a1dbba…/devices`, got a 403 and
was signed out of the product.

## Goal

Two independent defects, one visible symptom. Both are *classes*, and both are
closed structurally rather than instance-by-instance:

1. **The SPA treated every `403` on a `GET` as a dead session** — it cleared the
   sign-in hint and pushed to `/login`. A 403 is an authorization verdict on a
   credential the server just *accepted*; it is the opposite of a 401. Under
   that rule, every permission-gated read anywhere in the product was a logout
   waiting for the first caller who lacked the bit.
2. **A system-managed role's permission mask was written once, at tenant
   creation, and never again.** Adding a permission bit therefore reached only
   organisations created *afterwards*. Measured on the hosted deployment: **63
   of 72 orgs** still carried `admin = 0x7ffff7` — no `MANAGE_AGENTS`, no
   `REMOTE_CONTROL`, neither audit view — every fleet feature shipped since
   those orgs were created, invisible to their own admins.

## The field event, end to end

The reporting member holds `member` (`0x3cf81`) + a hand-made **`Remote
Operator`** role (`0x3000000` = `MANAGE_AGENTS | REMOTE_CONTROL`) in GROX.
That custom role is itself evidence of defect 2 — the operator built it by hand
because the org's own seeded `admin` role could not see the fleet.

```
mask 0x303cf81
  ├─ MANAGE_AGENTS  ⇒ canSeeFleetNav() true  ⇒ the "Devices" nav item renders
  └─ MANAGE_TENANT  ABSENT
                     │
/tenant/{id}/devices ┤
  ├─ GET /device                    membership-gated   200
  ├─ GET /agent                     membership-gated   200
  ├─ GET /member                    membership-gated   200
  ├─ GET /overlay-node              membership-gated   200
  ├─ GET /tunnel-client             membership-gated   200
  └─ GET /ephemeral-key-settings    MANAGE_TENANT      403  ← EnrollKeysSection, on mount
                                                        │
                              api/client.ts: 403 + GET ⇒ clearSignedIn() + push('/login')
```

Enumerated on the LIVE page rather than read off the source, by loading it in
a browser and reading the network log — the devices page fires **12
tenant-scoped requests**, and `ephemeral-key-settings` is the only one that
needs anything beyond membership:

```
agent?per_page=100 · device?page=1… · agent · member · overlay-node ·
tunnel-client?per_page=100 · member/me · room ×2 · message/unread-count ×5
    ⇒ membership-gated, 200 for any member
ephemeral-key-settings
    ⇒ MANAGE_TENANT, 403 for everyone but the owner
```

One request, one bug, and nothing else on that page waiting to do it again.
(All 200 in the capture because it ran as the OWNER — which is the same reason
the defect went unreported for a week.)

`DevicesView.vue` mounts `<enroll-keys-section>` unconditionally; the card
"fetches the switch itself and stays empty otherwise" (FR-51 P4, #1148,
2026-09-01). The store's `catch` — commented *"left `null` on a 403 — 'not an
admin' is not 'disabled'"* — is correct and was never reached on the branch
that mattered: **the logout fired one layer below it, inside `request()`,
before the throw the store was waiting to catch.**

⚠️ **The owner is the only member of any org this did not hit.** `MANAGE_TENANT`
is deliberately absent from `DEFAULT_ADMIN`, so an org's own *admins* were
logged out by opening their Devices page too. Only the owner survived, via the
`ADMINISTRATOR` bypass — which is why it went unreported for a week.

### Why nothing caught it

`ui/src/__tests__/api/client.spec.ts` asserted the logout as **correct
behaviour**, with a comment explaining the case it existed for. And three
predicates in `utils/permissions.ts` (`canQueryAnalytics`, `canManageInvites`,
`canViewExecAudit`/`canViewSshAudit`) are fail-closed *specifically to route
around this rule* — each written after the same bug surfaced in a different
corner. The codebase had been paying for the rule in instances for months
without anyone pricing the rule itself.

## Key design

### 1. A 403 never ends a session (`ui/src/api/client.ts`)

| body `error` | client does | rationale |
|---|---|---|
| `not_a_member` | leave the **tenant** — `push({name:'dashboard'})`, session intact | You are not in this org. You are still in your others. |
| anything else, **including absent** | throw, nothing more | It is an answer. The caller already handles it. |

⚠️ **The default direction is the whole guarantee.** An *unclassified* 403 does
nothing but throw, so a newly-added permission-gated route is inert here by
construction — not "safe until somebody remembers to add a predicate for it".

⚠️ The verdict comes from the **server**, not a message-string sniff on the
client: `chat`'s `Forbidden("Not a member of this room")` is a 403 of exactly
the same shape, and evicting someone from their organisation over a private
channel would be absurd.

### 2. `not_a_member` is a machine-readable code, not prose

`ApiError::NotAMember` (`crates/core/src/error.rs`) ⇒ `403 {"error":
"not_a_member"}`; `DaoError::NotAMember` carries it up from
`TenantDao::get_member_permissions`. The 40 `ApiError::Forbidden("Not a
member".to_string())` call sites across the api crate and all six modules
become the variant; the one room-scoped refusal deliberately does not.

### 3. `MANAGED_ROLES` — one table, seeded from and reconciled to

`crates/db/src/models/role.rs`. Before it there were **two** divergent copies:
`TenantDao::create_default_roles` (which ran) and `RoleDao::seed_defaults`
(dead — Titlecased names, no `guest`, and a `Moderator` carrying
`MANAGE_MEETINGS` and *not* `REMOTE_CONTROL`, the reverse of the live one). The
dead copy is deleted; tenant creation and the reconcile now read the same rows.

`TenantDao::reconcile_managed_roles()` runs at startup under the **existing
`startup_maintenance` lease**, so exactly one pod does it per window.

⚠️ **Additive: `new = stored | definition`, never a replace.** A managed role's
mask *is* editable (`PUT …/role/{id}`; only *deletion* is refused), so an org
may legitimately have added a bit. Overwriting would silently revoke that;
OR-ing cannot. **The cost, stated so nobody rediscovers it: a bit REMOVED from
a definition — a tightening — does not propagate**, and needs its own migration
that says out loud whose permissions it takes away.

⚠️ Grouped by `(name, permissions)`, one `update_many` per group — 12 writes
for the whole deployment, not 360. Matching the exact stored value also makes
it idempotent and safe against a concurrent editor: a role changed underneath
falls out of its group's filter instead of being clobbered.

⚠️ A managed row whose name is not in the table is **reported and left alone**.
Visible drift beats drift corrected by guesswork.

### The measured drift

| role | stored | → target | gains | orgs |
|---|---|---|---|---|
| `owner` | `0xffffff` | `0x7fffffff` | `0x7f000000` | 63 |
| `owner` | `0x1fffffff` / `0x7ffffff` | `0x7fffffff` | — | 1 + 1 |
| `admin` | `0x7ffff7` | `0x577ffff7` | `0x57000000` | 63 |
| `admin` | `0x77ffff7` / `0x177ffff7` | `0x577ffff7` | — | 1 + 1 |
| `moderator` | `0x7ef91` | `0x207ef91` | `REMOTE_CONTROL` | 63 |
| `member` | `0x3cf81` | unchanged | none | 72 |
| `guest` | `0x801` | unchanged | none | 72 |

Each distinct `owner` mask is a snapshot of `permissions::ALL` on the day that
org was created — the freeze, visible in the data.

⚠️ `owner` gaining `0x7f000000` is a **no-op in effect**: the row already
carries `ADMINISTRATOR` and passes `has()` by the bypass. It is reconciled so
the stored mask stops lying, not to change an outcome.

⚠️ **`EXEC_DEVICE` and `SSH_DEVICE` are in no row below the `ADMINISTRATOR`
bypass.** The reconcile grants whatever the table says to every existing org,
so `DEFAULT_ADMIN |= EXEC_DEVICE` — a one-token edit that reads as tidying —
would open exec-as-SYSTEM on the whole deployment at the next boot, with no
migration to review and no admin action to audit.
`no_managed_role_below_administrator_seeds_a_root_shell` makes that
unrepresentable. (`owner` holds both as part of `ALL`, where the bypass already
answered true for every bit — the test skips a row carrying it, because failing
there would be failing for a reason unrelated to the risk. The first draft of
the test did not, and said so on the first run.)

### 4. The org switches are not asked for without the bit

The three `MANAGE_TENANT` GETs (`exec-settings`, `ssh-settings`,
`ephemeral-key-settings`) are gated **in the store**, not per call site — the
rule FR-75 (#1447) paid for on the `network` routes. After (1) a 403 there is
harmless; this is about not spending a round-trip on a refusal known in advance,
and not filling the browser console and server log with 403s that make the real
ones harder to see.

⚠️ It **awaits** `ensureMyMembership` rather than reading `myPermissions`:
AppLayout's `/member/me` is in flight while the page mounts, so a synchronous
read is a coin-flip between "hidden from the owner" and "fires anyway".

## Phases

| # | phase | kill switch | status |
|---|---|---|---|
| 1 | `not_a_member` on the wire; 40 call sites; the client stops logging out | none — reverting is a one-line revert of the 403 branch, and the old behaviour is the defect | built 2026-09-08 |
| 2 | `MANAGED_ROLES` as the one table; dead `seed_defaults` deleted | none — pure de-duplication, byte-identical seed output | built 2026-09-08 |
| 3 | `reconcile_managed_roles` under the startup lease | `ROOMLER__AUTH__RECONCILE_MANAGED_ROLES=false` (default on; warns at boot when off) | built 2026-09-08 |
| 4 | store-level `MANAGE_TENANT` gate on the three org switches | none needed — after phase 1 the ungated 403 is harmless, so the worst case of a wrong gate is a hidden card, not a logout | built 2026-09-08 |
| 5 | docs updated with diagrams, linked from `docs/README.md` | — | `docs/permissions.md`, 2026-09-08 |

## Acceptance criteria

- [ ] **AC1** A permission 403 on a GET leaves the session intact — locked by a
      unit test that replaces the one asserting the opposite.
- [ ] **AC2** An *unclassified* 403 neither logs out nor navigates.
- [ ] **AC3** A `not_a_member` 403 leaves the tenant and keeps the session.
- [ ] **AC4** A non-member's tenant-scoped GET answers `error: "not_a_member"`;
      a member-without-the-bit answers `error: "forbidden"` naming it.
- [ ] **AC5** `reconcile_managed_roles` raises a stale mask to its definition,
      **preserves a bit an org added itself**, and is a no-op on a second run.
- [ ] **AC6** No managed role below the `ADMINISTRATOR` bypass seeds
      `EXEC_DEVICE` or `SSH_DEVICE`.
- [ ] **AC7** The three org-switch GETs are not fired without `MANAGE_TENANT`,
      and the flag stays `null` (unknown) rather than `false`.
- [ ] **AC8** Field: the reporting member opens the GROX Devices page, the grid
      renders, and the session survives.
- [ ] **AC9** Field: prod logs show the reconcile's arithmetic once, and a
      second pod restart reports "already match their definitions".
- [ ] **AC10** Docs updated/created with diagrams, linked from `docs/README.md`.

## Open decisions

- **Should `MANAGE_TENANT` join `DEFAULT_ADMIN`?** Left alone deliberately —
  configuring the organisation is the owner's job, and that is a policy call,
  not a bug. The consequence, now visible rather than fatal: an org's admins do
  not see the Devices page's enrollment-keys card. Revisit if operators ask.
- **A bit removed from a definition** does not propagate (see §3). No mechanism
  proposed until there is a real tightening to carry.

## Out of scope

- `token_epoch` / session revocation (the open MEDIUM in `CLAUDE.md`): a stolen
  access token is still valid for 7 days. Unrelated to this arc, and a 401
  concern rather than a 403 one.
- The permission-bit ceiling (bit 52; `ui/src/utils/permissions.ts` does its
  mask arithmetic without bitwise operators for that reason).

## Field-verification log

_(to be filled — AC8/AC9 need a promoted hosted image.)_
