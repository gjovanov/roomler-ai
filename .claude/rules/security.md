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
  Resolve the object **within** the tenant: `helpers::require_room_in_tenant` /
  `require_message_in_tenant`, so a foreign id 404s and leaks neither content nor
  existence. ⚠️ A handler keyed by `message_id` must use the **message** guard — the
  two ids are decoupled. ⚠️ Never re-fetch with a bare `find_by_id` after a
  tenant-scoped write.
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
- **Agent tokens are status-checked on every use** via
  `crates/api/src/extractors/agent.rs::AuthAgent`. ⚠️ A lookup **failure is 500, not
  401** — a Mongo blip must not tell a healthy fleet its credentials were revoked,
  turning a database wobble into an enrollment storm. ⚠️ **Deletion wins over
  status**: an Online-looking tombstone still refuses.
- **A WG public key cannot be claimed by two live nodes in a network**
  (`wg_key_taken_by_other`, fail-CLOSED). Nothing proves possession of the private
  half, and the key is an *addressing* key — DERP authorizes registration against
  it, WireGuard keys peers by it.
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
