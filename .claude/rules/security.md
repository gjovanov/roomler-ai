---
paths:
  - "crates/api/**"
  - "crates/core/**"
  - "crates/modules/**"
  - "crates/services/**"
  - "crates/db/**"
  - "ui/src/**"
  - ".githooks/**"
  - ".claude/hooks/**"
  - "scripts/gh-*.sh"
---

# Security baseline — the rules that bind new code, with the why

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps one line per rule. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

## Security Baseline

Full treatment, with the reasoning behind every control:
**[`docs/security-baseline.md`](docs/security-baseline.md)**. Permission bits and
the managed-role reconcile: `docs/permissions.md`. The rules that bind new code:

- **`is_member(tid)` is NOT an authorization check for anything keyed by id.**
  Anyone can create a tenant for free, so a caller can always satisfy `is_member`
  for a tenant they own and then pass **another** tenant's `room_id`/`message_id`.
  Resolve the object **within** the tenant: `guards::require_room_in_tenant` /
  `require_message_in_room` / `require_file`, so a foreign id 404s and leaks neither content nor
  existence. ⚠️ A handler keyed by `message_id` must use the **message** guard — the
  two ids are decoupled. ⚠️ Never re-fetch with a bare `find_by_id` after a
  tenant-scoped write.
- **Membership authorizes no grant.** A route that writes an access grant (a tunnel or
  overlay ACL policy, a route or exit-node approval), or a setting every node of the
  org applies (`PUT …/magic-dns`), needs a management bit decided FIRST, before the id
  is looked up or the body read; a create route has no id to resolve and `is_member`
  is still not enough. Every `…/tunnel-policy` and `…/overlay-acl` route is
  `MANAGE_AGENTS`, the reads too: the list is the org's whole ACL. `GET …/magic-dns`
  stays any member's: every node receives it anyway. ⚠️ The forward reads no bit, and
  must not: the policy IS the grant, made for people who hold none
  (security-baseline.md §3).
- **A removed thing answers nothing** (#3327). `BaseDao::find_by_id_in_tenant`
  answers live rows only, and a soft delete matches a live row only. The
  tombstone-inclusive `find_any_by_id_in_tenant` is a `disallowed-methods` entry in
  the root `clippy.toml`: only a named DAO reader (`find_any_in_tenant`) calls it,
  under `#[expect(clippy::disallowed_methods, reason = "…")]`, for a refusal that
  must say why or an audit view of a removed thing. ⚠️ The live predicate is
  `deleted_at: null`; `{"$type": "null"}` is for partial unique indexes and 404s a
  live row with no field. ⚠️ A removed device must still be told it was removed:
  the revocation checks keep the tombstone reader, or the device retries a bare
  401 forever.
- **Call media is authorized by the caller's call seat (`call/join`), never by org membership.** Every `media:*` frame checks the seat, and losing the membership revokes it.
- **A 403 is an answer, not an expired credential.** It never ends a session, on any
  method. The only 403 with a navigation is `not_a_member`, matched on a
  **server-sent code**, never a message-string sniff — chat's
  `Forbidden("Not a member of this room")` is the same shape and must not evict
  anyone from their org. ⚠️⚠️ An unclassified 403 does nothing but throw, so a
  newly-gated route is inert on the client by construction.
- **A client never reads the database's words.** `DaoError::DuplicateKey` holds
  MongoDB's text and always answers `That already exists`; a sentence written for the
  person is `DaoError::Conflict`. ⚠️ Don't pass `msg` through to "keep the detail": it
  names the database, the index and the value, and the log already keeps where it
  collided (`duplicate_key_site`). ⚠️ A client reads the status or a code, never
  `message`: three organization forms once matched `E11000`.
- **A system-managed role is reconciled, not frozen at its birthday** — one
  `role::MANAGED_ROLES` table, reconciled at boot under the startup lease.
  ⚠️⚠️ **Additive (`stored | definition`), never a replace** — a managed mask *is*
  editable, so overwriting would silently revoke an org's own grant.
  ⚠️⚠️ `EXEC_DEVICE`/`SSH_DEVICE` are in no row below the `ADMINISTRATOR` bypass:
  `DEFAULT_ADMIN |= EXEC_DEVICE` — a one-token edit that reads as tidying — would
  open exec-as-SYSTEM deployment-wide at the next boot, with no migration to review
  and no admin action to audit.
- **`users.email` holds an address only if that account PROVED it.** It is a UNIQUE
  index, so it is a *reservation*, not a contact field, and it is what
  account-linking keys off. An unverified provider assertion takes a `.invalid`
  placeholder; linking checks the **target** account is verified too; a proven
  identity **evicts** an unproven claim. ⚠️ Do not "simplify" this back to a
  one-sided `email_verified` check — that state left both the reservation and the
  unactivated-signup paths open. ⚠️ Microsoft is the only provider that is *always*
  unverified.
- **An open form that mails whoever its body names answers before it looks.**
  `POST /api/auth/resend-activation` gives one 202 for every address, and
  `accept_resend` is deliberately not `async`, so neither the bytes nor the latency
  can depend on the row. The per-account cooldown is silent: a 429 there would fire
  only for an address with an account. Detaching just the mail is not enough, because
  a known address also costs more Mongo round trips.
- **Agent tokens are status-checked on every use** via
  `crates/api/src/extractors/agent.rs::AuthAgent`. ⚠️ A lookup **failure is 500, not
  401** — a Mongo blip must not tell a healthy fleet its credentials were revoked,
  turning a database wobble into an enrollment storm. ⚠️ **Deletion wins over
  status**: an Online-looking tombstone still refuses.
- **A single-use enrollment token is spent once, through one owner** (#3328). A door
  verifies it (an `Unspent` value, which mints nothing), runs every refusal that must
  leave it usable, spends it with `UsedTokenDao::spend`, and only then writes. The
  credential mints take the `Spent` value by value, so a new door that skips the spend
  does not compile; renewals keep their own `renew_*` entry. ⚠️ Keep the two refusals
  apart: 401 "already used" vs a retryable 503 when the ledger cannot answer.
  ⚠️ A tunnel client's known `machine_id` re-enrolls only for its owner (live) or a
  `MANAGE_AGENTS` holder (live or removed): decided before the spend, held again by the
  write (`ReviveScope`). A removed client the holder brings back becomes the holder's
  (`owner_user_id`, read and written in one pipeline update); a live one keeps its owner.
- **A tunnel client ends with its owner's org membership.** The member route's
  `MembershipLifecycle` cascade removes the person's tunnel clients in that org through
  `end_tunnel_client`, the manual DELETE's own path (tombstone, THEN the overlay release),
  and a tunnel enrollment token whose subject is no longer a member enrolls nothing (403
  before the spend, asked again after the write). ⚠️ network's membership hook answers
  `Ok` even when a client could not be ended (logged, `alert = membership_tunnel_end`):
  an `Err` would stop the cascade before the call seats and room rows; what it leaves is
  `POST /api/admin/tunnel-clients/prune`'s. Devices the person enrolled are out of it.
- **An enrollment credential ends with its issuer's right to enroll devices.** A daemon
  enrollment token and an FR-51 ephemeral key act for the admin who minted them, so
  `POST /api/agent/enroll` and `POST /api/agent/anchor` ask the mint's own gate again
  (`fleet::agent::issuer_may_enroll`, `require_permission` with `MANAGE_AGENTS` for the
  `sub`) after the archived gate and before the spend or `claim_use`. ⚠️ The refusal is a
  plain 403 `forbidden`, never `NotAMember`: it is about someone other than the caller.
  ⚠️ A failed read is a 500 that spends nothing. fleet's membership hook revokes the live
  keys the person minted there (`revoke_minted_by`, under the claim's liveness filter) and
  answers `Ok` on a failed write (`alert = membership_key_revoke`); a key no removal
  reached is revoked at its first use refused for not-a-member. Losing the permission
  while a member pauses a credential, never revokes it. Nothing is asked after the write.
- **A removed device that enrolls again starts from a new device's settings** (#3627).
  `AgentDao::revive` REPLACES the tombstone with `new_row` for the request and keeps
  only `_id`, `tenant_id`, `machine_id`, `created_at`, `enrolled_by`, `token_gen`,
  `desired_config.revision` and a `Quarantined` status (a restriction is never lifted by
  re-enrolling); `agent_revived` (network) first drops the device from the
  org's tunnel policies and releases a leftover lease. ⚠️ Never turn the revive back
  into a field-by-field `$set`: a grant field added later would come back with the
  machine id. ⚠️ `rehydrate` is live-only (`NotFound` on a tombstone), so a removal
  racing a live re-enrollment becomes a revive and is never undone. ⚠️ The revive's
  CAS names the exact `deleted_at` it read, so a stale read never writes a lower
  `token_gen` back.
- **A WG public key cannot be claimed by two live nodes in a network**
  (`wg_key_taken_by_other`, fail-CLOSED). Nothing proves possession of the private
  half, and the key is an *addressing* key — DERP authorizes registration against
  it, WireGuard keys peers by it.
- **SYSTEM acts below the machine's folders only through `machine_tree::HeldPath`**
  (FR-145): each folder opened below the handle above it, never through a link, its
  final path checked; what it makes sealed when made; a write is a fresh file renamed in
  by handle; a removal deletes what the same walk opened. Never check a path, then act on it.
- **Push endpoints are SSRF-validated at subscribe time** — the server POSTs to a
  browser-supplied URL from inside the cluster on every fan-out.
- **Uploads are sniffed, never trusted** (`crate::media_type::resolve`); the
  extension fallback structurally cannot produce `image/*` or `text/html`, which are
  exactly the types worth lying about.
- **The message-HTML allowlist is a security control** —
  `ui/src/composables/useMarkdown.ts` deliberately excludes `style`, and it is the
  ONLY XSS boundary for message content.
- **The CSP allowlist contains loopback origins on purpose** (`http://127.0.0.1:*`)
  — the RC viewer probes the local agent's loopback-TURN relay and clipboard bridge.
  ⚠️ When touching CSP, exercise the remote-control page, not just the main SPA.
- **Publishing identity is an allowlist at four layers.** This repo is **public**,
  and a commit's author email and a GitHub account login leak *who* rather than
  *what* — both invisible to the machine-name guards. ⚠️ `gh auth switch` is
  **global**; use `scripts/gh-scoped-config.sh`. ⚠️ Neither leak is recoverable
  downstream.
- **Prod refuses to boot on the default JWT secret**; session cookies carry `Secure`
  in production; CORS defaults to the frontend's own origin only; `tower_governor`
  caps 60 req/min per IP, and exec/SSH additionally have per-(caller, device)
  ceilings enforced **after** the identity gates so a refusal is attributable.
