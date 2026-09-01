# FR-52: Cross-org remote access — an outsider, a device password, and a server that cannot use it

**Issue:** [#1100](https://github.com/gjovanov/roomler-ai/issues/1100) ·
**Status:** proposed · **Owner:** remote-control (pillar 1) + control plane ·
**Anchors verified against master `46cdfc42`**

The gap TeamViewer and AnyDesk fill and roomler does not: someone **outside the
organization** views and controls a device, authorised by a password the device
holds — with consent, with an audit trail, and without the server ever being able
to learn or replay that password.

## Goal

A device owner can let a person who is **not a member of the device's org** open a
remote-desktop session against it, by giving them a **connect code** and a
**password**. Both the org and the device can refuse; the device has the last word;
every attempt, including every refusal, is recorded.

Four properties, stated as the acceptance bar rather than as a description:

1. **The server is not the gate.** A compromised or malicious control plane cannot
   open an external session, cannot recover the password, and cannot substitute
   itself for the device. This is the same property `exec_enabled` / `ssh_enabled` /
   `remote_config_enabled` provide, applied where it matters most — an outsider has
   no tenant membership behind them, so the password *is* the whole authorization.
2. **The outside door is harder than the inside one.** An internal controller passes
   org policy → a permission bit → a device allowlist → consent. An external one
   passes org policy → an admin approval → a device opt-in → a cryptographic proof →
   consent. Nothing about this feature may make the internal path more permissive.
3. **Both TeamViewer workflows, one mechanism.** Durable *unattended* access (the
   personal password) and *ad-hoc attended* support (a one-time code the host reads
   out) differ only in the lifetime of the secret and who initiates. They must not
   be two wires.
4. **An empty audit is never mistaken for an empty history.** Refusals are the
   load-bearing rows, as in `ssh_audit`.

## 1. The gap, measured in the code

Every remote-desktop session passes through **one** function:
`resolve_session_authz` — `crates/api/src/ws/remote_control.rs:1708`. After the
self-control shortcut, the whole of the cross-org story is three lines:

```rust
// crates/api/src/ws/remote_control.rs:1782
let perms = state.tenants
    .get_member_permissions(agent.tenant_id, controller_user_id)
    .await
    .unwrap_or(0);            // ← non-member ⇒ Forbidden ⇒ 0
...
// :1804
if !permissions::has(perms, permissions::REMOTE_CONTROL) {
    return Err("you don't have permission to control others' devices".to_string());
}
```

There is no other door. The org boundary is not a policy that can be relaxed with a
setting — it is the shape of the only authorization path. `docs/compare/vs-teamviewer.md:63`
already concedes the consequence in public: *"Attended support for a stranger's PC |
**no** — enrolment model | yes, session codes"*.

Two workflows sit behind the request, and they are not the same feature:

| | Unattended access | Ad-hoc attended support |
|---|---|---|
| Their name for it | personal password | QuickSupport |
| Secret | durable, owner-set | one-time, host-generated |
| Who is at the machine | nobody | the person asking for help |
| Initiated by | the outsider | the host |
| Risk | the higher of the two | the lower of the two |
| Consent default | prompt (owner may set auto) | pre-satisfied by the host's own act |

## 2. The constraint that decides the design

`CLAUDE.md` states it for exec, SSH, peer relays and remote config, and the code
enforces it: **the last gate is owned by the device and the server cannot write it.**
`remote_config_enabled` (`crates/agent-core/src/config.rs:138`) is not merely
defaulted-off — it is *structurally absent* from `DesiredConfig`, and
`crates/remote_control/src/models.rs:915` says so in as many words:

> ⚠️ `remote_config_enabled` is deliberately ABSENT and must never be added.

An external-access password is that class of thing, only more so. So:

### 2a. The design that fails

Browser POSTs the password → API compares an Argon2 hash stored on the `agents` row
→ API tells the agent to start the session.

This puts the gate in the server. A compromised server then opens **any** device in
the fleet; a database dump is a fleet-wide credential dump; and the one property
every neighbouring subsystem is built to preserve is inverted precisely where the
caller is a stranger. It must not ship, and it must be named in the spec so that a
later reviewer recognises it as the tempting shortcut rather than the obvious build.

### 2b. The design that holds

The password lives **only on the device**, in `config.toml`, under the same
atomic + fsync + `.prev` + 0600/ACL treatment `ssh_host_key` already gets
(`crates/agent-core/src/config.rs:199`) — and, exactly as with SSH, **if it cannot be
persisted the feature stays off** rather than running on a per-boot secret.
Verification is a handshake the server merely relays.

### 2c. There is no free permission bit

`VIEW_SSH_AUDIT` is `1 << 30` (`crates/db/src/models/role.rs:91`) and `ALL` is
`(1 << 31) - 1` (`:151`); the UI mask is a signed int32 with bit 30 as the ceiling
(#888). A `MANAGE_EXTERNAL_ACCESS` bit **is not available**. Admin approval is
therefore a *compound* of existing bits — `MANAGE_AGENTS` **+** `REMOTE_CONTROL` —
exactly as FR-19 used `MANAGE_AGENTS` + `EXEC_DEVICE`. Clearing an approval needs
only `MANAGE_AGENTS`: revocation is not a grant.

## 3. The gate chain

Five gates, each owned by a different party, each default-deny, every decision
audited. The order is load-bearing: an earlier refusal means the later gates are
never consulted, and never leak that they exist.

| # | Gate | Owner | Mechanism |
|---|---|---|---|
| 1 | Org kill-switch | org admin | `TenantSettings.external_rc_mode`, default `off` — the twin of `remote_exec_enabled` / `remote_ssh_enabled` (`crates/db/src/models/tenant.rs:128`) and deliberately separate from both. Off ⇒ a connect code does not resolve. |
| 2 | Per-device approval | org admin | `Agent.external_access_policy`, shaped on `PeerRelayPolicy` (`crates/remote_control/src/models.rs:1678`): default closed, set by `MANAGE_AGENTS` + `REMOTE_CONTROL`. Carries a **permission ceiling** (an org may allow external *view* without external *input*) and an optional expiry. |
| 3 | Device opt-in | device owner, locally | `external_access_enabled` in the agent's own config, default off, **absent from `DesiredConfig`**. Alongside: `external_consent_mode`, `external_max_permissions`. The refusal that survives a compromised server. |
| 4 | The password proof | device verifies, outsider proves | §4. The substitute for tenant membership, and the only gate the outsider can satisfy by their own action. |
| 5 | Host consent | whoever is at the machine | The existing `ConsentMode` path, resolved from `external_consent_mode` rather than `AccessPolicy.consent_mode` (`:430`), defaulting to `Prompt`. |

Gate 5 needs one wire addition: the prompt must say the controller is **outside the
organization**. `ServerMsg::Request` (`crates/remote_control/src/signaling.rs:1254`)
carries `tenant_name` for the multi-org case, which answers *which* org is asking —
not whether the asker is in one at all. A `controller_scope: external` field is
additive and serde-defaulted, so older agents keep today's prompt text; a device
that cannot say "outside your organization" must therefore not be gate-2 approvable,
which the `RpcCap` check in §5 enforces.

## 4. The handshake

The requirement is narrow: the party *brokering* the exchange must not be able to
learn the password, mount an offline attack on it, replay a captured proof, or
impersonate the device in order to harvest it. That is the textbook case for an
**augmented PAKE**.

**Decision: OPAQUE.** `opaque-ke` on the agent, `@cloudflare/opaque-ts` in the
browser — ristretto255, no big-integer modexp, both maintained. SRP-6a has the same
properties on paper and a long history of implementation footguns (parameter
validation, `B = 0`, group choice); the modexp would also land in the browser bundle.

| # | Outsider's browser | Server | Device |
|---|---|---|---|
| 1 | connect code + password | resolves code → agent; gates 1 + 2, quota, rate limit | — |
| 2 | sends `KE1` (blinded) | relays; learns nothing | gate 3; loads its OPAQUE record |
| 3 | — | relays; learns nothing | replies `KE2` |
| 4 | derives `K`; **verifies the device**; sends `KE3` | relays; learns nothing | derives `K`, verifies `KE3`, grants or refuses, **counts the failure locally** |
| 5 | puts `MAC(K, dtls_fingerprint)` on its SDP offer | forwards the offer as today | **refuses an offer whose fingerprint is not authenticated under `K`** |
| 6 | — | normal consent + session flow | gate 5 |

**Step 5 is the step that earns the claim.** Without it the server is still in the
middle of the media path and "the device verified the password" buys less than it
sounds like. With it, an authenticated session key is bound to the actual DTLS
transport, and a substituted peer fails closed.

**Registration is local and one-directional.** The owner sets the password on the
machine — `roomler rc password set`, or the desktop companion — and the device
computes and stores the OPAQUE record itself. The dashboard may show *set / not set*
and may **clear**; it can never **set**, because a password typed into a web form has
already crossed the server, which is the one thing this design exists to prevent.
(The same asymmetry as gate 2's clear-vs-approve, and for the same reason.)

### 4a. The weaker alternative, and why it is named rather than chosen

Argon2id in the browser plus `HMAC(K, nonce)` is far better than §2a and much less
work. It fails on two counts: the device must then store a **password-equivalent**
secret, and the relaying server gets an **offline cracking oracle** — guess a
password, derive, compute the tag over the nonce *it* chose, compare with the tag it
observed. Against a human-chosen password that is a real break, by exactly the party
this design excludes. It also gives the client no way to authenticate the device.

If it is ever shipped as a stopgap, the wire must carry `extauth_v` from the first
commit: retrofitting a PAKE afterwards means a forced password reset on every device
in the field.

### 4b. Failure counting belongs on the device

The backoff and lockout that make a 4-word password survivable are gate-4 state, so
they live where gate 4 lives. A server-side counter does not survive the threat this
gate exists for. Server-side per-(principal, code) ceilings ride
`crates/api/src/rate_limit.rs:52` as a *second* limit, not the only one.

## 5. Addressing: how an outsider names a device

An outsider cannot browse the org's device list and must not be able to. They
address the device by a **connect code** — a new field, never `agent_id`, which is an
internal key and an ObjectId (timestamp-prefixed, therefore partly predictable).

**Decision:** 12 characters of Crockford base32 grouped `XXXX-XXXX-XXXX` — 60 bits,
dictatable over a phone, no `I`/`L`/`O`/`U` to mishear. Live-scoped unique index on
the agent row, rotatable by owner or admin; **rotation is the revocation story** when
a code leaks.

Two properties of the resolution endpoint, both easy to get wrong:

- **It is not an existence oracle.** A code that does not exist, a device whose org
  has gate 1 off, and a device that is merely offline must produce the same response
  *and the same latency*. Otherwise the endpoint enumerates the fleet.
- **It is rate-limited before it is useful.** `rate_limit.rs` keys on
  `(caller, device)`; this needs a second keying that works *before* a caller is
  known — per source IP and per code globally.

The capability verb is `RpcCap::ExternalAccess` (`crates/remote_control/src/models.rs:271`),
matched by **equality, never prefix** — the `ssh` / `ssh-consent` rule. A device that
does not advertise it cannot be gate-2 approved, which is what keeps §3's
`controller_scope` prompt from being a promise an old agent silently breaks.

## 6. Five findings from reading master

### F1 — the hub keys controllers by `ObjectId`, so an anonymous principal forks everything

`Hub::register_controller(user_id: ObjectId)` (`crates/remote_control/src/hub.rs:563`),
`RemoteSession.controller_user_id: ObjectId` (`models.rs:2544`), the `remote_audit`
rows and the TURN credential all key on a real user id. A synthetic principal would
fork the session record, the audit, the credential and the rate limiter — and leave
*"someone controlled your machine"* in the log.

### F2 — a user with no org is already legal, so the fix costs one signup and no new type

`routes/auth.rs:108` creates a user and **no tenant**. "Free account, member of
nothing" needs no new identity type: the external controller is an ordinary user who
simply is not a member of the device's tenant. **Decision: an account is required**
for unattended access. For the P6 ad-hoc flow a guest principal minted from a
one-time code is acceptable, because the host is present and watching — the
`routes/consent.rs:20` public-capability route is the established pattern for it.

### F3 — TURN credentials key on the user id, so the meter must key on the *device's* tenant

`turn_creds::ice_servers_for(user_id, …)` (`crates/remote_control/src/turn_creds.rs:358`)
issues under the controller's id. An external controller therefore consumes the org's
relay capacity attributably — but they have **no tenant**, and an external session is
relay-heavy by construction (the outsider is not on the mesh). If FR-20's ledger keys
on the controller's tenant, every external session meters to nothing. **Verify before
P5, do not assume.**

### F4 — `local_relay` is validated as *an* overlay address, not as *the caller's*

`is_overlay_relay_ip` (`crates/remote_control/src/hub.rs:58`) accepts any
`100.64.0.0/10` or `fc00::/7` literal, and `hub.rs:1167` hands it to the agent as a
TURN URL. An external controller has no overlay presence, so for them the field must
be **rejected outright** — otherwise it is a probe into the org's mesh, dialled by
the agent, from outside the org.

### F5 — the unauthenticated capability route already exists

`routes/consent.rs:20` and the UI's `guest: true` `/consent/:token` route are a
working precedent for *the token is the capability*, with single-use resolution via a
CAS. P6 follows it rather than inventing one.

## 7. Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| P1 | Addressing + policy, **no access path**: connect code (unique, live-scoped, rotatable), `external_rc_mode`, `external_access_policy`, admin UI, `external_rc_audit` (90 d TTL). `decide()` returns `Result<Granted, DenyReason>` so one place records both arms. | `external_rc_mode = off` (default) | not started |
| P2 | Device-side credential: `external_access_enabled`, the OPAQUE record, `external_consent_mode`, `external_max_permissions`; `roomler rc password set\|clear\|status`; companion UI; `RpcCap::ExternalAccess` on the hello. Test asserts none of the keys can appear in a `DesiredConfig` push. | `external_access_enabled = false` (default) | not started |
| P3 | The handshake: `rc:extauth.*` frames, server as blind relay, agent-side verify + backoff. **Proven on loopback against the real agent first**, as FR-19's bind handshake was. | P2's flag; no client surface ships | not started |
| P4 | Session establishment: the external branch in `resolve_session_authz`, transport binding at offer time, external consent path, public `/connect` page. Permission ceiling enforced **at the agent**, not merely offered by the server. | revert the authz branch; gates 1–3 still refuse | not started |
| P5 | Visibility + accounting: owner notification on a first-ever external session by a principal and on repeated failures; audit UI beside `SshAuditSection`; per-principal revocation; relay bytes metered to the device's tenant (F3); a plan limit. | n/a — read-only surfaces | not started |
| P6 | Ad-hoc attended support: host generates a short one-time code from tray/CLI. **Same wire** — the one-time secret takes the password's place. | separate `external_rc_mode` value; independent of unattended access | not started |

## 8. Acceptance criteria

- [ ] With `external_rc_mode = off` (the default), a valid connect code + correct
      password yields the **same response and latency** as a code that does not exist.
- [ ] With gates 1 and 2 open but `external_access_enabled = false`, the session is
      refused **by the device**, and the refusal is audited.
- [ ] A server that is asked to start an external session **without** a client proof
      cannot: demonstrated by driving the mint path directly against a real agent.
- [ ] The password never appears in any server-side log, request body, or collection.
      Demonstrated by a capture of the full exchange.
- [ ] An SDP offer whose DTLS fingerprint is not authenticated under `K` is refused
      by the agent (negative arm run explicitly — a pass with no failing arm proves
      nothing).
- [ ] A `DesiredConfig` push carrying any `external_*` key is rejected, and a test
      asserts the fields cannot be serialised into one.
- [ ] An external controller's `local_relay` is rejected (F4), with a log line.
- [ ] The host consent prompt names the controller as **outside the organization**,
      verified on a real device with a real second account.
- [ ] Every refusal reason appears in `external_rc_audit`, and the audit read is
      gated on `VIEW_REMOTE_AUDIT`.
- [ ] Relay bytes from an external session appear against the **device's** org in the
      FR-20 ledger (F3), with a direct-path arm metering zero.
- [ ] Rotating a connect code invalidates the old one immediately.
- [ ] `docs/compare/vs-teamviewer.md:63` is updated — and only after the field run,
      not on merge.

## 9. Open decisions

1. **Does gate 2 apply when the device owner is also the org's only admin?** As
   specified, a solo owner clicks approve on their own device — friction with no
   safety gain. A `MANAGE_TENANT`-holder-owns-the-device shortcut is tempting and is
   exactly the shape of the FR-27 owner shortcut that made `consent_mode` invisible
   for a year. Default: **no shortcut**; revisit with evidence.
2. **Where does an external session appear in the device's own UI?** A session the
   org's admins cannot see would be worse than the gap it closes; a session that
   spams every admin is noise. Proposal: the existing session list, badged, plus a
   notification on the *first* session per principal only.
3. **Does an external principal get `FILES` / `CLIPBOARD` by default?** The internal
   default is `VIEW | INPUT | CLIPBOARD` (+ the grandfathered `FILES`). Proposal for
   external: `VIEW | INPUT` only, with the rest reachable through gate 2's ceiling.
4. **Plan tier.** This is the feature that replaces a paid TeamViewer seat
   (`docs/business-model.md`), so it is a pricing lever, not just a limit. Needs a
   decision before P5, not after.

## 10. Out of scope

- **Mobile device support** — the other half of what `vs-teamviewer.md` concedes.
  Unrelated mechanism, separate FR.
- **Unattended access without a device password** (e.g. a bearer link that grants
  control). It would put the gate back in the server.
- **Setting the password remotely.** §4. Deliberately impossible, not merely unbuilt.
- **Changing the internal authorization path.** Nothing here may loosen
  `resolve_session_authz` for org members.
- **Session recording of external sessions.** The `ssh_activity` reasoning applies:
  recording what an operator typed ships it off the host.

## 11. Related

- `docs/remote-control.md` — the subsystem this extends.
- `docs/compare/vs-teamviewer.md:63` — the gap, conceded in public.
- `docs/fr/FR-27-host-consent.md` (#854) — the consent surface chain gate 5 rides.
- `docs/fr/FR-19-peer-relays.md` (#805) — the four-gate shape and the
  `MANAGE_AGENTS` + second-bit compound this mirrors.
- `docs/roomler-ssh.md` — the device-held-credential precedent (`ssh_host_key`,
  `ssh_authorized_keys`, "if it cannot be persisted, stay off").
- `docs/remote-config.md` — why `external_*` keys are absent from `DesiredConfig`.
- `docs/fr/FR-20-relay-cost-metering.md` (#807) — F3.

## 12. Field-verification log

| date | version | what was tested | result |
|---|---|---|---|
| — | — | nothing yet — spec only | — |
